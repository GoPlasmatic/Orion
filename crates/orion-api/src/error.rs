//! The error envelope: `{"error": {code, message, details[], request_id}}`.
//!
//! Every non-2xx response the server sends carries this shape. The server's
//! internal error enum (`OrionError`) stays server-side — what is contract is
//! only what serializes, which is exactly what this module holds.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The stable machine-readable `error.code` registry.
///
/// A client branches on these, so renaming one is a breaking API change. The
/// server's wire-contract tests pin each variant's code with a string literal
/// on purpose — the literals there are the pin, these constants are what
/// production code (server mapping, client hints) is written against, and the
/// tests are what keep the two honest.
pub mod codes {
    pub const NOT_FOUND: &str = "NOT_FOUND";
    pub const VALIDATION_ERROR: &str = "VALIDATION_ERROR";
    pub const UNAUTHORIZED: &str = "UNAUTHORIZED";
    pub const FORBIDDEN: &str = "FORBIDDEN";
    pub const CONFLICT: &str = "CONFLICT";
    pub const UNSUPPORTED_MEDIA_TYPE: &str = "UNSUPPORTED_MEDIA_TYPE";
    pub const METHOD_NOT_ALLOWED: &str = "METHOD_NOT_ALLOWED";
    pub const SERVICE_UNAVAILABLE: &str = "SERVICE_UNAVAILABLE";
    pub const RATE_LIMITED: &str = "RATE_LIMITED";
    pub const TIMEOUT: &str = "TIMEOUT";
    pub const RESPONSE_TOO_LARGE: &str = "RESPONSE_TOO_LARGE";
    pub const INTERNAL_ERROR: &str = "INTERNAL_ERROR";
    pub const CONFIG_ERROR: &str = "CONFIG_ERROR";
    pub const STORAGE_ERROR: &str = "STORAGE_ERROR";
    pub const ENGINE_ERROR: &str = "ENGINE_ERROR";
    pub const SERIALIZATION_ERROR: &str = "SERIALIZATION_ERROR";
    /// An open circuit breaker shed the request before it reached a connector.
    pub const CIRCUIT_OPEN: &str = "CIRCUIT_OPEN";
}

/// Per-field validation detail returned in the `error.details[]` array.
///
/// `path` is a dotted/indexed pointer into the offending request body
/// (e.g. `channel.protocol`, `tasks[2].function.input.connector`).
/// `code` is a stable machine-readable identifier such as `REQUIRED`,
/// `ENUM_MISMATCH`, or `INVALID_FORMAT`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct FieldError {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub got: Option<Value>,
}

impl FieldError {
    /// `code` is `&'static str` on purpose: field codes are a closed, stable
    /// vocabulary, and a computed string here is a code no client can rely on.
    pub fn new(path: impl Into<String>, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            code: code.to_string(),
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

/// The object under the `error` key.
///
/// Serialization matches the server's v1.0 behaviour exactly: `details` is
/// omitted when empty (clients expecting only the v0.1 `{code, message}`
/// envelope still parse), `request_id` is omitted when the request had none.
/// Deserialization defaults every field so a pre-1.0 body still reads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ErrorBody {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<FieldError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// The full error response body: `{"error": {...}}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_details_and_request_id_are_omitted() {
        let env = ErrorEnvelope {
            error: ErrorBody {
                code: codes::NOT_FOUND.to_string(),
                message: "no such workflow".to_string(),
                details: Vec::new(),
                request_id: None,
            },
        };
        let value = serde_json::to_value(&env).expect("test");
        assert_eq!(value["error"]["code"], "NOT_FOUND");
        assert_eq!(value["error"]["message"], "no such workflow");
        assert!(value["error"].get("details").is_none());
        assert!(value["error"].get("request_id").is_none());
    }

    #[test]
    fn field_errors_serialize_with_optional_expected_got() {
        let f = FieldError::new("channel.protocol", "ENUM_MISMATCH", "unknown protocol")
            .with_expected(serde_json::json!(["rest", "http", "kafka"]))
            .with_got("REST");
        let value = serde_json::to_value(&f).expect("test");
        assert_eq!(value["path"], "channel.protocol");
        assert_eq!(value["code"], "ENUM_MISMATCH");
        assert_eq!(
            value["expected"],
            serde_json::json!(["rest", "http", "kafka"])
        );
        assert_eq!(value["got"], "REST");

        let bare = FieldError::new("x", "REQUIRED", "missing");
        let value = serde_json::to_value(&bare).expect("test");
        assert!(value.get("expected").is_none());
        assert!(value.get("got").is_none());
    }

    #[test]
    fn a_v01_body_still_parses() {
        // Pre-1.0 servers sent only {code, message}.
        let env: ErrorEnvelope =
            serde_json::from_str(r#"{"error": {"code": "CONFLICT", "message": "dup"}}"#)
                .expect("test");
        assert_eq!(env.error.code, codes::CONFLICT);
        assert_eq!(env.error.message, "dup");
        assert!(env.error.details.is_empty());
        assert!(env.error.request_id.is_none());
    }

    #[test]
    fn a_degenerate_body_parses_to_defaults() {
        let env: ErrorEnvelope = serde_json::from_str(r#"{"error": {}}"#).expect("test");
        assert!(env.error.code.is_empty());
        assert!(env.error.message.is_empty());
    }

    #[test]
    fn roundtrip_preserves_details_and_request_id() {
        let env = ErrorEnvelope {
            error: ErrorBody {
                code: codes::VALIDATION_ERROR.to_string(),
                message: "bad".to_string(),
                details: vec![FieldError::new("a.b", "REQUIRED", "missing")],
                request_id: Some("req-1".to_string()),
            },
        };
        let json = serde_json::to_string(&env).expect("test");
        let back: ErrorEnvelope = serde_json::from_str(&json).expect("test");
        assert_eq!(back.error.details.len(), 1);
        assert_eq!(back.error.details[0].path, "a.b");
        assert_eq!(back.error.request_id.as_deref(), Some("req-1"));
    }
}
