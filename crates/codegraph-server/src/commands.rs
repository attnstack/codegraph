//! 构建编排（walk → parse → resolve → store）与查询命令的共享逻辑。
//!
//! 编排的关键决策：「变更检测 = mtime+size 预筛 → hash 确认」，
//! 「重解析只做变更文件，重解析ing 永远跑全量视图」。二者合起来
//! 让增量更新既快（贵的步骤只做增量）又对（便宜的步骤全量重算）。

use crate::git;
use anyhow::{anyhow, Context, Result};
use codegraph_core::config::Config;
use codegraph_core::model::NodeRef;
use codegraph_core::parse::{self, RawFile};
use codegraph_core::resolve;
use codegraph_core::view::{ViewFile, ViewRepo};
use codegraph_core::walk::{self, DiscoveredFile};
use codegraph_store::{BuildPlan, FileMeta, GraphDb};
use rayon::prelude::*;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;

pub fn load_cfg(root: Option<std::path::PathBuf>) -> Result<Config> {
    let root = root.unwrap_or_else(|| std::path::PathBuf::from("."));
    let cfg = Config::load(&root);
    Ok(cfg)
}

/// 内容指纹。DefaultHasher::new() 使用固定键（SipHash-1-3），跨进程稳定；
/// 只用于内部变更检测，不是密码学摘要（诚实标注，避免被误当 SHA）。
pub fn content_hash(bytes: &[u8]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// 构建数据库不存在时打开查询命令会直接报错并给出修复动作
pub fn open_for_query(cfg: &Config) -> Result<(GraphDb, codegraph_core::model::Snapshot)> {
    let db_path = cfg.db_path();
    if !db_path.exists() {
        return Err(anyhow!(
            "未找到索引 {} —— 先运行 `codegraph index --root {}`",
            db_path.display(),
            cfg.root.display()
        ));
    }
    let mut db = GraphDb::open(&db_path)?;
    if cfg.auto_refresh {
        // 查询前自愈：文件系统与图谱之间可能存在漂移（mtime/size 预筛，零哈希开销）
        if needs_refresh(&db, cfg)? {
            tracing::info!("检测到文件变化，自动增量重建…");
            run_build(cfg, false)?;
            db = GraphDb::open(&db_path)?;
        }
    }
    let snap = db.load_snapshot(cfg.cross_file_bonus)?;
    Ok((db, snap))
}

fn needs_refresh(db: &GraphDb, cfg: &Config) -> Result<bool> {
    let discovered = walk::discover(&cfg.root, cfg)?;
    let existing: HashMap<String, (i64, i64)> = db
        .list_files()?
        .into_iter()
        .map(|f| (f.path, (f.size, f.mtime_ns)))
        .collect();
    let mut seen: HashSet<&str> = HashSet::new();
    for f in &discovered {
        seen.insert(f.rel.as_str());
        match existing.get(&f.rel) {
            None => return Ok(true),
            Some(&(size, mtime)) => {
                if size != f.size as i64 || mtime != f.mtime_ns as i64 {
                    return Ok(true);
                }
            }
        }
    }
    for p in existing.keys() {
        if !seen.contains(p.as_str()) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// MCP 层复用：工具调用前的自愈检测
pub fn needs_refresh_pub(db: &GraphDb, cfg: &Config) -> Result<bool> {
    needs_refresh(db, cfg)
}

#[derive(Debug, Serialize, Clone)]
pub struct BuildStatsPublic {
    pub files: usize,
    pub changed: usize,
    pub deleted: usize,
    pub nodes: usize,
    pub refs: usize,
    pub edges: usize,
    pub duration_ms: u128,
}

pub fn run_build(cfg: &Config, force: bool) -> Result<BuildStatsPublic> {
    let t0 = std::time::Instant::now();
    let mut db = GraphDb::open(&cfg.db_path())?;
    let discovered = walk::discover(&cfg.root, cfg)
        .with_context(|| format!("遍历目录 {}", cfg.root.display()))?;

    let existing: HashMap<String, codegraph_store::FileRow> = db
        .list_files()?
        .into_iter()
        .map(|f| (f.path.clone(), f))
        .collect();

    // ---- 变更检测：mtime+size 预筛 → hash 确认 ----
    let mut changed_files: Vec<&DiscoveredFile> = Vec::new();
    let mut changed_hash: HashMap<String, String> = HashMap::new();
    for f in &discovered {
        let need = force
            || match existing.get(&f.rel) {
                None => true,
                Some(row) => row.size != f.size as i64 || row.mtime_ns != f.mtime_ns as i64,
            };
        if need {
            let bytes =
                std::fs::read(&f.abs).with_context(|| format!("读取文件 {}", f.abs.display()))?;
            let hash = content_hash(&bytes);
            let really_changed = match existing.get(&f.rel) {
                // mtime 变了但内容没变（touch / 编辑器重写）：免于重解析
                Some(row) => row.hash != hash,
                None => true,
            };
            if really_changed {
                changed_hash.insert(f.rel.clone(), hash);
                changed_files.push(f);
            }
        }
    }
    let discovered_set: HashSet<&str> = discovered.iter().map(|f| f.rel.as_str()).collect();
    let deleted: Vec<String> = existing
        .keys()
        .filter(|p| !discovered_set.contains(p.as_str()))
        .cloned()
        .collect();

    // ---- 并行解析变更文件 ----
    let parsed: HashMap<String, RawFile> = changed_files
        .par_iter()
        .map(|f| -> anyhow::Result<(String, RawFile)> {
            let bytes = std::fs::read(&f.abs)?;
            Ok((f.rel.clone(), parse::parse_file(&f.rel, &bytes)))
        })
        .collect::<anyhow::Result<HashMap<_, _>>>()
        .context("并行解析失败")?;
    for (path, raw) in &parsed {
        if raw.error_count > 0 {
            tracing::warn!(
                "{}: {} 个语法错误节点，结果记为 partial",
                path,
                raw.error_count
            );
        }
    }

    // ---- 组装全量视图（未变更文件从 DB 拉平）----
    let material: HashMap<String, codegraph_store::FileMaterial> = db
        .load_view_material()?
        .into_iter()
        .map(|fm| (fm.0.clone(), fm))
        .collect();
    let mut view_files: Vec<ViewFile> = Vec::with_capacity(discovered.len());
    for f in &discovered {
        if let Some(raw) = parsed.get(&f.rel) {
            view_files.push(ViewFile::from_raw(&f.rel, raw.clone()));
        } else if let Some((_, _lang, nodes, imports, refs)) = material.get(&f.rel) {
            let lang = codegraph_core::parse::Lang::from_path(&f.rel).unwrap_or_default();
            view_files.push(ViewFile::from_rows(&f.rel, lang, nodes, imports, refs));
        }
        // else: 发现与 DB 之间竞态（文件刚被删），下一轮自愈，这里静默跳过
    }
    let view = ViewRepo::new(view_files);

    // ---- 全量解析 + 落库 ----
    // 可选第四级解析：--types 时用 tsc sidecar 收集类型提示，三级失败处兜底
    let mut hints: Option<codegraph_core::resolve::TypeHints> = None;
    if cfg.use_types && crate::typesidecar::available(cfg) {
        match crate::typesidecar::collect_hints(cfg, &view) {
            Ok(h) => {
                tracing::info!("tsc 类型提示 {} 条", h.map.len());
                hints = Some(h);
            }
            Err(e) => tracing::warn!("tsc sidecar 失败，退化为纯语法解析: {e:#}"),
        }
    }
    let output = resolve::resolve_all(&view, cfg, hints.as_ref())?;
    let mut file_metas: Vec<FileMeta> = Vec::with_capacity(view.files.len());
    for vf in &view.files {
        let (hash, size, mtime_ns, loc) =
            if let Some(f) = discovered.iter().find(|d| d.rel == vf.path) {
                let h = changed_hash.get(&vf.path).cloned().unwrap_or_else(|| {
                    existing
                        .get(&vf.path)
                        .map(|r| r.hash.clone())
                        .unwrap_or_default()
                });
                (h, f.size as i64, f.mtime_ns as i64, vf.loc as i64)
            } else {
                continue;
            };
        let error = parsed.get(&vf.path).and_then(|raw| {
            (raw.error_count > 0).then(|| format!("{} 个语法错误节点", raw.error_count))
        });
        file_metas.push(FileMeta {
            path: vf.path.clone(),
            hash,
            size,
            mtime_ns,
            loc,
            lang: vf.lang.as_str().to_string(),
            status: if error.is_some() {
                "partial".into()
            } else {
                "ok".into()
            },
            error,
        });
    }
    let changed_set: HashSet<String> = changed_hash.keys().cloned().collect();
    let plan = BuildPlan {
        file_metas,
        changed: changed_set,
        deleted,
        output,
    };
    let stats = db.apply_build(plan)?;

    tracing::info!(
        "build 完成: files={} changed={} deleted={} nodes={} refs={} edges={} 用时 {}ms",
        stats.files,
        stats.changed,
        stats.deleted,
        stats.nodes,
        stats.refs,
        stats.edges,
        stats.duration_ms
    );
    let _ = t0;
    Ok(BuildStatsPublic {
        files: stats.files,
        changed: stats.changed,
        deleted: stats.deleted,
        nodes: stats.nodes,
        refs: stats.refs,
        edges: stats.edges,
        duration_ms: stats.duration_ms,
    })
}

// ---------- 符号寻址 ----------

pub fn resolve_unique(db: &GraphDb, input: &str) -> Result<NodeRef> {
    let cands = db.resolve_symbol_candidates(input)?;
    match cands.len() {
        0 => Err(anyhow!(
            "符号 `{input}` 未找到。先用 `codegraph search {input}` 看看有哪些相近符号"
        )),
        1 => Ok(cands.into_iter().next().unwrap()),
        _ => {
            // 多候选：优先唯一完全名命中（name 或 qualified_name 精确等于输入）
            let exact: Vec<&NodeRef> = cands
                .iter()
                .filter(|c| c.name == input || c.qualified_name == input)
                .collect();
            if exact.len() == 1 {
                return Ok(exact[0].clone());
            }
            let list = cands
                .iter()
                .map(|c| {
                    format!(
                        "  - {} ({}) {}",
                        c.qualified_name,
                        c.kind,
                        c.file.as_deref().unwrap_or("<external>")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Err(anyhow!(
                "符号 `{input}` 有 {} 个同名候选，请用 `文件路径::符号名` 消歧：\n{}",
                cands.len(),
                list
            ))
        }
    }
}

// ---------- 代码考古 ----------

#[derive(Debug, Serialize, Clone)]
pub struct CommitSummary {
    pub sha: String,
    pub summary: String,
    pub author: String,
    pub date: String,
    pub lines: usize,
    pub pr: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ArchReport {
    pub target: String,
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
    pub total_lines: usize,
    pub authors: Vec<(String, usize)>,
    pub commits: Vec<CommitSummary>,
    pub note: String,
}

pub fn archaeology(cfg: &Config, target: &str) -> Result<ArchReport> {
    let (db, _snap) = open_for_query(cfg)?;
    // 先按符号解析；失败再按 `path:start-end` / `path:line` 解析
    let (file, ls, le) = match resolve_unique(&db, target) {
        Ok(node) => {
            let Some(f) = node.file.clone() else {
                return Err(anyhow!(
                    "符号 `{target}` 不属于仓库内文件（外部包节点无法考古）"
                ));
            };
            let abs = cfg.root.join(&f);
            let start = node.line.unwrap_or(1);
            let end_line = symbol_end_line(&abs, start);
            (f, start, end_line)
        }
        Err(_) => parse_line_target(target)?,
    };

    let blame = git::blame_lines(&cfg.root, &file, ls, le)
        .map_err(|e| anyhow!("git blame 失败（需要 git 仓库与 git 可执行文件）: {e}"))?;

    let total = blame.len();
    let mut authors: HashMap<String, usize> = HashMap::new();
    let mut commits: HashMap<String, CommitSummary> = HashMap::new();
    for b in &blame {
        *authors.entry(b.author.clone()).or_insert(0) += 1;
        let e = commits
            .entry(b.commit.clone())
            .or_insert_with(|| CommitSummary {
                sha: short_sha(&b.commit),
                summary: b.summary.clone(),
                author: b.author.clone(),
                date: unix_to_date(b.author_time),
                lines: 0,
                pr: extract_pr(&b.summary),
            });
        e.lines += 1;
    }
    let mut authors: Vec<(String, usize)> = authors.into_iter().collect();
    authors.sort_by_key(|(name, lines)| (std::cmp::Reverse(*lines), name.clone()));
    let mut commits: Vec<CommitSummary> = commits.into_values().collect();
    commits.sort_by_key(|c| std::cmp::Reverse(c.lines));

    Ok(ArchReport {
        target: target.to_string(),
        file,
        line_start: ls,
        line_end: le,
        total_lines: total,
        authors,
        commits,
        note: "贡献度按当前仍存活的行归属（git blame 语义）；改名/移动由 -C 跟踪".into(),
    })
}

fn parse_line_target(target: &str) -> Result<(String, u32, u32)> {
    let (path, range) = target.split_once(':').ok_or_else(|| {
        anyhow!("无法识别目标 `{target}`：应为 `符号`、`path:line` 或 `path:start-end`")
    })?;
    let (ls, le) = if let Some((a, b)) = range.split_once('-') {
        (
            a.parse::<u32>().context("起始行号解析失败")?,
            b.parse::<u32>().context("结束行号解析失败")?,
        )
    } else {
        let l: u32 = range.parse().context("行号解析失败")?;
        (l, l)
    };
    if le < ls {
        return Err(anyhow!("行范围无效：start {ls} > end {le}"));
    }
    Ok((path.to_string(), ls, le))
}

/// 从节点起始行向后扫到下一个顶级声明之前，估算符号结束行（考古用，误差可接受）
fn symbol_end_line(abs: &Path, start_line: u32) -> u32 {
    let Ok(text) = std::fs::read_to_string(abs) else {
        return start_line;
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = (start_line as usize).saturating_sub(1).min(lines.len());
    let mut depth: i32 = 0;
    let mut seen_open = false;
    for (i, line) in lines.iter().enumerate().skip(start) {
        for c in line.chars() {
            match c {
                '{' => {
                    depth += 1;
                    seen_open = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        if seen_open && depth <= 0 {
            return (i + 1) as u32;
        }
        // 单行声明（箭头函数表达式等）：连续 1 行无 { 视为结束
        if !seen_open && i > start && (line.trim().ends_with(';') || line.trim().is_empty()) {
            return (i + 1) as u32;
        }
    }
    (lines.len().max(start_line as usize)) as u32
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(8).collect()
}

fn extract_pr(summary: &str) -> Option<String> {
    // 只认 message 里的 (#123) 约定（本地仓库无 GitHub API 依赖）
    let pos = summary.find("(#")?;
    let rest = &summary[pos + 2..];
    let end = rest.find(')')?;
    let num = &rest[..end];
    if num.chars().all(|c| c.is_ascii_digit()) && !num.is_empty() {
        Some(format!("#{}", num))
    } else {
        None
    }
}

fn unix_to_date(ts: i64) -> String {
    // Unix 天数 → (Y, M, D)：Howard Hinnant 的 civil_from_days 算法，标准库无日期格式化
    let days = ts.div_euclid(86400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}
