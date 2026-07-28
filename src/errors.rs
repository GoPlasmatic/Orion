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

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Configuration error: {message}")]
    Config { message: String },

    #[error("Rate limited: {0}")]
    RateLimited(String),

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

    #[error("Queue error: {0}")]
    Queue(String),

    #[error("{context}")]
    InternalSource {
        context: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

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
            OrionError::Queue(_) => true,
            OrionError::ServiceUnavailable(_) => true,
            OrionError::Timeout { .. } => true,
            _ => false,
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
    /// internal detail separately via [`OrionError::log_internal_detail`].
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
            OrionError::RateLimited(msg) => {
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
            OrionError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "An internal error occurred".to_string(),
            ),
            OrionError::Config { .. } => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "CONFIG_ERROR",
                "A configuration error occurred".to_string(),
            ),
            OrionError::Queue(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "QUEUE_ERROR",
                "An internal queue error occurred".to_string(),
            ),
            OrionError::InternalSource { .. } => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "An internal error occurred".to_string(),
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
            OrionError::Internal(msg) => {
                tracing::error!(error.category = "internal", error = %msg, "internal error")
            }
            OrionError::Config { message } => {
                tracing::error!(error.category = "config", error = %message, "config error")
            }
            OrionError::Queue(msg) => {
                tracing::error!(error.category = "queue", error = %msg, "queue error")
            }
            OrionError::Storage(e) => {
                tracing::error!(error.category = "storage", error = %e, "storage error")
            }
            OrionError::Serialization(e) => {
                tracing::error!(error.category = "serialization", error = %e, "serialization error")
            }
            OrionError::InternalSource { context, source } => tracing::error!(
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
        if let Ok(rid) = crate::server::request_context::REQUEST_ID.try_with(|id| id.clone())
            && !rid.is_empty()
        {
            error_obj.insert("request_id".to_string(), Value::String(rid));
        }

        let body = json!({ "error": Value::Object(error_obj) });

        (status, axum::Json(body)).into_response()
    }
}

/// Marker prefixed to the message of the `DataflowError` an open circuit
/// breaker returns.
///
/// `AsyncFunctionHandler` must return a `DataflowError`, and dataflow-rs 3.0's
/// enum is closed, `Serialize`, and has no extension point — so the breaker
/// rejection travels as `Http { status: 503 }`, the one variant whose
/// `retryable()` is already true *because* it is a 503, and which survives
/// serialization into `message.errors` / trace rows / DLQ payloads. The marker
/// is what separates an Orion breaker rejection from a genuine downstream 503
/// relayed by `http_call`; only the former becomes `CIRCUIT_OPEN`.
pub const CIRCUIT_OPEN_MARKER: &str = "orion.circuit_open: ";

/// Prefix marking a `DataflowError::Validation` message that names a connector
/// or other internal topology. Such messages are redacted before they reach a
/// caller (the data plane is anonymous) but are logged and stored on the trace
/// in full. Same mechanism as [`CIRCUIT_OPEN_MARKER`]; the prefix is stripped
/// from anything that does get surfaced.
pub const CONNECTOR_DETAIL_MARKER: &str = "orion.connector_detail: ";

/// Build the error an open breaker returns from a function handler.
pub fn circuit_open_dataflow_error(connector: &str, channel: &str) -> dataflow_rs::DataflowError {
    dataflow_rs::DataflowError::Http {
        status: 503,
        message: format!(
            "{CIRCUIT_OPEN_MARKER}Circuit breaker open for connector '{connector}' on channel '{channel}'"
        ),
    }
}

/// Map DataflowError variants to appropriate HTTP status codes and sanitized messages.
fn engine_error_response(e: &dataflow_rs::DataflowError) -> (StatusCode, &'static str, String) {
    use dataflow_rs::DataflowError;
    match e {
        // G3: validation messages that name a connector must not reach the
        // anonymous data plane — "operation 'delete' is disabled on connector
        // 'prod-billing-db'" hands out connector inventory for free. Producers
        // tag those with CONNECTOR_DETAIL_MARKER; the detail is logged and kept
        // on the trace, and the caller gets a generic message.
        //
        // Untagged validation messages are workflow-structural and safe —
        // "max call depth 10 exceeded", "'delete' has no filter" — and stay
        // verbatim, because they are what makes a misconfigured workflow
        // diagnosable from the response.
        DataflowError::Validation(msg) if msg.starts_with(CONNECTOR_DETAIL_MARKER) => (
            StatusCode::BAD_REQUEST,
            "VALIDATION_ERROR",
            "Request validation failed".to_string(),
        ),
        DataflowError::Validation(msg) => {
            (StatusCode::BAD_REQUEST, "VALIDATION_ERROR", msg.clone())
        }
        DataflowError::Timeout(msg) => (StatusCode::GATEWAY_TIMEOUT, "TIMEOUT_ERROR", msg.clone()),
        // Must precede the catch-all: a shed dependency is a 503 the client
        // can retry, not a 500 server bug.
        DataflowError::Http {
            status: 503,
            message,
        } if message.starts_with(CIRCUIT_OPEN_MARKER) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "CIRCUIT_OPEN",
            message
                .strip_prefix(CIRCUIT_OPEN_MARKER)
                .unwrap_or(message)
                .to_string(),
        ),
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
        let err = OrionError::Internal("something broke".to_string());
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

    #[test]
    fn test_queue_error_status() {
        let err = OrionError::Queue("queue is closed".to_string());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_internal_source_status() {
        let source = std::io::Error::other("disk full");
        let err = OrionError::InternalSource {
            context: "Failed to write file".to_string(),
            source: Box::new(source),
        };
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_internal_source_preserves_chain() {
        let source = std::io::Error::other("connection reset");
        let err = OrionError::InternalSource {
            context: "Failed to connect to database".to_string(),
            source: Box::new(source),
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
        assert!(OrionError::Queue("closed".to_string()).is_retryable());
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
        assert!(!OrionError::Internal("err".to_string()).is_retryable());
    }

    #[test]
    fn test_internal_source_not_retryable() {
        let err = OrionError::InternalSource {
            context: "ctx".to_string(),
            source: Box::new(std::io::Error::other("err")),
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
            OrionError::Queue("closed".to_string())
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
            !message.contains(CIRCUIT_OPEN_MARKER),
            "the internal marker must not reach the client: {message}"
        );
        assert!(message.contains("orders-api") && message.contains("orders"));
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

    #[tokio::test]
    async fn test_request_id_embedded_when_scoped() {
        use crate::server::request_context::REQUEST_ID;
        let response = REQUEST_ID
            .scope("req-abc-123".to_string(), async {
                OrionError::BadRequest("x".to_string()).into_response()
            })
            .await;
        let body = body_to_value(response).await;
        assert_eq!(body["error"]["request_id"], "req-abc-123");
    }

    #[tokio::test]
    async fn test_request_id_absent_when_empty() {
        use crate::server::request_context::REQUEST_ID;
        let response = REQUEST_ID
            .scope(String::new(), async {
                OrionError::BadRequest("x".to_string()).into_response()
            })
            .await;
        let body = body_to_value(response).await;
        assert!(body["error"].get("request_id").is_none());
    }
}
