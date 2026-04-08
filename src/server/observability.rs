use std::time::Instant;

use axum::extract::{MatchedPath, Request};
use axum::http::header;
use axum::middleware::Next;
use axum::response::Response;

use crate::metrics;

/// HTTP metrics middleware that records request count, duration, and emits a
/// structured access log line per request.
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

    // Extract request ID set by SetRequestIdLayer (inner layer, runs before us)
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");

    // Borrow request_id for logging; avoid allocating unless tracing needs it
    let request_id = request_id.to_string();

    let start = Instant::now();
    let response = next.run(req).await;
    let duration = start.elapsed().as_secs_f64();

    let status = response.status().as_u16();

    let content_length = response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");

    tracing::info!(
        request_id = %request_id,
        http.method = %method,
        http.route = %path,
        http.status_code = status,
        http.response_content_length = %content_length,
        duration_ms = format_args!("{:.2}", duration * 1000.0),
        "HTTP request"
    );

    // Pass owned Strings to avoid re-allocation inside record_http_request
    metrics::record_http_request(method, path, status, duration);

    response
}
