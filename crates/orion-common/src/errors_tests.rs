use super::*;
use serde_json::Value;

#[test]
fn test_not_found_status() {
    let err = OrionError::not_found("workflow xyz".to_string());
    let response = err.into_response();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[test]
fn test_unauthorized_status() {
    let err = OrionError::unauthorized("missing token".to_string());
    let response = err.into_response();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn test_unauthorized_not_retryable() {
    assert!(!OrionError::unauthorized("bad".to_string()).is_retryable());
}

#[test]
fn test_conflict_status() {
    let err = OrionError::conflict("duplicate".to_string());
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
fn test_engine_budget_returns_400_with_the_message() {
    let err = OrionError::Engine(dataflow_rs::Error::budget(
        "crypto: Operation budget exceeded: 86 charged against 20",
    ));
    let (status, code, message) = err.response_parts();
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code, codes::VALIDATION_ERROR);
    assert!(message.contains("budget"), "{message}");
}

#[test]
fn test_config_error_status() {
    let err = OrionError::Config {
        message: "port must be > 0".to_string(),
    };
    let response = err.into_response();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

/// G11: a closed trace queue is a **503**, like a full one — the OpenAPI
/// document describes queue-full and queue-closed together as `503` — and is
/// not retryable, which is where the two differ: full is backpressure and
/// clears, closed means the dispatcher is gone and does not. `Unavailable` is
/// what lets one variant say both.
#[test]
fn a_closed_queue_is_service_unavailable_but_not_retryable() {
    let err = OrionError::unavailable(Unavailable::QueueClosed, "queue is closed");
    assert!(
        !err.is_retryable(),
        "a closed queue has no consumer — retrying cannot help"
    );
    let response = err.into_response();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        !response
            .headers()
            .contains_key(axum::http::header::RETRY_AFTER),
        "a 503 that will not clear must not invite a retry loop"
    );
}

/// The complement: backpressure says come back, and says when.
#[test]
fn an_at_capacity_refusal_carries_retry_after() {
    let err = OrionError::unavailable(Unavailable::AtCapacity, "channel is at capacity");
    assert!(err.is_retryable());
    let response = err.into_response();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("1"),
        "backpressure clears as fast as the work in front of it"
    );
}

/// A quarantined channel is the other non-transient cause, and the one a
/// client is most likely to hammer: it looks exactly like an outage.
#[test]
fn a_quarantined_channel_does_not_invite_a_retry() {
    let err = OrionError::unavailable(
        Unavailable::ChannelQuarantined,
        "Channel 'orders' failed to load and is not being served",
    );
    assert!(!err.is_retryable());
    let response = err.into_response();
    assert!(
        !response
            .headers()
            .contains_key(axum::http::header::RETRY_AFTER),
        "it clears when an operator fixes the definition, not on a timer"
    );
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
    assert!(OrionError::unavailable(Unavailable::AtCapacity, "queue is full").is_retryable());
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
    assert!(OrionError::unavailable(Unavailable::AtCapacity, "queue full").is_retryable());
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

/// G11: was 502 `BAD_GATEWAY`; no upstream is involved in an oversized
/// workflow result, so it is a 500 now.
#[test]
fn test_response_too_large_status() {
    let err = OrionError::ResponseTooLarge("10MB exceeded".to_string());
    let response = err.into_response();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
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
    // G14: server-side only (row decode, response serialize) — a 500,
    // so it lands in 5xx SLOs instead of masquerading as a client error.
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[test]
fn test_serialization_not_retryable() {
    let serde_err: serde_json::Error =
        serde_json::from_str::<serde_json::Value>("invalid").expect_err("test");
    assert!(!OrionError::Serialization(serde_err).is_retryable());
}

#[test]
fn test_not_found_not_retryable() {
    assert!(!OrionError::not_found("x".to_string()).is_retryable());
}

#[test]
fn test_conflict_not_retryable() {
    assert!(!OrionError::conflict("dup".to_string()).is_retryable());
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
    let err = OrionError::Engine(dataflow_rs::Error::service(
        "UNKNOWN_ERROR",
        "unknown issue",
    ));
    let (status, code, message) = err.response_parts();
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(code, codes::ENGINE_ERROR);
    assert_eq!(message, "An internal engine error occurred", "sanitised");
}

#[test]
fn test_error_display_messages() {
    assert!(
        OrionError::not_found("workflow".to_string())
            .to_string()
            .contains("workflow")
    );
    assert!(OrionError::validation("bad").to_string().contains("bad"));
    assert!(
        OrionError::conflict("dup".to_string())
            .to_string()
            .contains("dup")
    );
    assert!(
        OrionError::unavailable(Unavailable::AtCapacity, "closed")
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
        OrionError::Http { status, .. } => http_label(*status),
        OrionError::Validation { .. } => "Validation",
        OrionError::UnauthorizedToken { .. } => "UnauthorizedToken",
        OrionError::Internal { .. } => "Internal",
        OrionError::Config { .. } => "Config",
        OrionError::RateLimited(_) => "RateLimited",
        OrionError::RateLimitKeyUnavailable(_) => "RateLimitKeyUnavailable",
        OrionError::ResponseTooLarge(_) => "ResponseTooLarge",
        OrionError::ServiceUnavailable { .. } => "ServiceUnavailable",
        OrionError::Timeout { .. } => "Timeout",
        OrionError::Storage(_) => "Storage",
        OrionError::Engine(_) => "Engine",
        OrionError::Serialization(_) => "Serialization",
    }
}

/// The number of variants [`variant_name`] enumerates, counting each
/// [`OrionError::Http`] constructor as its own. Bumping this is the
/// second half of the prompt: the compile error says "name the variant",
/// this assertion says "and state its wire contract below".
const VARIANT_COUNT: usize = 20;

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
            OrionError::not_found("workflow xyz"),
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
        ),
        (
            OrionError::UnauthorizedToken {
                message: "Channel authentication failed".into(),
                challenge: BearerChallenge::InvalidToken(Some("token expired")),
            },
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
        ),
        (
            OrionError::validation("invalid"),
            StatusCode::BAD_REQUEST,
            "VALIDATION_ERROR",
        ),
        (
            OrionError::unauthorized("no key"),
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
        ),
        (
            OrionError::forbidden("nope"),
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
        ),
        (
            OrionError::conflict("dup"),
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
            OrionError::payload_too_large("body too big"),
            StatusCode::PAYLOAD_TOO_LARGE,
            "PAYLOAD_TOO_LARGE",
        ),
        (
            OrionError::ResponseTooLarge("big".into()),
            StatusCode::INTERNAL_SERVER_ERROR,
            "RESPONSE_TOO_LARGE",
        ),
        (
            OrionError::unavailable(Unavailable::AtCapacity, "closed"),
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
            OrionError::unsupported_media_type("text/plain"),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "UNSUPPORTED_MEDIA_TYPE",
        ),
        (
            OrionError::method_not_allowed("PUT"),
            StatusCode::METHOD_NOT_ALLOWED,
            "METHOD_NOT_ALLOWED",
        ),
        (
            OrionError::Storage(sqlx::Error::PoolTimedOut),
            StatusCode::INTERNAL_SERVER_ERROR,
            "STORAGE_ERROR",
        ),
        // `Engine` delegates to `engine_error_response`; the arms that
        // branch on the inner engine error are covered by the tests above.
        (
            OrionError::Engine(dataflow_rs::Error::service("UNKNOWN_ERROR", "x")),
            StatusCode::INTERNAL_SERVER_ERROR,
            "ENGINE_ERROR",
        ),
        (
            OrionError::Serialization(serde_json::from_str::<i32>("not json").expect_err("test")),
            StatusCode::INTERNAL_SERVER_ERROR,
            "SERIALIZATION_ERROR",
        ),
        (
            OrionError::upgrade_required("use /api/v2/admin/deployments"),
            StatusCode::UPGRADE_REQUIRED,
            "UPGRADE_REQUIRED",
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

/// RFC 6750 §3: no error code when no bearer credential was presented
/// (§3.1), `invalid_token` otherwise, and a description only for expiry.
#[test]
fn a_refused_token_answers_with_its_challenge() {
    for (challenge, want) in [
        (BearerChallenge::Bare, "Bearer"),
        (
            BearerChallenge::InvalidToken(None),
            "Bearer error=\"invalid_token\"",
        ),
        (
            BearerChallenge::InvalidToken(Some("token expired")),
            "Bearer error=\"invalid_token\", error_description=\"token expired\"",
        ),
    ] {
        let response = OrionError::UnauthorizedToken {
            message: "Channel authentication failed".into(),
            challenge,
        }
        .into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok()),
            Some(want)
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
                .with_expected(serde_json::json!(["rest", "http", "cron"]))
                .with_got(Value::String("REST".to_string())),
        ],
    };
    let response = err.into_response();
    let body = body_to_value(response).await;
    let detail = &body["error"]["details"][0];
    assert_eq!(
        detail["expected"],
        serde_json::json!(["rest", "http", "cron"])
    );
    assert_eq!(detail["got"], "REST");
}

#[tokio::test]
async fn test_v01_envelope_unchanged_for_non_validation_errors() {
    // A detail-free error must produce the v0.1 envelope: code+message,
    // no details key.
    let err = OrionError::not_found("classic v0.1 message".to_string());
    let response = err.into_response();
    let body = body_to_value(response).await;
    let error = &body["error"];
    assert_eq!(error["code"], "NOT_FOUND");
    assert_eq!(error["message"], "classic v0.1 message");
    assert!(error.get("details").is_none());
}

#[test]
fn test_validation_not_retryable() {
    let err = OrionError::invalid_field("y", "REQUIRED", "z");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn test_request_id_embedded_when_scoped() {
    use crate::request_context::{REQUEST_CONTEXT, RequestContext};
    let ctx = RequestContext {
        request_id: "req-abc-123".to_string(),
        ..Default::default()
    };
    let response = REQUEST_CONTEXT
        .scope(ctx, async { OrionError::validation("x").into_response() })
        .await;
    let body = body_to_value(response).await;
    assert_eq!(body["error"]["request_id"], "req-abc-123");
}

#[tokio::test]
async fn test_request_id_absent_when_empty() {
    use crate::request_context::{REQUEST_CONTEXT, RequestContext};
    let response = REQUEST_CONTEXT
        .scope(RequestContext::default(), async {
            OrionError::validation("x").into_response()
        })
        .await;
    let body = body_to_value(response).await;
    assert!(body["error"].get("request_id").is_none());
}
