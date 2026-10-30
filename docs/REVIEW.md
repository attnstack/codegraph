# codegraph 技术方案审查（阶段 A 完整版）

> 审查对象：《技术方案.md》v1.0（2026-10-29）
> 审查立场：方案整体方向正确（AST 图谱 + 确定性检索 + MCP 出口是真需求、差异化成立），
> 但按原稿直接开工，会在三个地方翻车：**符号解析的真实难度、增量一致性的传播边界、
> 风险评分的数学定义**。以下按 P0/P1/P2 给出问题 → 为什么致命 → 怎么改。
> 修订后的方案已经直接实现为代码（见 README），本文档记录审查结论与取舍。

---

## A1 一句话判断

**能落地，但原稿低估了「引用解析」这一层的复杂度——它才是项目 70% 的工作量和 100% 的正确性风险所在；
最大风险是图谱边全是错的还自以为对（假阳性边直接摧毁「确定性」这个核心卖点）。**

---

## A2 致命问题（P0）与设计缺陷（P1）

### P0-1 「引用解析三级策略」回避了最难的 80%

- **问题**：4.2 节的三级策略（同文件 → import 图 → 全局兜底）把「沿 import 解析到目标模块导出符号」当成已有能力一句话带过。但 TS 模块解析本身就有一整层：相对/别名/裸名三类 specifier、`.js` 后缀实指 `.ts` 的 TS 惯例、`index.*` 目录解析、tsconfig `paths`+`baseUrl`（且 tsconfig 是 JSONC，允许注释）、`extends` 继承配置、barrel 文件的 `export *` 链式转发、default/named/namespace 三种绑定形态、CommonJS `require()`。
- **为什么致命**：import 解析错了，第二级解析整层失效，调用图大面积断边或错边。「所有检索结果确定、可解释」的产品承诺建立在最薄弱的一层上。
- **怎么改**：把 import 解析做成显式规则表（每条规则可测试）：相对路径候选序列（精确 → 扩展名枚举 → `.js`→`.ts` 交换 → `index.*`）、tsconfig paths（特异性排序，最长前缀优先）+ JSONC 宽容解析 + extends 链（限深 5）、裸名一律判外部包不展开、barrel 转发用带环保护的 `resolve_export` 递归。已实现于 `crates/codegraph-core/src/resolve.rs`。

### P0-2 裸调用的「全局兜底」会制造假边，摧毁确定性承诺

- **问题**：4.2 节第三级「全局：跨文件索引兜底」没有说明歧义怎么处理。JS 里 `foo()` 可能是参数、局部变量、导入、内建全局（`console`/`describe`）或真仓库函数。无脑兜底 = 大量假边。
- **为什么致命**：影响面分析返回一个假的调用方，用户改代码时被误导——这比查不到更糟，直接推翻「确定性」卖点，面试被问「你们的边有假阳性吗」时无法回答。
- **怎么改**：显式解析序 + 不确定一律不猜：
  1. 名字被本文件参数/局部变量**遮蔽** → `dynamic`（解析层持久化 shadowed 标记，增量重解析时不依赖未落库的局部名表）；
  2. 命中 import 绑定 → 沿绑定解析（含 barrel 链）；
  3. 命中同文件定义 → 本地边；
  4. 命中内建全局名单（console/describe/expect…约 50 个）→ `builtin`，绝不进图；
  5. 全局同名导出**恰好 1 个**才兜底，>1 个判 `ambiguous`；
  6. 其余 `dynamic`。所有非 resolved 状态单独呈现为「间接影响」。

### P0-3 增量失效范围「反向依赖一层」在语义上是错的

- **问题**：6.5 节的失效规则是「重解析变更文件 + 沿 called_by/imported_by 反向依赖一层」。两个 bug：① `export *` barrel 文件让依赖链**传递**（A 改导出 → B 转发 A → C 从 B 拿符号，C 在 B 的 called_by 一层之外）；② 一层重解析只修「解析结果」，但解析的**输入**（B 的 import 绑定指向哪个文件）没变——如果 A 的改动只是新增导出，B 根本不需要重解析，白做；如果 A 删了导出，B 的调用点该变成 unresolved，这靠重解析 B 才能发现，而「一层」不保证覆盖所有引用方。
- **为什么致命**：增量更新是三大卖点之一，错了比没有更糟——图会静默腐烂。
- **怎么改**：**换正确性模型，而不是补丁**：把「重解析（tree-sitter，贵）」与「重解析（符号解析，便宜）」拆开。变更文件重解析；未变更文件的解析原材料（引用的 name/receiver/遮蔽标记、import 的 spec）本来就在 DB 里，拉平成全量视图后**每次更新都对全量重跑一遍符号解析**——它是 O(refs) 的哈希查找，10 万行仓库毫秒级。于是「增量结果 == 全量重建」变成确定性等式，用 fuzz 测试（随机变异 10 轮 × 双库全查询对比）执行证明。见 `tests/fuzz_incremental.rs`。

### P0-4 风险评分公式在数学上不可实现（Σ over paths 是 #P 难）

- **问题**：6.1 节定义 `risk(n) = Σ_paths w^dist × type_w`。「对所有路径求和」在有环调用图上路径数指数爆炸（#P-hard），原稿没有定义路径枚举如何终止。
- **为什么致命**：面试官顺着公式问一句「环怎么办」就穿帮；实现了也跑不起来。
- **怎么改**：改语义为「**单源最优影响路径**」：`risk(n) = max over in-edges (risk(parent) × decay × type_w)`，用最大堆松弛（Dijkstra 同型，O(E log V)）。语义上等价于「最坏情况影响强度」，对用户更有用（给出的是最强传播链）。松弛时记录父指针，证据链免费重建。README 里明确标注这是对 Σ 的近似及理由。

### P1-1 节点唯一键把 span 放进身份，增量更新会自我毁灭

- **问题**：3.1 节节点唯一键 = `(repo, path, kind, name, span)`。任何文件顶部加一行，该文件所有节点 span 变化 → 全部「新节点」。
- **为什么致命**：增量更新后节点的 id 大规模洗牌，历史边全废，增量失去意义。
- **怎么改**：身份 = `(file_id, kind, qualified_name)`（限定名 = `Class.method` / `Outer.inner`），span 只是属性。同名重载（TS overload）按先到者保留并合并导出标志。schema 见 `codegraph-store/src/schema.sql`。

### P1-2 证据链与边建模冲突：evidence 是单值但调用点是多值

- **问题**：5 节 `edges.evidence TEXT` 存「调用点 span」，但 `PRIMARY KEY(src,dst,kind)` 意味着 A 调 B 十次的十个调用点只能挤进一个 TEXT 字段。
- **为什么致命**：「每条结果附证据链」是 README 标题句，数据模型装不下。
- **怎么改**：每个**调用点**一行 `refs`（name/receiver/状态/目标/span），`edges` 是按 (src,dst,kind) 聚合的物化视图（weight=次数）。证据 = 沿路径逐跳 join refs。顺带解决增量：refs 就是重解析的原材料。

### P1-3 混合层级的 BFS 闭包语义不清

- **问题**：6.1 节边集 `{called_by, overridden_by, extends, implements, imported_by}` 把符号级边（调用/重写）和文件级边（import）混在一个闭包里。函数 f 的 import 闭包会把「所有 import 该文件但不调用 f 的函数」都算进来。
- **怎么改**：拆成两个输出：符号级闭包（calls/overrides/extends/implements 反向）+ 文件级影响（import 传播清单），各自呈现。语义清晰且各自可解释。

### P1-4 MCP stdio 未指定实现方式与日志纪律

- **问题**：只写了「MCP Server (stdio)」。两个坑：① stdio 通道上**任何** stdout 日志都会破坏 JSON-RPC 帧（最高频的 MCP 实现事故）；② 协议版本协商、工具错误该用 `isError` 结果还是协议错误，原稿无约定。
- **怎么改**：日志一律 stderr（tracing）；协议版本按客户端版本协商（未知则回落最新已知）；工具级错误返回 `isError:true` 结果，协议级错误才用 JSON-RPC error；实现为手写的最小 JSON-RPC 子集（initialize/tools.list/tools.call，约 200 行），正确性由官方 TS SDK 契约测试验证——比拖整个 tokio+tower 依赖栈更符合「单二进制」卖点。见 `mcp.rs` + `tests-ts/`。

### P1-5 依赖选型空缺

- **问题**：技术栈只写了 "Rust + tree-sitter + SQLite"，没有具体 crate。tree-sitter 运行时与 grammar 的版本兼容是个真实的坑（0.2x 系列横跨多代 API）。
- **怎么改**：锁定 `tree-sitter 0.27 + tree-sitter-typescript 0.23`（共用 tree-sitter-language ^0.1，已核实兼容）；`rusqlite 0.41 (bundled)`（免系统依赖）；`ignore` crate（ripgrep 的遍历器，gitignore 感知开箱即用）；MCP 不引入 SDK（理由见 P1-4）。

### P2（记录在案，MVP 不做）

- `notify` watcher：触发机制与失效机制是两回事，MVP 用 `update` 命令 + 查询前 mtime/size 预筛自愈，watcher 留作后续（Windows 上还有 case-insensitivity 与重命名的坑）。
- API Route / DB Table 节点：静态检测 SQL 访问要解析字符串 SQL，不可靠；Next.js 路由靠约定文件名，属于 adapter 扩展点而非 MVP。
- `cyclomatic` 复杂度：tree-sitter 不给，需要自己数分支节点——便宜但放 W6 之后。
- napi-rs 发 npm 包：分发优化，非核心。

---

## A3 被遗漏的部分（逐项检查）

| 项 | 原稿是否覆盖 | 结论与处理 |
|---|---|---|
| 错误处理与降级 | 部分（dynamic 不猜） | **补**：单文件语法错误 → tree-sitter 容错解析 + 文件标记 `partial`，不中断索引；tsconfig 损坏 → 降级无别名模式并告警；配置文件损坏 → 降级默认值；git 不可用 → 考古命令报可读错误 |
| 超时与重试 | 未覆盖 | 本地工具无需重试；查询侧用 depth/节点数双上限熔断（原稿有深度无节点上限，已补 max_nodes=500） |
| 并发与锁 | 未覆盖 | **补**：WAL + busy_timeout=5s；构建在单事务内完成，MCP 读与 CLI 写并发安全（快照隔离） |
| 边界条件 | 未覆盖 | **补**：空仓库/无 TS 文件 → 空图可用；环状 import/继承 → 全部遍历带 seen 集；语法错误文件 → partial；外部包 → 伪节点不展开；深嵌套 AST → 显式栈防递归爆栈；Windows 反斜杠 → walk 层统一 `/` |
| 数据一致性 | 有（fuzz） | 原稿的 fuzz 方向完全正确，保留并强化为「增量==全量」等式测试（P0-3） |
| 测试策略 | 有 | 保留五层，落地为：单测（解析/解析/算法）+ golden 快照 + fuzz 一致性 + CLI 端到端 + 官方 SDK 契约测试 |
| 可观测性 | 未覆盖 | **补**：tracing 结构化日志走 stderr；`-v` 分级；构建输出统计（files/changed/nodes/refs/edges/耗时） |
| 配置管理 | 未覆盖 | **补**：默认值 ← `.codegraph.toml` ← CLI 三层合并，全参数外置（权重/深度/剪枝规则），附 `.codegraph.toml.example` |
| 安全与权限 | 未覆盖 | 参数化 SQL（rusqlite prepared）；文件遍历不出 root；无网络依赖、无遥测——代码不出本机本身就是隐私卖点 |
| 数据合规 | 未覆盖 | 本地索引、本地查询，无云端依赖（README 明示） |
| 成本控制 | 不适用 | 纯本地计算；CI 只跑标准测试 |
| 性能瓶颈预估 | 未覆盖 | **补**：预估瓶颈 = 冷启动解析与引用解析；SQLite 规模（10 万行 ≈ 万级节点/十万级引用）远小于 SQLite 舒适区；真实数字由 bench 实测（见 README，禁止编造） |
| 迁移与兼容 | 未覆盖 | **补**：PRAGMA user_version 单步迁移；节点计数校验失败自动降级重写（自愈路径） |
| 文档与上手 | 有 README 规划 | Quick Start 30 秒可复制 + 真实命令输出演示 + 架构图 |
| 第三方依赖稳定性 | 有（grammar 锁版本） | 保留 Cargo.lock 锁定 + CI 回归；新增：依赖清单全部注明「为什么需要它」 |

## A4 算法与选型质疑

1. **影响面 BFS 为什么不用 SQLite 递归 CTE？** 递归 CTE 能做闭包，但风险评分需要「每路径类型不同权重 + 按分数松弛 + 父指针重建」，SQL 里写出来是黑魔法；数据规模（边 < 10^5）下全量载入内存做 Dijkstra 变体更简单更快，且算法可单测。**更简单的方案赢了。**
2. **PageRank 是否过度设计？** 实现约 80 行、万级节点迭代收敛毫秒级、面试叙事价值高（悬空节点处理是标准考点）。保留，但明确它的局限：度量的是「被重要代码调用」，不等于「改了最危险」——后者才是 impact 的职责，两者互补不重叠。
3. **双向 BFS trace 为什么是 5+5？** 调用图出度小入度大，双向各 d 层覆盖 O(b^{d/2})；5+5 覆盖 10 跳内的链，实测夹具足够。限深可配。
4. **不用 tsc/type checker 做类型级消歧，亏不亏？** 亏——但那是另一个项目量级（tsc API 绑定 + 类型表）。MVP 的「不猜」策略 + dynamic 标记是这个约束下的最优解，README 诚实列为局限。
5. **符号寻址 `path::name`**：原稿 CLI 示例用了这个形式但没定义歧义规则。补：同名多候选时报错并列出全部候选让用户消歧，绝不悄悄选第一个。

## A5 MVP 收敛

**必须保留（端到端可演示）**：文件发现（gitignore 感知）→ tree-sitter 解析 → 三级符号解析（含 dynamic 标记）→ SQLite 存储（refs+edges）→ who_calls / impact（剪枝+风险分+证据链）/ trace / search / hotspots → CLI + MCP 双出口 → 增量（hash 差异 + 全量重解析）+ fuzz 等式测试 → 考古（git blame，演示价值/成本比最高）。

**砍掉**：notify watcher（触发器非失效机制）、DB Table/API Route 节点、`returns` 边、napi-rs npm 分发、LSP。

**工期对照**：原稿排 3 周。按修订后方案，核心链路（第 1-2 周）可直接复用本仓库已实现的代码；剩余时间应投给「真实仓库回归 + README 数字实测 + 面试预演」，而不是加功能。
