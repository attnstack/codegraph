//! 核心层错误类型：可恢复（单文件解析失败→partial）与不可恢复（IO/配置）严格区分。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("symbol not found: {0}")]
    SymbolNotFound(String),

    #[error("ambiguous symbol `{0}`, candidates: {1}")]
    AmbiguousSymbol(String, String),

    #[error("{0}")]
    Other(String),
}

pub type CoreResult<T> = std::result::Result<T, CoreError>;
