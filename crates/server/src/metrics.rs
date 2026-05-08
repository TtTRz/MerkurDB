use axum::body::Body;
use axum::extract::MatchedPath;
use axum::http::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use metrics::{counter, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::sync::OnceLock;
use std::time::Instant;

static PROM_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the global metrics recorder. Must be called once at startup.
pub fn init_metrics() {
    let handle = PrometheusBuilder::new()
        .install_recorder()
        .expect("failed to install prometheus recorder");
    PROM_HANDLE
        .set(handle)
        .expect("metrics already initialized");
}

/// Middleware that records request count and latency.
pub async fn track(req: Request<Body>, next: Next) -> Response {
    let method = req.method().to_string();
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| req.uri().path().to_owned());

    let start = Instant::now();
    let response = next.run(req).await;
    let elapsed = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();

    let labels = [
        ("method", method.clone()),
        ("path", path.clone()),
        ("status", status),
    ];
    counter!("merkur_requests_total", &labels).increment(1);
    histogram!(
        "merkur_request_duration_seconds",
        &[("method", method), ("path", path)]
    )
    .record(elapsed);

    response
}

/// GET /v1/metrics — prometheus exposition format.
pub async fn metrics_handler() -> impl IntoResponse {
    let handle = PROM_HANDLE.get().expect("metrics not initialized");
    handle.render()
}
