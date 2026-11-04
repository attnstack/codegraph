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

/// Python 适配器端到端断言：导入解析（绝对/相对/子模块）、self 方法链、
/// 继承重写、歧义判定、import 环、私有孤儿函数——第二语言的插件化证明。
#[test]
fn golden_python_fixture() {
    use codegraph_core::model::EdgeKind;
    use std::path::Path;

    let manifest = env!("CARGO_MANIFEST_DIR");
    let fixture = Path::new(manifest).join("../../fixtures/demo-py");
    let tmp = common::temp_root("golden-py");
    common::copy_dir(&fixture, &tmp);
    let cfg = Config::load(&tmp);
    commands::run_build(&cfg, true).expect("python 构建失败");
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

    // who_calls charge：命名空间导入（payment.charge）与相对导入（.payment import charge）双路径
    let charge = resolve_id("charge");
    let wc = query::who_calls(&snap, charge);
    let callers: Vec<&str> = wc
        .callers
        .iter()
        .map(|c| c.node.qualified_name.as_str())
        .collect();
    assert!(
        callers.contains(&"Checkout.create_order"),
        "缺少 create_order: {callers:?}"
    );
    assert!(
        callers.contains(&"RefundService.process_refund"),
        "缺少 process_refund: {callers:?}"
    );
    // 每个调用方都有源码证据
    for c in &wc.callers {
        assert!(
            !c.sites.is_empty(),
            "调用方 {} 缺证据链",
            c.node.qualified_name
        );
    }

    // callees charge：format 与 log（具名导入 → 模块内符号）
    let callees = query::callees(&snap, charge);
    let targets: Vec<&str> = callees
        .callees
        .iter()
        .map(|c| c.node.qualified_name.as_str())
        .collect();
    assert!(
        targets.contains(&"format"),
        "callees 缺 format: {targets:?}"
    );
    assert!(targets.contains(&"log"), "callees 缺 log: {targets:?}");

    // override 闭包：python 的 self 方法 + 类继承链
    let validate = resolve_id("Checkout.validate");
    let imp = query::impact(&snap, validate, &cfg, None, None);
    assert!(
        imp.items
            .iter()
            .any(|i| i.node.qualified_name == "RefundService.validate"),
        "override 闭包应含 RefundService.validate: {imp:?}"
    );

    // trace：create_order → charge → format
    let tr = query::trace(
        &snap,
        resolve_id("Checkout.create_order"),
        resolve_id("format"),
        &cfg,
        None,
    );
    assert!(tr.found, "trace 应找到 create_order→format 的路径");

    // import 环：money ↔ logger
    let cy = query::cycles(&snap);
    assert!(
        cy.cycles
            .iter()
            .any(|c| c.contains(&"demo_py/utils/money.py".to_string())
                && c.contains(&"demo_py/utils/logger.py".to_string())),
        "应检出 money↔logger import 环: {:?}",
        cy.cycles
    );

    // 死代码：单下划线私有且无调用者
    let dc = query::dead_code(&snap, &cfg);
    assert!(
        dc.items
            .iter()
            .any(|n| n.qualified_name == "_orphan_helper"),
        "dead-code 应命中 _orphan_helper: {:?}",
        dc.items
            .iter()
            .map(|n| n.qualified_name.clone())
            .collect::<Vec<_>>()
    );

    // 歧义判定：两个同名 handle，broker 裸调用必须不猜
    let handles = db.resolve_symbol_candidates("handle").unwrap();
    assert!(handles.len() >= 2, "两个 handle 导出都应可见");
    let ref_row = snap
        .dynamic_by_name
        .get("handle")
        .expect("broker 的 handle 调用应记录在 dynamic 桶");
    assert!(!ref_row.is_empty());

    // 继承边存在
    let refund_cls = resolve_id("RefundService");
    let has_ext = snap
        .out
        .get(&refund_cls)
        .map(|es| es.iter().any(|e| e.kind == EdgeKind::Extends))
        .unwrap_or(false);
    assert!(has_ext, "RefundService 应有 extends 边");

    common::cleanup(&tmp);
}
