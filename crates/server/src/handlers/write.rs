use std::collections::HashMap;
use std::time::Instant;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use merkur_core::{NewMemory, WriteItem, limits};
use serde::Deserialize;
use serde_json::json;

use crate::app_state::AppState;
use crate::error::{ApiError, ApiResult};

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

fn check_content(content: &str) -> ApiResult<()> {
    if content.is_empty() {
        return Err(ApiError::bad_request("content must not be empty"));
    }
    if content.len() > limits::MAX_CONTENT_BYTES {
        return Err(ApiError::bad_request(format!(
            "content exceeds {} bytes",
            limits::MAX_CONTENT_BYTES
        )));
    }
    Ok(())
}

pub async fn write(
    State(state): State<AppState>,
    Json(req): Json<WriteRequest>,
) -> ApiResult<impl IntoResponse> {
    let start = Instant::now();
    check_content(&req.content)?;

    let embedding = state.embedder.encode(&req.content).await?;

    let new_mem = NewMemory {
        content: req.content,
        category: None,
        context: req.context.unwrap_or_default(),
        metadata: req.metadata.unwrap_or_default(),
        embedding: Some(embedding),
    };

    let id = state.storage.insert_memory(&new_mem).await?;
    let time_ms = start.elapsed().as_millis() as u64;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": id,
            "status": "ok",
            "searchable": true,
            "time_ms": time_ms
        })),
    ))
}

pub async fn write_batch(
    State(state): State<AppState>,
    Json(req): Json<WriteBatchRequest>,
) -> ApiResult<impl IntoResponse> {
    let start = Instant::now();
    if req.items.len() > limits::MAX_BATCH_ITEMS {
        return Err(ApiError::bad_request(format!(
            "items exceeds limit of {}",
            limits::MAX_BATCH_ITEMS
        )));
    }

    let mut errors = Vec::new();
    let mut eligible: Vec<(usize, &WriteItem)> = Vec::with_capacity(req.items.len());
    for (i, item) in req.items.iter().enumerate() {
        match check_content(&item.content) {
            Ok(()) => eligible.push((i, item)),
            Err(e) => errors.push(json!({"index": i, "code": e.code, "message": e.message})),
        }
    }

    let texts: Vec<String> = eligible
        .iter()
        .map(|(_, item)| item.content.clone())
        .collect();
    let embeddings = if texts.is_empty() {
        Vec::new()
    } else {
        match state.embedder.encode_batch(&texts).await {
            Ok(v) => v,
            Err(e) => {
                for (i, _) in &eligible {
                    errors.push(json!({
                        "index": *i,
                        "code": "EMBED_FAILED",
                        "message": e.to_string()
                    }));
                }
                return Ok((
                    StatusCode::CREATED,
                    Json(json!({
                        "ids": Vec::<String>::new(),
                        "count": 0,
                        "requested": req.items.len(),
                        "errors": errors,
                        "time_ms": start.elapsed().as_millis() as u64
                    })),
                ));
            }
        }
    };

    let mut ids = Vec::with_capacity(eligible.len());
    for ((i, item), embedding) in eligible.iter().zip(embeddings.into_iter()) {
        let new_mem = NewMemory {
            content: item.content.clone(),
            category: None,
            context: item.context.clone().unwrap_or_default(),
            metadata: item.metadata.clone().unwrap_or_default(),
            embedding: Some(embedding),
        };
        match state.storage.insert_memory(&new_mem).await {
            Ok(id) => ids.push(id),
            Err(e) => errors.push(json!({
                "index": *i,
                "code": "WRITE_FAILED",
                "message": e.to_string()
            })),
        }
    }

    let time_ms = start.elapsed().as_millis() as u64;
    let status = if ids.is_empty() && !errors.is_empty() {
        StatusCode::MULTI_STATUS
    } else {
        StatusCode::CREATED
    };
    Ok((
        status,
        Json(json!({
            "ids": ids,
            "count": ids.len(),
            "requested": req.items.len(),
            "errors": errors,
            "time_ms": time_ms
        })),
    ))
}
