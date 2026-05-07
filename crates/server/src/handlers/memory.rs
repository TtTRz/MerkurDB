use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::json;

use crate::app_state::AppState;
use crate::error::{ApiError, ApiResult};

pub async fn get_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let memory = state
        .storage
        .get_memory(&id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("Memory {id} not found")))?;
    Ok((
        StatusCode::OK,
        Json(json!({
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
    ))
}

#[derive(Debug, Deserialize)]
pub struct UpdateRequest {
    pub content: String,
}

pub async fn update_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateRequest>,
) -> ApiResult<impl IntoResponse> {
    if req.content.is_empty() {
        return Err(ApiError::bad_request("content must not be empty"));
    }
    // Existence check up front avoids burning an embedding (and an OpenAI fee)
    // for a non-existent id.
    if !state.storage.memory_exists(&id).await? {
        return Err(ApiError::not_found(format!("Memory {id} not found")));
    }
    let embedding = state.embedder.encode(&req.content).await?;
    state
        .storage
        .update_memory(&id, &req.content, Some(&embedding))
        .await?;
    Ok((
        StatusCode::OK,
        Json(json!({ "status": "updated", "id": id })),
    ))
}

pub async fn delete_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    state.storage.delete_memory(&id).await?;
    Ok((
        StatusCode::OK,
        Json(json!({ "status": "deleted", "id": id })),
    ))
}
