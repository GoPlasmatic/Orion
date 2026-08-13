//! The client's typed error: what went wrong, kept structured.

use orion_api::ErrorBody;
use reqwest::StatusCode;

/// Why a request failed. The `Api` variant keeps the server's error envelope
/// structured so callers can branch on `status`/`code` instead of matching
/// prose that a server-side rewording would silently break.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The server could not be reached at all (connect, DNS, TLS, timeout).
    #[error("{source}")]
    Connect {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    /// A non-2xx response carrying the Orion error envelope.
    ///
    /// The Display form (`HTTP 404 NOT_FOUND: …`) is the one the server's
    /// `package` CLI has always printed — kept stable on purpose.
    #[error("HTTP {} {}: {}", .status.as_u16(), .error.code, .error.message)]
    Api {
        status: StatusCode,
        error: ErrorBody,
    },

    /// A non-2xx response without a parseable envelope (a proxy page, an
    /// empty body, a plain-text error).
    #[error("Request failed ({status})")]
    Http { status: StatusCode, body: String },

    /// A 2xx response whose body did not parse as the expected type.
    #[error("Failed to parse response JSON: {source}")]
    Decode {
        #[source]
        source: serde_json::Error,
    },

    /// The request was sent but reading the response failed mid-flight.
    #[error("{source}")]
    Transport {
        #[source]
        source: reqwest::Error,
    },
}

impl ClientError {
    /// The HTTP status, when the server answered at all.
    pub fn status(&self) -> Option<StatusCode> {
        match self {
            Self::Api { status, .. } | Self::Http { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Classify a non-2xx response body: the Orion envelope when it parses
    /// as one, the raw text otherwise. The one place this decision lives.
    pub(crate) fn from_response(status: StatusCode, body: &[u8]) -> Self {
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body)
            && value.get("error").is_some()
            && let Ok(envelope) = serde_json::from_value::<orion_api::ErrorEnvelope>(value)
        {
            return Self::Api {
                status,
                error: envelope.error,
            };
        }
        Self::Http {
            status,
            body: String::from_utf8_lossy(body).into_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_envelope_body_becomes_a_structured_api_error() {
        let err = ClientError::from_response(
            StatusCode::NOT_FOUND,
            br#"{"error": {"code": "NOT_FOUND", "message": "no such workflow"}}"#,
        );
        match &err {
            ClientError::Api { status, error } => {
                assert_eq!(*status, StatusCode::NOT_FOUND);
                assert_eq!(error.code, "NOT_FOUND");
            }
            other => panic!("expected Api, got {other:?}"),
        }
        // The package CLI's historical Display form.
        assert_eq!(err.to_string(), "HTTP 404 NOT_FOUND: no such workflow");
    }

    #[test]
    fn a_non_envelope_body_becomes_http() {
        let err = ClientError::from_response(StatusCode::BAD_GATEWAY, b"<html>proxy</html>");
        match &err {
            ClientError::Http { status, body } => {
                assert_eq!(*status, StatusCode::BAD_GATEWAY);
                assert_eq!(body, "<html>proxy</html>");
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn an_error_key_that_is_not_an_envelope_falls_back_to_http() {
        // `{"error": "boom"}` has the key but not the shape.
        let err = ClientError::from_response(StatusCode::BAD_REQUEST, br#"{"error": "boom"}"#);
        assert!(matches!(err, ClientError::Http { .. }));
    }

    #[test]
    fn details_and_request_id_survive() {
        let err = ClientError::from_response(
            StatusCode::BAD_REQUEST,
            br#"{"error": {"code": "VALIDATION_ERROR", "message": "bad",
                 "details": [{"path": "a.b", "code": "REQUIRED", "message": "missing"}],
                 "request_id": "req-1"}}"#,
        );
        let ClientError::Api { error, .. } = err else {
            panic!("expected Api");
        };
        assert_eq!(error.details.len(), 1);
        assert_eq!(error.details[0].path, "a.b");
        assert_eq!(error.request_id.as_deref(), Some("req-1"));
    }
}
