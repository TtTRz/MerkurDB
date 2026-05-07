use std::collections::HashSet;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use merkur_core::{EdgeType, NewEdge, limits};
use serde::Deserialize;
use serde_json::json;

use crate::app_state::AppState;
use crate::error::{ApiError, ApiResult};
use crate::scheduler;

pub async fn trigger_consolidate(State(state): State<AppState>) -> ApiResult<impl IntoResponse> {
    let report = scheduler::Scheduler::run_consolidation_once(
        &*state.storage,
        &*state.consolidator,
        state.config.consolidation.batch_size,
    )
    .await;
    Ok((
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "processed": report.memories_processed,
            "edges_created": report.edges_created,
            "errors": report.errors
        })),
    ))
}

pub async fn trigger_forget(State(state): State<AppState>) -> ApiResult<impl IntoResponse> {
    let (archived, downgraded, cleaned) = scheduler::Scheduler::run_forgetting_once(
        &*state.storage,
        &*state.forgetter,
        state.config.forgetting.batch_size,
        state.config.forgetting.archive_days,
    )
    .await;
    Ok((
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "archived": archived,
            "downgraded": downgraded,
            "cleaned": cleaned
        })),
    ))
}

#[derive(Debug, Deserialize)]
pub struct RelateRequest {
    pub source_id: String,
    pub target_id: String,
    pub weight: Option<f64>,
    pub relation: Option<String>,
}

async fn validate_edge(state: &AppState, src: &str, dst: &str) -> ApiResult<()> {
    if src == dst {
        return Err(ApiError::bad_request(
            "source_id and target_id must differ (no self-edges)",
        ));
    }
    if !state.storage.memory_exists(src).await? {
        return Err(ApiError::not_found(format!(
            "source memory {src} not found"
        )));
    }
    if !state.storage.memory_exists(dst).await? {
        return Err(ApiError::not_found(format!(
            "target memory {dst} not found"
        )));
    }
    Ok(())
}

pub async fn relate(
    State(state): State<AppState>,
    Json(req): Json<RelateRequest>,
) -> ApiResult<impl IntoResponse> {
    validate_edge(&state, &req.source_id, &req.target_id).await?;
    let edge = NewEdge {
        source_id: req.source_id,
        target_id: req.target_id,
        weight: req.weight,
        relation: req.relation,
        edge_type: EdgeType::Manual,
    };
    state.storage.insert_edge(&edge).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "status": "edge_created" })),
    ))
}

#[derive(Debug, Deserialize)]
pub struct RelateBatchRequest {
    pub edges: Vec<RelateRequest>,
}

pub async fn relate_batch(
    State(state): State<AppState>,
    Json(req): Json<RelateBatchRequest>,
) -> ApiResult<impl IntoResponse> {
    if req.edges.len() > limits::MAX_BATCH_ITEMS {
        return Err(ApiError::bad_request(format!(
            "edges exceeds limit of {}",
            limits::MAX_BATCH_ITEMS
        )));
    }
    let mut created = 0;
    let mut errors = Vec::new();
    for (i, r) in req.edges.iter().enumerate() {
        if let Err(e) = validate_edge(&state, &r.source_id, &r.target_id).await {
            errors.push(json!({"index": i, "code": e.code, "message": e.message}));
            continue;
        }
        let edge = NewEdge {
            source_id: r.source_id.clone(),
            target_id: r.target_id.clone(),
            weight: r.weight,
            relation: r.relation.clone(),
            edge_type: EdgeType::Manual,
        };
        match state.storage.insert_edge(&edge).await {
            Ok(()) => created += 1,
            Err(e) => errors.push(json!({"index": i, "message": e.to_string()})),
        }
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "status": "ok",
            "created": created,
            "requested": req.edges.len(),
            "errors": errors
        })),
    ))
}

#[derive(Debug, Deserialize)]
pub struct GraphQuery {
    pub depth: Option<usize>,
    pub degree_limit: Option<usize>,
}

pub async fn get_graph(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<GraphQuery>,
) -> ApiResult<impl IntoResponse> {
    let depth = params
        .depth
        .unwrap_or_else(|| state.config.default_depth())
        .clamp(1, limits::MAX_BFS_DEPTH);
    let degree_limit = params
        .degree_limit
        .unwrap_or_else(|| state.config.default_degree_limit())
        .clamp(1, limits::MAX_BFS_DEGREE);

    let seeds = std::slice::from_ref(&id);
    let neighborhood = state.storage.bfs_expand(seeds, depth, degree_limit).await?;

    // Include edges for every node in the neighborhood plus the center, so the
    // returned graph reflects the actual local structure rather than a star.
    let mut node_ids: HashSet<String> = neighborhood.iter().map(|m| m.id.clone()).collect();
    node_ids.insert(id.clone());
    let mut all_edges = Vec::new();
    let mut seen_edge_ids: HashSet<i64> = HashSet::new();
    for nid in &node_ids {
        if let Ok(edges) = state.storage.get_edges(nid).await {
            for e in edges {
                if seen_edge_ids.insert(e.id) {
                    all_edges.push(e);
                }
            }
        }
    }

    Ok((
        StatusCode::OK,
        Json(json!({
            "center": id,
            "depth": depth,
            "degree_limit": degree_limit,
            "neighborhood": neighborhood.iter().map(|m| {
                json!({
                    "id": m.id,
                    "content": m.content,
                    "abstract": m.abstract_,
                    "score": m.score,
                    "level": m.level,
                })
            }).collect::<Vec<_>>(),
            "edges": all_edges.iter().map(|e| {
                json!({
                    "id": e.id,
                    "source_id": e.source_id,
                    "target_id": e.target_id,
                    "weight": e.weight,
                    "relation": e.relation,
                    "edge_type": e.edge_type,
                })
            }).collect::<Vec<_>>()
        })),
    ))
}
