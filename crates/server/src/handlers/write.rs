use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use merkur_core::{MerkurError, NewMemory, WriteItem};
use serde::Deserialize;
use tracing::error;

use crate::app_state::AppState;

#[derive(Debug, Deserialize)]
pub struct WriteRequest {
    pub content: String,
    pub context: Option<HashMap<String, String>>,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
pub struct WriteBatchRequest {
    pub items: Vec<WriteItem>,
}

pub async fn write(
    State(state): State<AppState>,
    Json(req): Json<WriteRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let start = std::time::Instant::now();

    let embedding = match state.embedder.encode(&req.content).await {
        Ok(vec) => Some(vec),
        Err(e) => {
            error!("Embedding failed: {e:?}");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "EMBED_FAILED", e);
        }
    };

    let new_mem = NewMemory {
        content: req.content,
        category: None,
        context: req.context.unwrap_or_default(),
        metadata: req.metadata.unwrap_or_default(),
        embedding,
    };

    match state.storage.insert_memory(&new_mem).await {
        Ok(id) => {
            let time_ms = start.elapsed().as_millis() as u64;
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "id": id,
                    "status": "ok",
                    "searchable": true,
                    "time_ms": time_ms
                })),
            )
        }
        Err(e) => {
            error!("Write failed: {e:?}");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "WRITE_FAILED", e)
        }
    }
}

pub async fn write_batch(
    State(state): State<AppState>,
    Json(req): Json<WriteBatchRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let start = std::time::Instant::now();
    let mut ids = Vec::new();

    let mut errors = Vec::new();
    for (i, item) in req.items.iter().enumerate() {
        let embedding = match state.embedder.encode(&item.content).await {
            Ok(vec) => Some(vec),
            Err(e) => {
                errors.push(serde_json::json!({"index": i, "code": "EMBED_FAILED", "message": e.to_string()}));
                continue;
            }
        };

        let new_mem = NewMemory {
            content: item.content.clone(),
            category: None,
            context: item.context.clone().unwrap_or_default(),
            metadata: item.metadata.clone().unwrap_or_default(),
            embedding,
        };

        match state.storage.insert_memory(&new_mem).await {
            Ok(id) => ids.push(id),
            Err(e) => {
                errors.push(serde_json::json!({"index": i, "code": "WRITE_FAILED", "message": e.to_string()}));
            }
        }
    }

    let time_ms = start.elapsed().as_millis() as u64;
    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "ids": ids,
            "count": ids.len(),
            "requested": req.items.len(),
            "errors": errors,
            "time_ms": time_ms
        })),
    )
}

pub fn error_response(
    status: StatusCode,
    code: &str,
    err: MerkurError,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({
            "error": {
                "code": code,
                "message": err.to_string()
            }
        })),
    )
}
