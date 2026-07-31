use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Value, json};

/// Per-field validation detail returned in the `error.details[]` array.
///
/// `path` is a dotted/indexed pointer into the offending request body
/// (e.g. `channel.protocol`, `tasks[2].function.input.connector`).
/// `code` is a stable machine-readable identifier such as `REQUIRED`,
/// `ENUM_MISMATCH`, or `INVALID_FORMAT`.
#[derive(Debug, Clone, Serialize)]
pub struct FieldError {
    pub path: String,
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub got: Option<Value>,
}

impl FieldError {
    pub fn new(path: impl Into<String>, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            code,
            message: message.into(),
            expected: None,
            got: None,
        }
    }

    pub fn with_expected(mut self, expected: impl Into<Value>) -> Self {
        self.expected = Some(expected.into());
        self
    }

    pub fn with_got(mut self, got: impl Into<Value>) -> Self {
        self.got = Some(got.into());
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OrionError {
    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Bad request: {0}")]
    BadRequest(String),

    /// A validation failure with structured per-field details.
    /// Maps to HTTP 400 with `code` defaulting to `VALIDATION_ERROR`.
    /// `details` is omitted from the response body when empty so clients
    /// expecting only the v0.1 `{code, message}` envelope still work.
    #[error("Validation failed: {message}")]
    Validation {
        code: &'static str,
        message: String,
        details: Vec<FieldError>,
    },

    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Forbidden: {0}")]
    Forbidden(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    /// An internal failure, with the cause attached when there is one.
    ///
    /// G11: this was two variants. `Internal(String)` was `InternalSource`
    /// minus a cause, and since G2 routed every 5xx through one redaction
    /// policy the pair became indistinguishable at the boundary — both answer
    /// 500 `INTERNAL_ERROR` with "An internal error occurred" and log the
    /// detail. Two variants that provably differ in nothing a caller or an
    /// operator can observe are one variant with an optional field.
    #[error("{context}")]
    Internal {
        context: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    #[error("Configuration error: {message}")]
    Config { message: String },

    #[error("Rate limited: {0}")]
    RateLimited(String),

    /// A channel's `rate_limit.key_logic` could not be evaluated, so the
    /// request cannot be metered into any bucket (N5 refuses rather than
    /// counting it in the wrong one).
    ///
    /// Answers `429` like [`OrionError::RateLimited`], because a caller that
    /// cannot be metered must be told to back off and there is nothing
    /// actionable to disclose about a channel's own logic. It is a separate
    /// variant because the *condition* is not the same: being over a limit is
    /// transient and clears with time, while a key expression that fails on
    /// this payload will fail on every redelivery of it. The Kafka ingress
    /// reads that difference — an over-limit record is retried, an
    /// unevaluable-key record is dead-lettered instead of blocking its
    /// partition forever.
    #[error("Rate limit key unavailable: {0}")]
    RateLimitKeyUnavailable(String),

    #[error("Response too large: {0}")]
    ResponseTooLarge(String),

    #[error("Service unavailable: {0}")]
    ServiceUnavailable(String),

    #[error("Timeout: channel '{channel}' exceeded {timeout_ms}ms")]
    Timeout { channel: String, timeout_ms: u64 },

    #[error("Unsupported media type: {0}")]
    UnsupportedMediaType(String),

    #[error("Method not allowed: {0}")]
    MethodNotAllowed(String),

    #[error("Storage error: {0}")]
    Storage(#[from] sqlx::Error),

    #[error("Engine error: {0}")]
    Engine(#[from] dataflow_rs::DataflowError),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl OrionError {
    /// Whether this error is likely transient and the operation could succeed on retry.
    pub fn is_retryable(&self) -> bool {
        match self {
            OrionError::Storage(_) => true,
            OrionError::Engine(e) => e.retryable(),
            OrionError::RateLimited(_) => true,
            OrionError::ServiceUnavailable(_) => true,
            OrionError::Timeout { .. } => true,
            _ => false,
        }
    }

    /// An internal failure with no cause to attach.
    pub fn internal(context: impl Into<String>) -> Self {
        OrionError::Internal {
            context: context.into(),
            source: None,
        }
    }

    /// An internal failure carrying the error that caused it. The cause reaches
    /// the log, never the client.
    pub fn internal_from(
        context: impl Into<String>,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        OrionError::Internal {
            context: context.into(),
            source: Some(source.into()),
        }
    }

    /// Construct a `Validation` error with no field details yet. For the
    /// common single-field case use [`OrionError::invalid_field`].
    pub fn validation(message: impl Into<String>) -> Self {
        OrionError::Validation {
            code: "VALIDATION_ERROR",
            message: message.into(),
            details: Vec::new(),
        }
    }

    /// Build a single-field validation error in one call. Most validators
    /// only fail on one field at a time — this keeps the call site tight.
    pub fn invalid_field(
        path: impl Into<String>,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        let message = message.into();
        OrionError::Validation {
            code: "VALIDATION_ERROR",
            message: message.clone(),
            details: vec![FieldError::new(path, code, message)],
        }
    }
}

impl OrionError {
    /// The HTTP status, error code, and **client-safe** message for this error.
    ///
    /// This is the single place the redaction policy lives. It used to be a
    /// per-arm convention inside `IntoResponse` — three different 5xx shapes
    /// coexisted, and whether an internal string reached the client was decided
    /// one arm at a time. That is how G2, G3, G5 and G8 each became true
    /// independently, and why nothing outside `IntoResponse` could ask "is this
    /// message safe to show?" — which is what let the bulk-import handlers
    /// embed raw driver text in a 200 body.
    ///
    /// Does not log: callers that surface the error (`IntoResponse`) log the
    /// internal detail separately via `log_internal_detail`.
    pub fn response_parts(&self) -> (StatusCode, &'static str, String) {
        match self {
            OrionError::NotFound(msg) => (StatusCode::NOT_FOUND, "NOT_FOUND", msg.clone()),
            OrionError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "BAD_REQUEST", msg.clone()),
            OrionError::Validation { code, message, .. } => {
                (StatusCode::BAD_REQUEST, code, message.clone())
            }
            OrionError::Unauthorized(msg) => {
                (StatusCode::UNAUTHORIZED, "UNAUTHORIZED", msg.clone())
            }
            OrionError::Forbidden(msg) => (StatusCode::FORBIDDEN, "FORBIDDEN", msg.clone()),
            OrionError::Conflict(msg) => (StatusCode::CONFLICT, "CONFLICT", msg.clone()),
            OrionError::UnsupportedMediaType(msg) => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "UNSUPPORTED_MEDIA_TYPE",
                msg.clone(),
            ),
            OrionError::MethodNotAllowed(msg) => (
                StatusCode::METHOD_NOT_ALLOWED,
                "METHOD_NOT_ALLOWED",
                msg.clone(),
            ),
            OrionError::ServiceUnavailable(msg) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "SERVICE_UNAVAILABLE",
                msg.clone(),
            ),
            OrionError::RateLimited(msg) | OrionError::RateLimitKeyUnavailable(msg) => {
                (StatusCode::TOO_MANY_REQUESTS, "RATE_LIMITED", msg.clone())
            }
            OrionError::Timeout {
                channel,
                timeout_ms,
            } => (
                StatusCode::GATEWAY_TIMEOUT,
                "TIMEOUT",
                format!(
                    "Workflow execution on channel '{channel}' exceeded {timeout_ms}ms timeout"
                ),
            ),
            OrionError::ResponseTooLarge(msg) => {
                (StatusCode::BAD_GATEWAY, "RESPONSE_TOO_LARGE", msg.clone())
            }
            // G2: `Internal` and `Config` used to return their message verbatim.
            // Reachable `Config` messages carry filesystem paths (TLS cert/key)
            // and whole database URLs — `detect_backend` is called on a
            // *connector's* connection string at request time, so a DSN with
            // credentials could round-trip into a 500 body.
            OrionError::Internal { .. } => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "An internal error occurred".to_string(),
            ),
            OrionError::Config { .. } => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "CONFIG_ERROR",
                "A configuration error occurred".to_string(),
            ),
            OrionError::Storage(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "STORAGE_ERROR",
                "An internal storage error occurred".to_string(),
            ),
            OrionError::Engine(e) => engine_error_response(e),
            // G8: this variant is reached both by inbound parse failures (a
            // genuine 400) and by outbound *serialize* failures, which are
            // server bugs. The raw serde message also carries byte offsets and
            // type names, so it is not surfaced.
            OrionError::Serialization(_) => (
                StatusCode::BAD_REQUEST,
                "SERIALIZATION_ERROR",
                "Request body could not be processed".to_string(),
            ),
        }
    }

    /// The client-safe message alone. Use this anywhere an error string is
    /// embedded in a response body outside `IntoResponse` — notably the bulk
    /// import handlers, which report per-item failures inside a 200.
    pub fn client_message(&self) -> String {
        self.response_parts().2
    }

    /// Emit the internal detail to the log for the variants whose client-facing
    /// message is redacted. No-op for variants that surface their own message.
    fn log_internal_detail(&self) {
        match self {
            OrionError::Internal {
                context,
                source: None,
            } => {
                tracing::error!(error.category = "internal", error = %context, "internal error")
            }
            OrionError::Config { message } => {
                tracing::error!(error.category = "config", error = %message, "config error")
            }
            OrionError::Storage(e) => {
                tracing::error!(error.category = "storage", error = %e, "storage error")
            }
            OrionError::Serialization(e) => {
                tracing::error!(error.category = "serialization", error = %e, "serialization error")
            }
            OrionError::Internal {
                context,
                source: Some(source),
            } => tracing::error!(
                error.category = "internal",
                error.context = %context,
                error.source = %source,
                "Internal error"
            ),
            OrionError::Engine(e) => {
                tracing::error!(error.category = "engine", error = %e, "Engine error")
            }
            _ => {}
        }
    }
}

impl IntoResponse for OrionError {
    fn into_response(self) -> Response {
        // Pull the validation details out before consuming `self` in the match,
        // since the response body needs them as a separate JSON field.
        let validation_details = match &self {
            OrionError::Validation { details, .. } if !details.is_empty() => Some(details.clone()),
            _ => None,
        };

        self.log_internal_detail();
        let (status, code, message) = self.response_parts();

        let mut error_obj = serde_json::Map::new();
        error_obj.insert("code".to_string(), Value::String(code.to_string()));
        error_obj.insert("message".to_string(), Value::String(message));

        if let Some(details) = validation_details
            && let Ok(details_value) = serde_json::to_value(&details)
        {
            error_obj.insert("details".to_string(), details_value);
        }

        // Best-effort embed of the request_id set by the per-request scope
        // middleware. Omitted when the task-local isn't in scope (e.g. unit
        // tests calling IntoResponse directly) or when the inbound request
        // had no x-request-id header for the SetRequestIdLayer to populate.
        if let Some(rid) = crate::server::request_context::request_id() {
            error_obj.insert("request_id".to_string(), Value::String(rid));
        }

        let body = json!({ "error": Value::Object(error_obj) });

        let mut response = (status, axum::Json(body)).into_response();
        // A 429 always carries `retry-after`. It used to be added by the rate
        // limit middleware, which was the only producer; the per-channel limit
        // now refuses from the ingress guards (S15), so the header belongs to
        // the error rather than to one of its two call sites.
        if status == StatusCode::TOO_MANY_REQUESTS {
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("1"),
            );
        }
        response
    }
}

/// The classifications Orion's function handlers attach to a
/// [`dataflow_rs::DataflowError::Service`].
///
/// These used to be **prefixes on the error message**, matched back out with
/// `starts_with` in an order-sensitive `match`: `AsyncFunctionHandler` has to
/// return a `DataflowError`, and before 3.1 that enum was closed with no
/// extension point, so a handler's own classification had nowhere to live but
/// the text. The markers leaked verbatim into `message.errors`, trace rows and
/// DLQ payloads, because nothing on the async path stripped them.
///
/// `kind` is now a first-class field the engine never interprets, and it
/// survives being wrapped in `FunctionExecution` for context. `detail` is the
/// operator-only channel that `Display` never renders, so `to_string()` on one
/// of these is always safe to hand to an untrusted caller.
pub mod kind {
    /// An open circuit breaker shed the request before it reached a connector.
    pub const CIRCUIT_OPEN: &str = "circuit_open";
    /// A refusal whose text names a connector or other internal topology. G3:
    /// "operation 'delete' is disabled on connector 'prod-billing-db'" hands
    /// out connector inventory to an anonymous caller, so the caller gets a
    /// generic message and the real text rides in `detail`.
    pub const CONNECTOR_DETAIL: &str = "connector_detail";
    /// N16: a target channel's own ingress guards refused an in-process
    /// `channel_call`. One kind per outcome, so the status is declared by the
    /// producer rather than re-derived from a number.
    pub const CHANNEL_RATE_LIMITED: &str = "channel_rate_limited";
    pub const CHANNEL_FORBIDDEN: &str = "channel_forbidden";
    pub const CHANNEL_CONFLICT: &str = "channel_conflict";
    pub const CHANNEL_UNAVAILABLE: &str = "channel_unavailable";
}

/// Build the error a `channel_call` returns when the target channel's guards
/// refused it, preserving the refusal's own meaning (over limit, forbidden,
/// conflicting, or at capacity / fail-closed).
pub fn channel_refused_dataflow_error(
    status: StatusCode,
    message: String,
) -> dataflow_rs::DataflowError {
    let kind = match status.as_u16() {
        429 => kind::CHANNEL_RATE_LIMITED,
        403 => kind::CHANNEL_FORBIDDEN,
        409 => kind::CHANNEL_CONFLICT,
        _ => kind::CHANNEL_UNAVAILABLE,
    };
    // Retryable: the caller is over a limit or the target is at capacity.
    // Neither is a server fault and both clear on their own.
    dataflow_rs::DataflowError::service(kind, message)
        .retryable(!matches!(status.as_u16(), 403 | 409))
        .build()
}

/// Build the error an open breaker returns from a function handler.
///
/// The connector and channel names stay in the caller-facing message, as they
/// were under the marker. That is arguably at odds with the redaction
/// [`kind::CONNECTOR_DETAIL`] applies to the same kind of names, but it is the
/// shipped behaviour of `503 CIRCUIT_OPEN` and changing it is a data-plane
/// contract decision, not part of swapping the mechanism underneath it.
pub fn circuit_open_dataflow_error(connector: &str, channel: &str) -> dataflow_rs::DataflowError {
    dataflow_rs::DataflowError::service(
        kind::CIRCUIT_OPEN,
        format!("Circuit breaker open for connector '{connector}' on channel '{channel}'"),
    )
    // Retryable: a shed dependency is transient, and the DLQ retry loop must
    // re-attempt it — by the time it does, the breaker may well have closed.
    // Nothing retries it *within* the request: `guarded_handler` checks the
    // breaker outside `retry_with_policy`, so this never drives that loop.
    .retryable(true)
    .build()
}

/// Build a refusal whose text names internal topology (G3).
///
/// The caller gets `public`; `detail` is logged and kept on the trace.
pub fn connector_detail_error(detail: impl std::fmt::Display) -> dataflow_rs::DataflowError {
    dataflow_rs::DataflowError::service(kind::CONNECTOR_DETAIL, "Request validation failed")
        .detail(detail.to_string())
        .build()
}

/// Map DataflowError variants to appropriate HTTP status codes and sanitized messages.
fn engine_error_response(e: &dataflow_rs::DataflowError) -> (StatusCode, &'static str, String) {
    use dataflow_rs::DataflowError;

    // A handler-classified failure answers for itself, with no ordering hazard
    // between guards and no risk of a genuine downstream 503 relayed by
    // `http_call` being mistaken for one of Orion's own refusals.
    //
    // `Display` on a `Service` error is the caller-safe message alone, so
    // `to_string()` here can never leak the operator-only `detail`.
    if let Some(k) = e.kind() {
        let (status, code) = match k {
            kind::CIRCUIT_OPEN => (StatusCode::SERVICE_UNAVAILABLE, "CIRCUIT_OPEN"),
            kind::CONNECTOR_DETAIL => (StatusCode::BAD_REQUEST, "VALIDATION_ERROR"),
            kind::CHANNEL_RATE_LIMITED => (StatusCode::TOO_MANY_REQUESTS, "RATE_LIMITED"),
            kind::CHANNEL_FORBIDDEN => (StatusCode::FORBIDDEN, "FORBIDDEN"),
            kind::CHANNEL_CONFLICT => (StatusCode::CONFLICT, "CONFLICT"),
            kind::CHANNEL_UNAVAILABLE => (StatusCode::SERVICE_UNAVAILABLE, "SERVICE_UNAVAILABLE"),
            _ => {
                tracing::error!(kind = %k, "unhandled service error kind; mapped to 500");
                (StatusCode::INTERNAL_SERVER_ERROR, "ENGINE_ERROR")
            }
        };
        return (status, code, e.to_string());
    }

    match e {
        // Untagged validation messages are workflow-structural and safe —
        // "max call depth 10 exceeded", "'delete' has no filter" — and stay
        // verbatim, because they are what makes a misconfigured workflow
        // diagnosable from the response. Anything naming a connector goes
        // through `connector_detail_error` and is handled above.
        DataflowError::Validation(msg) => {
            (StatusCode::BAD_REQUEST, "VALIDATION_ERROR", msg.clone())
        }
        DataflowError::Timeout(msg) => (StatusCode::GATEWAY_TIMEOUT, "TIMEOUT_ERROR", msg.clone()),
        other => {
            // Surface unhandled DataflowError variants so a dataflow-rs upgrade
            // that adds new variants doesn't silently degrade them to a generic
            // 500. Add an explicit arm above if a variant deserves its own
            // status/code.
            tracing::error!(error = ?other, "unhandled DataflowError variant; mapped to 500");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "ENGINE_ERROR",
                "An internal engine error occurred".to_string(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_not_found_status() {
        let err = OrionError::NotFound("workflow xyz".to_string());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_bad_request_status() {
        let err = OrionError::BadRequest("invalid input".to_string());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_unauthorized_status() {
        let err = OrionError::Unauthorized("missing token".to_string());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_unauthorized_not_retryable() {
        assert!(!OrionError::Unauthorized("bad".to_string()).is_retryable());
    }

    #[test]
    fn test_conflict_status() {
        let err = OrionError::Conflict("duplicate".to_string());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn test_internal_status() {
        let err = OrionError::internal("something broke");
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_engine_validation_returns_400() {
        let err = OrionError::Engine(dataflow_rs::DataflowError::Validation(
            "bad input".to_string(),
        ));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_engine_timeout_returns_504() {
        let err = OrionError::Engine(dataflow_rs::DataflowError::Timeout("timed out".to_string()));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[test]
    fn test_config_error_status() {
        let err = OrionError::Config {
            message: "port must be > 0".to_string(),
        };
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// G11: a closed trace queue used to be `Queue`, a **500** that
    /// `is_retryable()` reported as `true` — a contradiction, and one the
    /// OpenAPI document never believed: it has always described queue-full and
    /// queue-closed together as `503`. The adjacent arm in `TraceQueue::submit`
    /// (queue *full*) was already `ServiceUnavailable`, so the two halves of one
    /// condition answered with different status codes.
    #[test]
    fn a_closed_queue_is_service_unavailable() {
        let err = OrionError::ServiceUnavailable("queue is closed".to_string());
        assert!(err.is_retryable());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn test_internal_source_status() {
        let source = std::io::Error::other("disk full");
        let err = OrionError::Internal {
            context: "Failed to write file".to_string(),
            source: Some(Box::new(source)),
        };
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_internal_source_preserves_chain() {
        let source = std::io::Error::other("connection reset");
        let err = OrionError::Internal {
            context: "Failed to connect to database".to_string(),
            source: Some(Box::new(source)),
        };
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn test_retryable_storage() {
        let err = OrionError::Storage(sqlx::Error::PoolTimedOut);
        assert!(err.is_retryable());
    }

    #[test]
    fn test_retryable_queue() {
        assert!(OrionError::ServiceUnavailable("closed".to_string()).is_retryable());
    }

    #[test]
    fn test_not_retryable_bad_request() {
        assert!(!OrionError::BadRequest("bad".to_string()).is_retryable());
    }

    #[test]
    fn test_not_retryable_config() {
        let err = OrionError::Config {
            message: "invalid".to_string(),
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_timeout_retryable() {
        let err = OrionError::Timeout {
            channel: "orders".to_string(),
            timeout_ms: 5000,
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn test_service_unavailable_retryable() {
        assert!(OrionError::ServiceUnavailable("queue full".to_string()).is_retryable());
    }

    #[test]
    fn test_rate_limited_status() {
        let err = OrionError::RateLimited("too many".to_string());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn test_rate_limited_retryable() {
        assert!(OrionError::RateLimited("too many".to_string()).is_retryable());
    }

    #[test]
    fn test_response_too_large_status() {
        let err = OrionError::ResponseTooLarge("10MB exceeded".to_string());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn test_response_too_large_not_retryable() {
        assert!(!OrionError::ResponseTooLarge("too big".to_string()).is_retryable());
    }

    #[test]
    fn test_serialization_error_status() {
        let serde_err: serde_json::Error =
            serde_json::from_str::<serde_json::Value>("invalid").expect_err("test");
        let err = OrionError::Serialization(serde_err);
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_serialization_not_retryable() {
        let serde_err: serde_json::Error =
            serde_json::from_str::<serde_json::Value>("invalid").expect_err("test");
        assert!(!OrionError::Serialization(serde_err).is_retryable());
    }

    #[test]
    fn test_not_found_not_retryable() {
        assert!(!OrionError::NotFound("x".to_string()).is_retryable());
    }

    #[test]
    fn test_conflict_not_retryable() {
        assert!(!OrionError::Conflict("dup".to_string()).is_retryable());
    }

    #[test]
    fn test_internal_not_retryable() {
        assert!(!OrionError::internal("err").is_retryable());
    }

    #[test]
    fn test_internal_source_not_retryable() {
        let err = OrionError::Internal {
            context: "ctx".to_string(),
            source: Some(Box::new(std::io::Error::other("err"))),
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_storage_error_status() {
        let err = OrionError::Storage(sqlx::Error::PoolTimedOut);
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_engine_generic_error_status() {
        let err = OrionError::Engine(dataflow_rs::DataflowError::Unknown(
            "unknown issue".to_string(),
        ));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_error_display_messages() {
        assert!(
            OrionError::NotFound("workflow".to_string())
                .to_string()
                .contains("workflow")
        );
        assert!(
            OrionError::BadRequest("bad".to_string())
                .to_string()
                .contains("bad")
        );
        assert!(
            OrionError::Conflict("dup".to_string())
                .to_string()
                .contains("dup")
        );
        assert!(
            OrionError::ServiceUnavailable("closed".to_string())
                .to_string()
                .contains("closed")
        );
        assert!(
            OrionError::RateLimited("limit".to_string())
                .to_string()
                .contains("limit")
        );
        assert!(
            OrionError::ResponseTooLarge("big".to_string())
                .to_string()
                .contains("big")
        );
    }

    /// The variant's name, as an exhaustive match with **no wildcard arm**.
    ///
    /// This exists for its compile error: adding a variant to [`OrionError`]
    /// stops this file building until the variant is named here, which is the
    /// prompt to give it a sample in [`wire_contract`] too.
    fn variant_name(err: &OrionError) -> &'static str {
        match err {
            OrionError::NotFound(_) => "NotFound",
            OrionError::BadRequest(_) => "BadRequest",
            OrionError::Validation { .. } => "Validation",
            OrionError::Unauthorized(_) => "Unauthorized",
            OrionError::Forbidden(_) => "Forbidden",
            OrionError::Conflict(_) => "Conflict",
            OrionError::Internal { .. } => "Internal",
            OrionError::Config { .. } => "Config",
            OrionError::RateLimited(_) => "RateLimited",
            OrionError::RateLimitKeyUnavailable(_) => "RateLimitKeyUnavailable",
            OrionError::ResponseTooLarge(_) => "ResponseTooLarge",
            OrionError::ServiceUnavailable(_) => "ServiceUnavailable",
            OrionError::Timeout { .. } => "Timeout",
            OrionError::UnsupportedMediaType(_) => "UnsupportedMediaType",
            OrionError::MethodNotAllowed(_) => "MethodNotAllowed",
            OrionError::Storage(_) => "Storage",
            OrionError::Engine(_) => "Engine",
            OrionError::Serialization(_) => "Serialization",
        }
    }

    /// The number of variants [`variant_name`] enumerates. Bumping this is the
    /// second half of the prompt: the compile error says "name the variant",
    /// this assertion says "and state its wire contract below".
    const VARIANT_COUNT: usize = 18;

    /// One sample per variant with the `(status, code)` it must answer with.
    ///
    /// The `code` is the machine-readable half of the documented
    /// `{"error": {"code", "message"}}` envelope — a client branches on it, so
    /// renaming one is a breaking API change. The tests around this one assert
    /// status and retryability but not `code`, which left most of the strings
    /// unpinned: `STORAGE_ERROR`, `CONFIG_ERROR`, `UNSUPPORTED_MEDIA_TYPE` and
    /// `RESPONSE_TOO_LARGE` could each be renamed with the whole suite green.
    fn wire_contract() -> Vec<(OrionError, StatusCode, &'static str)> {
        vec![
            (
                OrionError::NotFound("workflow xyz".into()),
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
            ),
            (
                OrionError::BadRequest("bad".into()),
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
            ),
            (
                OrionError::validation("invalid"),
                StatusCode::BAD_REQUEST,
                "VALIDATION_ERROR",
            ),
            (
                OrionError::Unauthorized("no key".into()),
                StatusCode::UNAUTHORIZED,
                "UNAUTHORIZED",
            ),
            (
                OrionError::Forbidden("nope".into()),
                StatusCode::FORBIDDEN,
                "FORBIDDEN",
            ),
            (
                OrionError::Conflict("dup".into()),
                StatusCode::CONFLICT,
                "CONFLICT",
            ),
            (
                OrionError::internal("boom"),
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
            ),
            (
                OrionError::Config {
                    message: "bad toml".into(),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
                "CONFIG_ERROR",
            ),
            (
                OrionError::RateLimited("slow down".into()),
                StatusCode::TOO_MANY_REQUESTS,
                "RATE_LIMITED",
            ),
            // Shares RATE_LIMITED with the variant above by design: a caller
            // that cannot be metered is told to back off the same way.
            (
                OrionError::RateLimitKeyUnavailable("key logic failed".into()),
                StatusCode::TOO_MANY_REQUESTS,
                "RATE_LIMITED",
            ),
            (
                OrionError::ResponseTooLarge("big".into()),
                StatusCode::BAD_GATEWAY,
                "RESPONSE_TOO_LARGE",
            ),
            (
                OrionError::ServiceUnavailable("closed".into()),
                StatusCode::SERVICE_UNAVAILABLE,
                "SERVICE_UNAVAILABLE",
            ),
            (
                OrionError::Timeout {
                    channel: "orders".into(),
                    timeout_ms: 500,
                },
                StatusCode::GATEWAY_TIMEOUT,
                "TIMEOUT",
            ),
            (
                OrionError::UnsupportedMediaType("text/plain".into()),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "UNSUPPORTED_MEDIA_TYPE",
            ),
            (
                OrionError::MethodNotAllowed("PUT".into()),
                StatusCode::METHOD_NOT_ALLOWED,
                "METHOD_NOT_ALLOWED",
            ),
            (
                OrionError::Storage(sqlx::Error::PoolTimedOut),
                StatusCode::INTERNAL_SERVER_ERROR,
                "STORAGE_ERROR",
            ),
            // `Engine` delegates to `engine_error_response`; the arms that
            // branch on the inner DataflowError are covered by the tests above.
            (
                OrionError::Engine(dataflow_rs::DataflowError::Unknown("x".into())),
                StatusCode::INTERNAL_SERVER_ERROR,
                "ENGINE_ERROR",
            ),
            (
                OrionError::Serialization(
                    serde_json::from_str::<i32>("not json").expect_err("test"),
                ),
                StatusCode::BAD_REQUEST,
                "SERIALIZATION_ERROR",
            ),
        ]
    }

    #[test]
    fn every_variant_states_its_status_and_code() {
        for (err, want_status, want_code) in wire_contract() {
            let name = variant_name(&err);
            let (status, code, _) = err.response_parts();
            assert_eq!(status, want_status, "{name} answers the wrong status");
            assert_eq!(code, want_code, "{name} answers the wrong error code");
        }
    }

    #[test]
    fn the_wire_contract_covers_every_variant() {
        let covered: std::collections::HashSet<&str> = wire_contract()
            .iter()
            .map(|(e, ..)| variant_name(e))
            .collect();
        assert_eq!(
            covered.len(),
            VARIANT_COUNT,
            "wire_contract() covers {} of {VARIANT_COUNT} variants; a variant with no sample \
             has its status and error code unpinned",
            covered.len()
        );
    }

    /// The `code` a client branches on must survive the trip through
    /// `IntoResponse`, not just `response_parts`.
    #[tokio::test]
    async fn the_code_reaches_the_response_body() {
        for (err, _, want_code) in wire_contract() {
            let name = variant_name(&err);
            let body = body_to_value(err.into_response()).await;
            assert_eq!(
                body["error"]["code"], want_code,
                "{name} did not put its code on the wire (body: {body})"
            );
        }
    }

    async fn body_to_value(response: Response) -> Value {
        let body_bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("test");
        serde_json::from_slice(&body_bytes).expect("test")
    }

    #[tokio::test]
    async fn test_validation_variant_status_is_400() {
        let err = OrionError::validation("invalid request");
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_validation_no_details_omits_details_key() {
        let err = OrionError::validation("invalid request");
        let response = err.into_response();
        let body = body_to_value(response).await;
        let error = &body["error"];
        assert_eq!(error["code"], "VALIDATION_ERROR");
        assert_eq!(error["message"], "invalid request");
        assert!(
            error.get("details").is_none(),
            "details must be omitted when empty (v0.1 compat)"
        );
    }

    #[tokio::test]
    async fn test_validation_with_field_emits_details_array() {
        let err = OrionError::invalid_field(
            "channel.protocol",
            "ENUM_MISMATCH",
            "unknown protocol 'REST'",
        );
        let response = err.into_response();
        let body = body_to_value(response).await;
        let details = &body["error"]["details"];
        assert!(details.is_array(), "details should be an array");
        let arr = details.as_array().expect("test");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["path"], "channel.protocol");
        assert_eq!(arr[0]["code"], "ENUM_MISMATCH");
        assert_eq!(arr[0]["message"], "unknown protocol 'REST'");
    }

    #[tokio::test]
    async fn test_invalid_field_one_shot_constructor() {
        let err = OrionError::invalid_field(
            "channel.route_pattern",
            "REQUIRED",
            "required when protocol=\"rest\"",
        );
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_to_value(response).await;
        let arr = body["error"]["details"].as_array().expect("test");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["path"], "channel.route_pattern");
        assert_eq!(arr[0]["code"], "REQUIRED");
    }

    #[tokio::test]
    async fn test_field_error_with_expected_and_got() {
        let err = OrionError::Validation {
            code: "VALIDATION_ERROR",
            message: "bad enum".to_string(),
            details: vec![
                FieldError::new("channel.protocol", "ENUM_MISMATCH", "unknown protocol")
                    .with_expected(serde_json::json!(["rest", "http", "kafka"]))
                    .with_got(Value::String("REST".to_string())),
            ],
        };
        let response = err.into_response();
        let body = body_to_value(response).await;
        let detail = &body["error"]["details"][0];
        assert_eq!(
            detail["expected"],
            serde_json::json!(["rest", "http", "kafka"])
        );
        assert_eq!(detail["got"], "REST");
    }

    #[tokio::test]
    async fn test_v01_envelope_unchanged_for_non_validation_errors() {
        // BadRequest must produce the v0.1 envelope: code+message, no details key.
        let err = OrionError::BadRequest("classic v0.1 message".to_string());
        let response = err.into_response();
        let body = body_to_value(response).await;
        let error = &body["error"];
        assert_eq!(error["code"], "BAD_REQUEST");
        assert_eq!(error["message"], "classic v0.1 message");
        assert!(error.get("details").is_none());
    }

    #[test]
    fn test_validation_not_retryable() {
        let err = OrionError::invalid_field("y", "REQUIRED", "z");
        assert!(!err.is_retryable());
    }

    // ---- F5: an open breaker surfaces as 503 CIRCUIT_OPEN end-to-end ----

    #[tokio::test]
    async fn test_engine_circuit_open_returns_503_with_code() {
        let err = OrionError::Engine(circuit_open_dataflow_error("orders-api", "orders"));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_to_value(response).await;
        assert_eq!(body["error"]["code"], "CIRCUIT_OPEN");
        let message = body["error"]["message"].as_str().expect("test");
        assert!(
            !message.contains("orion.circuit_open"),
            "no internal classification token may reach the client: {message}"
        );
        assert!(message.contains("orders-api") && message.contains("orders"));
    }

    /// N16: a target channel's guard refusal keeps its own status through the
    /// engine. Before, `channel_call` reported "the target is over its rate
    /// limit" as a `500 ENGINE_ERROR` — a server bug, for a condition the
    /// caller can retry and no code can act on.
    #[tokio::test]
    async fn test_channel_refusal_keeps_the_guards_status() {
        for (status, code) in [
            (StatusCode::TOO_MANY_REQUESTS, "RATE_LIMITED"),
            (StatusCode::SERVICE_UNAVAILABLE, "SERVICE_UNAVAILABLE"),
            (StatusCode::FORBIDDEN, "FORBIDDEN"),
            (StatusCode::CONFLICT, "CONFLICT"),
        ] {
            let err = OrionError::Engine(channel_refused_dataflow_error(
                status,
                "channel_call to 'billing': refused".to_string(),
            ));
            let response = err.into_response();
            assert_eq!(response.status(), status);
            let body = body_to_value(response).await;
            assert_eq!(body["error"]["code"], code, "{body}");
            let message = body["error"]["message"].as_str().expect("test");
            assert!(
                !message.contains("orion.channel_refused"),
                "no internal classification token may reach the client: {message}"
            );
            assert!(message.contains("channel_call to 'billing'"), "{message}");
        }
    }

    /// A plain `Http` error from a downstream call keeps the generic engine
    /// mapping — the `kind`, not the status, is what identifies an Orion-side
    /// channel refusal, and a relayed 429 carries none.
    #[tokio::test]
    async fn test_downstream_429_is_not_reported_as_a_channel_refusal() {
        let err = OrionError::Engine(dataflow_rs::DataflowError::http(429, "Too Many Requests"));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_engine_circuit_open_is_retryable() {
        let err = OrionError::Engine(circuit_open_dataflow_error("api", "orders"));
        assert!(
            err.is_retryable(),
            "DLQ retry must classify a shed dependency as retryable"
        );
    }

    #[tokio::test]
    async fn test_downstream_503_is_not_reported_as_circuit_open() {
        // A genuine 503 relayed by http_call keeps the generic engine mapping.
        let err = OrionError::Engine(dataflow_rs::DataflowError::http(503, "Service Unavailable"));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_to_value(response).await;
        assert_eq!(body["error"]["code"], "ENGINE_ERROR");
    }

    /// No classification token survives into any persisted text.
    ///
    /// The three classifications used to be message prefixes, and the async
    /// path never stripped them: `queue::processing` hands `e.to_string()`
    /// straight to `handle_failure`, so `orion.circuit_open: ` and friends were
    /// written verbatim into `traces.error` and `trace_dlq` rows. `kind` is a
    /// field now, so `Display` is the caller-safe sentence and nothing has to
    /// remember to strip anything.
    #[test]
    fn no_classification_token_reaches_persisted_error_text() {
        let errors = [
            circuit_open_dataflow_error("billing-api", "orders"),
            channel_refused_dataflow_error(
                StatusCode::TOO_MANY_REQUESTS,
                "channel_call to 'billing': over limit".to_string(),
            ),
            connector_detail_error("operation 'delete' is disabled on connector 'prod-billing'"),
        ];
        for e in errors {
            // This is exactly what the DLQ and the failed-trace row store.
            let persisted = e.to_string();
            assert!(
                !persisted.contains("orion."),
                "classification leaked into persisted text: {persisted}"
            );
            assert!(e.kind().is_some(), "each of these must carry a kind");
        }
    }

    /// The operator-only channel stays out of `Display`, so the topology a
    /// connector refusal names cannot reach an anonymous caller even though it
    /// is kept on the trace for whoever has to debug it (G3).
    #[tokio::test]
    async fn connector_detail_is_kept_off_the_wire_but_not_lost() {
        let secret = "operation 'delete' is disabled on connector 'prod-billing-db'";
        let e = connector_detail_error(secret);
        assert_eq!(e.detail(), Some(secret));
        assert!(!e.to_string().contains("prod-billing-db"));

        let response = OrionError::Engine(e).into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_to_value(response).await;
        assert_eq!(body["error"]["code"], "VALIDATION_ERROR");
        assert_eq!(body["error"]["message"], "Request validation failed");
        assert!(
            !body.to_string().contains("prod-billing-db"),
            "connector inventory must not reach the data plane: {body}"
        );
    }

    #[tokio::test]
    async fn test_request_id_embedded_when_scoped() {
        use crate::server::request_context::{REQUEST_CONTEXT, RequestContext};
        let ctx = RequestContext {
            request_id: "req-abc-123".to_string(),
            ..Default::default()
        };
        let response = REQUEST_CONTEXT
            .scope(ctx, async {
                OrionError::BadRequest("x".to_string()).into_response()
            })
            .await;
        let body = body_to_value(response).await;
        assert_eq!(body["error"]["request_id"], "req-abc-123");
    }

    #[tokio::test]
    async fn test_request_id_absent_when_empty() {
        use crate::server::request_context::{REQUEST_CONTEXT, RequestContext};
        let response = REQUEST_CONTEXT
            .scope(RequestContext::default(), async {
                OrionError::BadRequest("x".to_string()).into_response()
            })
            .await;
        let body = body_to_value(response).await;
        assert!(body["error"].get("request_id").is_none());
    }
}
