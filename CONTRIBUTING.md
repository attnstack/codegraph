# Contributing to codegraph

## 开发环境

```bash
git clone <repo> && cd codegraph
cargo build              # debug 构建
cargo test               # 全部测试（含 fuzz 一致性测试，约 1-2 分钟）
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

TS 契约测试（需要 Node 20+）：

```bash
cargo build
cd tests-ts && npm install && node --test
```

## 如何添加一门新语言

架构上的插件点只有三处，以 TypeScript 适配器为例（`crates/codegraph-core/src/parse.rs`）：

1. **grammar 依赖**：`crates/codegraph-core/Cargo.toml` 加 `tree-sitter-<lang>`，
   注意运行时与 grammar 的 `tree-sitter-language` 版本必须兼容。
2. **queries 文件**：新建 `languages/<lang>/queries.scm`，按
   `; --- group: defs/calls/jsx ---` 分组书写抽取规则。每组**独立编译**，
   单组失败只降级该组并打日志，不会拖垮整体（见 `compile_set`）。
3. **注册**：`language_for_path` 里加扩展名 → grammar 的映射；
   若该语言的 import 语义不同（如 Python 的 `import x.y`），
   在 `parse.rs` 的手写遍历通道补对应分支。

调试工具：`codegraph debug-ast <file>` 打印真实 grammar 节点名，照着写 queries。

## 提交规范

- 一个 PR 一个主题；核心算法改动必须连带测试：
  - 动了 `resolve.rs` / 增量逻辑 → `fuzz_incremental` 必须跑（CI 已跑）；
  - 动了查询语义 → 更新 golden 快照（`UPDATE_SNAPSHOTS=1 cargo test -p codegraph-server --test golden`）并在 PR 里贴 diff 说明。
- 提交信息用祈使句，第一行 ≤ 72 字符。

## 代码风格

- 错误处理：可恢复用 `Result` + 类型化错误；禁止裸 `unwrap`（测试代码除外）。
- 每个文件顶部一行注释说明职责；关键算法注释解释 **why** 而非 what。
- 性能数字只能来自 `scripts/bench.sh` 实测，禁止凭感觉写进文档。

## 许可

提交即表示同意以 MIT 许可（见 LICENSE）授权你的贡献。
