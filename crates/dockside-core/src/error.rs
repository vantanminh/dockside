use thiserror::Error;

pub type CoreResult<T> = Result<T, CoreError>;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("invalid path '{path}': {reason}")]
    InvalidPath { path: String, reason: String },

    #[error("secret file blocked by policy: {path}")]
    SecretBlocked { path: String },

    #[error("object not found: {0}")]
    NotFound(String),

    #[error("blob integrity mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },

    #[error("invalid hash format: {0}")]
    InvalidHash(String),

    #[error("migration rule violation: {0}")]
    MigrationViolation(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("remote storage error: {0}")]
    RemoteStorage(String),

    #[error("serialization error: {0}")]
    Serialize(String),

    #[error("compression error: {0}")]
    Compression(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
