//! 图算法：风险传播（Dijkstra 变体）、双向 BFS、加权 PageRank。
//!
//! 设计取舍：直观写法 risk = Σ_paths w^dist × type_w 的「对所有路径求和」在
//! 有环图上是 #P 难的（路径数指数爆炸）。这里改为「单源最优路径」语义：
//! risk(n) = 沿影响传播链的最大衰减得分，用最大堆松弛（Dijkstra 同型），
//! 复杂度 O(E log V)，且松弛时顺手记录父指针，证据链免费重建。

use crate::model::{EdgeKind, NodeRef, Snapshot};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
pub struct RiskEntry {
    pub node: i64,
    pub risk: f64,
    pub hops: usize,
    /// (传播来源节点, 边类型)：从受影响节点回溯到起点的指针
    pub parent: Option<(i64, EdgeKind)>,
}

struct HeapItem {
    score: f64,
    id: i64,
}
impl PartialEq for HeapItem {
    fn eq(&self, o: &Self) -> bool {
        self.score.total_cmp(&o.score) == Ordering::Equal
    }
}
impl Eq for HeapItem {}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for HeapItem {
    fn cmp(&self, o: &Self) -> Ordering {
        self.score.total_cmp(&o.score)
    }
}

pub struct ImpactLimits {
    pub depth: usize,
    pub max_nodes: usize,
    pub decay: f64,
}

fn type_weight(kind: EdgeKind, p: &crate::config::Config) -> f64 {
    match kind {
        EdgeKind::Calls => p.weight_call,
        EdgeKind::Overrides => p.weight_override,
        EdgeKind::Extends => p.weight_extends,
        EdgeKind::Implements => p.weight_implements,
        EdgeKind::Imports => 0.0, // 文件级边不参与符号闭包（另列为 affected_files）
    }
}

/// 反向闭包 + 风险评分。pruned 对「候选节点的文件相对路径」判定，
/// 命中即整支剪掉（测试/生成代码的传播会淹没真实影响面）。
pub fn impact_closure(
    snap: &Snapshot,
    start: i64,
    limits: &ImpactLimits,
    cfg: &crate::config::Config,
    pruned: impl Fn(&str) -> bool,
) -> (Vec<RiskEntry>, bool) {
    let mut best: HashMap<i64, RiskEntry> = HashMap::new();
    let mut heap = std::collections::BinaryHeap::new();
    best.insert(
        start,
        RiskEntry {
            node: start,
            risk: 1.0,
            hops: 0,
            parent: None,
        },
    );
    heap.push(HeapItem {
        score: 1.0,
        id: start,
    });
    let mut truncated = false;

    while let Some(HeapItem { score, id }) = heap.pop() {
        // 懒删除：堆里可能有过期（更低分）的副本
        if let Some(e) = best.get(&id) {
            if e.risk > score + 1e-12 {
                continue;
            }
        }
        if best.len() > limits.max_nodes {
            truncated = true;
            break;
        }
        let hops = best.get(&id).map(|e| e.hops).unwrap_or(0);
        if hops >= limits.depth {
            continue;
        }
        // 反向遍历：inn[id] 的边 (p → id)，p 是「被 id 影响」的候选
        if let Some(edges) = snap.inn.get(&id) {
            for e in edges {
                let Some(pn) = snap.node(e.other) else {
                    continue;
                };
                if let Some(path) = &pn.file {
                    if pruned(path) {
                        continue;
                    }
                }
                let tw = type_weight(e.kind, cfg);
                if tw <= 0.0 {
                    continue;
                }
                let cand = score * limits.decay * tw;
                let cand_hops = hops + 1;
                let better = match best.get(&e.other) {
                    None => true,
                    Some(old) => cand > old.risk + 1e-12,
                };
                if better && cand_hops <= limits.depth {
                    best.insert(
                        e.other,
                        RiskEntry {
                            node: e.other,
                            risk: cand,
                            hops: cand_hops,
                            parent: Some((id, e.kind)),
                        },
                    );
                    heap.push(HeapItem {
                        score: cand,
                        id: e.other,
                    });
                }
            }
        }
    }

    let mut out: Vec<RiskEntry> = best.into_values().filter(|e| e.node != start).collect();
    out.sort_by(|a, b| {
        b.risk
            .total_cmp(&a.risk)
            .then(a.hops.cmp(&b.hops))
            .then_with(|| {
                let qa = snap
                    .node(a.node)
                    .map(|n| n.qualified_name.clone())
                    .unwrap_or_default();
                let qb = snap
                    .node(b.node)
                    .map(|n| n.qualified_name.clone())
                    .unwrap_or_default();
                qa.cmp(&qb)
            })
    });
    (out, truncated)
}

/// 从影响节点沿 parent 指针回溯到起点，产出「调用方向」的链。
/// 每跳附证据（调用点位置）：calls/new/jsx 的 refs 是证据载体。
pub fn reconstruct_chain(
    snap: &Snapshot,
    start: i64,
    mut cur: i64,
    parent_of: &HashMap<i64, (i64, EdgeKind)>,
) -> Vec<crate::model::ChainHop> {
    use crate::model::{CallSite, ChainHop};
    let mut raw: Vec<(i64, EdgeKind, i64)> = Vec::new(); // (from, kind, to) 调用方向
    let mut guard = 0;
    while cur != start {
        let Some(&(p, kind)) = parent_of.get(&cur) else {
            break;
        };
        // parent_of[cur] = (p)：cur 是调用方（受影响方），p 是被调方（传播源），
        // 调用方向 = cur → p
        raw.push((cur, kind, p));
        cur = p;
        guard += 1;
        if guard > 1000 {
            break; // 理论不可达（parent 链无环）；防御性上限
        }
    }
    raw.reverse();
    raw.into_iter()
        .map(|(from, kind, to)| {
            let site = snap
                .evidence
                .get(&(from, to))
                .and_then(|sites| sites.first().cloned())
                .map(|s| CallSite {
                    file: s.file,
                    line: s.line,
                });
            ChainHop {
                edge_kind: kind.as_str().to_string(),
                from: snap
                    .node(from)
                    .cloned()
                    .unwrap_or(NodeRef::placeholder(from)),
                to: snap.node(to).cloned().unwrap_or(NodeRef::placeholder(to)),
                site,
            }
        })
        .collect()
}

/// 双向 BFS：正向（calls 出边）+ 反向（calls 入边），任一侧相遇即返回最短路径。
/// 双向而非单向（why）：调用图出度小、入度大，两侧各限深 d 时
/// 覆盖范围是 O(b^(d/2)) 对 O(b^d) 的改进，10 跳以内的链基本都能找到。
pub fn bidirectional_trace(
    snap: &Snapshot,
    entry: i64,
    sink: i64,
    depth: usize,
) -> Option<Vec<i64>> {
    if entry == sink {
        return Some(vec![entry]);
    }
    let mut f_prev: HashMap<i64, i64> = HashMap::new(); // node → 前驱
    let mut b_next: HashMap<i64, i64> = HashMap::new(); // node → 后继
    let mut f_seen: HashSet<i64> = HashSet::from([entry]);
    let mut b_seen: HashSet<i64> = HashSet::from([sink]);
    let mut f_front = vec![entry];
    let mut b_front = vec![sink];

    for _ in 0..depth {
        // 正向扩展一层
        let mut f_next: Vec<i64> = Vec::new();
        for &n in &f_front {
            if let Some(es) = snap.out.get(&n) {
                for e in es {
                    if e.kind == EdgeKind::Calls && f_seen.insert(e.other) {
                        f_prev.insert(e.other, n);
                        f_next.push(e.other);
                    }
                }
            }
        }
        for &n in &f_next {
            if b_seen.contains(&n) {
                return Some(rebuild_path(entry, sink, n, &f_prev, &b_next));
            }
        }
        if f_next.is_empty() {
            return None;
        }
        // 反向扩展一层
        let mut b_next_front: Vec<i64> = Vec::new();
        for &n in &b_front {
            if let Some(es) = snap.inn.get(&n) {
                for e in es {
                    if e.kind == EdgeKind::Calls && b_seen.insert(e.other) {
                        b_next.insert(e.other, n);
                        b_next_front.push(e.other);
                    }
                }
            }
        }
        for &n in &b_next_front {
            if f_seen.contains(&n) {
                return Some(rebuild_path(entry, sink, n, &f_prev, &b_next));
            }
        }
        if b_next_front.is_empty() {
            return None;
        }
        f_front = f_next;
        b_front = b_next_front;
    }
    None
}

fn rebuild_path(
    entry: i64,
    sink: i64,
    meet: i64,
    f_prev: &HashMap<i64, i64>,
    b_next: &HashMap<i64, i64>,
) -> Vec<i64> {
    let mut left = Vec::new();
    let mut cur = meet;
    while cur != entry {
        left.push(cur);
        let Some(&p) = f_prev.get(&cur) else { break };
        cur = p;
    }
    left.push(entry);
    left.reverse();
    let mut right = Vec::new();
    let mut cur = meet;
    while cur != sink {
        let Some(&n) = b_next.get(&cur) else { break };
        right.push(n);
        cur = n;
    }
    left.extend(right);
    left
}

/// 加权 PageRank（悬空节点质量均匀回流；跨文件边在权重上乘加成）。
/// 返回 (node_id, score)，未排序。
pub fn pagerank(
    snap: &Snapshot,
    damping: f64,
    tolerance: f64,
    max_iter: usize,
    cross_file_bonus: f64,
) -> Vec<(i64, f64)> {
    // 参与调用图的节点：有 calls 出边或入边
    let mut in_calls: HashSet<i64> = HashSet::new();
    for (&n, es) in &snap.out {
        if es.iter().any(|e| e.kind == EdgeKind::Calls) {
            in_calls.insert(n);
        }
    }
    for (&n, es) in &snap.inn {
        if es.iter().any(|e| e.kind == EdgeKind::Calls) {
            in_calls.insert(n);
        }
    }
    let mut ids: Vec<i64> = in_calls.into_iter().collect();
    ids.sort_unstable();
    let n = ids.len();
    if n == 0 {
        return Vec::new();
    }
    let idx: HashMap<i64, usize> = ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();

    // 边权 = 调用次数 × 跨文件加成
    let mut w_out: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    let mut out_sum = vec![0.0f64; n];
    for (&src, es) in &snap.out {
        let Some(&si) = idx.get(&src) else { continue };
        for e in es {
            if e.kind != EdgeKind::Calls {
                continue;
            }
            let Some(&di) = idx.get(&e.other) else {
                continue;
            };
            let bonus = match (snap.node(src), snap.node(e.other)) {
                (Some(a), Some(b)) => match (&a.file, &b.file) {
                    (Some(fa), Some(fb)) if fa != fb => cross_file_bonus,
                    _ => 1.0,
                },
                _ => 1.0,
            };
            let w = e.weight * bonus;
            w_out[si].push((di, w));
            out_sum[si] += w;
        }
    }

    let mut rank = vec![1.0 / n as f64; n];
    let base = (1.0 - damping) / n as f64;
    for _ in 0..max_iter {
        let mut dangling = 0.0f64;
        for i in 0..n {
            if out_sum[i] <= 0.0 {
                dangling += rank[i];
            }
        }
        let mut next = vec![base + damping * dangling / n as f64; n];
        for i in 0..n {
            if out_sum[i] <= 0.0 {
                continue;
            }
            let share = damping * rank[i] / out_sum[i];
            for &(di, w) in &w_out[i] {
                next[di] += share * w;
            }
        }
        let diff: f64 = next
            .iter()
            .zip(rank.iter())
            .map(|(a, b)| (a - b).abs())
            .sum();
        rank = next;
        if diff < tolerance {
            break;
        }
    }
    ids.into_iter().zip(rank).collect()
}
