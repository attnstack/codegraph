//! SQLite 图存储。
//! 写路径：`apply_build` 单事务原子落库（读者要么看到旧图要么看到新图，WAL 快照隔离）。
//! 读路径：`load_snapshot` 一次装载内存图，算法层不再触库——查询延迟可预测。

use codegraph_core::model::{
    AdjEdge, CallSite, DynamicRef, EdgeKind, ImportRow, NodeKind, NodeRef, NodeRow, RefRow,
    RefStatus, Snapshot,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type StoreResult<T> = Result<T, StoreError>;

/// 单个文件的拉平材料：(path, nodes, imports, refs)，未变更文件重建视图用
pub type FileMaterial = (String, String, Vec<NodeRow>, Vec<ImportRow>, Vec<RefRow>);

#[derive(Debug, Clone, Serialize)]
pub struct FileRow {
    pub id: i64,
    pub path: String,
    pub hash: String,
    pub size: i64,
    pub mtime_ns: i64,
    pub loc: i64,
    pub status: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FileMeta {
    pub path: String,
    pub hash: String,
    pub size: i64,
    pub mtime_ns: i64,
    pub loc: i64,
    /// ok | partial
    pub status: String,
    /// ts | python
    pub lang: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub files: i64,
    pub nodes: i64,
    pub edges: i64,
    pub refs: i64,
    pub externals: i64,
    pub partial_files: i64,
    pub db_bytes: u64,
    pub by_node_kind: Vec<(String, i64)>,
    pub by_edge_kind: Vec<(String, i64)>,
    pub by_ref_status: Vec<(String, i64)>,
}

/// 一次构建计划：view 顺序的文件元数据 + 变更集 + 解析输出。
/// 约定：ResolveOutput 中 file_id 字段在构建期承载「view 顺序下标」，
/// 落库时由本模块翻译为真实 files.id；nodes.temp_id 承载输出数组下标。
pub struct BuildPlan {
    pub file_metas: Vec<FileMeta>,
    pub changed: std::collections::HashSet<String>,
    pub deleted: Vec<String>,
    /// 解析输出；file_id 字段承载 view 顺序下标（见上方约定）
    pub output: codegraph_core::resolve::ResolveOutput,
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildStats {
    pub files: usize,
    pub changed: usize,
    pub deleted: usize,
    pub nodes: usize,
    pub refs: usize,
    pub edges: usize,
    pub duration_ms: u128,
}

pub struct GraphDb {
    conn: Connection,
    path: PathBuf,
}

impl GraphDb {
    pub fn open(path: &Path) -> StoreResult<GraphDb> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 1 {
            conn.execute_batch(include_str!("schema.sql"))?;
            conn.pragma_update(None, "user_version", 2)?;
        } else if version == 1 {
            // v1 -> v2：files 加 lang 列（python 语言支持）
            conn.execute_batch("ALTER TABLE files ADD COLUMN lang TEXT NOT NULL DEFAULT 'ts';")?;
            conn.pragma_update(None, "user_version", 2)?;
        }
        Ok(GraphDb {
            conn,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get_meta(&self, key: &str) -> StoreResult<Option<String>> {
        let v = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?;
        Ok(v)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> StoreResult<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn list_files(&self) -> StoreResult<Vec<FileRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, hash, size, mtime_ns, loc, status, error FROM files ORDER BY path",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(FileRow {
                id: r.get(0)?,
                path: r.get(1)?,
                hash: r.get(2)?,
                size: r.get(3)?,
                mtime_ns: r.get(4)?,
                loc: r.get(5)?,
                status: r.get(6)?,
                error: r.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 全量拉平 DB 里的文件材料（未变更文件重建视图用）。
    /// 返回按 path 排序的 (path, nodes, imports, refs)。nodes 含文件节点（kind=file）。
    pub fn load_view_material(&self) -> StoreResult<Vec<FileMaterial>> {
        // 分三次全量查询再按 file_id 分组（比每文件一查少 N 次 round-trip）
        let mut by_file: HashMap<i64, (String, String)> = HashMap::new();
        {
            let mut stmt = self.conn.prepare("SELECT id, path, lang FROM files")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (id, path, lang) = row?;
                by_file.insert(id, (path, lang));
            }
        }
        let mut nodes: HashMap<i64, Vec<NodeRow>> = HashMap::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT id, file_id, kind, name, qualified_name, is_exported,
                        span_start, span_end, line_start, line_end, extra
                 FROM nodes ORDER BY id",
            )?;
            let rows = stmt.query_map([], |r| {
                let kind_s: String = r.get(2)?;
                Ok((
                    r.get::<_, i64>(1)?,
                    NodeRow {
                        temp_id: 0,
                        file_id: Some(r.get::<_, i64>(1)?),
                        kind: node_kind_from_str(&kind_s),
                        name: r.get(3)?,
                        qualified_name: r.get(4)?,
                        is_exported: r.get::<_, i64>(5)? != 0,
                        span_start: r.get::<_, i64>(6)? as usize,
                        span_end: r.get::<_, i64>(7)? as usize,
                        line_start: r.get(8)?,
                        line_end: r.get(9)?,
                        extra: r.get(10)?,
                    },
                ))
            })?;
            for row in rows {
                let (fid, n) = row?;
                nodes.entry(fid).or_default().push(n);
            }
        }
        let mut imports: HashMap<i64, Vec<ImportRow>> = HashMap::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT file_id, spec, kind, local_name, remote_name, target_path, package FROM imports",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    ImportRow {
                        file_id: 0,
                        spec: r.get(1)?,
                        kind: r.get(2)?,
                        local_name: r.get(3)?,
                        remote_name: r.get(4)?,
                        target_path: r.get(5)?,
                        package: r.get(6)?,
                    },
                ))
            })?;
            for row in rows {
                let (fid, mut i) = row?;
                i.file_id = fid;
                imports.entry(fid).or_default().push(i);
            }
        }
        let mut refs: HashMap<i64, Vec<RefRow>> = HashMap::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT file_id, owner_id, kind, name, receiver, shadowed, status, target_id,
                        span_start, span_end, line_start
                 FROM refs",
            )?;
            let rows = stmt.query_map([], |r| {
                let status_s: String = r.get(6)?;
                Ok((
                    r.get::<_, i64>(0)?,
                    RefRow {
                        file_id: 0,
                        owner_temp_id: r.get::<_, Option<i64>>(1)?.unwrap_or(0) as usize,
                        kind: r.get(2)?,
                        name: r.get(3)?,
                        receiver: r.get(4)?,
                        shadowed: r.get::<_, i64>(5)? != 0,
                        status: ref_status_from_str(&status_s),
                        target_temp_id: r.get::<_, Option<i64>>(7)?.map(|x| x as usize),
                        span_start: r.get::<_, i64>(8)? as usize,
                        span_end: r.get::<_, i64>(9)? as usize,
                        line_start: r.get(10)?,
                    },
                ))
            })?;
            for row in rows {
                let (fid, mut r) = row?;
                r.file_id = fid;
                refs.entry(fid).or_default().push(r);
            }
        }
        let mut out = Vec::new();
        for (fid, (path, lang)) in by_file {
            out.push((
                path,
                lang,
                nodes.remove(&fid).unwrap_or_default(),
                imports.remove(&fid).unwrap_or_default(),
                refs.remove(&fid).unwrap_or_default(),
            ));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// 单事务原子落库。见 BuildPlan 注释里的 id 翻译约定。
    pub fn apply_build(&mut self, plan: BuildPlan) -> StoreResult<BuildStats> {
        let start = Instant::now();
        let tx = self.conn.transaction()?;

        // 1) 删除消失的文件（FK 级联清掉其 nodes/imports/refs）
        {
            let mut stmt = tx.prepare("DELETE FROM files WHERE path = ?1")?;
            for p in &plan.deleted {
                stmt.execute([p.as_str()])?;
            }
        }

        // 2) upsert files → 真实 id
        let mut file_ids: Vec<i64> = Vec::with_capacity(plan.file_metas.len());
        {
            let mut stmt = tx.prepare(
                "INSERT INTO files(path, hash, size, mtime_ns, loc, status, lang, error)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(path) DO UPDATE SET hash = excluded.hash, size = excluded.size,
                   mtime_ns = excluded.mtime_ns, loc = excluded.loc, status = excluded.status,
                   lang = excluded.lang, error = excluded.error",
            )?;
            for m in &plan.file_metas {
                stmt.execute(params![
                    m.path, m.hash, m.size, m.mtime_ns, m.loc, m.status, m.lang, m.error
                ])?;
            }
        }
        {
            let mut stmt = tx.prepare("SELECT id FROM files WHERE path = ?1")?;
            for m in &plan.file_metas {
                let id: i64 = stmt.query_row([&m.path], |r| r.get(0))?;
                file_ids.push(id);
            }
        }

        // 3) 节点翻译表：temp_id → 真实 id
        let n_rows = plan.output.nodes.len();
        let mut node_ids: Vec<Option<i64>> = vec![None; n_rows];
        let insert_node = |tx: &rusqlite::Transaction,
                           fid: Option<i64>,
                           row: &NodeRow,
                           node_ids: &mut Vec<Option<i64>>|
         -> StoreResult<()> {
            tx.execute(
                "INSERT INTO nodes(file_id, kind, name, qualified_name, is_exported,
                                   span_start, span_end, line_start, line_end, extra)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    fid,
                    row.kind.as_str(),
                    row.name,
                    row.qualified_name,
                    row.is_exported as i64,
                    row.span_start as i64,
                    row.span_end as i64,
                    row.line_start,
                    row.line_end,
                    row.extra
                ],
            )?;
            node_ids[row.temp_id] = Some(tx.last_insert_rowid());
            Ok(())
        };

        // 3a) 变更文件：清空重写
        {
            let mut wipe = tx.prepare("DELETE FROM nodes WHERE file_id = ?1")?;
            for (order, m) in plan.file_metas.iter().enumerate() {
                if !plan.changed.contains(&m.path) {
                    continue;
                }
                let fid = file_ids[order];
                wipe.execute([fid])?;
                for row in plan
                    .output
                    .nodes
                    .iter()
                    .filter(|r| r.file_id == Some(order as i64))
                {
                    insert_node(&tx, Some(fid), row, &mut node_ids)?;
                }
            }
        }
        // 3b) 未变更文件：按 id 序对齐（节点插入顺序 = temp 顺序）
        {
            let mut q = tx.prepare("SELECT id FROM nodes WHERE file_id = ?1 ORDER BY id")?;
            for (order, m) in plan.file_metas.iter().enumerate() {
                if plan.changed.contains(&m.path) {
                    continue;
                }
                let fid = file_ids[order];
                let ids: Vec<i64> = q
                    .query_map([fid], |r| r.get(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                let expected: Vec<&NodeRow> = plan
                    .output
                    .nodes
                    .iter()
                    .filter(|r| r.file_id == Some(order as i64))
                    .collect();
                if ids.len() == expected.len() {
                    for (row, id) in expected.into_iter().zip(ids) {
                        node_ids[row.temp_id] = Some(id);
                    }
                } else {
                    // 计数不符（历史损坏/schema 漂移）：降级为该文件重写，保证自愈
                    tracing::warn!(
                        "文件 {} 节点计数不符（{} vs {}），降级重写",
                        m.path,
                        ids.len(),
                        expected.len()
                    );
                    drop(q); // q 借用了 tx，先释放再写
                    tx.execute("DELETE FROM nodes WHERE file_id = ?1", [fid])?;
                    for row in expected {
                        insert_node(&tx, Some(fid), row, &mut node_ids)?;
                    }
                    q = tx.prepare("SELECT id FROM nodes WHERE file_id = ?1 ORDER BY id")?;
                }
            }
        }
        // 3c) 外部包伪节点：get_or_create（UNIQUE 索引对 NULL file_id 不生效，代码层去重）
        for row in plan.output.nodes.iter().filter(|r| r.file_id.is_none()) {
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT id FROM nodes WHERE kind = 'external' AND name = ?1",
                    [&row.name],
                    |r| r.get(0),
                )
                .optional()?;
            let id = match existing {
                Some(id) => id,
                None => {
                    tx.execute(
                        "INSERT INTO nodes(file_id, kind, name, qualified_name, is_exported,
                                           span_start, span_end, line_start, line_end, extra)
                         VALUES (NULL, ?1, ?2, ?3, 0, 0, 0, 0, 0, NULL)",
                        params![row.kind.as_str(), row.name, row.qualified_name],
                    )?;
                    tx.last_insert_rowid()
                }
            };
            node_ids[row.temp_id] = Some(id);
        }

        // 4) imports：只重写变更文件的
        {
            let mut wipe = tx.prepare("DELETE FROM imports WHERE file_id = ?1")?;
            let mut ins = tx.prepare(
                "INSERT INTO imports(file_id, spec, kind, local_name, remote_name, target_path, package)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (order, m) in plan.file_metas.iter().enumerate() {
                if !plan.changed.contains(&m.path) {
                    continue;
                }
                let fid = file_ids[order];
                wipe.execute([fid])?;
                for imp in plan
                    .output
                    .imports
                    .iter()
                    .filter(|i| i.file_id == order as i64)
                {
                    ins.execute(params![
                        fid,
                        imp.spec,
                        imp.kind,
                        imp.local_name,
                        imp.remote_name,
                        imp.target_path,
                        imp.package
                    ])?;
                }
            }
        }

        // 5) refs 全量重写（增量正确性的核心：解析结果永远是全量重算的）
        {
            tx.execute("DELETE FROM refs", [])?;
            let mut ins = tx.prepare(
                "INSERT INTO refs(file_id, owner_id, kind, name, receiver, shadowed, status,
                                  target_id, span_start, span_end, line_start)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            for r in &plan.output.refs {
                let fid = file_ids[r.file_id as usize];
                let owner = node_ids.get(r.owner_temp_id).copied().flatten();
                let target = match r.target_temp_id {
                    Some(t) => node_ids.get(t).copied().flatten(),
                    None => None,
                };
                if owner.is_none() {
                    tracing::warn!(
                        "ref owner 缺失（file={}, name={}），跳过",
                        r.file_id,
                        r.name
                    );
                    continue;
                }
                ins.execute(params![
                    fid,
                    owner,
                    r.kind,
                    r.name,
                    r.receiver,
                    r.shadowed as i64,
                    r.status.as_str(),
                    target,
                    r.span_start as i64,
                    r.span_end as i64,
                    r.line_start
                ])?;
            }
        }

        // 6) edges 全量重写
        {
            tx.execute("DELETE FROM edges", [])?;
            let mut ins = tx.prepare(
                "INSERT OR IGNORE INTO edges(src, dst, kind, weight) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for e in &plan.output.edges {
                let (Some(src), Some(dst)) = (
                    node_ids.get(e.src_temp_id).copied().flatten(),
                    node_ids.get(e.dst_temp_id).copied().flatten(),
                ) else {
                    tracing::warn!("edge 端点缺失（kind={}），跳过", e.kind.as_str());
                    continue;
                };
                ins.execute(params![src, dst, e.kind.as_str(), e.weight])?;
            }
        }

        // 7) meta
        tx.execute(
            "INSERT INTO meta(key, value) VALUES ('last_indexed_at', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![chrono_now()],
        )?;
        tx.execute(
            "INSERT INTO meta(key, value) VALUES ('file_count', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![plan.file_metas.len().to_string()],
        )?;

        let stats = BuildStats {
            files: plan.file_metas.len(),
            changed: plan.changed.len(),
            deleted: plan.deleted.len(),
            nodes: plan.output.nodes.len(),
            refs: plan.output.refs.len(),
            edges: plan.output.edges.len(),
            duration_ms: start.elapsed().as_millis(),
        };
        tx.commit()?;
        Ok(stats)
    }

    /// 装载查询快照。cross_file_bonus 由调用方传入（PageRank 边权加成）。
    pub fn load_snapshot(&self, cross_file_bonus: f64) -> StoreResult<Snapshot> {
        let mut snap = Snapshot::default();
        {
            let mut stmt = self.conn.prepare(
                "SELECT n.id, n.kind, n.name, n.qualified_name, n.is_exported, f.path, n.line_start
                 FROM nodes n LEFT JOIN files f ON n.file_id = f.id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(NodeRef {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    name: r.get(2)?,
                    qualified_name: r.get(3)?,
                    is_exported: r.get::<_, i64>(4)? != 0,
                    file: r.get(5)?,
                    line: r.get::<_, Option<i64>>(6)?.map(|x| x as u32),
                })
            })?;
            for n in rows {
                let n = n?;
                snap.nodes.insert(n.id, n);
            }
        }
        {
            let mut stmt = self
                .conn
                .prepare("SELECT src, dst, kind, weight FROM edges")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?;
            for row in rows {
                let (src, dst, kind_s, weight) = row?;
                let Some(kind) = edge_kind_from_str(&kind_s) else {
                    continue;
                };
                let bonus = match (snap.nodes.get(&src), snap.nodes.get(&dst)) {
                    (Some(a), Some(b)) => match (&a.file, &b.file) {
                        (Some(fa), Some(fb)) if fa != fb => cross_file_bonus,
                        _ => 1.0,
                    },
                    _ => 1.0,
                };
                let ae = AdjEdge {
                    other: dst,
                    kind,
                    weight: weight as f64 * bonus,
                };
                snap.out.entry(src).or_default().push(ae.clone());
                let ae_in = AdjEdge {
                    other: src,
                    kind,
                    weight: weight as f64 * bonus,
                };
                snap.inn.entry(dst).or_default().push(ae_in);
            }
        }
        {
            let mut stmt = self.conn.prepare(
                "SELECT r.owner_id, r.target_id, f.path, r.line_start
                 FROM refs r JOIN files f ON r.file_id = f.id
                 WHERE r.status = 'resolved' AND r.target_id IS NOT NULL AND r.owner_id IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?;
            for row in rows {
                let (owner, target, file, line) = row?;
                snap.evidence
                    .entry((owner, target))
                    .or_default()
                    .push(CallSite {
                        file,
                        line: line as u32,
                    });
            }
        }
        {
            let mut stmt = self.conn.prepare(
                "SELECT name, f.path, r.line_start, r.status
                 FROM refs r JOIN files f ON r.file_id = f.id
                 WHERE r.status IN ('dynamic', 'ambiguous')",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(DynamicRef {
                    name: r.get(0)?,
                    file: r.get(1)?,
                    line: r.get::<_, i64>(2)? as u32,
                    status: r.get(3)?,
                })
            })?;
            for row in rows {
                let d = row?;
                snap.dynamic_by_name
                    .entry(d.name.clone())
                    .or_default()
                    .push(d);
            }
        }
        Ok(snap)
    }

    /// 符号寻址解析：`path::name` / `name` / `qualified.name`。
    /// 返回候选列表：0 个 = 未找到；多个 = 歧义（由调用方决定报错或让用户选择）。
    pub fn resolve_symbol_candidates(&self, input: &str) -> StoreResult<Vec<NodeRef>> {
        let mut out = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT n.id, n.kind, n.name, n.qualified_name, n.is_exported, f.path, n.line_start
             FROM nodes n JOIN files f ON n.file_id = f.id
             WHERE (f.path = ?1 OR f.path LIKE '%/' || ?1)
               AND (n.name = ?2 OR n.qualified_name = ?2 OR n.qualified_name LIKE '%.' || ?2)",
        )?;
        if let Some((file_part, name_part)) = input.split_once("::") {
            let rows = stmt.query_map(params![file_part, name_part], row_to_noderef)?;
            for r in rows {
                out.push(r?);
            }
            if !out.is_empty() {
                return Ok(out);
            }
        }
        let mut stmt2 = self.conn.prepare(
            "SELECT n.id, n.kind, n.name, n.qualified_name, n.is_exported, f.path, n.line_start
             FROM nodes n LEFT JOIN files f ON n.file_id = f.id
             WHERE n.name = ?1 OR n.qualified_name = ?1
             ORDER BY n.is_exported DESC, n.kind, n.qualified_name",
        )?;
        let rows = stmt2.query_map([input], row_to_noderef)?;
        for r in rows {
            out.push(r?);
        }
        if !out.is_empty() {
            return Ok(out);
        }
        // 前缀兜底（LIKE 大小写不敏感，ASCII 下符合直觉）
        let escaped = escape_like(input);
        let mut stmt3 = self.conn.prepare(
            "SELECT n.id, n.kind, n.name, n.qualified_name, n.is_exported, f.path, n.line_start
             FROM nodes n LEFT JOIN files f ON n.file_id = f.id
             WHERE n.name LIKE ?1 || '%' ESCAPE '\\'
             ORDER BY n.is_exported DESC, n.kind, n.qualified_name
             LIMIT 20",
        )?;
        let rows = stmt3.query_map([escaped], row_to_noderef)?;
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn search_nodes(&self, query: &str, limit: usize) -> StoreResult<Vec<(NodeRef, String)>> {
        let mut out: Vec<(NodeRef, String)> = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT n.id, n.kind, n.name, n.qualified_name, n.is_exported, f.path, n.line_start
             FROM nodes n LEFT JOIN files f ON n.file_id = f.id
             WHERE n.name = ?1 OR n.qualified_name = ?1
             ORDER BY n.is_exported DESC, n.kind, n.qualified_name LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![query, limit as i64], |r| {
            Ok((row_to_noderef(r)?, "exact".to_string()))
        })?;
        for r in rows {
            out.push(r?);
        }
        if out.len() < limit {
            let remaining = limit - out.len();
            let escaped = escape_like(query);
            let mut stmt2 = self.conn.prepare(
                "SELECT n.id, n.kind, n.name, n.qualified_name, n.is_exported, f.path, n.line_start
                 FROM nodes n LEFT JOIN files f ON n.file_id = f.id
                 WHERE n.name LIKE ?1 || '%' ESCAPE '\\'
                   AND n.name != ?2 AND n.qualified_name != ?2
                 ORDER BY n.is_exported DESC, n.name LIMIT ?3",
            )?;
            let rows = stmt2.query_map(params![escaped, query, remaining as i64], |r| {
                Ok((row_to_noderef(r)?, "prefix".to_string()))
            })?;
            for r in rows {
                out.push(r?);
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    pub fn stats(&self) -> StoreResult<Stats> {
        let count =
            |sql: &str| -> StoreResult<i64> { Ok(self.conn.query_row(sql, [], |r| r.get(0))?) };
        let files = count("SELECT COUNT(*) FROM files")?;
        let nodes = count("SELECT COUNT(*) FROM nodes")?;
        let edges = count("SELECT COUNT(*) FROM edges")?;
        let refs = count("SELECT COUNT(*) FROM refs")?;
        let externals = count("SELECT COUNT(*) FROM nodes WHERE kind = 'external'")?;
        let partial_files = count("SELECT COUNT(*) FROM files WHERE status != 'ok'")?;
        let mut by_node_kind = group_counts(
            &self.conn,
            "SELECT kind, COUNT(*) FROM nodes GROUP BY kind ORDER BY 2 DESC",
        )?;
        let by_edge_kind = group_counts(
            &self.conn,
            "SELECT kind, COUNT(*) FROM edges GROUP BY kind ORDER BY 2 DESC",
        )?;
        let by_ref_status = group_counts(
            &self.conn,
            "SELECT status, COUNT(*) FROM refs GROUP BY status ORDER BY 2 DESC",
        )?;
        by_node_kind.retain(|(k, _)| k != "external");
        let db_bytes = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        Ok(Stats {
            files,
            nodes,
            edges,
            refs,
            externals,
            partial_files,
            db_bytes,
            by_node_kind,
            by_edge_kind,
            by_ref_status,
        })
    }

    /// 全部节点 id + qualified_name（fuzz 对比 / 导出用）
    pub fn all_symbol_nodes(&self) -> StoreResult<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, qualified_name FROM nodes WHERE kind IN ('function','method','class','interface') ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn export_dot(&self) -> StoreResult<String> {
        let mut out = String::from("digraph codegraph {\n  rankdir=LR;\n  node [shape=box, fontname=\"monospace\", fontsize=9];\n");
        let mut stmt = self.conn.prepare(
            "SELECT n.id, n.kind, n.name, n.qualified_name, COALESCE(f.path,'') FROM nodes n
             LEFT JOIN files f ON n.file_id = f.id
             WHERE n.kind IN ('function','method','class','interface','file') ORDER BY n.id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut label_of: HashMap<i64, String> = HashMap::new();
        for row in rows {
            let (id, kind, name, qname, path) = row?;
            let label = if kind == "file" {
                path.clone()
            } else {
                format!("{qname}\\n{path}")
            };
            label_of.insert(id, label.clone());
            let _ = name;
            out.push_str(&format!("  n{id} [label=\"{label}\"];\n"));
        }
        let mut stmt2 = self
            .conn
            .prepare("SELECT src, dst, kind FROM edges WHERE kind IN ('calls','overrides','extends','implements') ORDER BY src, dst")?;
        let rows = stmt2.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (src, dst, kind) = row?;
            if label_of.contains_key(&src) && label_of.contains_key(&dst) {
                out.push_str(&format!("  n{src} -> n{dst} [label=\"{kind}\"];\n"));
            }
        }
        out.push_str("}\n");
        Ok(out)
    }
}

fn row_to_noderef(r: &rusqlite::Row<'_>) -> rusqlite::Result<NodeRef> {
    Ok(NodeRef {
        id: r.get(0)?,
        kind: r.get(1)?,
        name: r.get(2)?,
        qualified_name: r.get(3)?,
        is_exported: r.get::<_, i64>(4)? != 0,
        file: r.get(5)?,
        line: r.get::<_, Option<i64>>(6)?.map(|x| x as u32),
    })
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn group_counts(conn: &Connection, sql: &str) -> StoreResult<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn node_kind_from_str(s: &str) -> NodeKind {
    match s {
        "file" => NodeKind::File,
        "class" => NodeKind::Class,
        "interface" => NodeKind::Interface,
        "function" => NodeKind::Function,
        "method" => NodeKind::Method,
        _ => NodeKind::External,
    }
}

fn edge_kind_from_str(s: &str) -> Option<EdgeKind> {
    match s {
        "calls" => Some(EdgeKind::Calls),
        "overrides" => Some(EdgeKind::Overrides),
        "extends" => Some(EdgeKind::Extends),
        "implements" => Some(EdgeKind::Implements),
        "imports" => Some(EdgeKind::Imports),
        _ => None,
    }
}

fn ref_status_from_str(s: &str) -> RefStatus {
    match s {
        "resolved" => RefStatus::Resolved,
        "builtin" => RefStatus::Builtin,
        "ambiguous" => RefStatus::Ambiguous,
        "external" => RefStatus::External,
        _ => RefStatus::Dynamic,
    }
}

fn chrono_now() -> String {
    // RFC3339 时间戳；不引入 chrono，标准库足够（只用于 meta 展示）
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("unix:{}ms", now.as_millis())
}
