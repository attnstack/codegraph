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

/// 谁的下游：这个符号调用了谁（who_calls 的镜像），每条出边附调用点证据。
pub fn callees(snap: &Snapshot, start_id: i64) -> CalleesReport {
    let mut callees: Vec<Caller> = Vec::new();
    if let Some(edges) = snap.out.get(&start_id) {
        for e in edges {
            if e.kind != EdgeKind::Calls {
                continue;
            }
            let Some(node) = snap.node(e.other).cloned() else {
                continue;
            };
            let sites = snap
                .evidence
                .get(&(start_id, e.other))
                .cloned()
                .unwrap_or_default();
            callees.push(Caller { node, sites });
        }
    }
    callees.sort_by(|a, b| a.node.qualified_name.cmp(&b.node.qualified_name));
    CalleesReport {
        symbol: snap
            .node(start_id)
            .cloned()
            .unwrap_or_else(|| NodeRef::placeholder(start_id)),
        callees,
    }
}

/// 孤儿函数/方法：没有任何静态调用者、也没被重写指向，且按语言约定不属于对外 API。
/// - TS：非导出（is_exported=false）
/// - Python：单下划线开头的私有约定（`__init__` 等 dunder 是隐式调用，排除）
pub fn dead_code(snap: &Snapshot, cfg: &Config) -> DeadCodeReport {
    use crate::parse::Lang;
    let mut items: Vec<NodeRef> = Vec::new();
    let mut scanned = 0usize;
    for (id, n) in &snap.nodes {
        if n.kind != "function" && n.kind != "method" {
            continue;
        }
        let Some(file) = &n.file else { continue };
        if cfg.is_pruned(file) {
            continue; // 测试/生成代码里的未调用函数不进死代码清单
        }
        scanned += 1;
        // API 判定按语言约定
        let lang = Lang::from_path(file).unwrap_or_default();
        let is_api = match lang {
            Lang::Python => !(n.name.starts_with('_') && !n.name.starts_with("__")),
            _ => n.is_exported,
        };
        if is_api {
            continue;
        }
        // 入边：没有任何 calls 指进来，也没有任何方法重写指向它
        let has_inbound = snap
            .inn
            .get(id)
            .map(|es| {
                es.iter()
                    .any(|e| matches!(e.kind, EdgeKind::Calls | EdgeKind::Overrides))
            })
            .unwrap_or(false);
        if !has_inbound {
            items.push(n.clone());
        }
    }
    items.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.line.cmp(&b.line))
            .then(a.qualified_name.cmp(&b.qualified_name))
    });
    DeadCodeReport {
        items,
        scanned,
        note: "仅统计静态可达性；动态调用（回调/反射/装饰器注册）不在图内，删除前请人工确认".into(),
    }
}

/// import 环：文件级 imports 边上的强连通分量（>1 个文件，或自环）。
pub fn cycles(snap: &Snapshot) -> CyclesReport {
    // 子图：文件节点 + imports 边
    let mut ids: Vec<i64> = snap
        .nodes
        .iter()
        .filter(|(_, n)| n.kind == "file")
        .map(|(id, _)| *id)
        .collect();
    ids.sort_unstable();
    let idx: HashMap<i64, usize> = ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    let n = ids.len();
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (&src, es) in &snap.out {
        let Some(&si) = idx.get(&src) else { continue };
        for e in es {
            if e.kind != EdgeKind::Imports {
                continue;
            }
            if let Some(&di) = idx.get(&e.other) {
                if !adj[si].contains(&di) {
                    adj[si].push(di);
                }
            }
        }
    }
    // 迭代版 Tarjan SCC：import 图很浅，但显式栈比递归稳
    struct Frame {
        v: usize,
        child_i: usize,
    }
    let mut index: Vec<i32> = vec![-1; n];
    let mut low: Vec<i32> = vec![0; n];
    let mut on_stack: Vec<bool> = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut counter: i32 = 0;
    let mut sccs: Vec<Vec<usize>> = Vec::new();
    for start in 0..n {
        if index[start] != -1 {
            continue;
        }
        let mut frames = vec![Frame {
            v: start,
            child_i: 0,
        }];
        while let Some(frame) = frames.last_mut() {
            let v = frame.v;
            if frame.child_i == 0 {
                index[v] = counter;
                low[v] = counter;
                counter += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            if frame.child_i < adj[v].len() {
                let w = adj[v][frame.child_i];
                frame.child_i += 1;
                if index[w] == -1 {
                    frames.push(Frame { v: w, child_i: 0 });
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                frames.pop();
                if let Some(parent) = frames.last_mut() {
                    low[parent.v] = low[parent.v].min(low[v]);
                }
                if low[v] == index[v] {
                    let mut comp = Vec::new();
                    while let Some(&w) = stack.last() {
                        stack.pop();
                        on_stack[w] = false;
                        comp.push(w);
                        if w == v {
                            break;
                        }
                    }
                    sccs.push(comp);
                }
            }
        }
    }
    let mut cycles: Vec<Vec<String>> = sccs
        .into_iter()
        .filter(|c| c.len() > 1)
        .map(|c| {
            let mut paths: Vec<String> = c
                .into_iter()
                .filter_map(|i| snap.node(ids[i]).map(|n| n.name.clone()))
                .collect();
            paths.sort();
            paths
        })
        .collect();
    cycles.sort();
    CyclesReport {
        cycles,
        scanned_files: n,
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
