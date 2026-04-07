use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum OrionError {
    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Bad request: {0}")]
    BadRequest(String),

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
}

impl IntoResponse for OrionError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            OrionError::NotFound(msg) => (StatusCode::NOT_FOUND, "NOT_FOUND", msg.clone()),
            OrionError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "BAD_REQUEST", msg.clone()),
            OrionError::Unauthorized(msg) => {
                (StatusCode::UNAUTHORIZED, "UNAUTHORIZED", msg.clone())
            }
            OrionError::Forbidden(msg) => (StatusCode::FORBIDDEN, "FORBIDDEN", msg.clone()),
            OrionError::Conflict(msg) => (StatusCode::CONFLICT, "CONFLICT", msg.clone()),
            OrionError::CircuitOpen { connector, channel } => (
                StatusCode::SERVICE_UNAVAILABLE,
                "CIRCUIT_OPEN",
                format!(
                    "Circuit breaker open for connector '{}' on channel '{}'",
                    connector, channel
                ),
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
                    "Workflow execution on channel '{}' exceeded {}ms timeout",
                    channel, timeout_ms
                ),
            ),
            OrionError::ResponseTooLarge(msg) => {
                (StatusCode::BAD_GATEWAY, "RESPONSE_TOO_LARGE", msg.clone())
            }
            OrionError::Internal(msg) => {
                tracing::error!(error.category = "internal", error.message = %msg, "Internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    msg.clone(),
                )
            }
            OrionError::Config { message } => {
                tracing::error!(error.category = "config", error.message = %message, "Config error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "CONFIG_ERROR",
                    message.clone(),
                )
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
                engine_error_response(e)
            }
            OrionError::Serialization(_) => (
                StatusCode::BAD_REQUEST,
                "SERIALIZATION_ERROR",
                self.to_string(),
            ),
        };

        let body = json!({
            "error": {
                "code": code,
                "message": message,
            }
        });

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
}
