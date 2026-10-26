//! The service errors Orion's functions raise — a circuit breaker shedding a
//! call, a `channel_call` its target's guards or recursion limits refused, a
//! connector refusal that names internal topology, a backend integrity
//! violation — each built with its kind, caller-facing message and
//! retryability. The kinds are `orion_common::errors::kind`, which the HTTP
//! mapping (`OrionError::Engine`) reads; building the errors is the engine's
//! business, not the HTTP error type's.

use axum::http::StatusCode;
use orion_common::errors::kind;

/// Which declared rule a backend refused a statement against.
///
/// Deliberately not the driver's own code: a per-backend SQLSTATE table in
/// Orion would duplicate the one sqlx already maintains, and would have to be
/// extended for every backend added later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrityKind {
    Unique,
    ForeignKey,
    NotNull,
    Check,
}

/// A guard refusal's kind, and whether a retry may clear it: the table
/// behind [`service::channel_refused`].
fn channel_refusal(status: StatusCode) -> (&'static str, bool) {
    let kind = match status.as_u16() {
        429 => kind::CHANNEL_RATE_LIMITED,
        403 => kind::CHANNEL_FORBIDDEN,
        409 => kind::CHANNEL_CONFLICT,
        _ => kind::CHANNEL_UNAVAILABLE,
    };
    // Retryable: the caller is over a limit or the target is at capacity.
    // Neither is a server fault and both clear on their own.
    (kind, !matches!(status.as_u16(), 403 | 409))
}

/// What an open breaker tells the caller.
fn circuit_open_message(connector: &str, channel: &str) -> String {
    format!("Circuit breaker open for connector '{connector}' on channel '{channel}'")
}

/// The caller-facing half of a [`kind::CONNECTOR_DETAIL`] refusal: the caller
/// gets this, and the text naming internal topology (G3) is logged and kept
/// on the trace.
const CONNECTOR_DETAIL_MESSAGE: &str = "Request validation failed";

impl IntegrityKind {
    /// The service kind a workflow branches on, and the generic sentence the
    /// caller gets in place of the driver's text: the table behind
    /// [`service::integrity`].
    fn kind_and_message(self) -> (&'static str, &'static str) {
        match self {
            IntegrityKind::Unique => (
                kind::INTEGRITY_UNIQUE,
                "The request conflicts with an existing record",
            ),
            IntegrityKind::ForeignKey => (
                kind::INTEGRITY_FOREIGN_KEY,
                "The request references a record that does not exist",
            ),
            IntegrityKind::NotNull => (kind::INTEGRITY_NOT_NULL, "A required value is missing"),
            IntegrityKind::Check => (
                kind::INTEGRITY_CHECK,
                "A value in the request is not allowed",
            ),
        }
    }
}

/// The four service-classified failures Orion's functions raise, each with
/// its kind, caller-facing message and retryability. An `Error` has no
/// `detail` field: the operator-only text rides as the error's
/// [`source`](std::error::Error::source), a
/// [`ServiceDetail`](service::ServiceDetail), which its JSON never carries.
mod builders {
    use super::{IntegrityKind, StatusCode, kind};
    use dataflow_rs::Error;

    /// The operator-only text of a service error: logged and kept for the
    /// operator, never shown to the caller. Carried as the error's source.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ServiceDetail(pub String);

    impl std::fmt::Display for ServiceDetail {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }

    impl std::error::Error for ServiceDetail {}

    /// `error` carrying `detail` as its operator-only text.
    pub fn with_detail(error: Error, detail: impl std::fmt::Display) -> Error {
        error.with_source(ServiceDetail(detail.to_string()))
    }

    /// The operator-only text `error` carries, if its source is one.
    pub fn detail_of(error: &Error) -> Option<&str> {
        std::error::Error::source(error)?
            .downcast_ref::<ServiceDetail>()
            .map(|detail| detail.0.as_str())
    }

    /// A `channel_call` the target channel's guards refused, keeping the
    /// refusal's meaning: over limit, forbidden, conflicting, or at capacity.
    pub fn channel_refused(status: StatusCode, message: String) -> Error {
        let (kind, retryable) = super::channel_refusal(status);
        Error::service(kind, message).with_retryable(retryable)
    }

    /// The `channel_call` recursion guards refused a call: a cycle or a chain
    /// past the maximum depth. Never retryable: the same channels loop again.
    pub fn channel_loop(message: String) -> Error {
        Error::service(kind::CHANNEL_LOOP, message).with_retryable(false)
    }

    /// An open breaker shed the request. Retryable: a shed dependency is
    /// transient, and the DLQ retry loop must re-attempt it. The connector
    /// and channel names stay in the caller-facing message, the shipped
    /// behaviour of `503 CIRCUIT_OPEN`.
    pub fn circuit_open(connector: &str, channel: &str) -> Error {
        Error::service(
            kind::CIRCUIT_OPEN,
            super::circuit_open_message(connector, channel),
        )
        .with_retryable(true)
    }

    /// A refusal whose text names internal topology (G3): the caller gets the
    /// generic message, and `detail` rides as the source.
    pub fn connector_detail(detail: impl std::fmt::Display) -> Error {
        with_detail(
            Error::service(kind::CONNECTOR_DETAIL, super::CONNECTOR_DETAIL_MESSAGE),
            detail,
        )
    }

    /// The backend refused a statement against a constraint its schema
    /// declares. The caller gets a generic sentence per kind and the driver's
    /// text rides as the source, since it names tables, columns and often the
    /// conflicting value. Never retryable — the conflicting row is still
    /// there next time — which also keeps it off a connector's breaker.
    pub fn integrity(integrity: IntegrityKind, detail: impl std::fmt::Display) -> Error {
        let (kind, message) = integrity.kind_and_message();
        with_detail(Error::service(kind, message), detail)
    }
}

pub use builders::*;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use orion_common::errors::OrionError;

    async fn body_to_value(response: axum::response::Response) -> serde_json::Value {
        let body_bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("test");
        serde_json::from_slice(&body_bytes).expect("test")
    }

    #[tokio::test]
    async fn test_engine_circuit_open_returns_503_with_code() {
        let err = OrionError::Engine(circuit_open("orders-api", "orders"));
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
            let err = OrionError::Engine(channel_refused(
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

    #[test]
    fn test_engine_circuit_open_is_retryable() {
        let err = OrionError::Engine(circuit_open("api", "orders"));
        assert!(
            err.is_retryable(),
            "DLQ retry must classify a shed dependency as retryable"
        );
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
            circuit_open("billing-api", "orders"),
            channel_refused(
                StatusCode::TOO_MANY_REQUESTS,
                "channel_call to 'billing': over limit".to_string(),
            ),
            connector_detail("operation 'delete' is disabled on connector 'prod-billing'"),
        ];
        for e in errors {
            // This is exactly what the DLQ and the failed-trace row store.
            let persisted = e.to_string();
            assert!(
                !persisted.contains("orion."),
                "classification leaked into persisted text: {persisted}"
            );
            assert!(
                matches!(e.kind(), dataflow_rs::ErrorKind::Service { .. }),
                "each of these must carry a kind"
            );
        }
    }

    /// The operator-only channel stays out of `Display`, so the topology a
    /// connector refusal names cannot reach an anonymous caller even though it
    /// is kept on the trace for whoever has to debug it (G3).
    #[tokio::test]
    async fn connector_detail_is_kept_off_the_wire_but_not_lost() {
        let secret = "operation 'delete' is disabled on connector 'prod-billing-db'";
        let e = connector_detail(secret);
        assert_eq!(detail_of(&e), Some(secret));
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
}

#[cfg(test)]
mod service_error_tests {
    use super::*;
    use orion_api::codes;
    use orion_common::errors::OrionError;

    /// What a client could see of an error: its recorded code, its
    /// caller-facing message, its retryability and its operator-only detail.
    fn seen(e: &dataflow_rs::Error) -> (String, String, bool, Option<String>) {
        (
            e.code().to_string(),
            e.message().to_string(),
            e.is_retryable(),
            detail_of(e).map(str::to_string),
        )
    }

    /// How `e` answers over HTTP: status, wire code and client-safe message.
    fn mapped(e: &dataflow_rs::Error) -> (StatusCode, &'static str, String) {
        let error = OrionError::Engine(e.clone());
        let (status, code, message) = error.response_parts();
        (status, code, message.into_owned())
    }

    fn facts(
        code: &str,
        message: &str,
        retryable: bool,
        detail: Option<&str>,
    ) -> (String, String, bool, Option<String>) {
        (
            code.to_string(),
            message.to_string(),
            retryable,
            detail.map(str::to_string),
        )
    }

    /// Each service error records 1.12's code, message, retryability and
    /// detail, and answers with the status and wire code 1.12 gave it.
    #[test]
    fn each_service_error_is_what_1_12_recorded() {
        for (status, code, retryable, wire) in [
            (
                StatusCode::TOO_MANY_REQUESTS,
                kind::CHANNEL_RATE_LIMITED,
                true,
                codes::RATE_LIMITED,
            ),
            (
                StatusCode::FORBIDDEN,
                kind::CHANNEL_FORBIDDEN,
                false,
                codes::FORBIDDEN,
            ),
            (
                StatusCode::CONFLICT,
                kind::CHANNEL_CONFLICT,
                false,
                codes::CONFLICT,
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                kind::CHANNEL_UNAVAILABLE,
                true,
                codes::SERVICE_UNAVAILABLE,
            ),
            (
                StatusCode::BAD_GATEWAY,
                kind::CHANNEL_UNAVAILABLE,
                true,
                codes::SERVICE_UNAVAILABLE,
            ),
        ] {
            let e = channel_refused(status, "refused".into());
            assert_eq!(
                seen(&e),
                facts(code, "refused", retryable, None),
                "{status}"
            );
            let (answered, wire_code, message) = mapped(&e);
            let expected = if status == StatusCode::BAD_GATEWAY {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                status
            };
            assert_eq!(
                (answered, wire_code, message.as_str()),
                (expected, wire, "refused")
            );
        }

        let e = circuit_open("billing", "orders");
        let message = "Circuit breaker open for connector 'billing' on channel 'orders'";
        assert_eq!(seen(&e), facts(kind::CIRCUIT_OPEN, message, true, None));
        assert_eq!(
            mapped(&e),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                codes::CIRCUIT_OPEN,
                message.to_string()
            )
        );

        let e = connector_detail("operation 'read' is disabled");
        assert_eq!(
            seen(&e),
            facts(
                kind::CONNECTOR_DETAIL,
                "Request validation failed",
                false,
                Some("operation 'read' is disabled")
            )
        );
        assert_eq!(
            mapped(&e),
            (
                StatusCode::BAD_REQUEST,
                codes::VALIDATION_ERROR,
                "Request validation failed".to_string()
            )
        );

        for (integrity, code, message, status, wire) in [
            (
                IntegrityKind::Unique,
                kind::INTEGRITY_UNIQUE,
                "The request conflicts with an existing record",
                StatusCode::CONFLICT,
                codes::CONFLICT,
            ),
            (
                IntegrityKind::ForeignKey,
                kind::INTEGRITY_FOREIGN_KEY,
                "The request references a record that does not exist",
                StatusCode::CONFLICT,
                codes::CONFLICT,
            ),
            (
                IntegrityKind::NotNull,
                kind::INTEGRITY_NOT_NULL,
                "A required value is missing",
                StatusCode::BAD_REQUEST,
                codes::VALIDATION_ERROR,
            ),
            (
                IntegrityKind::Check,
                kind::INTEGRITY_CHECK,
                "A value in the request is not allowed",
                StatusCode::BAD_REQUEST,
                codes::VALIDATION_ERROR,
            ),
        ] {
            let e = builders::integrity(integrity, "UNIQUE constraint failed");
            assert_eq!(
                seen(&e),
                facts(code, message, false, Some("UNIQUE constraint failed")),
                "{integrity:?}"
            );
            assert_eq!(
                mapped(&e),
                (status, wire, message.to_string()),
                "{integrity:?}"
            );
        }
    }

    /// The operator-only text is the error's source and nothing a caller is
    /// sent: v4's `Error` serializes its code, message and retryability only.
    #[test]
    fn a_service_errors_detail_is_its_source_and_absent_from_its_json() {
        let e = connector_detail("connector 'prod-billing-db' is a cache");
        let source = std::error::Error::source(&e).expect("the detail is the source");
        assert_eq!(source.to_string(), "connector 'prod-billing-db' is a cache");
        assert_eq!(
            serde_json::to_value(&e).expect("serializes"),
            serde_json::json!({
                "code": "connector_detail",
                "message": "Request validation failed",
                "retryable": false
            })
        );
        assert!(!e.to_string().contains("prod-billing-db"));
        assert_eq!(detail_of(&dataflow_rs::Error::service("X", "m")), None);
    }
}
