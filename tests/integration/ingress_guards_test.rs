//! Ingress guard unification tests (proposal S1/F14/F4/G1):
//! every entry point enforces the target channel's declared contract.

use crate::common;
use crate::common::{body_json, json_request, post_with_idempotency_key};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

/// Channel config requiring `data.order_id` to be present.
fn require_order_id_config() -> serde_json::Value {
    json!({
        "validation_logic": { "!!": { "var": "data.order_id" } }
    })
}

// ============================================================
// S1: async submissions cannot bypass per-channel guards
// ============================================================

#[tokio::test]
async fn test_async_path_rejects_invalid_input_per_validation_logic() {
    let app = common::test_app().await;

    common::create_and_activate_channel_with_config(
        &app,
        "async-validated-ch",
        common::simple_log_workflow("Async Validated WF"),
        require_order_id_config(),
    )
    .await;

    // Regression for the `/async` bypass: invalid input must be rejected
    // with 400 before a trace is created, exactly like the sync path.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/async-validated-ch/async",
            Some(json!({"data": {"quantity": 5}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("validation failed")
    );

    // Valid input still gets accepted asynchronously.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/async-validated-ch/async",
            Some(json!({"data": {"order_id": "ORD-1"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_async_path_honors_deduplication() {
    let app = common::test_app().await;

    common::create_and_activate_channel_with_config(
        &app,
        "async-dedup-ch",
        common::simple_log_workflow("Async Dedup WF"),
        json!({
            "deduplication": { "header": "Idempotency-Key", "window_secs": 300 }
        }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v1/data/async-dedup-ch/async",
            "async-key-1",
            json!({"data": {"n": 1}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Same idempotency key within the window — must be rejected 409.
    let resp = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v1/data/async-dedup-ch/async",
            "async-key-1",
            json!({"data": {"n": 2}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // A different key is accepted.
    let resp = app
        .clone()
        .oneshot(post_with_idempotency_key(
            "/api/v1/data/async-dedup-ch/async",
            "async-key-2",
            json!({"data": {"n": 3}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

// ============================================================
// F14: channel_call enforces the target channel's guards
// ============================================================

/// A workflow whose single task channel_calls `target` with a fixed payload.
fn channel_call_workflow(name: &str, target: &str, data: serde_json::Value) -> serde_json::Value {
    json!({
        "name": name,
        "condition": true,
        "tasks": [{
            "id": "s1",
            "name": "Call target",
            "function": {
                "name": "channel_call",
                "input": {
                    "channel": target,
                    "data": data,
                    "response_path": "data.target_result"
                }
            }
        }]
    })
}

#[tokio::test]
async fn test_channel_call_enforces_target_validation_logic() {
    let app = common::test_app().await;

    common::create_and_activate_channel_with_config(
        &app,
        "guarded-target",
        common::simple_log_workflow("Guarded Target WF"),
        require_order_id_config(),
    )
    .await;

    // Caller that sends data violating the target's validation_logic.
    common::create_and_activate_channel(
        &app,
        "caller-invalid",
        channel_call_workflow(
            "Caller Invalid WF",
            "guarded-target",
            json!({"quantity": 1}),
        ),
    )
    .await;

    // Caller that satisfies the target's validation_logic.
    common::create_and_activate_channel(
        &app,
        "caller-valid",
        channel_call_workflow(
            "Caller Valid WF",
            "guarded-target",
            json!({"order_id": "ORD-9"}),
        ),
    )
    .await;

    // The nested validation failure propagates as DataflowError::Validation,
    // which the error envelope maps to 400.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/caller-invalid",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("validation"),
        "channel_call violating target validation_logic must surface a validation error, got: {body}"
    );

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/caller-valid",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(
        body["errors"].as_array().is_some_and(|e| e.is_empty()),
        "valid channel_call must pass target validation, got: {body}"
    );
}

// ============================================================
// F4: metadata.channel is stamped at every entry point
// ============================================================

#[tokio::test]
async fn test_metadata_channel_set_on_http_ingress() {
    let app = common::test_app().await;

    // validation_logic passes only when the ingress stamps metadata.channel
    // with the resolved channel name — a caller-supplied value must lose.
    common::create_and_activate_channel_with_config(
        &app,
        "f4-ch",
        common::simple_log_workflow("F4 WF"),
        json!({
            "validation_logic": { "==": [{ "var": "metadata.channel" }, "f4-ch"] }
        }),
    )
    .await;

    // Sync path — caller tries to spoof metadata.channel; the server value wins.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/f4-ch",
            Some(json!({"data": {}, "metadata": {"channel": "spoofed"}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Async path.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/f4-ch/async",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_metadata_channel_set_on_channel_call() {
    let app = common::test_app().await;

    // Target accepts only when metadata.channel equals its own name,
    // proving channel_call overrides the parent's channel on the child.
    common::create_and_activate_channel_with_config(
        &app,
        "f4-target",
        common::simple_log_workflow("F4 Target WF"),
        json!({
            "validation_logic": { "==": [{ "var": "metadata.channel" }, "f4-target"] }
        }),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "f4-caller",
        channel_call_workflow("F4 Caller WF", "f4-target", json!({"ok": true})),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/f4-caller",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(
        body["errors"].as_array().is_some_and(|e| e.is_empty()),
        "child message must carry metadata.channel == target, got: {body}"
    );
}

// ============================================================
// G1: data-plane responses carry sanitized errors only
// ============================================================

#[tokio::test]
async fn test_error_body_is_sanitized_but_trace_keeps_detail() {
    let app = common::test_app().await;

    // db_read against a nonexistent connector fails with a message naming
    // the connector — internal detail that must not reach the caller.
    let workflow = json!({
        "name": "G1 Failing WF",
        "condition": true,
        "continue_on_error": true,
        "tasks": [{
            "id": "t1",
            "name": "Failing DB read",
            "continue_on_error": true,
            "function": {
                "name": "db_read",
                "input": {
                    "connector": "ghost-db-g1",
                    "query": "SELECT 1",
                    "output": "data.db_result"
                }
            }
        }]
    });
    // R5 blocks activating against a missing connector — create then delete
    // it so the runtime failure path is still exercised.
    let conn_id = common::create_connector(&app, common::db_connector("ghost-db-g1")).await;
    common::create_and_activate_channel(&app, "g1-errors-ch", workflow).await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "DELETE",
            &format!("/api/v1/admin/connectors/{conn_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/g1-errors-ch",
            Some(json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;

    let errors = body["errors"].as_array().expect("errors array");
    assert!(!errors.is_empty(), "expected task errors, got: {body}");
    let body_str = serde_json::to_string(&body).unwrap();
    assert!(
        !body_str.contains("ghost-db-g1"),
        "connector name must not leak into the data-plane body: {body_str}"
    );
    assert!(errors[0]["code"].is_string(), "code is kept: {body}");
    assert!(
        body.get("request_id").is_some(),
        "sanitized body carries a correlation id: {body}"
    );

    // The persisted trace keeps the full error detail for operators.
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v1/admin/traces?channel=g1-errors-ch&limit=1",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let trace_id = list["data"][0]["id"].as_str().expect("trace id");

    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v1/admin/traces/{trace_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let trace = body_json(resp).await;
    let trace_str = serde_json::to_string(&trace).unwrap();
    assert!(
        trace_str.contains("ghost-db-g1"),
        "persisted trace must keep full error detail: {trace_str}"
    );
}

#[tokio::test]
async fn test_async_path_honors_origin_allow_list() {
    let app = common::test_app().await;

    common::create_and_activate_channel_with_config(
        &app,
        "async-cors-ch",
        common::simple_log_workflow("Async origin allow-list WF"),
        json!({
            "origin_allow_list": ["https://allowed.example"]
        }),
    )
    .await;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/data/async-cors-ch/async")
        .header("content-type", "application/json")
        .header("origin", "https://evil.example")
        .body(axum::body::Body::from(
            serde_json::to_string(&json!({"data": {}})).unwrap(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ============================================================
// N16: the guard matrix is uniform across transports
// ============================================================

/// Build a POST to `uri` with an `Origin` header.
fn post_with_origin(uri: &str, origin: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("origin", origin)
        .body(axum::body::Body::from(
            serde_json::to_string(&json!({"data": {}})).unwrap(),
        ))
        .unwrap()
}

/// Build a POST from a fixed client IP, so a per-channel rate limit keyed by
/// client identity buckets consistently across calls.
fn post_from_client(uri: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-forwarded-for", "203.0.113.7")
        .body(axum::body::Body::from(
            serde_json::to_string(&json!({"data": {}})).unwrap(),
        ))
        .unwrap()
}

/// S15/N16: a channel's own rate limit is the channel's contract, not a
/// feature of the HTTP middleware. It used to be enforced inside
/// `rate_limit_middleware`, which is installed only when `[rate_limit]
/// enabled = true` — so an operator who never turned the platform limiter on
/// (the default) got a channel `rate_limit` block that parsed, validated,
/// built a limiter, and throttled nothing.
#[tokio::test]
async fn test_channel_rate_limit_applies_without_the_platform_limiter() {
    // Default config: `rate_limit.enabled` is false.
    let app = common::test_app().await;

    common::create_and_activate_channel_with_config(
        &app,
        "n16-rl-sync",
        common::simple_log_workflow("N16 RL Sync WF"),
        json!({ "rate_limit": { "requests_per_second": 1, "burst": 1 } }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(post_from_client("/api/v1/data/n16-rl-sync"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(post_from_client("/api/v1/data/n16-rl-sync"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "the channel's own limit must apply with the platform limiter off"
    );
    // The 429 keeps the retry hint it had when the middleware owned it.
    assert_eq!(
        resp.headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("1")
    );
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "RATE_LIMITED", "{body}");
}

/// Attach a `ConnectInfo` peer address, as the serve layer does at runtime.
/// `tower::oneshot` supplies none, and the client-identity resolver falls
/// back to trusting forwarded headers when there is no peer — so a test
/// without this proves nothing about how a deployed instance keys its
/// buckets.
fn with_peer(
    mut req: axum::http::Request<axum::body::Body>,
    peer: &str,
) -> axum::http::Request<axum::body::Body> {
    let addr: std::net::SocketAddr = peer.parse().unwrap();
    req.extensions_mut()
        .insert(axum::extract::ConnectInfo(addr));
    req
}

/// Build a POST claiming to come from `forwarded_for`.
fn post_claiming_client(uri: &str, forwarded_for: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-forwarded-for", forwarded_for)
        .body(axum::body::Body::from(
            serde_json::to_string(&json!({"data": {}})).unwrap(),
        ))
        .unwrap()
}

/// S8/S15: the per-channel limit keys on the trusted-proxy-gated client
/// identity, and that trust list is parsed from `[rate_limit] trusted_proxies`
/// **whether or not** `rate_limit.enabled` is set.
///
/// It used to hang off `RateLimitState`, which is `None` with the platform
/// limiter off — precisely the configuration this channel-level limit exists
/// for. The list was therefore empty, forwarded headers were never honoured,
/// and behind a load balancer or ingress every client keyed on the proxy's
/// address: one bucket for the whole deployment, and a
/// `requests_per_second: 1` that admitted one request per second in total.
#[tokio::test]
async fn test_channel_rate_limit_keys_per_client_behind_a_trusted_proxy() {
    let mut config = orion::config::AppConfig::default();
    // The platform limiter stays off; only the trust list is configured.
    assert!(!config.rate_limit.enabled);
    config.rate_limit.trusted_proxies = vec!["10.0.0.0/8".to_string()];
    let app = common::test_app_with_config(config).await;

    common::create_and_activate_channel_with_config(
        &app,
        "n16-rl-proxied",
        common::simple_log_workflow("N16 RL Proxied WF"),
        json!({ "rate_limit": { "requests_per_second": 1, "burst": 1 } }),
    )
    .await;

    // Two clients arriving through the same trusted ingress. Each spends its
    // own token; if the trust list were empty they would share the proxy's.
    for client in ["203.0.113.7", "203.0.113.8"] {
        let resp = app
            .clone()
            .oneshot(with_peer(
                post_claiming_client("/api/v1/data/n16-rl-proxied", client),
                "10.0.0.7:5000",
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "client {client} must have its own bucket behind a trusted proxy"
        );
    }

    // ...and the first client's bucket really is spent.
    let resp = app
        .clone()
        .oneshot(with_peer(
            post_claiming_client("/api/v1/data/n16-rl-proxied", "203.0.113.7"),
            "10.0.0.7:5000",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

/// The other half of S8: an *untrusted* peer's forwarded header must not mint
/// a fresh bucket per request, or a channel's limit is bypassed by spoofing
/// one header.
#[tokio::test]
async fn test_channel_rate_limit_ignores_a_spoofed_forwarded_header() {
    // Default config: no trusted proxies, platform limiter off.
    let app = common::test_app().await;

    common::create_and_activate_channel_with_config(
        &app,
        "n16-rl-spoof",
        common::simple_log_workflow("N16 RL Spoof WF"),
        json!({ "rate_limit": { "requests_per_second": 1, "burst": 1 } }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(with_peer(
            post_claiming_client("/api/v1/data/n16-rl-spoof", "203.0.113.7"),
            "198.51.100.9:5000",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Same untrusted peer, a different claimed client. The peer is the
    // identity, so the bucket is the same one and is now empty.
    let resp = app
        .clone()
        .oneshot(with_peer(
            post_claiming_client("/api/v1/data/n16-rl-spoof", "203.0.113.99"),
            "198.51.100.9:5000",
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "an untrusted peer must not mint a new bucket by rewriting X-Forwarded-For"
    );
}

/// The same limit on the `/async` submission path: appending `/async` is not
/// a way around a channel's throughput contract.
#[tokio::test]
async fn test_channel_rate_limit_applies_to_async_submissions() {
    let app = common::test_app().await;

    common::create_and_activate_channel_with_config(
        &app,
        "n16-rl-async",
        common::simple_log_workflow("N16 RL Async WF"),
        json!({ "rate_limit": { "requests_per_second": 1, "burst": 1 } }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(post_from_client("/api/v1/data/n16-rl-async/async"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let resp = app
        .clone()
        .oneshot(post_from_client("/api/v1/data/n16-rl-async/async"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

/// N16: `channel_call` reached the target with its rate limit unenforced, so
/// a workflow fan-out could exceed a limit that the same channel enforced
/// strictly over HTTP. The target's budget is now spent by in-process calls
/// too.
#[tokio::test]
async fn test_channel_call_consumes_the_target_rate_limit() {
    let app = common::test_app().await;

    common::create_and_activate_channel_with_config(
        &app,
        "n16-rl-target",
        common::simple_log_workflow("N16 RL Target WF"),
        json!({ "rate_limit": { "requests_per_second": 1, "burst": 1 } }),
    )
    .await;
    common::create_and_activate_channel(
        &app,
        "n16-rl-caller",
        channel_call_workflow("N16 RL Caller WF", "n16-rl-target", json!({"n": 1})),
    )
    .await;

    // First in-process call spends the target's single token.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/n16-rl-caller",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(
        body["errors"].as_array().is_some_and(|e| e.is_empty()),
        "first call must pass the target's limit: {body}"
    );

    // The second is refused by the target, and the refusal surfaces as a
    // task failure on the calling workflow rather than being ignored.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/n16-rl-caller",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "a channel_call over the target's limit must surface the target's own \
         refusal, not a generic engine error: {body}"
    );
    assert_eq!(body["error"]["code"], "RATE_LIMITED", "{body}");
}

/// N16: the async worker used to apply `trace_queue.processing_timeout_ms`
/// unconditionally, so a channel declaring `timeout_ms = 50` timed out at
/// 50 ms over HTTP and ran for the global 60 s over `/async` — one channel,
/// two contracts. The workflow here takes 200 ms against a deliberately slow
/// upstream, so the channel's deadline is the only thing that can stop it.
#[tokio::test]
async fn test_async_worker_honours_the_channel_timeout() {
    let mock_app = axum::Router::new().route(
        "/slow",
        axum::routing::post(|| async {
            tokio::time::sleep(tokio::time::Duration::from_millis(400)).await;
            axum::Json(json!({"result": "done"}))
        }),
    );
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(mock_listener, mock_app).await.unwrap();
    });

    let app = common::test_app().await;
    common::create_connector(
        &app,
        json!({
            "id": "n16-slow-api",
            "name": "n16-slow-api",
            "connector_type": "http",
            "config": {
                "type": "http",
                "url": format!("http://{mock_addr}"),
                "retry": {"max_retries": 0, "retry_delay_ms": 10},
                "allow_private_urls": true
            }
        }),
    )
    .await;

    let workflow = json!({
        "name": "N16 Async Timeout WF",
        "condition": true,
        "tasks": [{
            "id": "slow-call",
            "name": "Slow HTTP Call",
            "function": {
                "name": "http_call",
                "input": {
                    "connector": "n16-slow-api",
                    "method": "POST",
                    "path": "/slow",
                    "body": {"test": true},
                    "output": "data.response",
                    "timeout_ms": 5000
                }
            }
        }]
    });
    common::create_and_activate_channel_with_config(
        &app,
        "n16-async-timeout",
        workflow,
        json!({ "timeout_ms": 50 }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/n16-async-timeout/async",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let trace_id = body["trace_id"].as_str().unwrap().to_string();
    let token = body["trace_token"].as_str().unwrap().to_string();

    let trace = common::poll_trace_until_done(&app, &trace_id, 60, Some(&token)).await;
    assert_eq!(
        trace["status"], "failed",
        "the channel's 50ms deadline must stop a 400ms workflow on the async path too: {trace}"
    );
    let detail = serde_json::to_string(&trace).unwrap();
    assert!(
        detail.contains("50ms"),
        "the failure must name the channel's timeout, not the global one: {detail}"
    );
}

// ============================================================
// N24: the per-channel origin allow-list, renamed for what it is
// ============================================================

/// The new key name enforces exactly what the old one did.
#[tokio::test]
async fn test_origin_allow_list_refuses_an_unlisted_origin() {
    let app = common::test_app().await;

    common::create_and_activate_channel_with_config(
        &app,
        "n24-origins",
        common::simple_log_workflow("N24 Origins WF"),
        json!({ "origin_allow_list": ["https://allowed.example"] }),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(post_with_origin(
            "/api/v1/data/n24-origins",
            "https://evil.example",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let resp = app
        .clone()
        .oneshot(post_with_origin(
            "/api/v1/data/n24-origins",
            "https://allowed.example",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // A request with no Origin at all is not a browser request and is not
    // checked.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/data/n24-origins",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// N24: the pre-1.0 `cors` spelling is refused at create, not accepted and
/// dropped.
///
/// Dropping it would leave the channel serving with no origin allow-list —
/// the same shape as a channel that deliberately checks nothing — so an
/// unlisted origin would be admitted with nothing to indicate the guard had
/// gone. A 400 naming the key is the only outcome that cannot be missed.
#[tokio::test]
async fn test_pre_1_0_cors_key_is_refused_at_create() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "n24-legacy-cors",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["POST"],
                "route_pattern": "/n24-legacy-cors",
                "workflow_id": "wf-does-not-matter",
                "config": { "cors": { "allowed_origins": ["https://allowed.example"] } }
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = common::body_json(resp).await;
    let message = body.to_string();
    assert!(
        message.contains("cors"),
        "the error must name the offending key so the fix is obvious: {message}"
    );
}

/// The general form of the guarantee above: an unrecognised key in a channel
/// config is a guard that would silently not run, so it is refused rather than
/// ignored.
#[tokio::test]
async fn test_misspelled_channel_config_key_is_refused_at_create() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/channels",
            Some(json!({
                "name": "typo-guard",
                "channel_type": "sync",
                "protocol": "rest",
                "methods": ["POST"],
                "route_pattern": "/typo-guard",
                "workflow_id": "wf-does-not-matter",
                "config": { "deduplicaton": { "header": "Idempotency-Key" } }
            })),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = common::body_json(resp).await;
    let message = body.to_string();
    assert!(
        message.contains("deduplicaton"),
        "the error must name the typo: {message}"
    );
}
