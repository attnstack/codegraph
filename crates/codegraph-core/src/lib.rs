//! codegraph-core：纯逻辑层。
//! 职责：图模型定义、文件发现、tree-sitter 解析抽取、import/符号三级解析、
//! 影响面/链路/PageRank 算法。不依赖 SQLite（持久化见 codegraph-store），
//! 因此所有算法都可以用内存视图直接单测。

pub mod config;
pub mod error;
pub mod model;
pub mod parse;
pub mod query;
pub mod rank;
pub mod resolve;
pub mod view;
pub mod walk;

pub use config::Config;
pub use error::{CoreError, CoreResult};
pub use model::NodeKind;
