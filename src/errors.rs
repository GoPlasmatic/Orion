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

    #[error("Circuit breaker open for connector '{connector}' on channel '{channel}'")]
    CircuitOpen { connector: String, channel: String },

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
            OrionError::CircuitOpen { .. } => true,
            OrionError::RateLimited(_) => true,
            OrionError::Queue(_) => true,
            _ => false,
        }
    }

    /// Construct a `Validation` error with no field details yet — use
    /// `.field(...)` or `.with_details(...)` to populate.
    pub fn validation(message: impl Into<String>) -> Self {
        OrionError::Validation {
            code: "VALIDATION_ERROR",
            message: message.into(),
            details: Vec::new(),
        }
    }

    /// Append a `FieldError` to a `Validation` variant. No-op on other variants.
    pub fn field(
        mut self,
        path: impl Into<String>,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        if let OrionError::Validation { details, .. } = &mut self {
            details.push(FieldError::new(path, code, message));
        }
        self
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

impl IntoResponse for OrionError {
    fn into_response(self) -> Response {
        // Pull the validation details out before consuming `self` in the match,
        // since the response body needs them as a separate JSON field.
        let validation_details = match &self {
            OrionError::Validation { details, .. } if !details.is_empty() => Some(details.clone()),
            _ => None,
        };

        let (status, code, message) = match self {
            OrionError::NotFound(msg) => (StatusCode::NOT_FOUND, "NOT_FOUND", msg),
            OrionError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "BAD_REQUEST", msg),
            OrionError::Validation { code, message, .. } => {
                (StatusCode::BAD_REQUEST, code, message)
            }
            OrionError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, "UNAUTHORIZED", msg),
            OrionError::Forbidden(msg) => (StatusCode::FORBIDDEN, "FORBIDDEN", msg),
            OrionError::Conflict(msg) => (StatusCode::CONFLICT, "CONFLICT", msg),
            OrionError::CircuitOpen { connector, channel } => (
                StatusCode::SERVICE_UNAVAILABLE,
                "CIRCUIT_OPEN",
                format!(
                    "Circuit breaker open for connector '{connector}' on channel '{channel}'"
                ),
            ),
            OrionError::UnsupportedMediaType(msg) => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "UNSUPPORTED_MEDIA_TYPE",
                msg,
            ),
            OrionError::ServiceUnavailable(msg) => {
                (StatusCode::SERVICE_UNAVAILABLE, "SERVICE_UNAVAILABLE", msg)
            }
            OrionError::RateLimited(msg) => (StatusCode::TOO_MANY_REQUESTS, "RATE_LIMITED", msg),
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
                (StatusCode::BAD_GATEWAY, "RESPONSE_TOO_LARGE", msg)
            }
            OrionError::Internal(msg) => {
                tracing::error!(error.category = "internal", error.message = %msg, "Internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", msg)
            }
            OrionError::Config { message } => {
                tracing::error!(error.category = "config", error.message = %message, "Config error");
                (StatusCode::INTERNAL_SERVER_ERROR, "CONFIG_ERROR", message)
            }
            OrionError::Queue(msg) => {
                tracing::error!(error.category = "queue", error.message = %msg, "Queue error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "QUEUE_ERROR",
                    "An internal queue error occurred".to_string(),
                )
            }
            OrionError::InternalSource { context, source } => {
                tracing::error!(
                    error.category = "internal",
                    error.context = %context,
                    error.source = %source,
                    "Internal error"
                );
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    "An internal error occurred".to_string(),
                )
            }
            OrionError::Storage(e) => {
                tracing::error!(error.category = "storage", error = %e, "Storage error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "STORAGE_ERROR",
                    "An internal storage error occurred".to_string(),
                )
            }
            OrionError::Engine(e) => {
                tracing::error!(error.category = "engine", error = %e, "Engine error");
                engine_error_response(&e)
            }
            OrionError::Serialization(e) => (
                StatusCode::BAD_REQUEST,
                "SERIALIZATION_ERROR",
                e.to_string(),
            ),
        };

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

/// Map DataflowError variants to appropriate HTTP status codes and sanitized messages.
fn engine_error_response(e: &dataflow_rs::DataflowError) -> (StatusCode, &'static str, String) {
    use dataflow_rs::DataflowError;
    match e {
        DataflowError::Validation(msg) => {
            (StatusCode::BAD_REQUEST, "VALIDATION_ERROR", msg.clone())
        }
        DataflowError::Timeout(msg) => (StatusCode::GATEWAY_TIMEOUT, "TIMEOUT_ERROR", msg.clone()),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "ENGINE_ERROR",
            "An internal engine error occurred".to_string(),
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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
    fn test_circuit_open_status() {
        let err = OrionError::CircuitOpen {
            connector: "api".to_string(),
            channel: "orders".to_string(),
        };
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn test_circuit_open_retryable() {
        let err = OrionError::CircuitOpen {
            connector: "api".to_string(),
            channel: "orders".to_string(),
        };
        assert!(err.is_retryable());
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
            serde_json::from_str::<serde_json::Value>("invalid").unwrap_err();
        let err = OrionError::Serialization(serde_err);
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_serialization_not_retryable() {
        let serde_err: serde_json::Error =
            serde_json::from_str::<serde_json::Value>("invalid").unwrap_err();
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
            .unwrap();
        serde_json::from_slice(&body_bytes).unwrap()
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
        let err = OrionError::validation("body invalid").field(
            "channel.protocol",
            "ENUM_MISMATCH",
            "unknown protocol 'REST'",
        );
        let response = err.into_response();
        let body = body_to_value(response).await;
        let details = &body["error"]["details"];
        assert!(details.is_array(), "details should be an array");
        let arr = details.as_array().unwrap();
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
        let arr = body["error"]["details"].as_array().unwrap();
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
        let err = OrionError::validation("x").field("y", "REQUIRED", "z");
        assert!(!err.is_retryable());
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
