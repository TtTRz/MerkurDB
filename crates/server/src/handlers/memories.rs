use axum::Json;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use merkur_core::MemoryListFilter;
use serde::Deserialize;
use serde_json::json;

use crate::app_state::AppState;
use crate::error::ApiResult;
use crate::handlers::namespace::Namespace;
use crate::handlers::search::parse_level_list;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub namespace: Option<String>,
    pub level: Option<String>,
    pub category: Option<String>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

pub async fn list_memories(
    State(state): State<AppState>,
    ns: Namespace,
    Query(params): Query<ListQuery>,
) -> ApiResult<impl IntoResponse> {
    let filter = MemoryListFilter {
        namespace: Some(params.namespace.unwrap_or(ns.0)),
        levels: params.level.as_deref().map(parse_level_list),
        category: params.category,
        offset: params.offset.unwrap_or(0),
        limit: params.limit.unwrap_or(20).clamp(1, 200),
    };
    let (items, total) = state.storage.list_memories(&filter).await?;
    Ok(Json(json!({
        "items": items.iter().map(|m| json!({
            "id": m.id,
            "content": m.content,
            "abstract": m.abstract_,
            "weight": m.weight,
            "level": m.level,
            "category": m.category,
            "context": m.context,
            "created_at": m.created_at,
            "namespace": m.namespace,
            "importance": m.importance,
            "invalid_at": m.invalid_at,
        })).collect::<Vec<_>>(),
        "total": total,
        "offset": filter.offset,
        "limit": filter.limit,
    })))
}
