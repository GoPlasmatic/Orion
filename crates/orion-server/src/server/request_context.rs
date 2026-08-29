//! The middleware that fills the per-request context.
//!
//! The context type and its accessors are in [`crate::request_context`] —
//! `errors` and `channel::error_body` read them, and neither is the HTTP
//! layer. What stays here is what genuinely needs a `Request`: reading the
//! headers, and resolving the client address under the trusted-proxy policy.

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

use crate::request_context::{
    MAX_CHANGE_CONTEXT_LEN, MAX_REQUEST_ID_LEN, MAX_USER_AGENT_LEN, REQUEST_CONTEXT, RequestContext,
};
use crate::server::state::AppState;

/// The header carrying the client's user-agent, truncated at
/// [`MAX_USER_AGENT_LEN`] before it is stored.
const USER_AGENT: &str = "user-agent";
const CHANGE_CONTEXT: &str = "x-orion-change-context";

/// A header's value, truncated to `max_len` bytes. `None` when absent, empty
/// or non-ASCII — `HeaderValue::to_str` only succeeds for visible ASCII, so
/// every char is one byte and the slice is always on a char boundary.
fn ascii_header(req: &Request, name: &str, max_len: usize) -> Option<String> {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(|v| v[..v.len().min(max_len)].to_string())
}

/// Middleware that scopes the per-request task-local [`REQUEST_CONTEXT`].
///
/// Must run inside `SetRequestIdLayer` so the `x-request-id` header is already
/// populated when we read it, and it takes `State` because resolving the
/// client address honestly needs the trusted-proxy list.
pub async fn request_context_scope(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    // Same ASCII/byte-boundary reasoning as `ascii_header`, spelled inline
    // because the request id keeps an empty-string (not None) representation.
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|v| &v[..v.len().min(MAX_REQUEST_ID_LEN)])
        .unwrap_or("")
        .to_string();
    let user_agent = ascii_header(&req, USER_AGENT, MAX_USER_AGENT_LEN);
    let change_context = ascii_header(&req, CHANGE_CONTEXT, MAX_CHANGE_CONTEXT_LEN);
    let client_ip = crate::server::rate_limit::extract_client_ip(&req, state.trusted_proxies());
    let ctx = RequestContext {
        request_id,
        client_ip,
        user_agent,
        change_context,
    };
    REQUEST_CONTEXT.scope(ctx, next.run(req)).await
}
