use thiserror::Error;

#[derive(Error, Debug)]
pub enum MerkurError {
    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Embedding error: {0}")]
    Embedding(String),

    #[error("Memory not found: {0}")]
    MemoryNotFound(String),

    #[error("Invalid configuration: {0}")]
    Config(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type MerkurResult<T> = Result<T, MerkurError>;
