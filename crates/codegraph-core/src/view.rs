//! 内存视图：把「本次重解析的文件 + DB 里未变更文件的持久化行」拉平成统一的解析输入。
//!
//! 这是增量正确性的支点（why）：解析（tree-sitter，贵）只重做变更文件，
//! 而符号解析（哈希查找，便宜）永远跑在全量视图上。
//! 于是「增量结果 == 全量重建」是一个确定性函数的两次求值，可被 fuzz 证明。
// span 包含关系的双下标扫描是本模块的核心原语（与 parse.rs 同规则），下标语义更直白
#![allow(clippy::needless_range_loop)]

use crate::model::{ImportRow, NodeKind, NodeRow, RefRow};
use crate::parse::{RawFile, RawImport, RawImportKind, RawRefKind};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ViewSymbol {
    pub kind: NodeKind,
    pub name: String,
    pub qualified_name: String,
    pub exported: bool,
    pub is_default: bool,
    pub span: (usize, usize),
    pub line_start: u32,
    pub line_end: u32,
    /// 外层符号在【本文件符号列表】中的下标（非全局 id）
    pub container: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct ViewRef {
    pub kind: RawRefKind,
    pub name: String,
    pub receiver: Option<String>,
    pub span: (usize, usize),
    pub line_start: u32,
    pub shadowed: bool,
    /// 最内层包含符号（本文件符号列表下标）；None = 文件顶层代码
    pub owner: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct ViewFile {
    pub path: String,
    pub symbols: Vec<ViewSymbol>,
    pub imports: Vec<RawImport>,
    pub refs: Vec<ViewRef>,
    pub error_count: usize,
    pub loc: usize,
}

#[derive(Debug, Default)]
pub struct ViewRepo {
    /// 按 path 排序，保证临时 id 分配确定
    pub files: Vec<ViewFile>,
    pub file_index: HashMap<String, usize>,
}

impl ViewFile {
    pub fn from_raw(path: &str, raw: RawFile) -> ViewFile {
        ViewFile {
            path: path.to_string(),
            symbols: raw
                .symbols
                .into_iter()
                .map(|s| ViewSymbol {
                    kind: s.kind,
                    name: s.name,
                    qualified_name: s.qualified_name,
                    exported: s.exported,
                    is_default: s.is_default,
                    span: s.span,
                    line_start: s.line_start,
                    line_end: s.line_end,
                    container: s.container,
                })
                .collect(),
            imports: raw.imports,
            refs: raw
                .refs
                .into_iter()
                .map(|r| ViewRef {
                    kind: r.kind,
                    name: r.name,
                    receiver: r.receiver,
                    span: r.span,
                    line_start: r.line_start,
                    shadowed: r.shadowed,
                    owner: r.owner,
                })
                .collect(),
            error_count: raw.error_count,
            loc: raw.loc,
        }
    }

    /// 从 DB 行重建（未变更文件的路径）。
    /// DB 不存 container —— span 包含关系可从节点 span 确定性重算，不必持久化。
    pub fn from_rows(
        path: &str,
        nodes: &[NodeRow],
        imports: &[ImportRow],
        refs: &[RefRow],
    ) -> ViewFile {
        let mut symbols: Vec<ViewSymbol> = Vec::new();
        for n in nodes {
            if n.kind == NodeKind::File {
                continue; // 文件节点在视图里由 ViewRepo 统一表达
            }
            let is_default = n
                .extra
                .as_deref()
                .map(|x| x.contains("\"default\""))
                .unwrap_or(false);
            symbols.push(ViewSymbol {
                kind: n.kind,
                name: n.name.clone(),
                qualified_name: n.qualified_name.clone(),
                exported: n.is_exported,
                is_default,
                span: (n.span_start, n.span_end),
                line_start: n.line_start,
                line_end: n.line_end,
                container: None,
            });
        }
        recompute_containers(&mut symbols);
        let imports = imports
            .iter()
            .map(|i| RawImport {
                spec: i.spec.clone(),
                kind: import_kind_from_str(&i.kind),
                local: i.local_name.clone(),
                remote: i.remote_name.clone(),
            })
            .collect();
        let vrefs = refs
            .iter()
            .map(|r| ViewRef {
                kind: ref_kind_from_str(&r.kind),
                name: r.name.clone(),
                receiver: r.receiver.clone(),
                span: (r.span_start, r.span_end),
                line_start: r.line_start,
                shadowed: r.shadowed,
                owner: None,
            })
            .collect();
        let mut vf = ViewFile {
            path: path.to_string(),
            symbols,
            imports,
            refs: vrefs,
            error_count: 0,
            loc: 0,
        };
        // DB 路径下 owner 同样可由 span 包含关系重算
        recompute_owners(&mut vf);
        vf
    }
}

impl ViewRepo {
    pub fn new(mut files: Vec<ViewFile>) -> ViewRepo {
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let file_index = files
            .iter()
            .enumerate()
            .map(|(i, f)| (f.path.clone(), i))
            .collect();
        ViewRepo { files, file_index }
    }

    pub fn file_paths(&self) -> std::collections::HashSet<String> {
        self.file_index.keys().cloned().collect()
    }
}

/// 重算符号容器：最小 span 包含者（与 parse.rs 的规则一致）
pub fn recompute_containers(symbols: &mut [ViewSymbol]) {
    let spans: Vec<(usize, usize)> = symbols.iter().map(|s| s.span).collect();
    for i in 0..spans.len() {
        let (si, ei) = spans[i];
        let mut best: Option<usize> = None;
        let mut best_size = usize::MAX;
        for j in 0..spans.len() {
            if i == j {
                continue;
            }
            let (sj, ej) = spans[j];
            if sj <= si && ei <= ej && (ej - sj) < best_size {
                best = Some(j);
                best_size = ej - sj;
            }
        }
        symbols[i].container = best;
    }
}

/// 重算引用 owner：最小 span 包含符号
pub fn recompute_owners(vf: &mut ViewFile) {
    let spans: Vec<(usize, usize)> = vf.symbols.iter().map(|s| s.span).collect();
    for r in vf.refs.iter_mut() {
        let (ri, re) = r.span;
        let mut best: Option<usize> = None;
        let mut best_size = usize::MAX;
        for (j, &(sj, ej)) in spans.iter().enumerate() {
            if sj <= ri && re <= ej && (ej - sj) < best_size {
                best = Some(j);
                best_size = ej - sj;
            }
        }
        r.owner = best;
    }
}

pub fn import_kind_from_str(s: &str) -> RawImportKind {
    match s {
        "named" => RawImportKind::Named,
        "default" => RawImportKind::Default,
        "namespace" => RawImportKind::Namespace,
        "re_export" => RawImportKind::ReExport,
        _ => RawImportKind::SideEffect,
    }
}

pub fn ref_kind_from_str(s: &str) -> RawRefKind {
    match s {
        "new" => RawRefKind::New,
        "jsx" => RawRefKind::Jsx,
        "extends" => RawRefKind::Extends,
        "implements" => RawRefKind::Implements,
        _ => RawRefKind::Call,
    }
}
