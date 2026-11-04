//! CLI 端到端集成测试：从 index 到各查询命令到 MCP 握手，全部通过真实二进制验证。

mod common;

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_codegraph")
}

struct TempRepo(std::path::PathBuf);
impl TempRepo {
    fn new() -> TempRepo {
        let dir = common::temp_root("cli");
        common::copy_fixture(&dir);
        TempRepo(dir)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempRepo {
    fn drop(&mut self) {
        common::cleanup(&self.0);
    }
}

fn run(args: &[&str], cwd: &std::path::Path) -> (i32, String) {
    let out = Command::new(bin())
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("启动 CLI 失败");
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), text)
}

#[test]
fn full_cli_walkthrough() {
    let repo = TempRepo::new();
    let root = repo.path().to_string_lossy().to_string();

    // 1) index
    let (code, out) = run(&["index", "--root", &root], repo.path());
    assert_eq!(code, 0, "index 失败: {out}");
    assert!(out.contains("索引完成"), "输出缺少构建摘要: {out}");

    // 2) who-calls charge：两个静态调用方
    let (code, out) = run(
        &["who-calls", "charge", "--json", "--root", &root],
        repo.path(),
    );
    assert_eq!(code, 0, "who-calls 失败: {out}");
    let v: Value = serde_json::from_str(out.trim()).expect("who-calls 输出不是 JSON");
    let callers: Vec<&str> = v["callers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["node"]["qualified_name"].as_str().unwrap())
        .collect();
    assert!(
        callers.contains(&"Checkout.createOrder"),
        "缺少 createOrder: {callers:?}"
    );
    assert!(
        callers.contains(&"RefundService.processRefund"),
        "缺少 processRefund: {callers:?}"
    );

    // 3) impact charge：两个直接调用方进入闭包
    let (code, out) = run(
        &["impact", "charge", "--json", "--root", &root],
        repo.path(),
    );
    assert_eq!(code, 0, "impact 失败: {out}");
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    let affected: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["node"]["qualified_name"].as_str().unwrap())
        .collect();
    assert!(
        affected.contains(&"Checkout.createOrder"),
        "impact 缺少 createOrder: {affected:?}"
    );
    assert!(
        affected.contains(&"RefundService.processRefund"),
        "impact 缺少 processRefund: {affected:?}"
    );
    // 证据链：每个受影响节点都应有以 charge 结尾的链
    for item in v["items"].as_array().unwrap() {
        assert!(
            item["chain"]
                .as_array()
                .unwrap()
                .iter()
                .any(|h| h["to"]["qualified_name"] == "charge"),
            "chain 应以 charge 结尾: {item}"
        );
    }
    // override 闭包：影响 Checkout.validate 应带出重写它的 RefundService.validate
    let (code, out) = run(
        &["impact", "Checkout.validate", "--json", "--root", &root],
        repo.path(),
    );
    assert_eq!(code, 0, "{out}");
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    let affected: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["node"]["qualified_name"].as_str().unwrap())
        .collect();
    assert!(
        affected.contains(&"RefundService.validate"),
        "override 闭包应含 RefundService.validate: {affected:?}"
    );

    // 4) trace createOrder → format
    let (code, out) = run(
        &[
            "trace",
            "Checkout.createOrder",
            "format",
            "--json",
            "--root",
            &root,
        ],
        repo.path(),
    );
    assert_eq!(code, 0, "trace 失败: {out}");
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["found"], json_bool(true), "trace 应找到路径");
    assert!(
        v["path"].as_array().unwrap().len() >= 2,
        "createOrder→charge→format 应为 2 跳: {v}"
    );

    // 4b) callees charge：下游是 log 和 format
    let (code, out) = run(
        &["callees", "charge", "--json", "--root", &root],
        repo.path(),
    );
    assert_eq!(code, 0, "callees 失败: {out}");
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    let targets: Vec<&str> = v["callees"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["node"]["qualified_name"].as_str().unwrap())
        .collect();
    assert!(
        targets.contains(&"format"),
        "callees 缺少 format: {targets:?}"
    );
    assert!(targets.contains(&"log"), "callees 缺少 log: {targets:?}");

    // 4c) dead-code 与 cycles 可执行
    let (code, _) = run(&["dead-code", "--json", "--root", &root], repo.path());
    assert_eq!(code, 0, "dead-code 失败");
    let (code, _) = run(&["cycles", "--json", "--root", &root], repo.path());
    assert_eq!(code, 0, "cycles 失败");

    // 5) search
    let (code, out) = run(
        &["search", "charge", "--json", "--root", &root],
        repo.path(),
    );
    assert_eq!(code, 0);
    assert!(out.contains("\"charge\""), "search 结果缺少 charge: {out}");

    // 6) stats
    let (code, out) = run(&["stats", "--json", "--root", &root], repo.path());
    assert_eq!(code, 0);
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert!(v["files"].as_i64().unwrap() >= 12, "文件数异常: {v}");

    // 7) export dot
    let dot_path = repo.path().join("graph.dot");
    let (code, out) = run(
        &[
            "export",
            "--out",
            &dot_path.to_string_lossy(),
            "--root",
            &root,
        ],
        repo.path(),
    );
    assert_eq!(code, 0, "export 失败: {out}");
    let dot = std::fs::read_to_string(&dot_path).unwrap();
    assert!(dot.contains("digraph codegraph"), "DOT 输出异常");

    // 8) 未找到符号 → 非零退出
    let (code, _) = run(
        &["who-calls", "definitely_not_a_symbol", "--root", &root],
        repo.path(),
    );
    assert_ne!(code, 0, "不存在的符号应报错");

    // 9) 歧义符号 → 报错并列出候选
    let (code, out) = run(&["who-calls", "handle", "--root", &root], repo.path());
    assert_ne!(code, 0, "歧义符号应报错");
    assert!(out.contains("同名候选"), "歧义报错应列出候选: {out}");
}

fn json_bool(b: bool) -> Value {
    Value::Bool(b)
}

#[test]
fn archaeology_on_git_history() {
    // 需要 git；没有则跳过（CI 与本机都有）
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("git 不可用，跳过考古测试");
        return;
    }
    let repo = TempRepo::new();
    let root_str = repo.path().to_string_lossy().to_string();
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(args)
            .output()
            .expect("git 失败")
    };
    // 构造两段历史：先提交初始版本，再修改 payment.ts 提交
    let payment = repo.path().join("src/services/payment.ts");
    std::fs::write(
        &payment,
        "import { format } from '@demo/utils/money';\n\nexport function charge(amount: number): string {\n  return format(amount);\n}\n",
    )
    .unwrap();
    let _ = git(&["init"]);
    let _ = git(&[
        "-c",
        "user.email=t@t.io",
        "-c",
        "user.name=alice",
        "add",
        ".",
    ]);
    let _ = git(&[
        "-c",
        "user.email=t@t.io",
        "-c",
        "user.name=alice",
        "commit",
        "-m",
        "init (#1)",
    ]);
    std::fs::write(
        &payment,
        "import { format } from '@demo/utils/money';\nimport { log } from '../utils/logger';\n\nexport function charge(amount: number): string {\n  log(`charging ${amount}`);\n  return format(amount);\n}\n\nexport function refund(amount: number): string {\n  return format(-amount);\n}\n",
    )
    .unwrap();
    let _ = git(&["-c", "user.email=t@t.io", "-c", "user.name=bob", "add", "."]);
    let _ = git(&[
        "-c",
        "user.email=t@t.io",
        "-c",
        "user.name=bob",
        "commit",
        "-m",
        "add refund flow (#2)",
    ]);

    let (code, out) = run(&["index", "--root", &root_str], repo.path());
    assert_eq!(code, 0, "{out}");
    let (code, out) = run(&["archaeology", "charge", "--root", &root_str], repo.path());
    assert_eq!(code, 0, "archaeology 失败: {out}");
    assert!(out.contains("alice"), "应包含初始作者 alice: {out}");
    assert!(out.contains("bob"), "应包含修改者 bob: {out}");
    assert!(out.contains("#2"), "应解析出 PR 引用 (#2): {out}");
}

#[test]
fn mcp_stdio_handshake_and_tool_call() {
    let repo = TempRepo::new();
    let root = repo.path().to_string_lossy().to_string();
    let (code, out) = run(&["index", "--root", &root], repo.path());
    assert_eq!(code, 0, "{out}");

    let mut child: Child = Command::new(bin())
        .args(["mcp", "--root", &root])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动 mcp 失败");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let read_line = |stdout: &mut BufReader<std::process::ChildStdout>| -> String {
        let mut line = String::new();
        stdout.read_line(&mut line).expect("读取 MCP 响应失败");
        line
    };

    // initialize
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"test","version":"0"}}}}}}"#
    )
    .unwrap();
    let resp = read_line(&mut stdout);
    assert!(
        resp.contains("\"protocolVersion\":\"2025-06-18\""),
        "initialize 响应异常: {resp}"
    );
    assert!(resp.contains("\"name\":\"codegraph\""), "{resp}");

    // initialized 通知（无响应）
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .unwrap();

    // tools/list
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}"#).unwrap();
    let resp = read_line(&mut stdout);
    let v: Value = serde_json::from_str(resp.trim()).unwrap();
    let tools: Vec<&str> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    let mut tools = tools;
    tools.sort_unstable();
    assert_eq!(
        tools,
        vec![
            "archaeology",
            "callees",
            "cycles",
            "dead_code",
            "hotspots",
            "impact",
            "search",
            "trace",
            "who_calls"
        ],
        "工具清单不符: {tools:?}"
    );

    // tools/call search
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"search","arguments":{{"query":"charge"}}}}}}"#
    )
    .unwrap();
    let resp = read_line(&mut stdout);
    let v: Value = serde_json::from_str(resp.trim()).unwrap();
    assert_eq!(v["id"], 3, "{resp}");
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert!(
        parsed
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["name"] == "charge"),
        "search 工具应返回 charge: {text}"
    );

    // tools/call who_calls
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"who_calls","arguments":{{"symbol":"charge"}}}}}}"#
    )
    .unwrap();
    let resp = read_line(&mut stdout);
    let v: Value = serde_json::from_str(resp.trim()).unwrap();
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("createOrder"),
        "who_calls 工具结果异常: {text}"
    );

    // 未知方法 → -32601
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":5,"method":"resources/list"}}"#
    )
    .unwrap();
    let resp = read_line(&mut stdout);
    assert!(resp.contains("-32601"), "未知方法应返回 -32601: {resp}");

    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
}
