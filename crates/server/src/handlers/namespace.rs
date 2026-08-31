//! `X-Merkur-Namespace` request-scoped bucket routing.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

/// Canonical header carrying the caller's bucket.
pub const NAMESPACE_HEADER: &str = "x-merkur-namespace";

/// Request-scoped bucket extracted from [`NAMESPACE_HEADER`].
///
/// Missing, empty, or non-UTF-8 values all fall back to the default bucket so
/// that a client can never *fail* a request merely by omitting the header —
/// isolation must be opt-in, not a footgun. Invalid UTF-8 specifically is
/// swallowed rather than 400'd: the header is metadata, not payload.
#[derive(Debug, Clone)]
pub struct Namespace(pub String);

impl Default for Namespace {
    fn default() -> Self {
        Self(merkur_core::DEFAULT_NAMESPACE.to_string())
    }
}

impl<S> FromRequestParts<S> for Namespace
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let ns = parts
            .headers
            .get(NAMESPACE_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| merkur_core::DEFAULT_NAMESPACE.to_string());
        Ok(Namespace(ns))
    }
}
