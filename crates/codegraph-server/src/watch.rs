//! `codegraph watch`：开发态文件监听，变更自动增量重建。
//!
//! 触发与失效是两件事（方案审查时的结论）：失效机制靠 hash 差异（build 内部），
//! watcher 只是把「手动敲 update」变成「保存即触发」。
//! 防抖 600ms：编辑器原子写常伴随多事件，等风暴过去再重建一次。

use crate::commands;
use anyhow::Result;
use codegraph_core::config::Config;
use codegraph_core::parse::Lang;
use notify::Watcher;
use std::time::{Duration, Instant};

/// 事件是否值得触发重建：只有源码文件、且不在排除目录（.codegraph/target 等）里才算。
/// 用语言扩展名判定而不是全量事件——DB 自身的写盘会反向触发重建死循环。
fn is_relevant(path: &std::path::Path, cfg: &Config) -> bool {
    let s = path.to_string_lossy().replace('\\', "/");
    for d in &cfg.exclude_dirs {
        if s.contains(&format!("/{d}/")) || s.ends_with(&format!("/{d}")) {
            return false;
        }
    }
    Lang::from_path(&s).is_some()
}

pub fn run_watch(cfg: &Config) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;
    watcher.watch(&cfg.root, notify::RecursiveMode::Recursive)?;

    let t0 = std::time::Instant::now();
    commands::run_build(cfg, false)?;
    println!(
        "✓ 初始索引完成（{}ms），监听 {} —— 修改文件自动重建，Ctrl-C 退出",
        t0.elapsed().as_millis(),
        cfg.root.display()
    );

    let debounce = Duration::from_millis(600);
    let mut pending = false;
    let mut last_change = Instant::now();

    loop {
        match rx.recv_timeout(Duration::from_millis(300)) {
            Ok(Ok(event)) => {
                if event.paths.iter().any(|p| is_relevant(p, cfg)) {
                    pending = true;
                    last_change = Instant::now();
                }
            }
            Ok(Err(e)) => tracing::warn!("watch 事件错误: {e}"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break, // 通道关闭 = watcher 掉线
        }
        if pending && last_change.elapsed() >= debounce {
            pending = false;
            match commands::run_build(cfg, false) {
                Ok(stats) => {
                    println!(
                        "[{:>8.1}s] 重建完成: {} 变更 / {} 删除，{} 节点 {} 边",
                        t0.elapsed().as_secs_f32(),
                        stats.changed,
                        stats.deleted,
                        stats.nodes,
                        stats.edges
                    );
                }
                Err(e) => eprintln!("重建失败（保留旧图，下个变更重试）: {e:#}"),
            }
        }
    }
    Ok(())
}
