use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;

tokio::task_local! {
    pub static REQUEST_ID: String;
}

/// Middleware that scopes the per-request task-local `REQUEST_ID`, populated
/// from the inbound `x-request-id` header (set by `SetRequestIdLayer`).
/// `OrionError::IntoResponse` reads it to embed `error.request_id` in the
/// JSON body — clients no longer have to correlate body to header.
pub async fn request_id_scope(req: Request, next: Next) -> Response {
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    REQUEST_ID.scope(request_id, next.run(req)).await
}
