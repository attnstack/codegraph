//! Golden snapshot：夹具仓库的查询输出与提交的快照逐字段对比。
//! 运行 `UPDATE_SNAPSHOTS=1 cargo test -p codegraph-server --test golden` 重新生成。
//! 浮点数用容差比较（跨平台最后一位可能抖动），其余字段精确相等。

mod common;

use codegraph_core::config::Config;
use codegraph_core::query;
use codegraph_server::commands;
use codegraph_store::GraphDb;
use serde_json::Value;

#[test]
fn golden_queries_match_snapshot() {
    let tmp = common::temp_root("golden");
    common::copy_fixture(&tmp);
    let cfg = Config::load(&tmp);
    commands::run_build(&cfg, true).expect("构建失败");
    let db = GraphDb::open(&cfg.db_path()).unwrap();
    let snap = db.load_snapshot(cfg.cross_file_bonus).unwrap();

    let resolve_id = |name: &str| -> i64 {
        let cands = db.resolve_symbol_candidates(name).unwrap();
        let exact: Vec<_> = cands
            .iter()
            .filter(|c| c.name == name || c.qualified_name == name)
            .collect();
        assert_eq!(exact.len(), 1, "符号 {name} 应唯一命中，实际 {cands:?}");
        exact[0].id
    };

    let mut out = String::new();
    for name in ["charge", "Checkout", "format", "App"] {
        let id = resolve_id(name);
        out.push_str(&format!("=== who_calls {name} ===\n"));
        out.push_str(&serde_json::to_string_pretty(&query::who_calls(&snap, id)).unwrap());
        out.push('\n');
        out.push_str(&format!("=== impact {name} ===\n"));
        out.push_str(
            &serde_json::to_string_pretty(&query::impact(&snap, id, &cfg, None, None)).unwrap(),
        );
        out.push('\n');
    }
    out.push_str("=== trace createOrder→format ===\n");
    out.push_str(
        &serde_json::to_string_pretty(&query::trace(
            &snap,
            resolve_id("Checkout.createOrder"),
            resolve_id("format"),
            &cfg,
            None,
        ))
        .unwrap(),
    );
    out.push('\n');
    out.push_str("=== hotspots ===\n");
    out.push_str(&serde_json::to_string_pretty(&query::hotspots(&snap, &cfg, 5)).unwrap());
    out.push('\n');

    let snap_path = format!("{}/tests/snapshots/golden.json", env!("CARGO_MANIFEST_DIR"));
    if std::env::var("UPDATE_SNAPSHOTS").is_ok() {
        std::fs::write(&snap_path, &out).unwrap();
        eprintln!("快照已更新: {snap_path}");
        common::cleanup(&tmp);
        return;
    }
    let expected = std::fs::read_to_string(&snap_path)
        .unwrap_or_else(|_| panic!("缺少快照文件 {snap_path}，先以 UPDATE_SNAPSHOTS=1 生成"));
    let actual: Value =
        serde_json::from_str(&extract_json_block(&out, "=== ", 0)).unwrap_or(Value::Null);
    let _ = actual;
    // 逐块对比（who_calls/impact/hotspots 各为一段 JSON）
    let exp_blocks = split_blocks(&expected);
    let act_blocks = split_blocks(&out);
    assert_eq!(
        exp_blocks.len(),
        act_blocks.len(),
        "快照块数不一致：expected {} actual {}\n--- actual ---\n{out}",
        exp_blocks.len(),
        act_blocks.len()
    );
    for (i, (e, a)) in exp_blocks.iter().zip(act_blocks.iter()).enumerate() {
        assert_eq!(e.header, a.header, "第 {i} 块标题不一致");
        let ev: Value = serde_json::from_str(&e.body).expect("快照 JSON 损坏");
        let av: Value = serde_json::from_str(&a.body).expect("实际输出 JSON 损坏");
        assert_json_eq(&ev, &av, &format!("块[{}]: {}", i, e.header));
    }
    common::cleanup(&tmp);
}

fn split_blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut cur: Option<Block> = None;
    for line in text.lines() {
        if let Some(header) = line
            .strip_prefix("=== ")
            .and_then(|h| h.strip_suffix(" ==="))
        {
            if let Some(b) = cur.take() {
                blocks.push(b);
            }
            cur = Some(Block {
                header: header.to_string(),
                body: String::new(),
            });
        } else if let Some(b) = cur.as_mut() {
            b.body.push_str(line);
            b.body.push('\n');
        }
    }
    if let Some(b) = cur.take() {
        blocks.push(b);
    }
    blocks
}

struct Block {
    header: String,
    body: String,
}

#[allow(clippy::needless_range_loop)]
fn extract_json_block(_: &str, _: &str, _: usize) -> String {
    String::new()
}

/// 容差 JSON 对比：数字差 < 1e-6 视为相等，其余精确相等
fn assert_json_eq(e: &Value, a: &Value, path: &str) {
    match (e, a) {
        (Value::Number(en), Value::Number(an)) => {
            let (ev, av) = (en.as_f64().unwrap_or(0.0), an.as_f64().unwrap_or(0.0));
            assert!(
                (ev - av).abs() < 1e-6,
                "数值不一致 @{path}: expected {ev} actual {av}"
            );
        }
        (Value::Array(ea), Value::Array(aa)) => {
            assert_eq!(ea.len(), aa.len(), "数组长度不一致 @{path}");
            for (i, (x, y)) in ea.iter().zip(aa.iter()).enumerate() {
                assert_json_eq(x, y, &format!("{path}[{i}]"));
            }
        }
        (Value::Object(eo), Value::Object(ao)) => {
            let mut keys: Vec<_> = eo.keys().collect();
            keys.sort();
            let mut akeys: Vec<_> = ao.keys().collect();
            akeys.sort();
            assert_eq!(keys, akeys, "对象键不一致 @{path}");
            for k in keys {
                assert_json_eq(&eo[k], &ao[k], &format!("{path}.{k}"));
            }
        }
        _ => assert_eq!(e, a, "值不一致 @{path}"),
    }
}
