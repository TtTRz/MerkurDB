use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;

use crate::app_state::AppState;

pub async fn status(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    match state.storage.stats().await {
        Ok(stats) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "total_memories": stats.total_memories,
                "total_edges": stats.total_edges,
                "pending_consolidation": stats.pending_consolidation,
                "by_level": stats.by_level,
                "uptime_seconds": (chrono::Utc::now() - state.started_at).num_seconds(),
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": { "code": "STATUS_ERROR", "message": e.to_string() }
            })),
        ),
    }
}
