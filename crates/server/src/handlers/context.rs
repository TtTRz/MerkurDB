//! `POST /v1/context` — token-budget context assembly (P1-6).
//!
//! The MCP-facing entry point: given a query and a token budget, return a
//! prompt-ready digest plus the surviving items, deduplicated and packed
//! greedily under the budget. Every step reuses the hybrid retrieval core so
//! this endpoint can never drift from `/v1/search` behavior.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::json;

use crate::app_state::AppState;
use crate::error::ApiResult;
use crate::handlers::namespace::Namespace;

/// Default Jaccard similarity above which two memories count as duplicates.
const MMR_THRESHOLD: f64 = 0.8;

#[derive(Debug, Deserialize)]
pub struct ContextRequest {
    /// Free-text query steering the hybrid recall.
    pub q: String,
    /// Hard ceiling on the assembled digest's approximate token count.
    pub token_budget: usize,
    /// Recall width before dedup/packing (default: 5x the expected yield).
    pub limit: Option<usize>,
}

pub async fn assemble_context(
    State(state): State<AppState>,
    ns: Namespace,
    Json(req): Json<ContextRequest>,
) -> ApiResult<impl IntoResponse> {
    if req.q.trim().is_empty() {
        return Err(crate::error::ApiError::bad_request("q must not be empty"));
    }
    if req.token_budget == 0 {
        return Err(crate::error::ApiError::bad_request(
            "token_budget must be positive",
        ));
    }

    // Over-recall so dedup and packing have room to select, then fuse. No
    // relevance floor: MMR + the token budget do the selection here.
    let recall = req.limit.unwrap_or(50).clamp(1, 200);
    let query_vec = state.embedder.encode(&req.q).await?;
    let mut hits = merkur_core::hybrid_recall(
        state.storage.as_ref(),
        &query_vec,
        &req.q,
        &ns.0,
        recall,
        0.0,
        &state.config.fusion_params(),
    )
    .await?;

    let deduped = merkur_core::mmr_dedup(&mut hits, MMR_THRESHOLD);
    let (packed, dropped) = merkur_core::greedy_pack(&deduped, req.token_budget);

    // Record demand for exactly the items served. Best-effort: a bookkeeping
    // failure must not fail the assembly.
    let served: Vec<String> = packed.iter().map(|m| m.id.clone()).collect();
    if let Err(e) = state.storage.record_access(&served).await {
        tracing::warn!(error = %e, "failed to record access for context items");
    }

    let digest = packed
        .iter()
        .map(|m| format!("- {}", m.content))
        .collect::<Vec<_>>()
        .join("\n");
    let token_estimate: usize = packed
        .iter()
        .map(|m| merkur_core::estimate_tokens(&m.content))
        .sum();

    Ok((
        StatusCode::OK,
        Json(json!({
            "digest": digest,
            "items": packed.iter().map(|m| json!({
                "id": m.id,
                "content": m.content,
                "score": m.score,
                "weight": m.weight,
                "importance": m.importance,
                "namespace": m.namespace,
            })).collect::<Vec<_>>(),
            "token_estimate": token_estimate,
            "dropped": dropped,
        })),
    ))
}
