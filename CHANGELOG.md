# 更新日志

格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循语义化版本。

## [0.1.2] — 2026-11-05

### 新增

- **Python 语言适配器**：绝对/相对导入解析、子模块命名空间绑定、self/cls 方法链、
  类继承与重写、python 内建名单；存储层 v2 迁移（files.lang 列）
- 三个新查询（CLI 与 MCP 同步提供）：
  - `callees`：某符号的调用下游，附调用点证据
  - `dead-code`：孤儿函数（TS 按导出性判定 / Python 按单下划线私有约定，dunder 排除）
  - `cycles`：import 循环依赖（文件级强连通分量）
- `codegraph watch`：开发态文件监听，保存即增量重建（600ms 防抖）
- `--types`：可选第四级解析——嵌入 tsc sidecar 用接收者类型消歧
  （需仓库有 node_modules/typescript 与 node，缺失时自动退化）
- npm 分发：`npm i -g codegraph-mcp-server`，安装时按平台下载预编译二进制
- Release 工作流：打 tag 自动构建 linux / windows / macOS(aarch64) 三平台二进制
- 英文 README

### 修复

- python class_definition 的节点类型映射缺失（类被标成 function，
  导致类层级/override 推导/self 方法链全断）——由 python golden 测试捕获

### 变更

- MCP 工具 6 → 9（新增 callees / dead_code / cycles）
- MCP 服务端快照进程内缓存：工具调用不再每次重装载数万级节点/边

## [0.1.1] — 2026-11-04

### 变更

- README 重写（终端演示图、双语言切换）；内部工作笔记移出版本库

### 弃用

- npm 上的 0.1.1 因 tarball 打包了错误二进制已标记 deprecated——**安装请用 ≥0.1.2**。
  事故根因与防复发守卫（prepublishOnly 残留二进制检查）记录在案

## [0.1.0] — 2026-10-30

首个可用版本。

- tree-sitter 解析（TS/JS）+ 三级符号解析：同文件 → import 链（含 barrel 转发、
  tsconfig paths、.js→.ts 惯例）→ 全局唯一兜底；不确定引用显式标记，不猜
- 影响面分析：反向调用闭包 + 路径分段剪枝 + Dijkstra 变体风险评分 + 逐跳源码证据链
- 增量更新：重解析只做变更文件、符号解析全量重跑，增量 ≡ 全量由随机变异 fuzz 证明
- CLI 与 MCP stdio 双出口（手写 JSON-RPC 子集，官方 TypeScript SDK 契约测试验证）
- 加权 PageRank hotspots、git blame 代码考古、DOT 导出、golden / fuzz / CLI / 契约四层测试

[0.1.2]: https://github.com/attnstack/codegraph/releases/tag/v0.1.2
[0.1.1]: https://github.com/attnstack/codegraph/releases/tag/v0.1.1
[0.1.0]: https://github.com/attnstack/codegraph/releases/tag/v0.1.0
