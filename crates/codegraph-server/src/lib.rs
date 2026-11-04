//! codegraph-server 库目标：CLI 与 MCP 共用的编排与出口逻辑。
//! 拆出 lib 是为了让集成测试（tests/）能直接调用构建编排，
//! 而不必通过 spawn 子进程（golden / fuzz 测试需要程序化访问）。

pub mod commands;
pub mod git;
pub mod mcp;
pub mod output;
pub mod watch;
