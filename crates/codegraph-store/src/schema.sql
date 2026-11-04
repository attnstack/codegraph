-- codegraph 图存储 schema v1。
-- 设计要点：
-- 1) refs 是证据链的物理载体（每个调用点一行，可跳源码）；edges 是按 (src,dst,kind)
--    聚合的物化视图（weight=调用次数），查询走 edges，证据走 refs。
-- 2) nodes 的身份是 (file_id, kind, qualified_name)，span 只是属性——
--    span 进身份会让每次编辑都洗牌全部 id，增量更新失去意义。
-- 3) 外部包是 kind='external' 的伪节点（file_id 为 NULL），import 边指向它们，
--    使 import 图完整可查但不展开包体。
-- 4) 每次 build 全量重写 refs/edges（改写量 ~ 数万行，毫秒级），
--    nodes/imports 只重写变更文件——正确性与写放大之间取的显式权衡。

CREATE TABLE IF NOT EXISTS files(
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL UNIQUE,
  hash TEXT NOT NULL,
  size INTEGER NOT NULL,
  mtime_ns INTEGER NOT NULL,
  loc INTEGER NOT NULL,
  status TEXT NOT NULL DEFAULT 'ok',
  lang TEXT NOT NULL DEFAULT 'ts',
  error TEXT
);

CREATE TABLE IF NOT EXISTS nodes(
  id INTEGER PRIMARY KEY,
  file_id INTEGER REFERENCES files(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  name TEXT NOT NULL,
  qualified_name TEXT NOT NULL,
  is_exported INTEGER NOT NULL DEFAULT 0,
  span_start INTEGER NOT NULL DEFAULT 0,
  span_end INTEGER NOT NULL DEFAULT 0,
  line_start INTEGER NOT NULL DEFAULT 0,
  line_end INTEGER NOT NULL DEFAULT 0,
  extra TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_nodes_ident ON nodes(file_id, kind, qualified_name);
CREATE INDEX IF NOT EXISTS idx_nodes_name ON nodes(name);
CREATE INDEX IF NOT EXISTS idx_nodes_qname ON nodes(qualified_name);

CREATE TABLE IF NOT EXISTS imports(
  id INTEGER PRIMARY KEY,
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  spec TEXT NOT NULL,
  kind TEXT NOT NULL,
  local_name TEXT,
  remote_name TEXT,
  target_path TEXT,
  package TEXT
);
CREATE INDEX IF NOT EXISTS idx_imports_file ON imports(file_id);

CREATE TABLE IF NOT EXISTS refs(
  id INTEGER PRIMARY KEY,
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  owner_id INTEGER REFERENCES nodes(id) ON DELETE SET NULL,
  kind TEXT NOT NULL,
  name TEXT NOT NULL,
  receiver TEXT,
  shadowed INTEGER NOT NULL DEFAULT 0,
  status TEXT NOT NULL,
  target_id INTEGER REFERENCES nodes(id) ON DELETE SET NULL,
  span_start INTEGER NOT NULL DEFAULT 0,
  span_end INTEGER NOT NULL DEFAULT 0,
  line_start INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_refs_file ON refs(file_id);
CREATE INDEX IF NOT EXISTS idx_refs_target ON refs(target_id) WHERE target_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_refs_dyn ON refs(name) WHERE status IN ('dynamic','ambiguous');

CREATE TABLE IF NOT EXISTS edges(
  src INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  dst INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  weight INTEGER NOT NULL DEFAULT 1,
  PRIMARY KEY(src, dst, kind)
);
CREATE INDEX IF NOT EXISTS idx_edges_dst ON edges(kind, dst);
CREATE INDEX IF NOT EXISTS idx_edges_src ON edges(kind, src);

CREATE TABLE IF NOT EXISTS meta(
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
