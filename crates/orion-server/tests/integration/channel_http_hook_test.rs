//! A hooked channel, end to end: an edition's driver that answers its
//! channels' data-plane requests itself (a GraphQL-like protocol here), behind
//! the request's guards, running the channel's workflow once per field.
//!
//! The driver is a stand-in for an Enterprise one. Its "document" is a plain
//! text body of comma-separated fields; it runs the workflow once per field
//! and answers a JSON array of what each run returned.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use orion_extension_api::{
    ChannelBinding, ChannelDriver, ChannelDriverCapabilities, ChannelProtocolFields,
    DispatchOutcome, DriverError, FieldError, HttpHookGuards, HttpHost, HttpRequest, HttpResponse,
};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common::{self, body_json, json_request};

const PROTOCOL: &str = "gql";
const JWT_SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// What the driver was handed, for the tests to read.
#[derive(Default)]
struct Seen {
    requests: Mutex<Vec<HttpRequest>>,
    bindings: Mutex<Vec<ChannelBinding>>,
}

struct GqlDriver(Arc<Seen>);

#[async_trait]
impl ChannelDriver for GqlDriver {
    fn protocol_id(&self) -> &str {
        PROTOCOL
    }

    fn capabilities(&self) -> ChannelDriverCapabilities {
        ChannelDriverCapabilities::none()
            .sync()
            .http_routes()
            .http_hook(HttpHookGuards::none().validation().deduplication())
    }

    fn validate_fields(&self, _fields: &ChannelProtocolFields<'_>) -> Vec<FieldError> {
        Vec::new()
    }

    async fn serve_http(
        &self,
        channel: &ChannelBinding,
        request: HttpRequest,
        host: Arc<dyn HttpHost>,
    ) -> Result<HttpResponse, DriverError> {
        self.0
            .requests
            .lock()
            .expect("seen lock")
            .push(request.clone());
        self.0
            .bindings
            .lock()
            .expect("seen lock")
            .push(channel.clone());
        let document = String::from_utf8(request.body.clone())
            .map_err(|_| DriverError::new("the document is not UTF-8"))?;
        let mut results = Vec::new();
        for field in document.split(',').map(str::trim).filter(|f| !f.is_empty()) {
            let mut trigger = serde_json::Map::new();
            trigger.insert("field".into(), json!(field));
            let outcome = host.run(json!({ "field": field }), trigger).await;
            results.push(match outcome {
                DispatchOutcome::Completed { output } => output,
                DispatchOutcome::Rejected { code, .. } => json!({ "rejected": code }),
                other => json!({ "other": format!("{other:?}") }),
            });
        }
        let body = json!({ "results": results, "principal": request.principal });
        Ok(
            HttpResponse::full(200, serde_json::to_vec(&body).expect("serializes"))
                .with_header("content-type", "application/json")
                .with_header("x-gql", "1"),
        )
    }
}

async fn node() -> (orion::runtime::state::AppState, axum::Router, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let mut config = orion::config::AppConfig::default();
    config.trace_storage.mode = orion::config::TraceStorageMode::Sync;
    let composition = orion::extensions::CompositionRoot::community()
        .with_channel_driver(Arc::new(GqlDriver(seen.clone())));
    let state = common::test_state_with_composition(config, composition).await;
    let app = orion::server::build_router(state.clone());
    (state, app, seen)
}

/// Echoes the field and what the run learns about the request.
fn field_workflow() -> Value {
    common::workflow_with_tasks(
        "Field Resolver",
        json!([{
            "id": "resolve",
            "name": "Resolve one field",
            "function": {
                "name": "map",
                "input": {
                    "mappings": [
                        { "path": "data.echo", "logic": { "var": "data.field" } },
                        { "path": "data.saw_type", "logic": { "var": "metadata.trigger.type" } },
                        { "path": "data.saw_field", "logic": { "var": "metadata.trigger.field" } },
                        { "path": "data.saw_request", "logic": { "var": "metadata.trigger.request_id" } },
                        { "path": "data.saw_sub", "logic": { "var": "metadata.auth.claims.sub" } }
                    ]
                }
            }
        }]),
    )
}

async fn deploy_graph(app: &axum::Router, config: Value) -> String {
    let workflow_id = common::create_and_activate_workflow(app, field_workflow()).await;
    common::deploy_channel(
        app,
        json!({
            "name": "graph",
            "channel_type": "sync",
            "protocol": PROTOCOL,
            "methods": ["POST"],
            "route_pattern": "/graph",
            "workflow_id": workflow_id,
            "config": config,
        }),
    )
    .await
}

fn text_request(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "text/plain")
        .header("authorization", "Bearer withheld")
        .header("x-custom", "kept")
        .body(Body::from(body.to_string()))
        .expect("request")
}

async fn send(app: &axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.expect("answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

#[tokio::test]
async fn a_hooked_channel_gets_its_raw_body() {
    let (_state, app, seen) = node().await;
    deploy_graph(&app, json!({})).await;

    let response = app
        .clone()
        .oneshot(text_request("/api/v2/data/graph?depth=2", "a, b"))
        .await
        .expect("answers");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-gql"], "1", "the driver's headers");
    let body = body_json(response).await;
    let results = body["results"].as_array().expect("results");
    assert_eq!(results.len(), 2, "{body}");
    assert_eq!(results[0]["echo"], "a");
    assert_eq!(results[1]["echo"], "b");
    assert_eq!(
        results[0]["saw_type"], PROTOCOL,
        "the host stamps the protocol"
    );
    assert_eq!(
        results[1]["saw_field"], "b",
        "the driver's trigger facts arrive"
    );
    assert_eq!(
        results[0]["saw_request"], results[1]["saw_request"],
        "one request id groups both runs"
    );

    let requests = seen.requests.lock().expect("seen lock");
    let request = &requests[0];
    assert_eq!(request.body, b"a, b", "the bytes as received, not JSON");
    assert_eq!(request.method, "POST");
    assert_eq!(request.query.get("depth").map(String::as_str), Some("2"));
    assert_eq!(request.header("x-custom"), Some("kept"));
    assert_eq!(
        request.header("authorization"),
        None,
        "credentials are withheld"
    );
    assert!(!request.request_id.is_empty());
    let bindings = seen.bindings.lock().expect("seen lock");
    assert_eq!(bindings[0].name(), "graph");
    assert!(
        bindings[0].fingerprint().starts_with("sha256:"),
        "the binding was built with the generation"
    );
}

#[tokio::test]
async fn an_unknown_channel_still_answers_415_then_400() {
    let (_state, app, _seen) = node().await;
    deploy_graph(&app, json!({})).await;

    // Unknown name, non-JSON body: the hook branch does not take it.
    let (status, _) = send(&app, text_request("/api/v2/data/nope", "a")).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    // Unknown name, malformed JSON: 400 before 404, as before.
    let (status, _) = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/api/v2/data/nope")
            .header("content-type", "application/json")
            .body(Body::from("{not json"))
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_hooked_channel_refuses_async() {
    let (_state, app, seen) = node().await;
    deploy_graph(&app, json!({})).await;
    let (status, body) = send(&app, text_request("/api/v2/data/graph/async", "a")).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");
    assert!(seen.requests.lock().expect("seen lock").is_empty());
}

#[tokio::test]
async fn auth_origin_and_rate_limit_refuse_before_the_driver() {
    let (_state, app, seen) = node().await;
    deploy_graph(
        &app,
        json!({
            "auth": { "mode": "api_key", "keys": ["s3cret-key-value-long-enough"] },
            "origin_allow_list": ["https://app.example"],
            "rate_limit": { "requests_per_second": 1, "burst": 1 }
        }),
    )
    .await;

    // No key.
    let (status, _) = send(&app, text_request("/api/v2/data/graph", "a")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(seen.requests.lock().expect("seen lock").is_empty());

    let keyed = |origin: &str| {
        Request::builder()
            .method("POST")
            .uri("/api/v2/data/graph")
            .header("content-type", "text/plain")
            .header("x-api-key", "s3cret-key-value-long-enough")
            .header("origin", origin)
            .body(Body::from("a"))
            .expect("request")
    };
    // A foreign origin, with the key.
    let mut statuses = Vec::new();
    for _ in 0..4 {
        statuses.push(send(&app, keyed("https://evil.example")).await.0);
    }
    assert!(
        statuses.contains(&StatusCode::TOO_MANY_REQUESTS),
        "the request's rate limit runs: {statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .all(|s| *s == StatusCode::FORBIDDEN || *s == StatusCode::TOO_MANY_REQUESTS),
        "{statuses:?}"
    );
    assert!(
        seen.requests.lock().expect("seen lock").is_empty(),
        "nothing refused reached the driver"
    );
}

#[tokio::test]
async fn validation_judges_what_the_driver_runs() {
    let (_state, app, _seen) = node().await;
    deploy_graph(
        &app,
        json!({ "validation_logic": { "!=": [{ "var": "data.field" }, "forbidden"] } }),
    )
    .await;
    let (status, body) = send(&app, text_request("/api/v2/data/graph", "ok,forbidden")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the request itself is not validated"
    );
    let results = body["results"].as_array().expect("results");
    assert_eq!(results[0]["echo"], "ok");
    assert!(
        results[1]["rejected"].is_string(),
        "the second run is refused by validation_logic: {body}"
    );
}

#[tokio::test]
async fn claims_reach_the_driver_and_every_run() {
    let (_state, app, _seen) = node().await;
    deploy_graph(
        &app,
        json!({ "auth": {
            "mode": "jwt",
            "algorithms": ["HS256"],
            "jwt_keys": [{ "algorithm": "HS256", "key": JWT_SECRET }]
        } }),
    )
    .await;
    let token = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &json!({ "sub": "user-42", "exp": chrono::Utc::now().timestamp() + 3600 }),
        &jsonwebtoken::EncodingKey::from_secret(JWT_SECRET.as_bytes()),
    )
    .expect("signs");
    let (status, body) = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/api/v2/data/graph")
            .header("content-type", "text/plain")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from("a,b"))
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["principal"]["sub"], "user-42", "{body}");
    for result in body["results"].as_array().expect("results") {
        assert_eq!(result["saw_sub"], "user-42", "{body}");
    }
}

#[tokio::test]
async fn one_trace_per_run_with_the_protocol_as_mode() {
    let (_state, app, _seen) = node().await;
    deploy_graph(&app, json!({})).await;
    let (status, _) = send(&app, text_request("/api/v2/data/graph", "a,b,c")).await;
    assert_eq!(status, StatusCode::OK);

    let response = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v2/admin/traces?channel=graph&limit=10",
            None,
        ))
        .await
        .expect("trace listing answers");
    assert_eq!(response.status(), StatusCode::OK);
    let page = body_json(response).await;
    let rows = page["data"].as_array().expect("a trace page");
    assert_eq!(rows.len(), 3, "one trace per run: {page}");
    for row in rows {
        assert_eq!(row["mode"], PROTOCOL, "{row}");
    }
}

#[tokio::test]
async fn a_dedup_key_is_claimed_once_per_request() {
    let (_state, app, seen) = node().await;
    deploy_graph(
        &app,
        json!({ "deduplication": { "header": "x-idempotency-key", "window_secs": 60 } }),
    )
    .await;
    let keyed = || {
        Request::builder()
            .method("POST")
            .uri("/api/v2/data/graph")
            .header("content-type", "text/plain")
            .header("x-idempotency-key", "k-1")
            .body(Body::from("a,b"))
            .expect("request")
    };
    let (first, body) = send(&app, keyed()).await;
    assert_eq!(
        first,
        StatusCode::OK,
        "two runs share the request's key: {body}"
    );
    let (replay, _) = send(&app, keyed()).await;
    assert_eq!(replay, StatusCode::CONFLICT);
    assert_eq!(seen.requests.lock().expect("seen lock").len(), 1);
}

#[tokio::test]
async fn channel_call_into_a_hooked_channel_is_refused() {
    let (_state, app, _seen) = node().await;
    deploy_graph(&app, json!({})).await;
    let caller = common::workflow_with_tasks(
        "Caller",
        json!([{
            "id": "call",
            "name": "Call the hooked channel",
            "function": {
                "name": "channel_call",
                "input": { "channel": "graph", "output": "data.graph_result" }
            }
        }]),
    );
    let caller_id = common::create_and_activate_workflow(&app, caller).await;
    common::deploy_channel(
        &app,
        json!({
            "name": "caller",
            "channel_type": "sync",
            "protocol": "rest",
            "methods": ["POST"],
            "route_pattern": "/caller",
            "workflow_id": caller_id,
        }),
    )
    .await;
    let (status, body) = send(
        &app,
        json_request("POST", "/api/v2/data/caller", Some(json!({ "data": {} }))),
    )
    .await;
    assert!(!status.is_success(), "{body}");
    // The caller's answer is redacted; its trace records why.
    let response = app
        .clone()
        .oneshot(json_request(
            "GET",
            "/api/v2/admin/traces?channel=caller&limit=5",
            None,
        ))
        .await
        .expect("trace listing answers");
    let page = body_json(response).await;
    let id = page["data"][0]["id"].as_str().expect("a trace").to_string();
    let response = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v2/admin/traces/{id}"),
            None,
        ))
        .await
        .expect("trace answers");
    let trace = body_json(response).await;
    assert!(
        trace.to_string().contains("served by its 'gql' driver"),
        "channel_call is refused: {trace}"
    );
}
