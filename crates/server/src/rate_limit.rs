use axum::extract::State;
use axum::http::Request;
use axum::middleware::Next;
use axum::response::Response;
use governor::{Quota, RateLimiter, clock::DefaultClock, state::InMemoryState, state::NotKeyed};
use std::num::NonZeroU32;
use std::sync::Arc;

use crate::app_state::AppState;
use crate::error::ApiError;

pub type GlobalLimiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

pub fn build_limiter(requests_per_second: u32) -> Arc<GlobalLimiter> {
    let quota = Quota::per_second(NonZeroU32::new(requests_per_second).unwrap_or(NonZeroU32::MIN));
    Arc::new(RateLimiter::direct(quota))
}

pub async fn check_rate_limit(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, ApiError> {
    if let Some(ref limiter) = state.rate_limiter
        && limiter.check().is_err()
    {
        return Err(ApiError {
            status: axum::http::StatusCode::TOO_MANY_REQUESTS,
            code: "RATE_LIMITED",
            message: "Too many requests".into(),
        });
    }
    Ok(next.run(req).await)
}
