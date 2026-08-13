use std::borrow::Cow;
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
    // `Method` is an inline enum for the standard verbs, so cloning it is
    // cheaper than the `to_string()` this used to do — and `as_str()` below
    // then costs nothing. Only an extension method allocates.
    let method = req.method().clone();
    // Borrowed from `matched_path`, which outlives the response. Only the
    // unmatched fallback allocates, and it has to: it reads from `req`, which
    // moves into `next.run` below.
    let path: Cow<'_, str> = match matched_path.as_ref() {
        Some(m) => Cow::Borrowed(m.as_str()),
        None => Cow::Owned(req.uri().path().to_string()),
    };

    // Request id set by SetRequestIdLayer (inner layer, runs before us).
    //
    // Owned rather than borrowed: `req` moves into `next.run(req)` below, so a
    // borrow of its headers cannot survive to the log line after the response.
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();

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
        http.method = %method.as_str(),
        http.route = %path,
        http.status_code = status,
        http.response_content_length = %content_length,
        duration_ms = format_args!("{:.2}", duration * 1000.0),
        "HTTP request"
    );

    metrics::record_http_request(method.as_str(), &path, status, duration);

    response
}
