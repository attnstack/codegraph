# codegraph 架构

## 总览

```
                        ┌────────────────────────────────────────────┐
                        │              codegraph (Rust 单二进制)       │
                        │                                            │
  文件发现 walk.rs ──►  解析 parse.rs ──►  解析 resolve.rs ──►  落库 store   │
  (gitignore 感知)     (tree-sitter)    (三级符号解析)     (SQLite/WAL)   │
                        │                                 │          │
                        │        内存快照 Snapshot         │          │
                        │              │                  │          │
                        │      算法 rank.rs/query.rs       │          │
                        │   (风险传播/双向BFS/PageRank)     │          │
                        │              │                  │          │
                        │        ┌─────┴─────┐            │          │
                        ▼        ▼           ▼            ▼          │
                     CLI (clap)   MCP stdio (手写 JSON-RPC)          │
                                  (官方 TS SDK 契约测试验证)           │
                        └────────────────────────────────────────────┘
```

## 三层 crate 的依赖方向（单向，无环）

```
codegraph-core（纯逻辑，不依赖 SQLite）
    ▲
codegraph-store（SQLite 持久化，依赖 core 的模型类型）
    ▲
codegraph-server（编排 + CLI + MCP + git，依赖前两者）
```

- **core**：图模型、文件发现、tree-sitter 抽取、import/符号三级解析、查询算法。
  全部算法可对内存视图（`ViewRepo` / `Snapshot`）单测，不需要数据库。
- **store**：schema 迁移（PRAGMA user_version）、单事务原子落库、快照装载、符号寻址。
- **server**：`walk → parse(rayon 并行) → resolve(全量视图) → store(单事务)` 的编排，
  以及 CLI/MCP 两个出口。构建编排放 server 而不是 core/store，是为了保住
  core 的纯度与 store 的职责单一。

## 数据流：一次构建

1. **发现**（walk.rs）：`ignore` crate 遍历，gitignore 感知，目录黑名单剪枝，
   输出统一 `/` 分隔的仓库相对路径（Windows 归一化点）。
2. **变更检测**（commands.rs）：mtime+size 预筛 → 内容 hash 确认。
   预筛让无变化的文件零哈希开销；hash 确认防止 touch 误判。
3. **解析**（parse.rs，rayon 并行，仅变更文件）：
   - query 通道：定义（函数/方法/类/接口/箭头变量）与调用（裸名/成员/new/JSX）；
   - 手写遍历通道：import/export 子句（形态多变，query 不划算）、局部名（遮蔽检测）、
     extends/implements、语法错误计数（>0 则该文件记 partial，不中断全局）。
   - 产出 `RawFile`：符号（带限定名/容器/导出标记）、引用（带 owner/遮蔽标记）、导入。
4. **全量视图**（view.rs）：变更文件来自上一步，未变更文件从 DB 拉平
   （引用的 name/receiver/遮蔽、导入的 spec 都是持久化的解析原材料）。
5. **解析**（resolve.rs，纯函数）：
   import 解析（相对/别名/裸名 + barrel 转发链）→ extends/类层级 → overrides 推导 →
   调用点三级解析（不确定一律 dynamic）。输出临时 id 空间的 nodes/imports/refs/edges。
6. **落库**（store::apply_build，单事务）：文件 upsert → 变更文件节点重写 +
   未变更文件按 id 序对齐（计数不符自动降级重写，自愈）→ refs/edges 全量重写。
   WAL + 单事务 = 并发读要么见旧图要么见新图。

## 关键设计决策

### D1 为什么「解析结果全量重算」而不是「反向依赖传播一层」

- 传播一层的方案要回答：barrel 链传递依赖算几层？新增导出要不要反向通知？
  每个问题都是一个 stale-edge bug 温床。
- 本设计：重解析（tree-sitter，贵）只做变更文件；符号解析（哈希查找，便宜）永远全量。
  「增量 ≡ 全量」成为可被 fuzz 测试执行的数学等式，而不是需要推理保证的命题。

### D2 为什么 refs 和 edges 分两张表

- refs = 证据链的物理载体（每个调用点一行，可跳源码）；
- edges = 按 (src,dst,kind) 聚合的物化视图（weight = 调用次数）。
- 查询走 edges（快），证据走 refs（可解释），增量原材料走 refs（解析输入持久化）。
  一份数据服务三个需求，是整个存储设计的支点。

### D3 为什么节点身份不含 span

- span 进身份 → 任何编辑都让节点 id 全体洗牌 → 增量更新自我毁灭。
- 身份 = (file_id, kind, qualified_name)，span 是属性。重载去重合并导出标志。

### D4 为什么手写 MCP 协议而不用 rmcp

- 只需要 3 个方法，协议面 ~200 行；rmcp 拖进 tokio+tower 全家桶，与「单二进制零依赖」矛盾；
- 正确性不靠信任自己，靠官方 TypeScript SDK 契约测试（tests-ts/）从外部验证；
- 日志纪律：协议走 stdout、日志走 stderr，从架构上杜绝帧污染。

### D5 为什么动态调用「不猜」

- 影响面分析的一个假调用方，代价是用户按错误信息改代码——比查不到更糟。
- 宁缺毋滥不是保守，是产品承诺（「确定性」）的实现方式。
  dynamic 调用点全部单独呈现为「间接影响」，信息不丢失，只是不混入确定性结论。

## 存储布局（SQLite v1）

```
files(id, path UNIQUE, hash, size, mtime_ns, loc, status, error)
nodes(id, file_id→files, kind, name, qualified_name, is_exported, span, line, extra)
      UNIQUE(file_id, kind, qualified_name)          ← 身份定义
imports(id, file_id→files, spec, kind, local, remote, target_path, package)
refs(id, file_id→files, owner_id→nodes, kind, name, receiver, shadowed,
     status, target_id→nodes, span, line)           ← 证据链 + 重解析原材料
edges(src→nodes, dst→nodes, kind, weight, PK(src,dst,kind))  ← 查询物化视图
meta(key, value)
```

索引：nodes(name)、nodes(qualified_name)、edges(kind,dst)、edges(kind,src)、
refs(target_id)（反向查询）、refs(name) WHERE status IN dynamic/ambiguous（间接影响）。

## 查询语义速查

| 查询 | 算法 | 复杂度 | 上限 |
|---|---|---|---|
| who_calls | edges 反向 + refs 证据 join | O(deg) | — |
| impact | 反向闭包 Dijkstra-max + 剪枝 | O(E log V) | depth 8 / nodes 500 |
| trace | 双向 BFS（calls 边） | O(b^(d/2)) | 每侧 5 跳 |
| hotspots | 加权 PageRank（跨文件 ×1.5） | O(iter × E) | L1<1e-6 或 100 轮 |
| search | SQL 精确 + 前缀 | O(log N) | 50 条 |
| archaeology | git blame -C -C --line-porcelain | git 决定 | 行区间 |

风险分（impact）：`risk(n) = max_{p→n} risk(p) × 0.8^1 × type_w(edge)`，
type_w：调用 1.0 / 重写 0.7 / 继承·实现 0.6；起点 risk=1。
是对「Σ所有路径」（#P 难）的单源最优路径近似——README 已诚实标注。
