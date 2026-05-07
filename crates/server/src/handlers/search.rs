use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use merkur_core::{SearchMode, limits};
use serde::Deserialize;
use serde_json::json;

use crate::app_state::AppState;
use crate::error::{ApiError, ApiResult};

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
    "fast".to_string()
}

pub async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchQuery>,
) -> ApiResult<impl IntoResponse> {
    let start = std::time::Instant::now();
    if params.q.is_empty() {
        return Err(ApiError::bad_request("q must not be empty"));
    }
    let mode = match params.mode.as_str() {
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

    let levels: Option<Vec<String>> = params
        .level
        .as_ref()
        .map(|s| s.split(',').map(str::trim).map(str::to_lowercase).collect());

    let query_vec = state.embedder.encode(&params.q).await?;

    let results = match mode {
        SearchMode::Fast => state.storage.vector_search(&query_vec, limit * 2).await?,
        SearchMode::Deep => {
            let seeds = state.storage.vector_search(&query_vec, limit).await?;
            let seed_ids: Vec<String> = seeds.iter().map(|s| s.id.clone()).collect();
            state
                .storage
                .bfs_expand(&seed_ids, depth, degree_limit)
                .await?
        }
    };

    let mut filtered: Vec<_> = results
        .into_iter()
        .filter(|r| r.score >= threshold)
        .filter(|r| {
            if let Some(ref levels) = levels {
                let rl = format!("{:?}", r.level).to_lowercase();
                levels.contains(&rl)
            } else {
                true
            }
        })
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
        for r in &mut filtered {
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
        filtered.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    let total = filtered.len();
    let paginated: Vec<_> = filtered.into_iter().skip(offset).take(limit).collect();

    let graph = if params.include_graph == Some(true) && !paginated.is_empty() {
        let result_ids: Vec<String> = paginated.iter().map(|r| r.id.clone()).collect();
        let mut graph_edges = Vec::new();
        for memory_id in &result_ids {
            if let Ok(edges) = state.storage.get_edges(memory_id).await {
                for e in edges {
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
                    "created_at": r.created_at
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
