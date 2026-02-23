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
        let (status, code) = match &self {
            OrionError::NotFound(_) => (StatusCode::NOT_FOUND, "NOT_FOUND"),
            OrionError::BadRequest(_) => (StatusCode::BAD_REQUEST, "BAD_REQUEST"),
            OrionError::Conflict(_) => (StatusCode::CONFLICT, "CONFLICT"),
            OrionError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR"),
            OrionError::Storage(_) => (StatusCode::INTERNAL_SERVER_ERROR, "STORAGE_ERROR"),
            OrionError::Engine(_) => (StatusCode::INTERNAL_SERVER_ERROR, "ENGINE_ERROR"),
            OrionError::Serialization(_) => (StatusCode::BAD_REQUEST, "SERIALIZATION_ERROR"),
        };

        let message = match &self {
            OrionError::Storage(e) => {
                tracing::error!(error = %e, "Storage error");
                "An internal storage error occurred".to_string()
            }
            _ => self.to_string(),
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
}
