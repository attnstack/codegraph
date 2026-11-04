//! 图模型与查询输出 DTO。
//! 设计要点：
//! 1) 节点身份 = (file_id, kind, qualified_name)，span 只是属性不是身份——
//!    否则文件顶部加一行注释会让所有节点 id 全部洗牌，增量更新失去意义。
//! 2) 边不直接存证据：每个调用点一行 refs（可跳源码），边按 (src,dst,kind) 聚合
//!    存 count。证据链 = 沿路径逐跳 join refs。

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    File,
    Class,
    Interface,
    Function,
    Method,
    External,
}

impl NodeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeKind::File => "file",
            NodeKind::Class => "class",
            NodeKind::Interface => "interface",
            NodeKind::Function => "function",
            NodeKind::Method => "method",
            NodeKind::External => "external",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Calls,
    Overrides,
    Extends,
    Implements,
    Imports,
}

impl EdgeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeKind::Calls => "calls",
            EdgeKind::Overrides => "overrides",
            EdgeKind::Extends => "extends",
            EdgeKind::Implements => "implements",
            EdgeKind::Imports => "imports",
        }
    }
}

/// 引用状态。`resolved` 是唯一会生成 calls 边的状态；
/// 其余都如实呈现在「间接影响」里，宁缺毋滥。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefStatus {
    /// 三级解析命中（同文件 / import / 全局唯一）
    Resolved,
    /// 无法消歧：参数回调、未知对象方法、重名歧义前的兜底
    Dynamic,
    /// JS 内建/测试框架全局（console、describe……），不可能指向仓库符号
    Builtin,
    /// 全局同名导出 > 1 个，拒绝猜测
    Ambiguous,
    /// import 指向 npm 包（包体不在仓库内，不展开）
    External,
}

impl RefStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RefStatus::Resolved => "resolved",
            RefStatus::Dynamic => "dynamic",
            RefStatus::Builtin => "builtin",
            RefStatus::Ambiguous => "ambiguous",
            RefStatus::External => "external",
        }
    }
}

// ---------- 持久化行模型（store 层原样落库） ----------

#[derive(Debug, Clone)]
pub struct NodeRow {
    /// 构建期临时 id（节点行数组下标语义），落库时由 store 映射为真实自增 id
    pub temp_id: usize,
    /// None 表示外部包伪节点（不属于任何文件）
    pub file_id: Option<i64>,
    pub kind: NodeKind,
    pub name: String,
    pub qualified_name: String,
    pub is_exported: bool,
    pub span_start: usize,
    pub span_end: usize,
    pub line_start: u32,
    pub line_end: u32,
    /// JSON 扩展位（如 {"default": true} 标记匿名默认导出）
    pub extra: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ImportRow {
    pub file_id: i64,
    pub spec: String,
    /// named | default | namespace | side_effect | re_export
    pub kind: String,
    pub local_name: Option<String>,
    pub remote_name: Option<String>,
    /// 解析到的仓库内相对路径；外部包为 None
    pub target_path: Option<String>,
    pub package: Option<String>,
}

/// 解析后的引用（落库形态：含状态与目标）
#[derive(Debug, Clone)]
pub struct RefRow {
    pub file_id: i64,
    /// 产生该引用的声明节点 temp_id（顶层代码则指向文件节点）
    pub owner_temp_id: usize,
    /// call | new | jsx | extends | implements
    pub kind: String,
    pub name: String,
    pub receiver: Option<String>,
    /// parse 阶段算好的遮蔽标记：增量重解析时不再需要局部名集合（它没有落库）
    pub shadowed: bool,
    pub status: RefStatus,
    /// 命中节点 temp_id；非 resolved 为 None
    pub target_temp_id: Option<usize>,
    pub span_start: usize,
    pub span_end: usize,
    pub line_start: u32,
}

#[derive(Debug, Clone)]
pub struct EdgeRow {
    pub src_temp_id: usize,
    pub dst_temp_id: usize,
    pub kind: EdgeKind,
    pub weight: i64,
}

// ---------- 查询快照（store 载入 → rank/query 算法消费） ----------

#[derive(Debug, Clone, Serialize)]
pub struct NodeRef {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub is_exported: bool,
    pub file: Option<String>,
    pub line: Option<u32>,
}

impl NodeRef {
    /// 快照缺失时的占位（不应出现，出现即上游 bug；保留可观测而非 panic）
    pub fn placeholder(id: i64) -> NodeRef {
        NodeRef {
            id,
            kind: "unknown".into(),
            name: format!("<missing:{id}>"),
            qualified_name: format!("<missing:{id}>"),
            is_exported: false,
            file: None,
            line: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CallSite {
    pub file: String,
    pub line: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdjEdge {
    pub other: i64,
    pub kind: EdgeKind,
    pub weight: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DynamicRef {
    pub file: String,
    pub line: u32,
    pub name: String,
    pub status: String,
}

/// 一次查询载入的内存图。规模上限：十万行仓库约数万节点/边，全量驻留内存 < 数十 MB。
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub nodes: std::collections::HashMap<i64, NodeRef>,
    pub out: std::collections::HashMap<i64, Vec<AdjEdge>>,
    pub inn: std::collections::HashMap<i64, Vec<AdjEdge>>,
    /// (caller_id, callee_id) → 调用点列表（证据链的物理载体）
    pub evidence: std::collections::HashMap<(i64, i64), Vec<CallSite>>,
    /// 未消歧引用按名字索引：影响面分析用它列「间接影响」
    pub dynamic_by_name: std::collections::HashMap<String, Vec<DynamicRef>>,
}

impl Snapshot {
    pub fn node(&self, id: i64) -> Option<&NodeRef> {
        self.nodes.get(&id)
    }

    /// 该节点所在文件被哪些文件 import（文件级影响面）
    pub fn importers_of(&self, file_of: Option<&str>) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(path) = file_of {
            for n in self.nodes.values() {
                if n.kind == "file" && n.name == *path {
                    if let Some(edges) = self.inn.get(&n.id) {
                        for e in edges {
                            if e.kind == EdgeKind::Imports {
                                if let Some(src) = self.nodes.get(&e.other) {
                                    out.push(src.name.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

// ---------- 查询输出 DTO ----------

#[derive(Debug, Clone, Serialize)]
pub struct ChainHop {
    pub edge_kind: String,
    /// 调用方向上的上游（发起方）
    pub from: NodeRef,
    pub to: NodeRef,
    /// 该跳的源码证据（调用点位置）；继承类边无调用点则为 None
    pub site: Option<CallSite>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WhoCallsReport {
    pub symbol: NodeRef,
    pub callers: Vec<Caller>,
    pub dynamic_callers: Vec<DynamicRef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Caller {
    pub node: NodeRef,
    pub sites: Vec<CallSite>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImpactParams {
    pub depth: usize,
    pub max_nodes: usize,
    pub decay: f64,
    pub weight_call: f64,
    pub weight_override: f64,
    pub weight_extends: f64,
    pub weight_implements: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImpactItem {
    pub node: NodeRef,
    pub risk: f64,
    pub hops: usize,
    /// 从受影响节点回溯到起点的因果链（方向 = 调用方向）
    pub chain: Vec<ChainHop>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImpactReport {
    pub symbol: NodeRef,
    pub items: Vec<ImpactItem>,
    /// 动态/歧义调用点：名字匹配但无法静态确认
    pub dynamic: Vec<DynamicRef>,
    /// 文件级影响：import 了起点所在文件的文件清单
    pub affected_files: Vec<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TraceReport {
    pub entry: NodeRef,
    pub sink: NodeRef,
    pub found: bool,
    pub path: Vec<ChainHop>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HotspotItem {
    pub node: NodeRef,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub node: NodeRef,
    pub match_kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CalleesReport {
    pub symbol: NodeRef,
    pub callees: Vec<Caller>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeadCodeReport {
    pub items: Vec<NodeRef>,
    pub scanned: usize,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CyclesReport {
    /// 每个 import 环：文件路径列表（按路径排序）
    pub cycles: Vec<Vec<String>>,
    pub scanned_files: usize,
}
