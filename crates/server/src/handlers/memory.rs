use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;

use crate::app_state::AppState;
use crate::handlers::write::error_response;

pub async fn get_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.storage.get_memory(&id).await {
        Ok(Some(memory)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": memory.id,
                "content": memory.content,
                "abstract": memory.abstract_,
                "category": memory.category,
                "weight": memory.weight,
                "level": memory.level,
                "pending_consolidation": memory.pending_consolidation,
                "metadata": memory.metadata,
                "context": memory.context,
                "created_at": memory.created_at,
                "updated_at": memory.updated_at,
                "accessed_at": memory.accessed_at,
                "access_count": memory.access_count
            })),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": {
                    "code": "MEMORY_NOT_FOUND",
                    "message": format!("Memory {id} not found")
                }
            })),
        ),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "STORAGE_ERROR", e),
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateRequest {
    pub content: String,
}

pub async fn update_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let embedding = match state.embedder.encode(&req.content).await {
        Ok(vec) => Some(vec),
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "EMBED_FAILED", e),
    };
    match state
        .storage
        .update_memory(&id, &req.content, embedding.as_deref())
        .await
    {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "updated", "id": id })),
        ),
        Err(e) => error_response(StatusCode::NOT_FOUND, "UPDATE_FAILED", e),
    }
}

pub async fn delete_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.storage.delete_memory(&id).await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "deleted", "id": id })),
        ),
        Err(e) => error_response(StatusCode::NOT_FOUND, "DELETE_FAILED", e),
    }
}
