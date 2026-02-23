use thiserror::Error;

#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum KingError {
    #[error("Database error: {0}")]
    Database(#[from] libsql::Error),

    #[error("Config error: {0}")]
    Config(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Agent error: {0}")]
    Agent(String),

    #[error("Internal error: {0}")]
    Internal(String),
}
