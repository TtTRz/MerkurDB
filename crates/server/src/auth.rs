use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;

use crate::app_state::AppState;
use crate::error::ApiError;

/// Bearer-token authentication middleware.
///
/// Tokens come from `auth.tokens` in the config (or the equivalent env vars).
/// Constant-time comparison is used so the middleware is not a token oracle.
pub async fn require_auth(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if state.config.auth.disabled {
        return Ok(next.run(req).await);
    }
    if state.config.auth.tokens.is_empty() {
        // Fail closed: an empty token list with `disabled = false` means the
        // operator forgot to configure auth. Config::validate() should have
        // already rejected this state, but defend in depth.
        return Err(ApiError::unauthorized());
    }

    let header = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = header.strip_prefix("Bearer ").unwrap_or("");
    if token.is_empty() {
        return Err(ApiError::unauthorized());
    }

    let valid = state
        .config
        .auth
        .tokens
        .iter()
        .any(|expected| constant_time_eq(token.as_bytes(), expected.as_bytes()));
    if !valid {
        return Err(ApiError::unauthorized());
    }

    Ok(next.run(req).await)
}

/// Length-aware constant-time compare. Different-length slices return false but
/// still walk the shorter slice to avoid leaking via timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        // Touch one of them to keep the timing profile similar regardless of
        // which side is longer.
        let _ = a.iter().fold(0u8, |acc, x| acc ^ *x);
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
