use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use merkur_core::{EdgeType, NewEdge};
use serde::Deserialize;

use crate::app_state::AppState;
use crate::handlers::write::error_response;
use crate::scheduler;

pub async fn trigger_consolidate(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let report = scheduler::Scheduler::run_consolidation_once(
        &*state.storage,
        &*state.consolidator,
        state.config.consolidation.batch_size,
    )
    .await;

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "processed": report.memories_processed,
            "edges_created": report.edges_created,
            "errors": report.errors
        })),
    )
}

pub async fn trigger_forget(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let (archived, downgraded, cleaned) = scheduler::Scheduler::run_forgetting_once(
        &*state.storage,
        &*state.forgetter,
        state.config.forgetting.batch_size,
        state.config.forgetting.archive_days,
    )
    .await;

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "archived": archived,
            "downgraded": downgraded,
            "cleaned": cleaned
        })),
    )
}

#[derive(Debug, Deserialize)]
pub struct RelateRequest {
    pub source_id: String,
    pub target_id: String,
    pub weight: Option<f64>,
    pub relation: Option<String>,
}

pub async fn relate(
    State(state): State<AppState>,
    Json(req): Json<RelateRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let edge = NewEdge {
        source_id: req.source_id,
        target_id: req.target_id,
        weight: req.weight,
        relation: req.relation,
        edge_type: EdgeType::Manual,
    };

    match state.storage.insert_edge(&edge).await {
        Ok(()) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "status": "edge_created" })),
        ),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "EDGE_FAILED", e),
    }
}

#[derive(Debug, Deserialize)]
pub struct RelateBatchRequest {
    pub edges: Vec<RelateRequest>,
}

pub async fn relate_batch(
    State(state): State<AppState>,
    Json(req): Json<RelateBatchRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut created = 0;
    let mut errors = Vec::new();

    for (i, r) in req.edges.iter().enumerate() {
        let edge = NewEdge {
            source_id: r.source_id.clone(),
            target_id: r.target_id.clone(),
            weight: r.weight,
            relation: r.relation.clone(),
            edge_type: EdgeType::Manual,
        };
        match state.storage.insert_edge(&edge).await {
            Ok(()) => created += 1,
            Err(e) => errors.push(serde_json::json!({"index": i, "message": e.to_string()})),
        }
    }

    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "status": "ok",
            "created": created,
            "errors": errors
        })),
    )
}

pub async fn get_graph(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let seeds = vec![id.clone()];
    let (neighborhood, edges) = match state.storage.bfs_expand(&seeds, 2, 20).await {
        Ok(memories) => {
            let edges = state.storage.get_edges(&id).await.unwrap_or_default();
            (memories, edges)
        }
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "GRAPH_FAILED", e),
    };

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "center": id,
            "neighborhood": neighborhood.iter().map(|m| {
                serde_json::json!({
                    "id": m.id,
                    "content": m.content,
                    "abstract": m.abstract_,
                    "score": m.score,
                    "level": m.level,
                })
            }).collect::<Vec<_>>(),
            "edges": edges.iter().map(|e| {
                serde_json::json!({
                    "id": e.id,
                    "source_id": e.source_id,
                    "target_id": e.target_id,
                    "weight": e.weight,
                    "relation": e.relation,
                    "edge_type": e.edge_type,
                })
            }).collect::<Vec<_>>()
        })),
    )
}
