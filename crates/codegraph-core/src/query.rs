//! 高层查询：who_calls / impact / trace / hotspots / search。
//! 输入是 store 载好的内存快照（Snapshot），输出是可序列化 DTO——
//! CLI 与 MCP 两个出口共享同一套查询语义，保证口径一致。

use crate::config::Config;
use crate::model::*;
use crate::rank::{bidirectional_trace, impact_closure, pagerank, reconstruct_chain, ImpactLimits};
use std::collections::HashMap;

pub fn who_calls(snap: &Snapshot, start_id: i64) -> WhoCallsReport {
    let mut callers: Vec<Caller> = Vec::new();
    if let Some(edges) = snap.inn.get(&start_id) {
        for e in edges {
            if e.kind != EdgeKind::Calls {
                continue;
            }
            let Some(node) = snap.node(e.other).cloned() else {
                continue;
            };
            let sites = snap
                .evidence
                .get(&(e.other, start_id))
                .cloned()
                .unwrap_or_default();
            callers.push(Caller { node, sites });
        }
    }
    callers.sort_by(|a, b| a.node.qualified_name.cmp(&b.node.qualified_name));
    let dynamic_callers = dyn_hits(snap, start_id);
    WhoCallsReport {
        symbol: snap
            .node(start_id)
            .cloned()
            .unwrap_or_else(|| NodeRef::placeholder(start_id)),
        callers,
        dynamic_callers,
    }
}

fn dyn_hits(snap: &Snapshot, start_id: i64) -> Vec<DynamicRef> {
    match snap.nodes.get(&start_id) {
        Some(n) => snap
            .dynamic_by_name
            .get(&n.name)
            .cloned()
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

pub fn impact(
    snap: &Snapshot,
    start_id: i64,
    cfg: &Config,
    depth: Option<usize>,
    max_nodes: Option<usize>,
) -> ImpactReport {
    let limits = ImpactLimits {
        depth: depth.unwrap_or(cfg.impact_depth),
        max_nodes: max_nodes.unwrap_or(cfg.impact_max_nodes),
        decay: cfg.risk_decay,
    };
    let (entries, truncated) = impact_closure(snap, start_id, &limits, cfg, |p| cfg.is_pruned(p));

    // parent 指针表：chain 重建用
    let mut parent_of: HashMap<i64, (i64, EdgeKind)> = HashMap::new();
    for e in &entries {
        if let Some((p, k)) = e.parent {
            parent_of.insert(e.node, (p, k));
        }
    }

    let items: Vec<ImpactItem> = entries
        .iter()
        .map(|e| ImpactItem {
            node: snap
                .node(e.node)
                .cloned()
                .unwrap_or_else(|| NodeRef::placeholder(e.node)),
            risk: (e.risk * 1000.0).round() / 1000.0,
            hops: e.hops,
            chain: reconstruct_chain(snap, start_id, e.node, &parent_of),
        })
        .collect();

    let start_node = snap.node(start_id);
    let dynamic = start_node
        .map(|n| {
            snap.dynamic_by_name
                .get(&n.name)
                .cloned()
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let affected_files = start_node
        .map(|n| snap.importers_of(n.file.as_deref()))
        .unwrap_or_default();

    ImpactReport {
        symbol: start_node
            .cloned()
            .unwrap_or_else(|| NodeRef::placeholder(start_id)),
        items,
        dynamic,
        affected_files,
        truncated,
    }
}

pub fn trace(
    snap: &Snapshot,
    entry_id: i64,
    sink_id: i64,
    cfg: &Config,
    depth: Option<usize>,
) -> TraceReport {
    let d = depth.unwrap_or(cfg.trace_depth);
    let path = bidirectional_trace(snap, entry_id, sink_id, d);
    let (found, hops) = match path {
        Some(p) if p.len() >= 2 => {
            let mut chain = Vec::new();
            for w in p.windows(2) {
                let (from, to) = (w[0], w[1]);
                let site = snap
                    .evidence
                    .get(&(from, to))
                    .and_then(|sites| sites.first().cloned())
                    .map(|s| CallSite {
                        file: s.file,
                        line: s.line,
                    });
                chain.push(ChainHop {
                    edge_kind: EdgeKind::Calls.as_str().to_string(),
                    from: snap
                        .node(from)
                        .cloned()
                        .unwrap_or_else(|| NodeRef::placeholder(from)),
                    to: snap
                        .node(to)
                        .cloned()
                        .unwrap_or_else(|| NodeRef::placeholder(to)),
                    site,
                });
            }
            (true, chain)
        }
        Some(_) => (true, Vec::new()), // entry == sink
        None => (false, Vec::new()),
    };
    TraceReport {
        entry: snap
            .node(entry_id)
            .cloned()
            .unwrap_or_else(|| NodeRef::placeholder(entry_id)),
        sink: snap
            .node(sink_id)
            .cloned()
            .unwrap_or_else(|| NodeRef::placeholder(sink_id)),
        found,
        path: hops,
    }
}

pub fn hotspots(snap: &Snapshot, cfg: &Config, top_k: usize) -> Vec<HotspotItem> {
    let mut ranks = pagerank(
        snap,
        cfg.pagerank_damping,
        cfg.pagerank_tolerance,
        cfg.pagerank_max_iter,
        cfg.cross_file_bonus,
    );
    ranks.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranks
        .into_iter()
        .take(top_k)
        .map(|(id, score)| HotspotItem {
            node: snap
                .node(id)
                .cloned()
                .unwrap_or_else(|| NodeRef::placeholder(id)),
            score: (score * 1e6).round() / 1e6,
        })
        .collect()
}
