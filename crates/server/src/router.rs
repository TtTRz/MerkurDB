use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::routing::{delete, get, post, put};
use merkur_core::limits;

use crate::app_state::AppState;
use crate::auth::require_auth;
use crate::handlers;

pub fn create_router(state: AppState) -> Router {
    // `/v1/health` is intentionally outside the auth middleware so health checks
    // and load balancers don't need to carry credentials.
    let public = Router::new().route("/v1/health", get(handlers::admin::health));

    let protected = Router::new()
        .route("/v1/write", post(handlers::write::write))
        .route("/v1/write-batch", post(handlers::write::write_batch))
        .route("/v1/search", get(handlers::search::search))
        .route("/v1/memory/{id}", get(handlers::memory::get_memory))
        .route("/v1/memory/{id}", put(handlers::memory::update_memory))
        .route("/v1/memory/{id}", delete(handlers::memory::delete_memory))
        .route("/v1/status", get(handlers::status::status))
        .route(
            "/v1/consolidate",
            post(handlers::trigger::trigger_consolidate),
        )
        .route(
            "/v1/consolidate/log",
            get(handlers::admin::consolidation_log),
        )
        .route("/v1/forget", post(handlers::trigger::trigger_forget))
        .route("/v1/relate", post(handlers::trigger::relate))
        .route("/v1/relate-batch", post(handlers::trigger::relate_batch))
        .route("/v1/graph/{id}", get(handlers::trigger::get_graph))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));

    public
        .merge(protected)
        .layer(DefaultBodyLimit::max(limits::MAX_BODY_BYTES))
        .with_state(state)
}
