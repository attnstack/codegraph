//! MCP stdio 服务：手写的最小 JSON-RPC 2.0 子集（newline-delimited）。
//!
//! 为什么不用官方 Rust SDK（rmcp）（面试点，主动讲）：
//!   1) 我们只需要 3 个方法：initialize / tools/list / tools/call，
//!      协议面小到手写 200 行以内——依赖一整套 tokio+tower 是用大炮打蚊子；
//!   2) 单二进制零运行时依赖是本项目的分发卖点，SDK 会拖进整个 async 运行时；
//!   3) 正确性不靠「信任 SDK」而是靠 tests-ts/ 里官方 TypeScript SDK 做的
//!      契约测试——客户端用标准实现，服务端被它验证。
//!
//! 铁律：协议消息只走 stdout；一切日志走 stderr。混写是 MCP 实现最常见的事故。

use crate::commands;
use anyhow::Result;
use codegraph_core::config::Config;
use codegraph_store::GraphDb;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const SUPPORTED_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];
const LATEST_VERSION: &str = "2025-06-18";

struct McpState {
    cfg: Config,
}

impl McpState {
    fn db(&self) -> Result<(GraphDb, codegraph_core::model::Snapshot)> {
        commands::open_for_query(&self.cfg)
    }
}

pub fn serve(cfg: Config) -> Result<()> {
    let state = McpState { cfg };
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut buf = String::new();

    loop {
        buf.clear();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            break; // EOF：客户端关闭
        }
        let line = buf.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                send(
                    &mut out,
                    &error_response(Value::Null, -32700, &format!("parse error: {e}")),
                )?;
                continue;
            }
        };
        let id = v.get("id").cloned().filter(|i| !i.is_null());
        let method = v
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let is_request = id.is_some();

        let response = match method.as_str() {
            "initialize" if is_request => {
                let client_version = v
                    .pointer("/params/protocolVersion")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let negotiated = if SUPPORTED_VERSIONS.contains(&client_version) {
                    client_version.to_string()
                } else {
                    tracing::warn!(
                        "MCP: 客户端协议版本 {client_version} 未知，回落到 {LATEST_VERSION}"
                    );
                    LATEST_VERSION.to_string()
                };
                Ok(json!({
                    "protocolVersion": negotiated,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": {
                        "name": "codegraph",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }))
            }
            "ping" if is_request => Ok(json!({})),
            "tools/list" if is_request => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" if is_request => {
                let name = v
                    .pointer("/params/name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = v.pointer("/params/arguments").cloned().unwrap_or(json!({}));
                match call_tool(&state, &name, &args) {
                    Ok(text) => Ok(json!({
                        "content": [{ "type": "text", "text": text }],
                        "isError": false
                    })),
                    Err(e) => {
                        // 工具级错误按 MCP 约定以 isError 结果返回，而非协议错误
                        Ok(json!({
                            "content": [{ "type": "text", "text": format!("error: {e}") }],
                            "isError": true
                        }))
                    }
                }
            }
            // 通知（无 id）：initialized / cancelled 等直接忽略
            _ if !is_request => continue,
            _ => Err((-32601, format!("method not found: {method}"))),
        };

        match response {
            Ok(result) => send(
                &mut out,
                &json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result }),
            )?,
            Err((code, msg)) => send(
                &mut out,
                &error_response(id.unwrap_or(Value::Null), code, &msg),
            )?,
        }
    }
    Ok(())
}

fn call_tool(state: &McpState, name: &str, args: &Value) -> Result<String> {
    let get = |k: &str| args.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    let get_usize = |k: &str| args.get(k).and_then(|v| v.as_u64()).map(|v| v as usize);
    match name {
        "who_calls" => {
            let symbol = get("symbol").ok_or_else(|| anyhow::anyhow!("缺少参数 symbol"))?;
            let (db, snap) = state.db()?;
            let start = commands::resolve_unique(&db, &symbol)?;
            let r = codegraph_core::query::who_calls(&snap, start.id);
            Ok(serde_json::to_string_pretty(&r)?)
        }
        "impact" => {
            let symbol = get("symbol").ok_or_else(|| anyhow::anyhow!("缺少参数 symbol"))?;
            let (db, snap) = state.db()?;
            let start = commands::resolve_unique(&db, &symbol)?;
            let r = codegraph_core::query::impact(
                &snap,
                start.id,
                &state.cfg,
                get_usize("depth"),
                get_usize("max_nodes"),
            );
            Ok(serde_json::to_string_pretty(&r)?)
        }
        "trace" => {
            let entry = get("entry").ok_or_else(|| anyhow::anyhow!("缺少参数 entry"))?;
            let sink = get("sink").ok_or_else(|| anyhow::anyhow!("缺少参数 sink"))?;
            let (db, snap) = state.db()?;
            let e = commands::resolve_unique(&db, &entry)?;
            let s = commands::resolve_unique(&db, &sink)?;
            let r = codegraph_core::query::trace(&snap, e.id, s.id, &state.cfg, get_usize("depth"));
            Ok(serde_json::to_string_pretty(&r)?)
        }
        "hotspots" => {
            let (_, snap) = state.db()?;
            let top = get_usize("top_k").unwrap_or(10);
            let items = codegraph_core::query::hotspots(&snap, &state.cfg, top);
            Ok(serde_json::to_string_pretty(&items)?)
        }
        "search" => {
            let query = get("query").ok_or_else(|| anyhow::anyhow!("缺少参数 query"))?;
            let (db, _) = state.db()?;
            let hits =
                db.search_nodes(&query, get_usize("limit").unwrap_or(state.cfg.search_limit))?;
            let v: Vec<Value> = hits
                .iter()
                .map(|(n, m)| {
                    let mut obj = serde_json::to_value(n)?;
                    obj["match"] = Value::String(m.clone());
                    Ok(obj)
                })
                .collect::<Result<_, serde_json::Error>>()?;
            Ok(serde_json::to_string_pretty(&v)?)
        }
        "archaeology" => {
            let target = get("target").ok_or_else(|| anyhow::anyhow!("缺少参数 target"))?;
            let r = commands::archaeology(&state.cfg, &target)?;
            Ok(serde_json::to_string_pretty(&r)?)
        }
        other => Err(anyhow::anyhow!("未知工具: {other}")),
    }
}

fn tool_definitions() -> Vec<Value> {
    fn schema(props: Value, required: &[&str]) -> Value {
        json!({
            "type": "object",
            "properties": props,
            "required": required
        })
    }
    vec![
        json!({
            "name": "who_calls",
            "description": "查询某个符号的全部调用方（静态可确认），附每个调用点的源码位置；无法静态消歧的动态调用点单独列出",
            "inputSchema": schema(json!({ "symbol": { "type": "string", "description": "符号名或 文件路径::符号名" } }), &["symbol"])
        }),
        json!({
            "name": "impact",
            "description": "影响面分析：反向调用闭包（排除测试/生成代码），按风险分数排序，每个节点附完整证据调用链与动态调用点",
            "inputSchema": schema(json!({
                "symbol": { "type": "string" },
                "depth": { "type": "integer", "description": "闭包深度上限，默认 8" },
                "max_nodes": { "type": "integer", "description": "节点数上限，默认 500" }
            }), &["symbol"])
        }),
        json!({
            "name": "trace",
            "description": "链路追踪：从入口符号到目标符号（如 DB/外部调用）的最短调用链（双向 BFS）",
            "inputSchema": schema(json!({
                "entry": { "type": "string" },
                "sink": { "type": "string" },
                "depth": { "type": "integer", "description": "单侧限深，默认 5" }
            }), &["entry", "sink"])
        }),
        json!({
            "name": "hotspots",
            "description": "加权 PageRank 计算的核心函数（跨文件调用加权），用于新人上手与重构风险提示",
            "inputSchema": schema(json!({ "top_k": { "type": "integer" } }), &[])
        }),
        json!({
            "name": "search",
            "description": "符号搜索：精确匹配 + 前缀匹配",
            "inputSchema": schema(json!({
                "query": { "type": "string" },
                "limit": { "type": "integer" }
            }), &["query"])
        }),
        json!({
            "name": "archaeology",
            "description": "代码考古：git blame 定位符号/行区间的引入 commit、作者贡献度与 PR 关联",
            "inputSchema": schema(json!({
                "target": { "type": "string", "description": "符号名，或 path:start-end / path:line" }
            }), &["target"])
        }),
    ]
}

fn send(out: &mut impl Write, v: &Value) -> Result<()> {
    let mut s = serde_json::to_string(v)?;
    s.push('\n');
    out.write_all(s.as_bytes())?;
    out.flush()?;
    Ok(())
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}
