use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::json;

use crate::app_state::AppState;
use crate::error::ApiResult;

pub async fn status(State(state): State<AppState>) -> ApiResult<impl IntoResponse> {
    let stats = state.storage.stats().await?;
    Ok((
        StatusCode::OK,
        Json(json!({
            "total_memories": stats.total_memories,
            "total_edges": stats.total_edges,
            "pending_consolidation": stats.pending_consolidation,
            "by_level": stats.by_level,
            "uptime_seconds": (chrono::Utc::now() - state.started_at).num_seconds(),
        })),
    ))
}
