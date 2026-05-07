use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;

use crate::app_state::AppState;
use crate::handlers::write::error_response;

pub async fn health() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION")
        })),
    )
}

#[derive(Debug, Deserialize)]
pub struct LogQuery {
    pub limit: Option<usize>,
}

pub async fn consolidation_log(
    State(state): State<AppState>,
    Query(params): Query<LogQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let limit = params.limit.unwrap_or(20);

    match state.storage.get_consolidation_log(limit).await {
        Ok(entries) => (
            StatusCode::OK,
            Json(serde_json::json!({ "entries": entries })),
        ),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "LOG_FAILED", e),
    }
}
