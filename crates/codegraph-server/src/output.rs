//! 输出格式化：人类可读（默认）与 JSON（--json）双模。
//! JSON 是给脚本和 MCP 的稳定契约；人类格式面向 5 分钟演示。

use crate::commands::{ArchReport, BuildStatsPublic};
use anyhow::Result;
use codegraph_core::model::{HotspotItem, ImpactReport, TraceReport, WhoCallsReport};
use codegraph_store::Stats;

pub fn print_build_stats(s: &BuildStatsPublic, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(s)?);
        return Ok(());
    }
    println!(
        "索引完成: {} 个文件（变更 {}，删除 {}），{} 节点 / {} 引用 / {} 边，用时 {}ms",
        s.files, s.changed, s.deleted, s.nodes, s.refs, s.edges, s.duration_ms
    );
    Ok(())
}

fn loc(node: &codegraph_core::model::NodeRef) -> String {
    match (&node.file, node.line) {
        (Some(f), Some(l)) => format!("{f}:{l}"),
        (Some(f), None) => f.clone(),
        _ => "<external>".into(),
    }
}

pub fn print_who_calls(r: &WhoCallsReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(r)?);
        return Ok(());
    }
    let s = &r.symbol;
    println!("`{}` ({}) {}", s.qualified_name, s.kind, loc(s));
    if r.callers.is_empty() {
        println!("  没有静态可确认的调用方。");
    } else {
        println!("调用方（{} 个，按限定名排序）:", r.callers.len());
        for c in &r.callers {
            println!("  ◂ {}  {}", c.node.qualified_name, loc(&c.node));
            for site in &c.sites {
                println!("      调用点 {}:{} (calls)", site.file, site.line);
            }
        }
    }
    if !r.dynamic_callers.is_empty() {
        println!(
            "动态调用点（{} 个，无法静态消歧，仅供参考）:",
            r.dynamic_callers.len()
        );
        for d in r.dynamic_callers.iter().take(10) {
            println!("  ?  {}  {}:{}  [{}]", d.name, d.file, d.line, d.status);
        }
    }
    Ok(())
}

pub fn print_impact(r: &ImpactReport, json: bool, show_dynamic: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(r)?);
        return Ok(());
    }
    let s = &r.symbol;
    println!(
        "影响面分析（{} 个受影响节点{}）— 起点 `{}` ({})",
        r.items.len(),
        if r.truncated { "，已截断" } else { "" },
        s.qualified_name,
        loc(s)
    );
    for (i, item) in r.items.iter().enumerate() {
        println!(
            " {:>2}. ▲ risk {:.3}  {}  {}  [{} 跳]",
            i + 1,
            item.risk,
            item.node.qualified_name,
            loc(&item.node),
            item.hops
        );
        let chain: Vec<String> = item
            .chain
            .iter()
            .map(|h| {
                let site = h
                    .site
                    .as_ref()
                    .map(|s| format!(" ({}:{})", s.file, s.line))
                    .unwrap_or_default();
                format!(
                    "{} → {}{}",
                    h.from.qualified_name, h.to.qualified_name, site
                )
            })
            .collect();
        if !chain.is_empty() {
            println!("       链: {}", chain.join(" ; "));
        }
    }
    if !r.affected_files.is_empty() {
        println!(
            "受影响文件（import 传播，{} 个）: {}",
            r.affected_files.len(),
            r.affected_files.join(", ")
        );
    }
    if !r.dynamic.is_empty() {
        if show_dynamic {
            println!("动态调用点（{} 个）:", r.dynamic.len());
            for d in r.dynamic.iter().take(20) {
                println!("  ?  {}  {}:{}  [{}]", d.name, d.file, d.line, d.status);
            }
        } else {
            println!(
                "另有 {} 个动态调用点无法静态消歧（--show-dynamic 展开）",
                r.dynamic.len()
            );
        }
    }
    Ok(())
}

pub fn print_trace(r: &TraceReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(r)?);
        return Ok(());
    }
    if !r.found {
        println!(
            "在限深内未找到 `{}` → `{}` 的调用链（可用 --depth 放宽）",
            r.entry.qualified_name, r.sink.qualified_name
        );
        return Ok(());
    }
    println!(
        "调用链（{} 跳）: `{}` → `{}`",
        r.path.len(),
        r.entry.qualified_name,
        r.sink.qualified_name
    );
    for h in &r.path {
        let site = h
            .site
            .as_ref()
            .map(|s| format!("  [{}:{}]", s.file, s.line))
            .unwrap_or_default();
        println!(
            "  {}  --{}-->  {}{}",
            h.from.qualified_name, h.edge_kind, h.to.qualified_name, site
        );
    }
    Ok(())
}

pub fn print_hotspots(items: &[HotspotItem], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(items)?);
        return Ok(());
    }
    println!("核心函数（加权 PageRank Top {}）:", items.len());
    for (i, h) in items.iter().enumerate() {
        println!(
            " {:>2}. {:<8.6}  {}  {}",
            i + 1,
            h.score,
            h.node.qualified_name,
            loc(&h.node)
        );
    }
    Ok(())
}

pub fn print_search(hits: &[(codegraph_core::model::NodeRef, String)], json: bool) -> Result<()> {
    if json {
        let v: Vec<serde_json::Value> = hits
            .iter()
            .map(|(n, m)| {
                let mut obj = serde_json::to_value(n)?;
                obj["match"] = serde_json::Value::String(m.clone());
                Ok(obj)
            })
            .collect::<Result<_, serde_json::Error>>()?;
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!("搜索结果（{} 个）:", hits.len());
    for (n, m) in hits {
        println!("  [{m}] {}  {}  {}", n.qualified_name, n.kind, loc(n));
    }
    Ok(())
}

pub fn print_archaeology(r: &ArchReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(r)?);
        return Ok(());
    }
    println!(
        "考古 `{}` — {}:{}-{}（{} 行）",
        r.target, r.file, r.line_start, r.line_end, r.total_lines
    );
    println!("谁最懂这块代码:");
    for (author, lines) in r.authors.iter().take(5) {
        let pct = 100usize
            .checked_mul(*lines)
            .and_then(|x| x.checked_div(r.total_lines))
            .unwrap_or(0);
        println!("  {}  {} 行 ({}%)", author, lines, pct);
    }
    println!("引入 commit（按存活行数排序）:");
    for c in r.commits.iter().take(8) {
        let pr =
            c.pr.as_deref()
                .map(|p| format!(" {}", p))
                .unwrap_or_default();
        println!(
            "  {}  {} 行  {} ({}) {}「{}」",
            c.sha, c.lines, c.author, c.date, pr, c.summary
        );
    }
    println!("注: {}", r.note);
    Ok(())
}

pub fn print_stats(s: &Stats, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(s)?);
        return Ok(());
    }
    println!(
        "文件 {}（partial {}）| 节点 {}（外部包 {}）| 边 {} | 引用 {} | 库大小 {:.1} KB",
        s.files,
        s.partial_files,
        s.nodes,
        s.externals,
        s.edges,
        s.refs,
        s.db_bytes as f64 / 1024.0
    );
    println!("节点分布: {:?}", s.by_node_kind);
    println!("边分布:   {:?}", s.by_edge_kind);
    println!("引用状态: {:?}", s.by_ref_status);
    Ok(())
}
