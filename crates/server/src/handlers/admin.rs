use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::json;

use crate::app_state::AppState;
use crate::error::ApiResult;

pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let db_ok = state.storage.stats().await.is_ok();
    let status = if db_ok { "ok" } else { "degraded" };
    (
        StatusCode::OK,
        Json(json!({
            "status": status,
            "version": env!("CARGO_PKG_VERSION"),
            "checks": {
                "database": if db_ok { "ok" } else { "error" },
                "embedder_dim": state.embedder.dim(),
            }
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
) -> ApiResult<impl IntoResponse> {
    let limit = params.limit.unwrap_or(20).clamp(1, 1000);
    let entries = state.storage.get_consolidation_log(limit).await?;
    Ok((StatusCode::OK, Json(json!({ "entries": entries }))))
}
