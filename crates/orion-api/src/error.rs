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
    /// The request body exceeded `ingest.max_payload_size` (data plane) or
    /// `server.max_admin_body_size` (admin plane). Distinct from
    /// [`RESPONSE_TOO_LARGE`], which is about what the server built.
    pub const PAYLOAD_TOO_LARGE: &str = "PAYLOAD_TOO_LARGE";
    pub const RESPONSE_TOO_LARGE: &str = "RESPONSE_TOO_LARGE";
    pub const INTERNAL_ERROR: &str = "INTERNAL_ERROR";
    pub const CONFIG_ERROR: &str = "CONFIG_ERROR";
    pub const STORAGE_ERROR: &str = "STORAGE_ERROR";
    pub const ENGINE_ERROR: &str = "ENGINE_ERROR";
    pub const SERIALIZATION_ERROR: &str = "SERIALIZATION_ERROR";
    /// An open circuit breaker shed the request before it reached a connector.
    pub const CIRCUIT_OPEN: &str = "CIRCUIT_OPEN";
}

/// The closed vocabulary of [`FieldError::code`].
///
/// These are the codes the server actually emits — the registry equivalent of
/// [`codes`] one level down. Client code should branch on these constants
/// rather than on string literals, and a server change that needs a new code
/// adds it here first: `field_code_literals_are_all_registered` in
/// orion-server fails on any literal that is not one of these.
pub mod field_codes {
    /// A required field was absent.
    pub const REQUIRED: &str = "REQUIRED";
    /// The field is required for this protocol/type, though optional in general.
    pub const REQUIRED_FOR_PROTOCOL: &str = "REQUIRED_FOR_PROTOCOL";
    /// Present and well-typed, but not an acceptable value.
    pub const INVALID: &str = "INVALID";
    /// Present but the wrong JSON type.
    pub const TYPE_MISMATCH: &str = "TYPE_MISMATCH";
    /// Longer than the column or protocol allows.
    pub const TOO_LONG: &str = "TOO_LONG";
    /// A key the strict parser does not accept — usually a typo or a pre-1.0
    /// spelling.
    pub const UNKNOWN_FIELD: &str = "UNKNOWN_FIELD";
    /// The same key appeared twice in one object.
    pub const DUPLICATE_FIELD: &str = "DUPLICATE_FIELD";
    /// Two steps in one workflow declare the same `id`. Tasks and task
    /// groups share one id namespace, so a group may collide with a task.
    pub const DUPLICATE_TASK_ID: &str = "DUPLICATE_TASK_ID";
    /// A task names a function the engine does not register — the workflow
    /// would be accepted and then fail at its first request.
    pub const UNKNOWN_FUNCTION: &str = "UNKNOWN_FUNCTION";
    /// The document still carries an authoring convenience that is resolved
    /// when a definition set is compiled — a `$from` shared value, a `use`
    /// task fragment. The admin API takes one document and has no set to
    /// resolve it against, so the compiled form is what it accepts:
    /// `orion-server compile <dir>` produces it.
    pub const UNCOMPILED_SOURCE: &str = "UNCOMPILED_SOURCE";
    /// A secret reference (`env://NAME`, `vault://…`) sits in a field that
    /// does not resolve one. Only five fields do — `crypto.key`,
    /// `jwt_sign.key`, and `jwt_verify`'s `keys`, `issuer` and `audience` —
    /// and everywhere else the string is sent on as itself, so a URL spelled
    /// `env://API_BASE` is requested verbatim. Move the value to a connector,
    /// or declare it in the config file: a deployment value under `[vars]`,
    /// read as `{"var": "metadata.vars.<name>"}`, and key material under
    /// `[secrets]`, read as `{"secret": "<name>"}` in one of the five fields.
    pub const UNRESOLVED_SECRET_REF: &str = "UNRESOLVED_SECRET_REF";

    /// Every code above, for exhaustiveness checks.
    pub const ALL: &[&str] = &[
        REQUIRED,
        REQUIRED_FOR_PROTOCOL,
        INVALID,
        TYPE_MISMATCH,
        TOO_LONG,
        UNKNOWN_FIELD,
        DUPLICATE_FIELD,
        DUPLICATE_TASK_ID,
        UNKNOWN_FUNCTION,
        UNCOMPILED_SOURCE,
        UNRESOLVED_SECRET_REF,
    ];
}

/// Per-field validation detail returned in the `error.details[]` array.
///
/// `code` is a stable machine-readable identifier from [`field_codes`].
///
/// `path` is a pointer to the offending field, and is rooted one of two ways
/// depending on how far the request got:
///
/// - **Validation reached** — the path is resource-rooted and may be indexed:
///   `channel.protocol`, `tasks[2].function.input.connector`.
/// - **The body failed to deserialize** — validation never ran, so the layer
///   that reports it knows the field name but not which resource was being
///   parsed. The path is `body.<field>`, or bare `body` when even the field
///   cannot be recovered from the parser's message.
///
/// Match on the trailing segment rather than the whole path if you need to
/// treat both the same way.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema), schema(as = ErrorFieldDetail))]
pub struct FieldError {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub path: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub code: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
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
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema), schema(as = ErrorDetail))]
pub struct ErrorBody {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub code: String,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<FieldError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// The full error response body: `{"error": {...}}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema), schema(as = ErrorResponse))]
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
