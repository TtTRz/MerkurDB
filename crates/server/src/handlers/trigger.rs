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
use crate::handlers::namespace::Namespace;
use crate::scheduler;

pub async fn trigger_consolidate(State(state): State<AppState>) -> ApiResult<impl IntoResponse> {
    let report = scheduler::Scheduler::run_consolidation_once(
        &*state.storage,
        &*state.consolidator,
        state.config.consolidation.batch_size,
        state.config.consolidation.adjudication_floor,
        state.config.consolidation.adjudication_candidates,
    )
    .await;
    Ok((
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "processed": report.memories_processed,
            "edges_created": report.edges_created,
            "absorptions": report.absorptions,
            "invalidations": report.invalidations,
            "errors": report.errors
        })),
    ))
}

pub async fn trigger_forget(State(state): State<AppState>) -> ApiResult<impl IntoResponse> {
    let (archived, downgraded, upgraded, cleaned, purged) = scheduler::Scheduler::run_forgetting_once(
        &*state.storage,
        &*state.forgetter,
        state.config.forgetting.batch_size,
        state.config.forgetting.archive_days,
        state.config.forgetting.purge_invalidated_days,
    )
    .await;
    Ok((
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "archived": archived,
            "downgraded": downgraded,
            "upgraded": upgraded,
            "cleaned": cleaned,
            "purged": purged
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

    let mut id_pool: HashSet<String> = HashSet::new();
    for r in &req.edges {
        id_pool.insert(r.source_id.clone());
        id_pool.insert(r.target_id.clone());
    }
    let id_pool_vec: Vec<String> = id_pool.iter().cloned().collect();
    let existing = state.storage.memory_exists_batch(&id_pool_vec).await?;

    let mut created = 0;
    let mut errors = Vec::new();
    for (i, r) in req.edges.iter().enumerate() {
        if r.source_id == r.target_id {
            errors.push(json!({
                "index": i,
                "code": "BAD_REQUEST",
                "message": "source_id and target_id must differ (no self-edges)"
            }));
            continue;
        }
        if !existing.contains(&r.source_id) {
            errors.push(json!({
                "index": i,
                "code": "NOT_FOUND",
                "message": format!("source memory {} not found", r.source_id)
            }));
            continue;
        }
        if !existing.contains(&r.target_id) {
            errors.push(json!({
                "index": i,
                "code": "NOT_FOUND",
                "message": format!("target memory {} not found", r.target_id)
            }));
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
    ns: Namespace,
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
    let neighborhood = state
        .storage
        .bfs_expand_ns(seeds, &ns.0, depth, degree_limit)
        .await?;

    let mut node_ids: HashSet<String> = neighborhood.iter().map(|m| m.id.clone()).collect();
    node_ids.insert(id.clone());
    let node_id_list: Vec<String> = node_ids.iter().cloned().collect();
    let edges_by_node = state
        .storage
        .get_edges_batch(&node_id_list)
        .await
        .unwrap_or_default();
    let mut all_edges = Vec::new();
    let mut seen_edge_ids: HashSet<i64> = HashSet::new();
    for edges in edges_by_node.values() {
        for e in edges {
            // Induced subgraph only: an edge whose far endpoint is outside the
            // visible node set (another bucket, or simply not returned) would
            // leak that endpoint's existence.
            if !node_ids.contains(&e.source_id) || !node_ids.contains(&e.target_id) {
                continue;
            }
            if seen_edge_ids.insert(e.id) {
                all_edges.push(e.clone());
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
