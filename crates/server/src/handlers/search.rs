use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use merkur_core::{MemoryLevel, SearchMode, limits};
use serde::Deserialize;
use serde_json::json;

use crate::app_state::AppState;
use crate::error::{ApiError, ApiResult};
use crate::handlers::namespace::Namespace;

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    pub limit: Option<usize>,
    pub score_threshold: Option<f64>,
    pub context: Option<String>,
    pub offset: Option<usize>,
    #[serde(default)]
    pub depth: Option<usize>,
    #[serde(default)]
    pub degree_limit: Option<usize>,
    pub level: Option<String>,
    pub category: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    #[serde(default)]
    pub include_graph: Option<bool>,
}

fn default_mode() -> String {
    "hybrid".to_string()
}

pub async fn search(
    State(state): State<AppState>,
    ns: Namespace,
    Query(params): Query<SearchQuery>,
) -> ApiResult<impl IntoResponse> {
    let start = std::time::Instant::now();
    if params.q.is_empty() {
        return Err(ApiError::bad_request("q must not be empty"));
    }
    let mode = match params.mode.as_str() {
        "hybrid" => SearchMode::Hybrid,
        "fast" => SearchMode::Fast,
        "deep" => SearchMode::Deep,
        other => {
            return Err(ApiError::bad_request(format!(
                "Unknown search mode: {other}"
            )));
        }
    };

    let limit = params
        .limit
        .unwrap_or_else(|| state.config.fast_limit())
        .clamp(1, limits::MAX_SEARCH_LIMIT);
    let depth = params
        .depth
        .unwrap_or_else(|| state.config.default_depth())
        .clamp(0, limits::MAX_BFS_DEPTH);
    let degree_limit = params
        .degree_limit
        .unwrap_or_else(|| state.config.default_degree_limit())
        .clamp(1, limits::MAX_BFS_DEGREE);
    let threshold = params
        .score_threshold
        .unwrap_or_else(|| state.config.score_threshold());
    let offset = params.offset.unwrap_or(0);

    let from_date: Option<chrono::DateTime<chrono::Utc>> = parse_optional_rfc3339(&params.from)?;
    let to_date: Option<chrono::DateTime<chrono::Utc>> = parse_optional_rfc3339(&params.to)?;

    let level_filter: Option<Vec<MemoryLevel>> = params.level.as_deref().map(parse_level_list);

    let query_vec = state.embedder.encode(&params.q).await?;

    let results = match mode {
        // Hybrid is the two-channel recall (BM25 x vector, RRF-fused with
        // normalized scores). Level/category/date filtering and the context
        // boost below apply to fused results exactly as they did to cosine.
        // The fused pool gets headroom beyond `limit` (at least 2x, or enough
        // to cover the requested page) so post-filters and offset pagination
        // do not silently starve.
        SearchMode::Hybrid => {
            let pool = limit.saturating_mul(2).max(offset.saturating_add(limit));
            merkur_core::hybrid_recall(
                state.storage.as_ref(),
                &query_vec,
                &params.q,
                &ns.0,
                pool,
                threshold,
                &state.config.fusion_params(),
            )
            .await?
        }
        SearchMode::Fast => {
            state
                .storage
                .vector_search_ns(&query_vec, &ns.0, limit * 2)
                .await?
        }
        SearchMode::Deep => {
            let seeds = state
                .storage
                .vector_search_ns(&query_vec, &ns.0, limit)
                .await?;
            let seed_ids: Vec<String> = seeds.iter().map(|s| s.id.clone()).collect();
            state
                .storage
                .bfs_expand_ns(&seed_ids, &ns.0, depth, degree_limit)
                .await?
        }
    };

    let mut candidates: Vec<_> = results
        .into_iter()
        .filter(|r| level_filter.as_ref().is_none_or(|ls| ls.contains(&r.level)))
        .filter(|r| {
            params
                .category
                .as_ref()
                .is_none_or(|cat| r.category == *cat)
        })
        .filter(|r| from_date.is_none_or(|f| r.created_at >= f))
        .filter(|r| to_date.is_none_or(|t| r.created_at <= t))
        .collect();

    if let Some(ref ctx_str) = params.context
        && let Ok(ctx_filter) = serde_json::from_str::<serde_json::Value>(ctx_str)
        && let Some(obj) = ctx_filter.as_object()
    {
        for r in &mut candidates {
            let mut boost = 0.0;
            for (k, v) in obj {
                if let Some(val) = r.context.get(k)
                    && val == v.as_str().unwrap_or("")
                {
                    boost += 0.1;
                }
            }
            r.score += boost;
        }
    }

    // Hybrid recall already gated on the fused relevance inside
    // `hybrid_recall`; re-applying the threshold to the composite score here
    // would double-gate, and the composite's structural floor (0.35 for a
    // fresh memory at default weights) would distort the semantics.
    let mut filtered: Vec<_> = if matches!(mode, SearchMode::Hybrid) {
        candidates
    } else {
        candidates
            .into_iter()
            .filter(|r| r.score >= threshold)
            .collect()
    };
    filtered.sort_by(|a, b| b.score.total_cmp(&a.score));

    let total = filtered.len();
    let paginated: Vec<_> = filtered.into_iter().skip(offset).take(limit).collect();

    // Record the demand signal for exactly the results being served.
    // Best-effort: a bookkeeping failure must not fail the search.
    let served: Vec<String> = paginated.iter().map(|r| r.id.clone()).collect();
    if let Err(e) = state.storage.record_access(&served).await {
        tracing::warn!(error = %e, "failed to record access for served results");
    }

    let graph = if params.include_graph == Some(true) && !paginated.is_empty() {
        let result_ids: Vec<String> = paginated.iter().map(|r| r.id.clone()).collect();
        let node_set: std::collections::HashSet<&String> = result_ids.iter().collect();
        let by_id = state
            .storage
            .get_edges_batch(&result_ids)
            .await
            .unwrap_or_default();
        let mut graph_edges = Vec::new();
        for edges in by_id.values() {
            for e in edges {
                // Induced subgraph only: an edge whose far endpoint is outside
                // the served page (another bucket, or just not returned) would
                // leak that endpoint's existence.
                if node_set.contains(&e.source_id) && node_set.contains(&e.target_id) {
                    graph_edges.push(json!({
                        "source_id": e.source_id,
                        "target_id": e.target_id,
                        "weight": e.weight,
                        "relation": e.relation,
                        "edge_type": e.edge_type,
                    }));
                }
            }
        }
        Some(json!({
            "nodes": result_ids,
            "edges": graph_edges,
        }))
    } else {
        None
    };

    let time_ms = start.elapsed().as_millis() as u64;

    Ok((
        StatusCode::OK,
        Json(json!({
            "mode": params.mode,
            "results": paginated.iter().map(|r| {
                json!({
                    "id": r.id,
                    "content": r.content,
                    "abstract": r.abstract_,
                    "score": r.score,
                    "weight": r.weight,
                    "level": r.level,
                    "category": r.category,
                    "context": r.context,
                    "created_at": r.created_at,
                    "namespace": r.namespace,
                    "importance": r.importance
                })
            }).collect::<Vec<_>>(),
            "total": total,
            "time_ms": time_ms,
            "filters": {
                "level": params.level,
                "category": params.category,
                "from": params.from,
                "to": params.to,
            },
            "graph": graph
        })),
    ))
}

fn parse_optional_rfc3339(s: &Option<String>) -> ApiResult<Option<chrono::DateTime<chrono::Utc>>> {
    match s.as_deref() {
        None => Ok(None),
        Some(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|dt| Some(dt.into()))
            .map_err(|e| ApiError::bad_request(format!("invalid RFC3339 date: {e}"))),
    }
}

/// Parse comma-separated level filter into typed values. Unknown tokens are
/// silently skipped.
fn parse_level_list(s: &str) -> Vec<MemoryLevel> {
    s.split(',')
        .map(str::trim)
        .filter(|tok| !tok.is_empty())
        .filter_map(|tok| match tok.to_ascii_lowercase().as_str() {
            "full" => Some(MemoryLevel::Full),
            "summary" => Some(MemoryLevel::Summary),
            "title" => Some(MemoryLevel::Title),
            "archived" => Some(MemoryLevel::Archived),
            _ => None,
        })
        .collect()
}
