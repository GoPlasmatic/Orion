use std::time::Instant;

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;

use crate::metrics;

/// HTTP metrics middleware that records request count and duration.
pub async fn http_metrics_middleware(
    matched_path: Option<MatchedPath>,
    req: Request,
    next: Next,
) -> Response {
    let method = req.method().to_string();
    let path = matched_path
        .as_ref()
        .map(|m: &MatchedPath| m.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());

    let start = Instant::now();
    let response = next.run(req).await;
    let duration = start.elapsed().as_secs_f64();

    let status = response.status().as_u16();
    metrics::record_http_request(&method, &path, status);
    metrics::record_http_request_duration(&method, &path, status, duration);

    response
}
