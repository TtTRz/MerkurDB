use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use merkur_core::SearchMode;
use serde::Deserialize;

use crate::app_state::AppState;
use crate::handlers::write::error_response;

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
    // Advanced filters
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
) -> (StatusCode, Json<serde_json::Value>) {
    let start = std::time::Instant::now();
    let mode = match params.mode.as_str() {
        "fast" => SearchMode::Fast,
        "deep" => SearchMode::Deep,
        _ => SearchMode::Fast,
    };
    let limit = params.limit.unwrap_or_else(|| state.config.fast_limit());
    let threshold = params
        .score_threshold
        .unwrap_or_else(|| state.config.score_threshold());
    let offset = params.offset.unwrap_or(0);

    // Parse date filters
    let from_date: Option<chrono::DateTime<chrono::Utc>> = params
        .from
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.into());
    let to_date: Option<chrono::DateTime<chrono::Utc>> = params
        .to
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.into());

    // Parse level filter (comma-separated)
    let levels: Option<Vec<String>> = params
        .level
        .as_ref()
        .map(|s| s.split(',').map(str::trim).map(str::to_lowercase).collect());

    let query_vec = match state.embedder.encode(&params.q).await {
        Ok(vec) => vec,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "EMBED_FAILED", e),
    };

    let results = match mode {
        SearchMode::Fast => match state.storage.vector_search(&query_vec, limit * 2).await {
            Ok(r) => r,
            Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "SEARCH_FAILED", e),
        },
        SearchMode::Deep => {
            let seeds = match state.storage.vector_search(&query_vec, limit).await {
                Ok(r) => r,
                Err(e) => {
                    return error_response(StatusCode::INTERNAL_SERVER_ERROR, "SEARCH_FAILED", e);
                }
            };
            let seed_ids: Vec<String> = seeds.iter().map(|s| s.id.clone()).collect();
            let depth = params.depth.unwrap_or(2);
            let degree_limit = params.degree_limit.unwrap_or(10);

            match state
                .storage
                .bfs_expand(&seed_ids, depth, degree_limit)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    return error_response(StatusCode::INTERNAL_SERVER_ERROR, "BFS_FAILED", e);
                }
            }
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
            if let Some(ref cat) = params.category {
                r.category == *cat
            } else {
                true
            }
        })
        .filter(|r| {
            if let Some(from) = from_date {
                r.created_at >= from
            } else {
                true
            }
        })
        .filter(|r| {
            if let Some(to) = to_date {
                r.created_at <= to
            } else {
                true
            }
        })
        .collect();

    // Context-aware filtering and boosting
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

    // Build graph data on demand
    let graph = if params.include_graph == Some(true) && !paginated.is_empty() {
        let mut graph_edges = Vec::new();
        let result_ids: Vec<String> = paginated.iter().map(|r| r.id.clone()).collect();
        for memory_id in &result_ids {
            if let Ok(edges) = state.storage.get_edges(memory_id).await {
                for e in edges {
                    graph_edges.push(serde_json::json!({
                        "source_id": e.source_id,
                        "target_id": e.target_id,
                        "weight": e.weight,
                        "relation": e.relation,
                        "edge_type": e.edge_type,
                    }));
                }
            }
        }
        Some(serde_json::json!({
            "nodes": paginated.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            "edges": graph_edges,
        }))
    } else {
        None
    };

    let time_ms = start.elapsed().as_millis() as u64;

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "mode": params.mode,
            "results": paginated.iter().map(|r| {
                serde_json::json!({
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
    )
}
