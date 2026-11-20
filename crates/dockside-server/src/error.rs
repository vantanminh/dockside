use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use dockside_core::CoreError;
use dockside_core::model::ApiErrorBody;
use thiserror::Error;

pub type ServerResult<T> = Result<T, ServerError>;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("unauthorized")]
    Unauthorized,

    #[error("core error: {0}")]
    Core(#[from] CoreError),

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("internal error: {0}")]
    Internal(String),
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        let (status, code, msg) = match &self {
            ServerError::NotFound(m) => (StatusCode::NOT_FOUND, "not_found", m.clone()),
            ServerError::BadRequest(m) => (StatusCode::BAD_REQUEST, "bad_request", m.clone()),
            ServerError::Conflict(m) => (StatusCode::CONFLICT, "conflict", m.clone()),
            ServerError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "invalid or missing bearer token".to_string(),
            ),
            ServerError::Core(CoreError::SecretBlocked { path }) => (
                StatusCode::BAD_REQUEST,
                "secret_blocked",
                format!("secret file blocked by policy: {path}"),
            ),
            ServerError::Core(CoreError::InvalidPath { path, reason }) => (
                StatusCode::BAD_REQUEST,
                "invalid_path",
                format!("invalid path '{path}': {reason}"),
            ),
            ServerError::Core(CoreError::NotFound(m)) => {
                (StatusCode::NOT_FOUND, "not_found", m.clone())
            }
            ServerError::Core(CoreError::InvalidHash(h)) => (
                StatusCode::BAD_REQUEST,
                "invalid_hash",
                format!("invalid hash: {h}"),
            ),
            ServerError::Core(CoreError::HashMismatch { expected, actual }) => (
                StatusCode::BAD_REQUEST,
                "hash_mismatch",
                format!("blob hash mismatch: expected {expected}, got {actual}"),
            ),
            other => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                other.to_string(),
            ),
        };
        (
            status,
            Json(ApiErrorBody {
                error: msg,
                code: code.to_string(),
            }),
        )
            .into_response()
    }
}
