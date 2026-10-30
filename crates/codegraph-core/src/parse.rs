//! tree-sitter 抽取层：源码字节流 → RawFile（符号 / 引用 / 导入 / 局部名）。
//!
//! 双通道设计（why）：tree-sitter query 擅长形态稳定的语法模式（定义、调用），
//! 但 import/export 子句形态繁多（default/namespace/named/re-export/星号），
//! 用 query 逐个枚举既冗长又脆——改用一次手写遍历统一处理。
//! 局部名（参数、内层变量）也走手写遍历，用于「遮蔽检测」。
//
// lint 豁免（有意为之的模式）：
// - needless_range_loop：span 包含关系的双下标扫描，按下标对比比迭代器更直白
// - type_complexity：抽取产物的组合类型，均为内部结构不外泄
// - collapsible_match：match 守卫拆分 require 特例，可读性优先
#![allow(
    clippy::needless_range_loop,
    clippy::type_complexity,
    clippy::collapsible_match
)]

use crate::model::NodeKind;
use std::collections::HashSet;
use std::sync::OnceLock;
use tree_sitter::{Node, Parser, Query, QueryCursor};
use tree_sitter_typescript::{LANGUAGE_TSX, LANGUAGE_TYPESCRIPT};

// ---------- 原始抽取产物 ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawRefKind {
    Call,
    New,
    Jsx,
    Extends,
    Implements,
}

#[derive(Debug, Clone)]
pub struct RawSymbol {
    pub kind: NodeKind,
    pub name: String,
    pub qualified_name: String,
    pub span: (usize, usize),
    pub line_start: u32,
    pub line_end: u32,
    pub exported: bool,
    pub is_default: bool,
    /// 外层符号下标（方法所属类、内层函数所属外层函数）
    pub container: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawImportKind {
    Named,
    Default,
    Namespace,
    SideEffect,
    ReExport,
}

#[derive(Debug, Clone)]
pub struct RawImport {
    pub spec: String,
    pub kind: RawImportKind,
    /// 绑定名；re_export 时 = 本文件对外导出的别名；export * 为 None
    pub local: Option<String>,
    /// named 的原始名 / re_export 的原名；side_effect / export * 为 None
    pub remote: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RawRef {
    pub kind: RawRefKind,
    pub name: String,
    pub receiver: Option<String>,
    pub span: (usize, usize),
    pub line_start: u32,
    /// 名字被本文件的参数/内层变量遮蔽 → 三级解析时直接判 dynamic，不猜
    pub shadowed: bool,
    /// 包含该引用的最内层符号下标；None = 文件顶层代码
    pub owner: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct RawFile {
    pub symbols: Vec<RawSymbol>,
    pub imports: Vec<RawImport>,
    pub refs: Vec<RawRef>,
    /// 语法错误节点数；>0 时文件状态记为 partial（tree-sitter 容错解析仍产出部分图）
    pub error_count: usize,
    pub loc: usize,
}

// ---------- 小工具 ----------

fn children_of(node: Node<'_>) -> Vec<Node<'_>> {
    (0..node.child_count())
        .filter_map(|i| node.child(i))
        .collect()
}

fn text(node: Node<'_>, bytes: &[u8]) -> String {
    node.utf8_text(bytes).unwrap_or("").to_string()
}

// tree-sitter 0.27 的 row 是 usize，图模型统一用 u32 存行号
fn row_start(node: Node<'_>) -> u32 {
    node.start_position().row as u32 + 1
}

fn row_end(node: Node<'_>) -> u32 {
    node.end_position().row as u32 + 1
}

fn strip_quotes(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .to_string()
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}

// ---------- 语言选择 ----------

fn language_for_path(rel: &str) -> Option<(tree_sitter::Language, bool)> {
    let ext = rel.rsplit('.').next()?;
    match ext {
        // TS grammar 是 JS 的语法超集，.js/.mjs/.cjs 复用同一 grammar（代价：.js 里的 JSX 不识别）
        "ts" | "mts" | "cts" | "js" | "mjs" | "cjs" => Some((LANGUAGE_TYPESCRIPT.into(), false)),
        "tsx" | "jsx" => Some((LANGUAGE_TSX.into(), true)),
        _ => None,
    }
}

// ---------- query 组管理 ----------

struct QueryGroup {
    query: Query,
    caps: Vec<(&'static str, u32)>,
}

impl QueryGroup {
    fn capture<'a>(&self, m: &'a tree_sitter::QueryMatch<'a, 'a>, name: &str) -> Option<Node<'a>> {
        let idx = self.caps.iter().find(|(n, _)| *n == name)?.1;
        m.captures().iter().find(|c| c.index == idx).map(|c| c.node)
    }
}

fn split_groups(source: &'static str) -> Vec<(&'static str, String)> {
    let mut groups: Vec<(&'static str, String)> = Vec::new();
    let mut current: Option<(&'static str, String)> = None;
    for line in source.lines() {
        if let Some(rest) = line.strip_prefix("; --- group: ") {
            if let Some(g) = current.take() {
                groups.push(g);
            }
            let name = rest.trim_end_matches(" ---").trim();
            current = Some((Box::leak(name.to_string().into_boxed_str()), String::new()));
        } else if let Some((_, buf)) = current.as_mut() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if let Some(g) = current.take() {
        groups.push(g);
    }
    groups
}

struct QuerySet {
    defs: Option<QueryGroup>,
    calls: Option<QueryGroup>,
    jsx: Option<QueryGroup>,
}

fn compile_set(lang: tree_sitter::Language) -> QuerySet {
    // 单组独立编译：一组的语法形态变化不拖垮其余组（优雅降级 + 可观测）
    let build = |src: &str| -> Option<QueryGroup> {
        match Query::new(&lang, src) {
            Ok(q) => {
                let caps = q
                    .capture_names()
                    .iter()
                    .enumerate()
                    .map(|(i, n)| {
                        let leaked: &'static str = Box::leak((*n).to_string().into_boxed_str());
                        (leaked, i as u32)
                    })
                    .collect();
                Some(QueryGroup { query: q, caps })
            }
            Err(e) => {
                tracing::warn!("query 组编译失败（该组降级为空）: {e}");
                None
            }
        }
    };
    static SCM: OnceLock<&'static str> = OnceLock::new();
    let scm = SCM.get_or_init(|| include_str!("../../../languages/typescript/queries.scm"));
    let mut set = QuerySet {
        defs: None,
        calls: None,
        jsx: None,
    };
    for (name, body) in split_groups(scm) {
        match name {
            "defs" => set.defs = build(&body),
            "calls" => set.calls = build(&body),
            "jsx" => set.jsx = build(&body),
            other => tracing::warn!("未知 query 组: {other}"),
        }
    }
    set
}

static TS_SET: OnceLock<QuerySet> = OnceLock::new();
static TSX_SET: OnceLock<QuerySet> = OnceLock::new();

// ---------- 主入口 ----------

/// 解析单文件。不返回 Result：单文件失败只降级为 error_count=1 的空结果，
/// 不允许一个坏文件中断整个仓库的索引（可恢复错误的边界画在文件级）。
pub fn parse_file(rel: &str, bytes: &[u8]) -> RawFile {
    let Some((lang, jsx_mode)) = language_for_path(rel) else {
        return RawFile::default();
    };
    let set = if jsx_mode {
        TSX_SET.get_or_init(|| compile_set(lang.clone()))
    } else {
        TS_SET.get_or_init(|| compile_set(lang.clone()))
    };

    let mut parser = Parser::new();
    if let Err(e) = parser.set_language(&lang) {
        tracing::warn!("set_language 失败 {rel}: {e}");
        return RawFile {
            error_count: 1,
            ..Default::default()
        };
    }
    let Some(tree) = parser.parse(bytes, None) else {
        tracing::warn!("tree-sitter 解析超时/取消 {rel}");
        return RawFile {
            error_count: 1,
            ..Default::default()
        };
    };
    let root = tree.root_node();

    let mut out = RawFile {
        loc: 1 + bytes.iter().filter(|&&b| b == b'\n').count(),
        ..Default::default()
    };

    let manual = manual_walk(root, bytes);
    out.error_count = manual.error_count;
    out.imports = manual.imports;

    // query 通道：定义与调用
    let mut cursor = QueryCursor::new();
    use streaming_iterator::StreamingIterator;

    if let Some(g) = &set.defs {
        let mut matches = cursor.matches(&g.query, root, bytes);
        while let Some(m) = matches.next() {
            let (Some(node), Some(name_node)) =
                (g.capture(m, "def.node"), g.capture(m, "def.name"))
            else {
                continue;
            };
            let name = text(name_node, bytes);
            let kind = match node.kind() {
                "function_declaration" | "variable_declarator" => NodeKind::Function,
                "method_definition" | "public_field_definition" => {
                    // 对象字面量里的方法不属于任何类，跳过（其调用点本来就解析不到类）
                    match node.parent() {
                        Some(p) if p.kind() == "class_body" => NodeKind::Method,
                        _ => continue,
                    }
                }
                "class_declaration" | "abstract_class_declaration" => NodeKind::Class,
                "interface_declaration" => NodeKind::Interface,
                _ => continue,
            };
            let span = (node.byte_range().start, node.byte_range().end);
            out.symbols.push(RawSymbol {
                kind,
                name,
                qualified_name: String::new(), // finish_symbols 统一计算
                span,
                line_start: row_start(node),
                line_end: row_end(node),
                exported: has_export_ancestor(node, 4),
                is_default: manual.default_spans.contains(&span),
                container: None,
            });
        }
    }

    if let Some(g) = &set.calls {
        let mut matches = cursor.matches(&g.query, root, bytes);
        while let Some(m) = matches.next() {
            let node = match g
                .capture(m, "call.node")
                .or_else(|| g.capture(m, "new.node"))
            {
                Some(n) => n,
                None => continue,
            };
            let kind = if node.kind() == "new_expression" {
                RawRefKind::New
            } else {
                RawRefKind::Call
            };
            if let Some(name_node) = g
                .capture(m, "call.name")
                .or_else(|| g.capture(m, "new.name"))
            {
                let name = text(name_node, bytes);
                if name == "require" {
                    // CommonJS：require("x") 转成 side_effect 导入，交给 import 解析
                    if let Some(spec) = first_string_arg(node, bytes) {
                        out.imports.push(RawImport {
                            spec,
                            kind: RawImportKind::SideEffect,
                            local: None,
                            remote: None,
                        });
                    }
                    continue;
                }
                push_ref(&mut out, kind, name, None, node, bytes, &manual.locals);
                continue;
            }
            let pair: Option<(Node, Node)> = if kind == RawRefKind::New {
                g.capture(m, "new.recv")
                    .and_then(|r| g.capture(m, "new.prop").map(|p| (r, p)))
            } else {
                g.capture(m, "call.recv")
                    .and_then(|r| g.capture(m, "call.prop").map(|p| (r, p)))
            };
            if let Some((recv_node, prop_node)) = pair {
                let recv_raw = text(recv_node, bytes);
                let prop = text(prop_node, bytes);
                let (recv, name) = if recv_raw == "this" {
                    (Some("this".to_string()), prop)
                } else {
                    (Some(truncate(&recv_raw, 64)), prop)
                };
                push_ref(&mut out, kind, name, recv, node, bytes, &manual.locals);
            }
        }
    }

    if jsx_mode {
        if let Some(g) = &set.jsx {
            let mut matches = cursor.matches(&g.query, root, bytes);
            while let Some(m) = matches.next() {
                let Some(node) = g.capture(m, "jsx.node") else {
                    continue;
                };
                let Some(name_node) = jsx_tag_name(node) else {
                    continue;
                };
                let full = text(name_node, bytes);
                // 小写开头 = DOM 标签（div/span），不属于仓库图，直接跳过
                if full
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_lowercase())
                    .unwrap_or(true)
                {
                    continue;
                }
                // `<Store.Cart />` → receiver=Store, name=Cart
                let (recv, name) = match full.split_once('.') {
                    Some((a, b)) => (Some(a.to_string()), b.to_string()),
                    None => (None, full),
                };
                push_ref(
                    &mut out,
                    RawRefKind::Jsx,
                    name,
                    recv,
                    node,
                    bytes,
                    &manual.locals,
                );
            }
        }
    }

    // extends / implements：手写遍历 class_heritage 子树（grammar 命名不稳定，query 反而脆）
    collect_heritage(root, bytes, &manual.locals, &mut out);

    // 合成匿名 default 导出
    for (kind, name, span, ls, le) in &manual.synthetic_defaults {
        out.symbols.push(RawSymbol {
            kind: *kind,
            name: name.clone(),
            qualified_name: String::new(),
            span: *span,
            line_start: *ls,
            line_end: *le,
            exported: true,
            is_default: true,
            container: None,
        });
    }

    // export { a, b as c }（无 source）名单 → 标记文件内顶层同名符号为导出
    for sym in out.symbols.iter_mut() {
        if sym.container.is_none() && manual.exported_clause_names.contains(&sym.name) {
            sym.exported = true;
        }
    }

    finish_symbols(&mut out);
    out
}

fn push_ref(
    out: &mut RawFile,
    kind: RawRefKind,
    name: String,
    receiver: Option<String>,
    node: Node<'_>,
    _bytes: &[u8],
    locals: &HashSet<String>,
) {
    // 遮蔽判定只对裸名字调用有意义；成员调用的属性名不受局部变量遮蔽影响
    let shadowed = receiver.is_none() && kind == RawRefKind::Call && locals.contains(&name);
    out.refs.push(RawRef {
        kind,
        name,
        receiver,
        span: (node.byte_range().start, node.byte_range().end),
        line_start: row_start(node),
        shadowed,
        owner: None,
    });
}

// ---------- 手写遍历：错误统计 / 局部名 / import / export ----------

struct ManualInfo {
    error_count: usize,
    locals: HashSet<String>,
    imports: Vec<RawImport>,
    default_spans: HashSet<(usize, usize)>,
    synthetic_defaults: Vec<(NodeKind, String, (usize, usize), u32, u32)>,
    exported_clause_names: Vec<String>,
}

fn manual_walk(root: Node<'_>, bytes: &[u8]) -> ManualInfo {
    let mut info = ManualInfo {
        error_count: 0,
        locals: HashSet::new(),
        imports: Vec::new(),
        default_spans: HashSet::new(),
        synthetic_defaults: Vec::new(),
        exported_clause_names: Vec::new(),
    };
    // 显式栈而非递归：深嵌套 JSX/链式调用可能把调用栈打爆
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "ERROR" || node.is_missing() {
            info.error_count += 1;
        }
        match node.kind() {
            "variable_declarator" => {
                let name = node.child_by_field_name("name").map(|n| text(n, bytes));
                let value = node.child_by_field_name("value");
                if let (Some(name), Some(value)) = (name, value) {
                    match value.kind() {
                        "arrow_function" | "function_expression" => {} // query 通道已捕获为符号
                        "call_expression"
                            // const x = require("y")：CJS 导入，绑定名 = 变量名
                            if is_require_call(value, bytes) => {
                                if let Some(spec) = first_string_arg(value, bytes) {
                                    info.imports.push(RawImport {
                                        spec,
                                        kind: RawImportKind::Default,
                                        local: Some(name),
                                        remote: None,
                                    });
                                }
                            }
                        _ => {
                            info.locals.insert(name);
                        }
                    }
                }
            }
            "required_parameter" | "optional_parameter" => {
                if let Some(p) = node.child_by_field_name("pattern") {
                    if p.kind() == "identifier" {
                        info.locals.insert(text(p, bytes));
                    }
                }
            }
            "catch_clause" => {
                if let Some(p) = node.child_by_field_name("parameter") {
                    if p.kind() == "identifier" {
                        info.locals.insert(text(p, bytes));
                    }
                }
            }
            "import_statement" => parse_import_statement(node, bytes, &mut info),
            "export_statement" => parse_export_statement(node, bytes, &mut info),
            _ => {}
        }
        for child in children_of(node) {
            stack.push(child);
        }
    }
    info
}

fn parse_import_statement(node: Node<'_>, bytes: &[u8], info: &mut ManualInfo) {
    let Some(spec_node) = node.child_by_field_name("source") else {
        return;
    };
    let spec = strip_quotes(&text(spec_node, bytes));
    // 按 kind 找子句而非按字段名：不同 grammar 版本对 import_clause 的字段绑定不一致
    let clause = children_of(node)
        .into_iter()
        .find(|c| c.kind() == "import_clause");
    let Some(clause) = clause else {
        info.imports.push(RawImport {
            spec,
            kind: RawImportKind::SideEffect,
            local: None,
            remote: None,
        });
        return;
    };
    for child in children_of(clause) {
        match child.kind() {
            "identifier" => info.imports.push(RawImport {
                spec: spec.clone(),
                kind: RawImportKind::Default,
                local: Some(text(child, bytes)),
                remote: None,
            }),
            "namespace_import" => {
                for c in children_of(child) {
                    if c.kind() == "identifier" {
                        info.imports.push(RawImport {
                            spec: spec.clone(),
                            kind: RawImportKind::Namespace,
                            local: Some(text(c, bytes)),
                            remote: None,
                        });
                    }
                }
            }
            "named_imports" => {
                for spec_node in children_of(child) {
                    if spec_node.kind() != "import_specifier" {
                        continue;
                    }
                    let remote = spec_node
                        .child_by_field_name("name")
                        .map(|n| text(n, bytes))
                        .unwrap_or_default();
                    let local = spec_node
                        .child_by_field_name("alias")
                        .map(|n| text(n, bytes))
                        .unwrap_or_else(|| remote.clone());
                    info.imports.push(RawImport {
                        spec: spec.clone(),
                        kind: RawImportKind::Named,
                        local: Some(local),
                        remote: Some(remote),
                    });
                }
            }
            _ => {}
        }
    }
}

fn parse_export_statement(node: Node<'_>, bytes: &[u8], info: &mut ManualInfo) {
    // 1) re-export：export { a as b } from './x' / export * from './x'
    if let Some(src_node) = node.child_by_field_name("source") {
        let spec = strip_quotes(&text(src_node, bytes));
        let mut found_clause = false;
        for child in children_of(node) {
            if child.kind() == "export_clause" {
                found_clause = true;
                for spec_node in children_of(child) {
                    if spec_node.kind() != "export_specifier" {
                        continue;
                    }
                    let remote = spec_node
                        .child_by_field_name("name")
                        .map(|n| text(n, bytes))
                        .unwrap_or_default();
                    let exported = spec_node
                        .child_by_field_name("alias")
                        .map(|n| text(n, bytes))
                        .unwrap_or_else(|| remote.clone());
                    info.imports.push(RawImport {
                        spec: spec.clone(),
                        kind: RawImportKind::ReExport,
                        local: Some(exported),
                        remote: Some(remote),
                    });
                }
            }
        }
        if !found_clause {
            // export * from './x'：本文件转出目标文件的全部导出
            info.imports.push(RawImport {
                spec,
                kind: RawImportKind::ReExport,
                local: None,
                remote: None,
            });
        }
        return;
    }
    // 2) export default <expr>
    let children = children_of(node);
    if children.iter().any(|c| c.kind() == "default") {
        let mut after_default = false;
        for child in children {
            if child.kind() == "default" {
                after_default = true;
                continue;
            }
            if !after_default {
                continue;
            }
            match child.kind() {
                "function_declaration" | "class_declaration" | "abstract_class_declaration" => {
                    info.default_spans
                        .insert((child.byte_range().start, child.byte_range().end));
                }
                "identifier" => {
                    // export default foo：转发本地符号 foo —— 用 foo 的名字建 is_default 符号，
                    // 这样 default import 解析与裸名调用解析都能命中同一个符号
                    let name = text(child, bytes);
                    info.synthetic_defaults.push((
                        NodeKind::Function,
                        name,
                        (child.byte_range().start, child.byte_range().end),
                        row_start(child),
                        row_end(child),
                    ));
                }
                "arrow_function" | "function_expression" => {
                    info.synthetic_defaults.push((
                        NodeKind::Function,
                        "default".into(),
                        (child.byte_range().start, child.byte_range().end),
                        row_start(child),
                        row_end(child),
                    ));
                }
                "class" => {
                    info.synthetic_defaults.push((
                        NodeKind::Class,
                        "default".into(),
                        (child.byte_range().start, child.byte_range().end),
                        row_start(child),
                        row_end(child),
                    ));
                }
                _ => {}
            }
        }
        return;
    }
    // 3) 具名导出：export { a, b as c }（无 source）
    for child in children {
        if child.kind() == "export_clause" {
            for spec_node in children_of(child) {
                if spec_node.kind() == "export_specifier" {
                    let name = spec_node
                        .child_by_field_name("name")
                        .map(|n| text(n, bytes));
                    let alias = spec_node
                        .child_by_field_name("alias")
                        .map(|n| text(n, bytes));
                    if let Some(n) = alias.or(name) {
                        info.exported_clause_names.push(n);
                    }
                }
            }
        }
    }
}

fn collect_heritage(root: Node<'_>, bytes: &[u8], locals: &HashSet<String>, out: &mut RawFile) {
    // class_heritage 子树里抓 extends/implements 的目标名。
    // 不用 query 的原因：不同 grammar 版本里 heritage 节点层级有差异，
    // 而子树首个 identifier 的扫描是跨版本稳定的。
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "class_heritage" {
            for child in children_of(node) {
                let k = child.kind();
                if k == "extends_clause" || k == "implements_clause" {
                    let kind = if k == "extends_clause" {
                        RawRefKind::Extends
                    } else {
                        RawRefKind::Implements
                    };
                    if let Some(name_node) = first_ident(child) {
                        let name = text(name_node, bytes);
                        let shadowed = locals.contains(&name);
                        out.refs.push(RawRef {
                            kind,
                            name,
                            receiver: None,
                            span: (child.byte_range().start, child.byte_range().end),
                            line_start: row_start(child),
                            shadowed,
                            owner: None,
                        });
                    }
                }
            }
        }
        for child in children_of(node) {
            stack.push(child);
        }
    }
}

fn first_ident(node: Node<'_>) -> Option<Node<'_>> {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "identifier" || n.kind() == "type_identifier" {
            return Some(n);
        }
        for c in children_of(n) {
            stack.push(c);
        }
    }
    None
}

fn is_require_call(call: Node<'_>, bytes: &[u8]) -> bool {
    let Some(f) = call.child_by_field_name("function") else {
        return false;
    };
    f.kind() == "identifier" && text(f, bytes) == "require"
}

fn first_string_arg(call: Node<'_>, bytes: &[u8]) -> Option<String> {
    let args = call.child_by_field_name("arguments")?;
    for i in 0..args.child_count() {
        let c = args.child(i)?;
        if c.kind() == "string" {
            return Some(strip_quotes(&text(c, bytes)));
        }
    }
    None
}

fn has_export_ancestor(mut node: Node<'_>, max_hops: usize) -> bool {
    for _ in 0..max_hops {
        let Some(p) = node.parent() else { return false };
        if p.kind() == "export_statement" {
            return true;
        }
        node = p;
    }
    false
}

fn jsx_tag_name<'a>(element: Node<'a>) -> Option<Node<'a>> {
    // 元素的第一个「像标签名」的命名子节点；不依赖 grammar 对标签名的具体规则名
    for i in 0..element.child_count() {
        let c = element.child(i)?;
        match c.kind() {
            "jsx_identifier"
            | "identifier"
            | "jsx_member_expression"
            | "member_expression"
            | "nested_identifier" => return Some(c),
            _ => {}
        }
    }
    None
}

// ---------- 收尾：限定名 / 容器 / owner / 去重 ----------

/// 调试用：缩进打印 AST 节点树（kind + 字节区间）。语言适配器作者用
/// `codegraph debug-ast <file>` 观察真实 grammar 节点名，再写 .scm 规则。
pub fn debug_tree(rel: &str, bytes: &[u8]) -> Option<String> {
    let (lang, _) = language_for_path(rel)?;
    let mut parser = Parser::new();
    parser.set_language(&lang).ok()?;
    let tree = parser.parse(bytes, None)?;
    let mut out = String::new();
    let mut stack = vec![(tree.root_node(), 0usize)];
    while let Some((node, depth)) = stack.pop() {
        let txt = node.utf8_text(bytes).unwrap_or("").replace('\n', "\\n");
        let txt = if txt.len() > 40 {
            format!("{}…", &txt[..40])
        } else {
            txt.to_string()
        };
        out.push_str(&format!(
            "{}{} [{}..{}] {:?}\n",
            "  ".repeat(depth),
            node.kind(),
            node.byte_range().start,
            node.byte_range().end,
            txt
        ));
        for child in children_of(node) {
            stack.push((child, depth + 1));
        }
    }
    Some(out)
}

fn finish_symbols(out: &mut RawFile) {
    // 容器：包含某符号的最小 span 符号（方法→类，内层函数→外层函数）
    let n = out.symbols.len();
    let spans: Vec<(usize, usize)> = out.symbols.iter().map(|s| s.span).collect();
    for i in 0..n {
        let (si, ei) = spans[i];
        let mut best: Option<usize> = None;
        let mut best_size = usize::MAX;
        for j in 0..n {
            if i == j {
                continue;
            }
            let (sj, ej) = spans[j];
            if sj <= si && ei <= ej && (ej - sj) < best_size {
                best = Some(j);
                best_size = ej - sj;
            }
        }
        out.symbols[i].container = best;
    }
    // 限定名：自顶向下展开（容器已算好，按 span 起点排序保证父先于子）
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| (spans[i].0, usize::MAX - spans[i].1));
    let mut qualified: Vec<String> = vec![String::new(); n];
    for &i in &order {
        qualified[i] = match out.symbols[i].container {
            Some(c) => format!("{}.{}", qualified[c], out.symbols[i].name),
            None => out.symbols[i].name.clone(),
        };
    }
    for i in 0..n {
        out.symbols[i].qualified_name = qualified[i].clone();
    }
    // 重名去重（TS 函数重载会产出同名声明）：保留先出现者，合并导出标志
    let mut kept: Vec<RawSymbol> = Vec::new();
    let mut first_kept_pos: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for s in out.symbols.drain(..) {
        match first_kept_pos.get(&s.qualified_name) {
            Some(&p) => {
                kept[p].exported |= s.exported;
                kept[p].is_default |= s.is_default;
            }
            None => {
                first_kept_pos.insert(s.qualified_name.clone(), kept.len());
                kept.push(s);
            }
        }
    }
    // swap_remove 打乱了顺序，重新按 span 排序恢复确定性
    kept.sort_by_key(|s| s.span);
    // 重算下标映射：swap_remove 后 owner 需要基于保留下来的符号重算
    let spans: Vec<(usize, usize)> = kept.iter().map(|s| s.span).collect();
    for r in out.refs.iter_mut() {
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
    // 容器下标同样以保留后的顺序重算
    let n2 = kept.len();
    for i in 0..n2 {
        let (si, ei) = spans[i];
        let mut best: Option<usize> = None;
        let mut best_size = usize::MAX;
        for j in 0..n2 {
            if i == j {
                continue;
            }
            let (sj, ej) = spans[j];
            if sj <= si && ei <= ej && (ej - sj) < best_size {
                best = Some(j);
                best_size = ej - sj;
            }
        }
        kept[i].container = best;
    }
    out.symbols = kept;
}
