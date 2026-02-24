use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum OrionError {
    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Storage error: {0}")]
    Storage(#[from] sqlx::Error),

    #[error("Engine error: {0}")]
    Engine(#[from] dataflow_rs::DataflowError),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl IntoResponse for OrionError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            OrionError::NotFound(msg) => (StatusCode::NOT_FOUND, "NOT_FOUND", msg.clone()),
            OrionError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "BAD_REQUEST", msg.clone()),
            OrionError::Conflict(msg) => (StatusCode::CONFLICT, "CONFLICT", msg.clone()),
            OrionError::Internal(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                msg.clone(),
            ),
            OrionError::Storage(e) => {
                tracing::error!(error = %e, "Storage error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "STORAGE_ERROR",
                    "An internal storage error occurred".to_string(),
                )
            }
            OrionError::Engine(e) => {
                tracing::error!(error = %e, "Engine error");
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
        let err = OrionError::NotFound("rule xyz".to_string());
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
}
