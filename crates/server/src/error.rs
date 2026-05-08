use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use merkur_core::MerkurError;
use serde_json::json;
use tracing::error;

/// Map a `MerkurError` to a structured HTTP error response.
///
/// Internal details (SQL strings, file paths, raw error chains) are deliberately
/// kept out of the response body. They are logged at `error!` level for
/// operators, while clients see a stable `code` and a generic message.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "BAD_REQUEST",
            message: msg.into(),
        }
    }

    pub fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "UNAUTHORIZED",
            message: "Authentication required".into(),
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "NOT_FOUND",
            message: msg.into(),
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "INTERNAL_ERROR",
            message: msg.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            error!(code = self.code, message = %self.message, "server error");
        }
        (
            self.status,
            Json(json!({
                "error": { "code": self.code, "message": self.message }
            })),
        )
            .into_response()
    }
}

impl From<MerkurError> for ApiError {
    fn from(e: MerkurError) -> Self {
        match e {
            MerkurError::MemoryNotFound(id) => ApiError {
                status: StatusCode::NOT_FOUND,
                code: "MEMORY_NOT_FOUND",
                message: format!("Memory {id} not found"),
            },
            MerkurError::BadRequest(msg) => ApiError {
                status: StatusCode::BAD_REQUEST,
                code: "BAD_REQUEST",
                message: msg,
            },
            MerkurError::Config(msg) => {
                error!("config error: {msg}");
                ApiError::internal("Server configuration error")
            }
            MerkurError::Storage(msg) => {
                error!("storage error: {msg}");
                ApiError::internal("Storage backend error")
            }
            MerkurError::Embedding(msg) => {
                error!("embedding error: {msg}");
                ApiError {
                    status: StatusCode::BAD_GATEWAY,
                    code: "EMBED_FAILED",
                    message: "Embedding backend error".into(),
                }
            }
            MerkurError::Consolidation(msg) => {
                error!("consolidation error: {msg}");
                ApiError::internal("Consolidator error")
            }
            MerkurError::Internal(msg) => {
                error!("internal error: {msg}");
                ApiError::internal("Internal server error")
            }
        }
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
