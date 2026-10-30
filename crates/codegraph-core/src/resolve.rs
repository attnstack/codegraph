//! 三级符号解析 + import 解析。纯函数：ViewRepo → ResolveOutput（临时 id 空间）。
//!
//! 面试点（必讲）：为什么「全量重解析」反而让增量更新更稳——
//! 解析（tree-sitter，贵）只对变更文件重做；符号解析是 O(refs) 哈希查找（便宜），
//! 每次对全量视图重跑。两阶段拆开后：
//!   1) 增量结果 ≡ 全量重建 成为可被 fuzz 证明的确定性等式；
//!   2) 消灭了「反向依赖只传播一层」这类经典 stale-edge bug 的 whole 类别。
//!
//! 三级解析策略（宁缺毋滥）：
//!   同文件符号 → import 绑定 → 全局唯一导出兜底；
//!   任何不确定（遮蔽 / 重名 / 未知对象）一律标记 dynamic，绝不猜。

use crate::config::Config;
use crate::error::CoreResult;
use crate::model::*;
use crate::parse::{RawImportKind, RawRefKind};
use crate::view::ViewRepo;
use std::collections::{HashMap, HashSet};
use std::path::Path;

// JS/Node/浏览器内建与测试框架全局。裸调用它们不可能指向仓库符号，
// 不拦截的话「全局唯一兜底」会把 console.log 之类接进仓库图。
const BUILTIN_GLOBALS: &[&str] = &[
    "console",
    "setTimeout",
    "setInterval",
    "clearTimeout",
    "clearInterval",
    "setImmediate",
    "queueMicrotask",
    "fetch",
    "Promise",
    "JSON",
    "Math",
    "Object",
    "Array",
    "String",
    "Number",
    "Boolean",
    "Date",
    "Error",
    "Map",
    "Set",
    "WeakMap",
    "WeakSet",
    "Symbol",
    "BigInt",
    "Reflect",
    "Proxy",
    "parseInt",
    "parseFloat",
    "isNaN",
    "encodeURIComponent",
    "decodeURIComponent",
    "structuredClone",
    "process",
    "Buffer",
    "globalThis",
    "window",
    "document",
    "navigator",
    "localStorage",
    "sessionStorage",
    "alert",
    "requestAnimationFrame",
    "describe",
    "it",
    "test",
    "expect",
    "beforeEach",
    "afterEach",
    "beforeAll",
    "afterAll",
    "vi",
    "jest",
    "assert",
];
const BUILTIN_RECEIVERS: &[&str] = &[
    "console",
    "process",
    "window",
    "document",
    "navigator",
    "Math",
    "JSON",
    "Object",
    "Array",
    "Promise",
    "Date",
    "Reflect",
    "globalThis",
    "localStorage",
    "sessionStorage",
];
const TS_EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"];

// ---------- tsconfig ----------

/// tsconfig paths 规则：(模式键, 目标列表)
pub type PathRules = Vec<(String, Vec<String>)>;

/// tsconfig paths/baseUrl（含 extends 链）。解析失败降级为 None（无别名），
/// 只影响别名导入的解析质量，不影响可用性。
#[derive(Debug, Clone, Default)]
pub struct TsPaths {
    /// (模式键, 目标列表)，已按特异性排序：无 * 的精确键在前，前缀越长越靠前
    pub rules: PathRules,
}

pub fn load_tsconfig(root: &Path) -> Option<TsPaths> {
    let mut rules: Vec<(String, Vec<String>)> = Vec::new();
    let mut current = root.join("tsconfig.json");
    let mut current_dir = root.to_path_buf();
    let mut depth = 0;
    // TS 语义：子配置覆盖父配置 → 父规则先入列（低优先级），子规则后入列
    let mut chain: Vec<(PathRules, Option<String>)> = Vec::new();
    while depth < 5 {
        let text = match std::fs::read_to_string(&current) {
            Ok(t) => t,
            Err(_) => break,
        };
        let Some(json) = parse_jsonc(&text) else {
            tracing::warn!("tsconfig 解析失败: {}", current.display());
            break;
        };
        // paths 目标必须表达为「仓库相对路径」才能命中已发现文件集合
        let rel_dir = current_dir
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let co = json
            .get("compilerOptions")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let base_url = co
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .map(|b| normalize_join(&rel_dir, b));
        let mut local: Vec<(String, Vec<String>)> = Vec::new();
        if let Some(paths) = co.get("paths").and_then(|v| v.as_object()) {
            for (k, v) in paths {
                let targets: Vec<String> = v
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|t| t.as_str())
                            .map(|t| match &base_url {
                                Some(base) => normalize_join(base, t),
                                None => normalize_join(&rel_dir, t),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                local.push((k.clone(), targets));
            }
        }
        let extends = json
            .get("extends")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let extends_ref = extends.clone();
        chain.push((local, extends));
        match extends_ref {
            Some(parent) => {
                let parent_path = if parent.starts_with('.') {
                    current_dir.join(parent)
                } else {
                    // 裸名 extends 指向 node_modules 里的共享配置：MVP 不支持
                    break;
                };
                current_dir = parent_path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or(current_dir.clone());
                current = parent_path;
                depth += 1;
            }
            None => break,
        }
    }
    for (local, _) in chain.iter().rev() {
        rules.extend(local.iter().cloned());
    }
    if rules.is_empty() {
        return None;
    }
    // 特异性排序：精确键优先，其后通配键按前缀长度降序
    rules.sort_by(|a, b| {
        let a_star = a.0.contains('*');
        let b_star = b.0.contains('*');
        match (a_star, b_star) {
            (false, true) => std::cmp::Ordering::Less,
            (true, false) => std::cmp::Ordering::Greater,
            _ => b.0.len().cmp(&a.0.len()),
        }
    });
    Some(TsPaths { rules })
}

/// tsconfig 是 JSONC（注释 + 尾逗号）。先严格解析；失败再做保守剥离后重试。
fn parse_jsonc(text: &str) -> Option<serde_json::Value> {
    if let Ok(v) = serde_json::from_str(text) {
        return Some(v);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut escape = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                while let Some(&n) = chars.peek() {
                    if n == '\n' {
                        break;
                    }
                    chars.next();
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                while let Some(n) = chars.next() {
                    if n == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => out.push(c),
        }
    }
    // 尾逗号：删掉 } 或 ] 前的逗号
    let mut cleaned = String::with_capacity(out.len());
    let mut pending_ws = String::new();
    for c in out.chars() {
        if c.is_whitespace() {
            pending_ws.push(c);
            continue;
        }
        if (c == '}' || c == ']') && pending_ws.contains(',') {
            pending_ws.clear();
        } else {
            cleaned.push_str(&pending_ws);
        }
        pending_ws.clear();
        cleaned.push(c);
    }
    cleaned.push_str(&pending_ws);
    serde_json::from_str(&cleaned).ok()
}

// ---------- import 解析 ----------

#[derive(Debug, Clone, PartialEq)]
pub enum ImportTarget {
    File(String),
    Package(String),
    NotFound,
}

fn normalize_join(from_dir: &str, spec: &str) -> String {
    let mut all: Vec<String> = from_dir
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .map(|s| s.to_string())
        .collect();
    for seg in spec.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                all.pop();
            }
            s => all.push(s.to_string()),
        }
    }
    all.join("/")
}

fn candidates(base: &str) -> Vec<String> {
    let mut v = vec![base.to_string()];
    for ext in TS_EXTS {
        v.push(format!("{base}.{ext}"));
    }
    // TS 惯例：源码里写 import './x.js'，实际解析到 x.ts（esModuleInterop 时代的默认行为）
    let swap: &[&str] = match base.strip_suffix(".js") {
        Some(_) => &["ts", "tsx"],
        None => match base.strip_suffix(".mjs") {
            Some(_) => &["mts"],
            None => match base.strip_suffix(".cjs") {
                Some(_) => &["cts"],
                None => &[],
            },
        },
    };
    for ext in swap {
        let stem = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(base);
        v.push(format!("{stem}.{ext}"));
    }
    for ext in TS_EXTS {
        v.push(format!("{base}/index.{ext}"));
    }
    v
}

fn try_file(base: &str, files: &HashSet<String>) -> Option<String> {
    candidates(base).into_iter().find(|c| files.contains(c))
}

fn package_name(spec: &str) -> String {
    if spec.starts_with('@') {
        spec.splitn(3, '/').take(2).collect::<Vec<_>>().join("/")
    } else {
        spec.split('/').next().unwrap_or(spec).to_string()
    }
}

pub fn resolve_spec(
    spec: &str,
    from_dir: &str,
    files: &HashSet<String>,
    ts: Option<&TsPaths>,
) -> ImportTarget {
    // 1) tsconfig paths 别名
    if let Some(ts) = ts {
        for (key, targets) in &ts.rules {
            let mapped: Option<Vec<String>> = if let Some(star) = key.find('*') {
                let pre = &key[..star];
                let suf = &key[star + 1..];
                if spec.starts_with(pre)
                    && spec.ends_with(suf)
                    && spec.len() >= pre.len() + suf.len()
                {
                    let mid = &spec[pre.len()..spec.len() - suf.len()];
                    Some(targets.iter().map(|t| t.replace('*', mid)).collect())
                } else {
                    None
                }
            } else if key == spec {
                Some(targets.clone())
            } else {
                None
            };
            if let Some(list) = mapped {
                for base in list {
                    if let Some(f) = try_file(&base, files) {
                        return ImportTarget::File(f);
                    }
                }
                // 别名命中但没有对应仓库文件：可能指向未索引目录，按外部包处理
                return ImportTarget::Package(package_name(spec));
            }
        }
    }
    // 2) 相对路径
    if spec.starts_with("./") || spec.starts_with("../") || spec == "." || spec.starts_with('/') {
        let base = if let Some(stripped) = spec.strip_prefix('/') {
            stripped.to_string()
        } else {
            normalize_join(from_dir, spec)
        };
        return match try_file(&base, files) {
            Some(f) => ImportTarget::File(f),
            None => ImportTarget::NotFound,
        };
    }
    // 3) 裸名 → npm 包（包体不入图）
    ImportTarget::Package(package_name(spec))
}

// ---------- 解析主流程 ----------

pub struct ResolveOutput {
    pub nodes: Vec<NodeRow>,
    pub imports: Vec<ImportRow>,
    pub refs: Vec<RefRow>,
    pub edges: Vec<EdgeRow>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum BindingKind {
    Named,
    Default,
    Namespace,
}

#[derive(Debug, Clone)]
struct Binding {
    kind: BindingKind,
    target_file: Option<String>,
    package: Option<String>,
    remote: Option<String>,
}

struct ReExport {
    exported: Option<String>,
    remote: Option<String>,
    target_file: Option<String>,
}

#[derive(Default)]
struct FileTables {
    /// 裸名 → 符号临时 id（仅 Function/Class/Interface；优先顶层）
    by_name: HashMap<String, usize>,
    /// 导出名 → 符号临时 id
    exports: HashMap<String, usize>,
    default_sym: Option<usize>,
    bindings: HashMap<String, Binding>,
    specs: HashMap<String, ImportTarget>,
    reexports: Vec<ReExport>,
}

pub fn resolve_all(view: &ViewRepo, cfg: &Config) -> CoreResult<ResolveOutput> {
    let ts = load_tsconfig(&cfg.root);
    let files_set = view.file_paths();
    let mut tables: Vec<FileTables> = (0..view.files.len())
        .map(|_| FileTables::default())
        .collect();
    let mut nodes: Vec<NodeRow> = Vec::new();
    let mut imports_out: Vec<ImportRow> = Vec::new();
    let mut external_index: HashMap<String, usize> = HashMap::new(); // pkg → temp id（后补）
    let mut external_names: Vec<String> = Vec::new();
    // (file_idx, spec) → target，供 re-export 链使用
    let mut file_bases: Vec<usize> = Vec::with_capacity(view.files.len());

    // ---- 1. 分配临时 id：文件节点 → 符号 ----
    for (fi, vf) in view.files.iter().enumerate() {
        let base = nodes.len();
        file_bases.push(base);
        nodes.push(NodeRow {
            temp_id: base,
            file_id: Some(fi as i64),
            kind: NodeKind::File,
            name: vf.path.clone(),
            qualified_name: vf.path.clone(),
            is_exported: false,
            span_start: 0,
            span_end: 0,
            line_start: 0,
            line_end: 0,
            extra: None,
        });
        for (si, s) in vf.symbols.iter().enumerate() {
            nodes.push(NodeRow {
                temp_id: base + 1 + si,
                file_id: Some(fi as i64),
                kind: s.kind,
                name: s.name.clone(),
                qualified_name: s.qualified_name.clone(),
                is_exported: s.exported,
                span_start: s.span.0,
                span_end: s.span.1,
                line_start: s.line_start,
                line_end: s.line_end,
                extra: if s.is_default {
                    Some("{\"default\":true}".into())
                } else {
                    None
                },
            });
        }
    }
    let symbol_temp = |fi: usize, sym_idx: usize| file_bases[fi] + 1 + sym_idx;

    // ---- 2. import 解析 + 绑定表 + re-export 表 ----
    for (fi, vf) in view.files.iter().enumerate() {
        let from_dir = match vf.path.rfind('/') {
            Some(p) => &vf.path[..p],
            None => "",
        };
        for imp in &vf.imports {
            let target = resolve_spec(&imp.spec, from_dir, &files_set, ts.as_ref());
            tables[fi].specs.insert(imp.spec.clone(), target.clone());
            let (target_path, package) = match &target {
                ImportTarget::File(f) => (Some(f.clone()), None),
                ImportTarget::Package(p) => (None, Some(p.clone())),
                ImportTarget::NotFound => (None, None),
            };
            if let Some(p) = &package {
                if !external_index.contains_key(p) {
                    external_index.insert(p.clone(), usize::MAX); // 占位，step 3 赋真 id
                    external_names.push(p.clone());
                }
            }
            let kind_str = match imp.kind {
                RawImportKind::Named => "named",
                RawImportKind::Default => "default",
                RawImportKind::Namespace => "namespace",
                RawImportKind::SideEffect => "side_effect",
                RawImportKind::ReExport => "re_export",
            };
            imports_out.push(ImportRow {
                file_id: fi as i64,
                spec: imp.spec.clone(),
                kind: kind_str.into(),
                local_name: imp.local.clone(),
                remote_name: imp.remote.clone(),
                target_path: target_path.clone(),
                package: package.clone(),
            });
            match imp.kind {
                RawImportKind::Named => {
                    if let (Some(local), Some(remote)) = (&imp.local, &imp.remote) {
                        tables[fi].bindings.insert(
                            local.clone(),
                            Binding {
                                kind: BindingKind::Named,
                                target_file: target_path.clone(),
                                package: package.clone(),
                                remote: Some(remote.clone()),
                            },
                        );
                    }
                }
                RawImportKind::Default => {
                    if let Some(local) = &imp.local {
                        tables[fi].bindings.insert(
                            local.clone(),
                            Binding {
                                kind: BindingKind::Default,
                                target_file: target_path.clone(),
                                package: package.clone(),
                                remote: None,
                            },
                        );
                    }
                }
                RawImportKind::Namespace => {
                    if let Some(local) = &imp.local {
                        tables[fi].bindings.insert(
                            local.clone(),
                            Binding {
                                kind: BindingKind::Namespace,
                                target_file: target_path.clone(),
                                package: package.clone(),
                                remote: None,
                            },
                        );
                    }
                }
                RawImportKind::ReExport => {
                    tables[fi].reexports.push(ReExport {
                        exported: imp.local.clone(),
                        remote: imp.remote.clone(),
                        target_file: target_path.clone(),
                    });
                }
                RawImportKind::SideEffect => {}
            }
        }
    }

    // ---- 3. 外部包伪节点 ----
    let external_base = nodes.len();
    for (i, pkg) in external_names.iter().enumerate() {
        let temp = external_base + i;
        *external_index.get_mut(pkg).unwrap() = temp;
        nodes.push(NodeRow {
            temp_id: temp,
            file_id: None,
            kind: NodeKind::External,
            name: pkg.clone(),
            qualified_name: pkg.clone(),
            is_exported: false,
            span_start: 0,
            span_end: 0,
            line_start: 0,
            line_end: 0,
            extra: None,
        });
    }

    // ---- 4. 符号查找表：by_name / exports / default ----
    for (fi, vf) in view.files.iter().enumerate() {
        for (si, s) in vf.symbols.iter().enumerate() {
            let temp = symbol_temp(fi, si);
            let resolvable = matches!(
                s.kind,
                NodeKind::Function | NodeKind::Class | NodeKind::Interface
            );
            if resolvable {
                let entry = tables[fi].by_name.entry(s.name.clone()).or_insert(temp);
                // 优先顶层符号：后遇到顶层定义时覆盖
                if s.container.is_none() {
                    *entry = temp;
                }
            }
            if s.exported && resolvable {
                tables[fi].exports.insert(s.name.clone(), temp);
            }
            if s.is_default && tables[fi].default_sym.is_none() {
                tables[fi].default_sym = Some(temp);
            }
        }
    }
    // 全局导出索引（第三级兜底）
    let mut global_exports: HashMap<String, Vec<usize>> = HashMap::new();
    for (fi, vf) in view.files.iter().enumerate() {
        for (si, s) in vf.symbols.iter().enumerate() {
            if s.exported
                && matches!(
                    s.kind,
                    NodeKind::Function | NodeKind::Class | NodeKind::Interface
                )
            {
                global_exports
                    .entry(s.name.clone())
                    .or_default()
                    .push(symbol_temp(fi, si));
            }
        }
    }

    // resolve_export：沿 re-export 链解析（barrel 文件支持），带深度与环路保护
    fn resolve_export(
        tables: &[FileTables],
        file_index: &HashMap<String, usize>,
        file: &str,
        name: &str,
        depth: usize,
        seen: &mut HashSet<String>,
    ) -> Option<usize> {
        if depth > 10 || !seen.insert(file.to_string()) {
            return None;
        }
        let fi = *file_index.get(file)?;
        let t = &tables[fi];
        if let Some(&temp) = t.exports.get(name) {
            return Some(temp);
        }
        if name == "default" {
            if let Some(temp) = t.default_sym {
                return Some(temp);
            }
        }
        for re in &t.reexports {
            let Some(tf) = &re.target_file else { continue };
            match (&re.exported, &re.remote) {
                (Some(exported), Some(remote)) if exported == name => {
                    if let Some(temp) =
                        resolve_export(tables, file_index, tf, remote, depth + 1, seen)
                    {
                        return Some(temp);
                    }
                }
                (None, _) => {
                    // export * from './x'
                    if let Some(temp) =
                        resolve_export(tables, file_index, tf, name, depth + 1, seen)
                    {
                        return Some(temp);
                    }
                }
                _ => {}
            }
        }
        None
    }

    // 方法查找：类内 → 沿 extends 链向上（最近祖先优先）
    #[allow(clippy::too_many_arguments)]
    fn lookup_method(
        view: &ViewRepo,
        file_bases: &[usize],
        class_parent: &HashMap<usize, usize>,
        class_temp: usize,
        prop: &str,
        depth: usize,
        seen: &mut HashSet<usize>,
    ) -> Option<usize> {
        if depth > 10 || !seen.insert(class_temp) {
            return None;
        }
        // 类节点 temp → 所在文件与文件内下标
        let fi = file_bases.iter().rposition(|&b| class_temp > b)?; // class_temp 属于最后一个 base < class_temp 的文件
        let local_idx = class_temp - file_bases[fi] - 1;
        if let Some(vf) = view.files.get(fi) {
            for (si, s) in vf.symbols.iter().enumerate() {
                if s.container == Some(local_idx)
                    && s.name == prop
                    && matches!(s.kind, NodeKind::Method | NodeKind::Function)
                {
                    return Some(file_bases[fi] + 1 + si);
                }
            }
        }
        if let Some(&parent) = class_parent.get(&class_temp) {
            return lookup_method(
                view,
                file_bases,
                class_parent,
                parent,
                prop,
                depth + 1,
                seen,
            );
        }
        None
    }

    // ---- 5. extends / implements 解析 → class_parent ----
    let mut refs_out: Vec<RefRow> = Vec::new();
    let mut class_parent: HashMap<usize, usize> = HashMap::new();
    for (fi, vf) in view.files.iter().enumerate() {
        for r in &vf.refs {
            if !matches!(r.kind, RawRefKind::Extends | RawRefKind::Implements) {
                continue;
            }
            let owner_temp = match r.owner {
                Some(oi) => symbol_temp(fi, oi),
                None => file_bases[fi],
            };
            // 解析父类名：绑定 → 同文件类 → 全局唯一。
            // 绑定一旦存在就锁定上下文（解析失败也保持 Dynamic），
            // 否则会错误回退到与导入无关的同名符号。
            let mut status = RefStatus::Dynamic;
            let mut target: Option<usize> = None;
            let mut binding_consumed = false;
            if !r.shadowed {
                if let Some(b) = tables[fi].bindings.get(&r.name) {
                    binding_consumed = true;
                    if b.package.is_some() {
                        status = RefStatus::External;
                        if let Some(pkg) = &b.package {
                            target = external_index.get(pkg).copied();
                        }
                    } else if let Some(tf) = &b.target_file {
                        let lookup_name = match b.kind {
                            BindingKind::Default => "default".to_string(),
                            _ => b.remote.clone().unwrap_or_else(|| r.name.clone()),
                        };
                        let mut seen = HashSet::new();
                        target = resolve_export(
                            &tables,
                            &view.file_index,
                            tf,
                            &lookup_name,
                            0,
                            &mut seen,
                        );
                        if target.is_none() {
                            status = RefStatus::Dynamic;
                        }
                    }
                }
                if !binding_consumed {
                    if let Some(&temp) = tables[fi].by_name.get(&r.name) {
                        if matches!(nodes[temp].kind, NodeKind::Class | NodeKind::Interface) {
                            target = Some(temp);
                        }
                    }
                    if target.is_none() {
                        let cands = global_exports.get(&r.name).cloned().unwrap_or_default();
                        if cands.len() == 1 {
                            target = Some(cands[0]);
                        } else if cands.len() > 1 {
                            status = RefStatus::Ambiguous;
                        }
                    }
                }
            }
            if let Some(t) = target {
                status = RefStatus::Resolved;
                let target_kind = nodes[t].kind;
                if r.kind == RawRefKind::Extends
                    && matches!(target_kind, NodeKind::Class | NodeKind::Interface)
                {
                    class_parent.insert(owner_temp, t);
                }
            }
            refs_out.push(RefRow {
                file_id: fi as i64,
                owner_temp_id: owner_temp,
                kind: match r.kind {
                    RawRefKind::Extends => "extends",
                    _ => "implements",
                }
                .into(),
                name: r.name.clone(),
                receiver: None,
                shadowed: r.shadowed,
                status,
                target_temp_id: target,
                span_start: r.span.0,
                span_end: r.span.1,
                line_start: r.line_start,
            });
        }
    }

    // ---- 6. overrides 推导：子类方法 → 最近祖先同名方法 ----
    let mut overrides_edges: Vec<(usize, usize)> = Vec::new();
    for (&child, &parent0) in &class_parent {
        let child_fi = file_bases.iter().rposition(|&b| child > b).unwrap();
        let child_local = child - file_bases[child_fi] - 1;
        let Some(child_vf) = view.files.get(child_fi) else {
            continue;
        };
        for (si, s) in child_vf.symbols.iter().enumerate() {
            if s.container != Some(child_local)
                || !matches!(s.kind, NodeKind::Method | NodeKind::Function)
            {
                continue;
            }
            let mut seen = HashSet::new();
            let mut cur = Some(parent0);
            while let Some(p) = cur {
                if !seen.insert(p) {
                    break;
                }
                if let Some(target) = lookup_method(
                    view,
                    &file_bases,
                    &class_parent,
                    p,
                    &s.name,
                    0,
                    &mut HashSet::new(),
                ) {
                    if target != file_bases[child_fi] + 1 + si {
                        overrides_edges.push((file_bases[child_fi] + 1 + si, target));
                    }
                    break; // 最近祖先优先
                }
                cur = class_parent.get(&p).copied();
            }
        }
    }

    // ---- 7. call / new / jsx 解析 ----
    let mut edge_acc: HashMap<(usize, usize, &'static str), i64> = HashMap::new();
    let bump = |edge_acc: &mut HashMap<(usize, usize, &'static str), i64>,
                src: usize,
                dst: usize,
                kind: &'static str| {
        *edge_acc.entry((src, dst, kind)).or_insert(0) += 1;
    };

    for (fi, vf) in view.files.iter().enumerate() {
        for r in &vf.refs {
            if !matches!(r.kind, RawRefKind::Call | RawRefKind::New | RawRefKind::Jsx) {
                continue;
            }
            let owner_temp = match r.owner {
                Some(oi) => symbol_temp(fi, oi),
                None => file_bases[fi],
            };
            let mut status = RefStatus::Dynamic;
            let mut target: Option<usize> = None;

            let resolve_binding_target = |b: &Binding, name: &str| -> (RefStatus, Option<usize>) {
                if let Some(pkg) = &b.package {
                    if let Some(&t) = external_index.get(pkg) {
                        return (RefStatus::External, Some(t));
                    }
                    return (RefStatus::External, None);
                }
                if let Some(tf) = &b.target_file {
                    let remote = b.remote.clone().unwrap_or_else(|| name.to_string());
                    let lookup_name = if b.kind == BindingKind::Default {
                        "default".to_string()
                    } else {
                        remote
                    };
                    let mut seen = HashSet::new();
                    if let Some(t) =
                        resolve_export(&tables, &view.file_index, tf, &lookup_name, 0, &mut seen)
                    {
                        return (RefStatus::Resolved, Some(t));
                    }
                }
                (RefStatus::Dynamic, None)
            };

            match (&r.receiver, r.kind) {
                // this.method()：在 owner 所属类（沿继承链）里找方法
                (Some(recv), _) if recv == "this" => {
                    // owner 链向上找最近的类
                    let mut cur = r.owner;
                    let mut class_temp: Option<usize> = None;
                    while let Some(oi) = cur {
                        let t = symbol_temp(fi, oi);
                        if nodes[t].kind == NodeKind::Class {
                            class_temp = Some(t);
                            break;
                        }
                        cur = view.files[fi].symbols.get(oi).and_then(|s| s.container);
                    }
                    if let Some(ct) = class_temp {
                        let mut seen = HashSet::new();
                        target = lookup_method(
                            view,
                            &file_bases,
                            &class_parent,
                            ct,
                            &r.name,
                            0,
                            &mut seen,
                        );
                        if target.is_some() {
                            status = RefStatus::Resolved;
                        }
                    }
                }
                // 命名空间/具名/默认导入的成员调用
                (Some(recv), _) => {
                    if let Some(b) = tables[fi].bindings.get(recv.as_str()).cloned() {
                        match b.kind {
                            BindingKind::Namespace => {
                                // `ns.prop()`：prop 就是目标文件的导出名本身
                                if let Some(pkg) = &b.package {
                                    status = RefStatus::External;
                                    target = external_index.get(pkg).copied();
                                } else if let Some(tf) = &b.target_file {
                                    let mut seen = HashSet::new();
                                    match resolve_export(
                                        &tables,
                                        &view.file_index,
                                        tf,
                                        &r.name,
                                        0,
                                        &mut seen,
                                    ) {
                                        Some(t) => {
                                            status = RefStatus::Resolved;
                                            target = Some(t);
                                        }
                                        None => status = RefStatus::Dynamic,
                                    }
                                } else {
                                    status = RefStatus::Dynamic;
                                }
                            }
                            // 具名/默认导入的成员调用：receiver 通常是类（静态方法/实例方法）
                            BindingKind::Named | BindingKind::Default => {
                                let (st, t0) = resolve_binding_target(&b, &r.name);
                                status = st;
                                target = t0;
                                if let (RefStatus::Resolved, Some(t)) = (status, target) {
                                    if nodes[t].kind == NodeKind::Class {
                                        let mut seen = HashSet::new();
                                        match lookup_method(
                                            view,
                                            &file_bases,
                                            &class_parent,
                                            t,
                                            &r.name,
                                            0,
                                            &mut seen,
                                        ) {
                                            Some(m) => target = Some(m),
                                            None => {
                                                status = RefStatus::Dynamic;
                                                target = None;
                                            }
                                        }
                                    } else if nodes[t].kind == NodeKind::External {
                                        status = RefStatus::External;
                                    } else {
                                        // receiver 是函数/接口目标：类型未知，不猜
                                        status = RefStatus::Dynamic;
                                        target = None;
                                    }
                                }
                            }
                        }
                    } else if BUILTIN_RECEIVERS.contains(&recv.as_str()) {
                        status = RefStatus::Builtin;
                    } else {
                        // 本地类名作接收者：Utils.helper() where class Utils
                        let mut hit = None;
                        for (si, s) in view.files[fi].symbols.iter().enumerate() {
                            if s.kind == NodeKind::Class && (s.name == recv.as_str()) {
                                hit = Some(symbol_temp(fi, si));
                                break;
                            }
                        }
                        if let Some(ct) = hit {
                            let mut seen = HashSet::new();
                            target = lookup_method(
                                view,
                                &file_bases,
                                &class_parent,
                                ct,
                                &r.name,
                                0,
                                &mut seen,
                            );
                            status = if target.is_some() {
                                RefStatus::Resolved
                            } else {
                                RefStatus::Dynamic
                            };
                        }
                    }
                }
                (None, _) => {
                    // 裸名调用三级策略
                    if r.shadowed {
                        status = RefStatus::Dynamic; // 被参数/局部变量遮蔽，不猜
                    } else if let Some(b) = tables[fi].bindings.get(&r.name).cloned() {
                        let (st, t0) = resolve_binding_target(&b, &r.name);
                        status = st;
                        target = t0;
                    } else if let Some(&t) = tables[fi].by_name.get(&r.name) {
                        status = RefStatus::Resolved;
                        target = Some(t);
                    } else if BUILTIN_GLOBALS.contains(&r.name.as_str()) {
                        status = RefStatus::Builtin;
                    } else {
                        match global_exports.get(&r.name).map(|v| v.len()).unwrap_or(0) {
                            1 => {
                                status = RefStatus::Resolved;
                                target =
                                    global_exports.get(&r.name).and_then(|v| v.first()).copied();
                            }
                            0 => status = RefStatus::Dynamic,
                            _ => status = RefStatus::Ambiguous,
                        }
                    }
                }
            }

            refs_out.push(RefRow {
                file_id: fi as i64,
                owner_temp_id: owner_temp,
                kind: match r.kind {
                    RawRefKind::Call => "call",
                    RawRefKind::New => "new",
                    _ => "jsx",
                }
                .into(),
                name: r.name.clone(),
                receiver: r.receiver.clone(),
                shadowed: r.shadowed,
                status,
                target_temp_id: target,
                span_start: r.span.0,
                span_end: r.span.1,
                line_start: r.line_start,
            });
            if status == RefStatus::Resolved {
                if let Some(t) = target {
                    bump(&mut edge_acc, owner_temp, t, "calls");
                }
            }
        }
    }

    // extends/implements → 边
    for r in &refs_out {
        if r.status == RefStatus::Resolved {
            if let Some(t) = r.target_temp_id {
                let kind: &'static str = match r.kind.as_str() {
                    "extends" => "extends",
                    "implements" => "implements",
                    _ => continue,
                };
                bump(&mut edge_acc, r.owner_temp_id, t, kind);
            }
        }
    }
    for (child, parent) in &overrides_edges {
        bump(&mut edge_acc, *child, *parent, "overrides");
    }
    // imports → 文件级边
    for imp in &imports_out {
        let src = file_bases[imp.file_id as usize];
        if let Some(tp) = &imp.target_path {
            if let Some(&dst) = view.file_index.get(tp) {
                bump(&mut edge_acc, src, file_bases[dst], "imports");
            }
        } else if let Some(pkg) = &imp.package {
            if let Some(&t) = external_index.get(pkg) {
                bump(&mut edge_acc, src, t, "imports");
            }
        }
    }

    let edges = edge_acc
        .into_iter()
        .map(|((src, dst, k), w)| EdgeRow {
            src_temp_id: src,
            dst_temp_id: dst,
            kind: match k {
                "calls" => EdgeKind::Calls,
                "overrides" => EdgeKind::Overrides,
                "extends" => EdgeKind::Extends,
                "implements" => EdgeKind::Implements,
                _ => EdgeKind::Imports,
            },
            weight: w,
        })
        .collect();

    Ok(ResolveOutput {
        nodes,
        imports: imports_out,
        refs: refs_out,
        edges,
    })
}
