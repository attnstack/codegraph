//! tsc 类型消歧 sidecar 驱动：把 `node + typescript` 的类型信息接进解析。
//!
//! 定位：可选的第四级解析。仓库里有 node_modules/typescript 且 node 可用时，
//! `codegraph index --types` 会把成员调用点喂给 sidecar，拿回「接收者类型的
//! 声明文件 + 类型名」，resolve 层据此把原本 dynamic 的调用升级为 resolved。
//! 单二进制不受影响：脚本内嵌在二进制里，运行时解压到临时目录。

use anyhow::{anyhow, Context, Result};
use codegraph_core::config::Config;
use codegraph_core::parse::Lang;
use codegraph_core::view::ViewRepo;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write as _};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};

pub const SIDECAR_SCRIPT: &str = include_str!("typescript_sidecar.mjs");

use codegraph_core::resolve::{TypeHintEntry, TypeHints};

struct Sidecar {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    script_path: PathBuf,
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.script_path);
    }
}

/// 环境是否具备：node 在 PATH 上 + 目标仓库装了 typescript
pub fn available(cfg: &Config) -> bool {
    let node_ok = Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let ts_ok = cfg
        .root
        .join("node_modules")
        .join("typescript")
        .join("lib")
        .join("typescript.js")
        .exists();
    node_ok && ts_ok
}

fn spawn_sidecar(root: &std::path::Path) -> Result<Sidecar> {
    let script_path = std::env::temp_dir().join(format!(
        "codegraph-sidecar-{}-{}.mjs",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ));
    std::fs::write(&script_path, SIDECAR_SCRIPT)?;
    let mut child = Command::new("node")
        .arg(&script_path)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("启动 node sidecar 失败")?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("sidecar stdin 缺失"))?;
    let stdout = BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("sidecar stdout 缺失"))?,
    );
    Ok(Sidecar {
        child,
        stdin,
        stdout,
        script_path,
    })
}

impl Sidecar {
    fn send(&mut self, msg: &serde_json::Value) -> Result<()> {
        self.stdin.write_all(msg.to_string().as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn read_line_json(&mut self) -> Result<serde_json::Value> {
        let mut line = String::new();
        self.stdout.read_line(&mut line)?;
        if line.is_empty() {
            return Err(anyhow!("sidecar 提前退出"));
        }
        Ok(serde_json::from_str(line.trim())?)
    }

    fn request(&mut self, msg: &serde_json::Value) -> Result<serde_json::Value> {
        self.send(msg)?;
        let resp = self.read_line_json()?;
        if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            return Err(anyhow!(
                "sidecar 错误: {}",
                resp.get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
            ));
        }
        Ok(resp)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SidecarCall {
    file: String,
    line: u32,
    prop: String,
    recv_type: String,
    recv_file: Option<String>,
    #[allow(dead_code)]
    recv_line: Option<u32>,
}

/// 从视图收集成员调用点，跑 sidecar，产出类型提示。
/// 只查「接收者未知」的成员调用——this/self 链和绑定命中不需要 tsc。
pub fn collect_hints(cfg: &Config, view: &ViewRepo) -> Result<TypeHints> {
    let mut files: Vec<String> = Vec::new();
    let mut site_count = 0usize;
    for vf in &view.files {
        if vf.lang != Lang::Ts && vf.lang != Lang::Tsx {
            continue;
        }
        let has_member_calls = vf.refs.iter().any(|r| {
            r.kind == codegraph_core::parse::RawRefKind::Call
                && r.receiver.as_deref().map(|x| x != "this").unwrap_or(false)
        });
        if !has_member_calls {
            continue;
        }
        files.push(vf.path.clone());
        site_count += vf
            .refs
            .iter()
            .filter(|r| r.receiver.as_deref().map(|x| x != "this").unwrap_or(false))
            .count();
    }
    if files.is_empty() {
        return Ok(TypeHints::default());
    }

    let mut sidecar = spawn_sidecar(&cfg.root)?;
    sidecar.request(&serde_json::json!({
        "cmd": "init",
        "root": cfg.root.to_string_lossy(),
        "tsPath": cfg.root.join("node_modules").join("typescript").to_string_lossy(),
    }))?;
    let resp = sidecar.request(&serde_json::json!({ "cmd": "scan", "files": files }))?;
    let calls: Vec<SidecarCall> = serde_json::from_value(resp["calls"].clone())?;

    let mut map = HashMap::new();
    for c in calls {
        let Some(rf) = c.recv_file else { continue };
        // FQN 去泛型取末段："demo.Checkout<string>" → "Checkout"
        let no_generic = c.recv_type.split('<').next().unwrap_or(&c.recv_type);
        let recv_name = no_generic
            .rsplit('.')
            .next()
            .unwrap_or(no_generic)
            .to_string();
        if recv_name.is_empty() {
            continue;
        }
        map.insert(
            (c.file, c.line, c.prop),
            TypeHintEntry {
                recv_file: rf,
                recv_name,
            },
        );
    }
    tracing::info!(
        "tsc sidecar: 扫描 {} 文件 / {} 调用点，{} 条类型提示",
        files.len(),
        site_count,
        map.len()
    );
    Ok(TypeHints { map })
}
