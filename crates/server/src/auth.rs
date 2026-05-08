use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;
use subtle::ConstantTimeEq;

use crate::app_state::AppState;
use crate::error::ApiError;

pub async fn require_auth(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if state.config.auth.disabled {
        return Ok(next.run(req).await);
    }
    if state.config.auth.tokens.is_empty() {
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

    let valid = state.config.auth.tokens.iter().any(|expected| {
        let a = token.as_bytes();
        let b = expected.as_bytes();
        a.len() == b.len() && a.ct_eq(b).into()
    });
    if !valid {
        return Err(ApiError::unauthorized());
    }

    Ok(next.run(req).await)
}
