use super::*;
use crate::functions::twin::{self, Failure, Seen};
use dataflow_rs::function::testing;
use dataflow_rs::program::Severity;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

fn config(input: Value) -> Result<HttpCallConfig, Vec<dataflow_rs::Issue>> {
    testing::config_with(&kit::test_exprs(), input)
}

/// The Rejected issues of a refused config, as `(path, code, message)`.
fn refused(input: Value) -> Vec<(String, IssueCode, String)> {
    config(input)
        .expect_err("the config is refused")
        .into_iter()
        .filter(|issue| issue.severity == Severity::Rejected)
        .map(|issue| (issue.path.to_string(), issue.code, issue.message))
        .collect()
}

// -- The config --

#[test]
fn every_field_kind_compiles() {
    let cfg = config(json!({
        "connector": "api",
        "method": "POST",
        "path": {"cat": ["/orders/", {"var": "data.id"}]},
        "headers": {"X-Id": {"var": "data.id"}, "X-Static": "v"},
        "body": {"var": "data.order"},
        "body_format": "form",
        "output": "data.response",
        "response_format": "text",
        "timeout_ms": 500
    }))
    .expect("compiles");
    assert_eq!(cfg.connector, "api");
    assert_eq!(cfg.method, "POST");
    assert!(cfg.path.as_ref().is_some_and(|p| p.constant().is_none()));
    assert_eq!(cfg.headers.len(), 2);
    assert!(cfg.headers["X-Static"].constant().is_some());
    let output = cfg.output.as_ref().and_then(Output::constant);
    assert_eq!(
        output.map(ToString::to_string).as_deref(),
        Some("data.response")
    );
    assert_eq!(
        cfg.timeout_ms.constant(),
        Some(&OwnedDataValue::from(&json!(500)))
    );
    assert!(cfg.primary_output().is_some());

    // 3.15's defaults: GET, 30 s, and no output, which writes nothing.
    let cfg = config(json!({"connector": "api"})).expect("compiles");
    assert_eq!(cfg.method, "GET");
    assert_eq!(
        cfg.timeout_ms.constant(),
        Some(&OwnedDataValue::from(&json!(30000)))
    );
    assert!(cfg.output.is_none() && cfg.primary_output().is_none());
    assert!(cfg.path.is_none() && cfg.body.is_none() && cfg.headers.is_empty());
}

/// The config loads what 3.15's `HttpCallConfig` deserialized and
/// refuses what it refused — a `null` where serde wanted a map or a
/// method, an unknown key, both spellings of an alias — but for the
/// named breaks, each refused here where 3.15 loaded it: a computed
/// `connector` (the shapes' note), an alias on its own (I186), and an
/// `output` that is not a path (an `Output`).
#[test]
fn the_load_verdict_is_3_15s_but_for_the_named_breaks() {
    let loads = [
        json!({"connector": "api"}),
        json!({"connector": "api", "method": "DELETE", "timeout_ms": null, "output": null}),
        json!({"connector": "api", "path": null, "body": null, "body_format": null,
               "response_format": null}),
        json!({"connector": "api", "headers": {"X": 1, "Y": null, "Z": {"var": "data.z"}}}),
        json!({"connector": "api", "body": {"cat": ["a"]}, "body_format": 7,
               "timeout_ms": "soon", "output": {"cat": ["data.", "x"]}}),
    ];
    let refused_alike = [
        json!({}),
        json!({"connector": "api", "headers": null}),
        json!({"connector": "api", "headers": [1]}),
        json!({"connector": "api", "method": "get"}),
        json!({"connector": "api", "method": null}),
        json!({"connector": "api", "unknown": 1}),
        json!({"connector": "api", "path": "/x", "path_logic": "/y"}),
        json!({"connector": "api", "output": "data.x", "response_path": "data.y"}),
    ];
    let breaks = [
        json!({"connector": 7}),
        json!({"connector": {"var": "data.api"}}),
        json!({"connector": "api", "path_logic": "/x"}),
        json!({"connector": "api", "body_logic": {"a": 1}}),
        json!({"connector": "api", "response_path": "data.r"}),
        json!({"connector": "api", "output": 7}),
    ];
    for input in loads {
        assert!(config(input.clone()).is_ok(), "{input}");
    }
    for input in refused_alike.into_iter().chain(breaks) {
        assert!(config(input.clone()).is_err(), "{input}");
    }
}

/// `connector` is a literal (the shapes' note): a computed one is refused
/// at load, and a task without one is Rejected (I183).
#[test]
fn the_connector_is_a_required_literal() {
    let issues = refused(json!({"connector": {"var": "data.api"}}));
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(
        (issues[0].0.as_str(), issues[0].1),
        ("connector", IssueCode::InputParse)
    );
    let issues = refused(json!({"method": "GET"}));
    assert_eq!(
        issues,
        [(
            "connector".to_string(),
            IssueCode::InputParse,
            "missing field `connector`".to_string()
        )]
    );
}

/// Break 2a: an unknown key is refused, 1.12's aliases among them: no
/// alias is read.
#[test]
fn an_unknown_key_is_refused_the_aliases_among_them() {
    for key in ["path_logic", "body_logic", "response_path", "retries"] {
        let issues = refused(json!({"connector": "api", key: "x"}));
        assert_eq!(issues.len(), 1, "{key}: {issues:?}");
        assert_eq!(issues[0].0, key);
        assert!(
            matches!(issues[0].1, IssueCode::UnknownKey | IssueCode::LikelyTypo),
            "{key}: {issues:?}"
        );
    }
}

/// The method vocabulary is 3.15's `HttpMethod`, idempotency included,
/// and what it refused is refused with its text.
#[test]
fn the_method_is_3_15s_with_its_refusal() {
    assert_eq!(METHODS, ["GET", "POST", "PUT", "PATCH", "DELETE"]);
    for (method, idempotent) in [
        ("GET", true),
        ("POST", false),
        ("PUT", true),
        ("PATCH", false),
        ("DELETE", true),
    ] {
        config(json!({"connector": "api", "method": method})).expect("compiles");
        assert_eq!(is_idempotent(method), idempotent, "{method}");
    }
    for method in ["get", "Post", "HEAD", "OPTIONS", ""] {
        let issues = refused(json!({"connector": "api", "method": method}));
        assert_eq!(
            issues,
            [(
                "method".to_string(),
                IssueCode::InputParse,
                format!(
                    "unknown variant `{method}`, expected one of \
                     `GET`, `POST`, `PUT`, `PATCH`, `DELETE`"
                )
            )],
            "{method:?}"
        );
    }
    // Not a string at all, which 3.15 refused too.
    assert_eq!(
        refused(json!({"connector": "api", "method": null}))[0].0,
        "method"
    );
}

// -- The call, against 1.12's records --

/// An HTTP upstream on a free loopback port.
async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}")
}

/// An upstream answering every request with what it received: the
/// method, the path and query, the `x-` and content-type headers, and the
/// body as text. `/slow` answers after two seconds, `/teapot` with a 418,
/// and `/plain` with a text body.
async fn echo() -> String {
    async fn answer(
        method: axum::http::Method,
        uri: axum::http::Uri,
        headers: axum::http::HeaderMap,
        body: String,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        match uri.path() {
            "/slow" => tokio::time::sleep(Duration::from_secs(2)).await,
            "/teapot" => {
                return (axum::http::StatusCode::IM_A_TEAPOT, "short and stout").into_response();
            }
            "/plain" => return "plain text".into_response(),
            _ => {}
        }
        let picked: serde_json::Map<String, Value> = headers
            .iter()
            .filter(|(k, _)| k.as_str().starts_with("x-") || k.as_str() == "content-type")
            .map(|(k, v)| (k.to_string(), json!(v.to_str().unwrap_or_default())))
            .collect();
        axum::Json(json!({
            "method": method.as_str(),
            "uri": uri.to_string(),
            "headers": picked,
            "body": body,
        }))
        .into_response()
    }
    serve(axum::Router::new().fallback(answer)).await
}

/// An HTTP connector to `url`, retrying twice a millisecond apart.
fn connector(url: &str) -> Value {
    json!({
        "type": "http",
        "url": url,
        "allow_private_urls": true,
        "retry": {"max_retries": 2, "retry_delay_ms": 1}
    })
}

fn function(registry: Arc<ConnectorRegistry>) -> HttpCallFunction {
    HttpCallFunction {
        registry,
        client: reqwest::Client::new(),
    }
}

/// One task over a fresh registry holding `connectors`, with the
/// upstream `url` a failure names spelled `<upstream>`, so the answer is
/// the same from one run's loopback port to the next.
async fn run(connectors: &[(&str, Value)], url: &str, input: Value, data: Value) -> Seen {
    let seen = twin::v4(
        NAME,
        function(twin::connectors(connectors).await),
        input,
        data,
    )
    .await;
    upstream_unnamed(seen, url)
}

/// Every field resolves as 3.15's `HttpCallConfig` resolved it: the path,
/// the headers and the body over each kind of value, the formats, a
/// computed output, a `null` read as unset, and the refusals of a bad
/// format or timeout, each with its code and text.
#[tokio::test]
async fn every_field_reads_as_3_15s_config_read_it() {
    let url = echo().await;
    let connectors = [("api", connector(&url))];
    let data = json!({"id": 7, "obj": {"k": [1, 2]}, "order": {"sku": "A1", "qty": 2}});
    let cases = [
        json!({"connector": "api", "output": "data.r"}),
        json!({"connector": "api", "path": {"cat": ["/orders/", {"var": "data.id"}]}, "output": "data.r"}),
        json!({"connector": "api", "path": {"var": "data.id"}, "output": "data.r"}),
        json!({"connector": "api", "path": {"var": "data.obj"}, "output": "data.r"}),
        json!({"connector": "api", "path": "/search?q=1", "output": "data.r"}),
        json!({"connector": "api", "path": null, "body": null, "body_format": null,
               "response_format": null, "output": "data.r"}),
        json!({"connector": "api", "headers": {"X-Id": {"var": "data.id"}, "X-Num": 7,
               "X-Null": null, "X-Obj": {"var": "data.obj"}, "X-Missing": {"var": "data.none"}},
               "output": "data.r"}),
        json!({"connector": "api", "method": "POST", "body": {"var": "data.order"},
               "output": "data.r"}),
        json!({"connector": "api", "method": "PUT", "body": {"var": "data.order"},
               "body_format": "form", "output": "data.r"}),
        json!({"connector": "api", "method": "PATCH", "body": "raw text",
               "body_format": {"cat": ["te", "xt"]}, "output": "data.r"}),
        json!({"connector": "api", "method": "DELETE", "body": [1, 2], "output": "data.r"}),
        json!({"connector": "api", "path": "/plain", "response_format": "text",
               "output": "data.r"}),
        json!({"connector": "api", "output": {"cat": ["data.", "computed"]}}),
        json!({"connector": "api", "body_format": "xml", "output": "data.r"}),
        json!({"connector": "api", "response_format": "yaml", "output": "data.r"}),
        json!({"connector": "api", "body": 7, "body_format": "text", "output": "data.r"}),
        json!({"connector": "api", "body": [1], "body_format": "form", "output": "data.r"}),
        json!({"connector": "api", "timeout_ms": "soon", "output": "data.r"}),
        json!({"connector": "api", "timeout_ms": -1, "output": "data.r"}),
        json!({"connector": "api", "timeout_ms": {"var": "data.none"}, "output": "data.r"}),
        json!({"connector": "api", "path": "/teapot", "output": "data.r"}),
    ];
    for input in cases {
        let v4 = run(&connectors, &url, input.clone(), data.clone()).await;
        twin::pinned(&v4, format!("every_field/{input}"));
    }
}

/// The connector lookups and the method gate refuse as 3.15's did.
#[tokio::test]
async fn the_connector_and_its_gate_refuse_as_3_15s_did() {
    let url = echo().await;
    let mut gated = connector(&url);
    gated["operations"] = json!({"methods": ["GET"]});
    let connectors = [
        ("api", gated),
        ("mail", json!({"type": "cache", "backend": "memory"})),
    ];
    for input in [
        json!({"connector": "missing"}),
        json!({"connector": "mail"}),
        json!({"connector": "api", "method": "POST"}),
        json!({"connector": "api", "method": "GET", "output": "data.r"}),
    ] {
        let v4 = run(&connectors, &url, input.clone(), json!({})).await;
        twin::pinned(&v4, format!("connector_and_gate/{input}"));
    }
}

/// `seen` with the upstream `url` a failure names spelled `<upstream>`,
/// for comparing answers from upstreams on different ports.
fn upstream_unnamed(seen: Seen, url: &str) -> Seen {
    seen.map_err(|failure| Failure {
        message: failure.message.replace(url, "<upstream>"),
        ..failure
    })
}

/// An upstream answering 503 `fails` times, then `{"ok": true}`; the
/// requests it has seen.
async fn flaky(fails: usize) -> (String, Arc<AtomicUsize>) {
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    let app = axum::Router::new().fallback(move || {
        let counter = Arc::clone(&counter);
        async move {
            use axum::response::IntoResponse;
            if counter.fetch_add(1, Ordering::SeqCst) < fails {
                (axum::http::StatusCode::SERVICE_UNAVAILABLE, "busy").into_response()
            } else {
                axum::Json(json!({"ok": true})).into_response()
            }
        }
    });
    (serve(app).await, seen)
}

/// The retry: an idempotent method is retried by the connector's policy,
/// a POST is not unless the connector opts in, and the attempts and the
/// answer are 1.12's.
#[tokio::test]
async fn the_retry_is_3_15s() {
    let cases = [
        // (method, retry_non_idempotent, failures, attempts expected)
        ("GET", false, 2, 3),
        ("PUT", false, 2, 3),
        ("GET", false, 5, 3),
        ("POST", false, 2, 1),
        ("POST", true, 2, 3),
    ];
    for (method, opt_in, fails, attempts) in cases {
        let input = json!({"connector": "api", "method": method, "output": "data.r"});
        let (url, seen) = flaky(fails).await;
        let mut conn = connector(&url);
        conn["retry_non_idempotent"] = json!(opt_in);
        let result = run(&[("api", conn)], &url, input, json!({})).await;
        twin::pinned(&result, format!("retry/{method}/{opt_in}/{fails}"));
        assert_eq!(
            seen.load(Ordering::SeqCst),
            attempts,
            "{method}, opt-in {opt_in}, {fails} failures"
        );
    }
}

/// A fake IdP issuing `tok-<n>`, and an API that refuses `tok-1` with a
/// 401 as a revoked token and answers any other; how many tokens the IdP
/// has issued.
async fn oauth_pair() -> (String, String, Arc<AtomicUsize>) {
    let issued = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&issued);
    let idp = axum::Router::new().route(
        "/token",
        axum::routing::post(move || {
            let counter = Arc::clone(&counter);
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                axum::Json(json!({
                    "access_token": format!("tok-{n}"),
                    "token_type": "Bearer",
                    "expires_in": 3600
                }))
            }
        }),
    );
    let api = axum::Router::new().fallback(|headers: axum::http::HeaderMap| async move {
        use axum::response::IntoResponse;
        let token = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if token == "Bearer tok-1" {
            (axum::http::StatusCode::UNAUTHORIZED, "revoked").into_response()
        } else {
            axum::Json(json!({"token": token})).into_response()
        }
    });
    (serve(idp).await, serve(api).await, issued)
}

/// An HTTP connector to `api` with managed OAuth2 against `idp`, its
/// token manager running.
async fn oauth_registry(api: &str, idp: &str) -> Arc<ConnectorRegistry> {
    let conn = json!({
        "type": "http",
        "url": api,
        "allow_private_urls": true,
        "retry": {"max_retries": 0, "retry_delay_ms": 1},
        "auth": {
            "type": "oauth2",
            "grant": "client_credentials",
            "token_url": format!("{idp}/token"),
            "client_id": "svc",
            "client_secret": "secret"
        }
    });
    let registry = twin::connectors(&[("partner", conn)]).await;
    registry
        .oauth()
        .init(crate::connector::oauth::OAuthRuntimeDeps {
            http_client: reqwest::Client::new(),
            repo: Arc::new(crate::connector::test_support::StubConnectorRepo::with(
                vec![],
            )),
            lease: None,
        });
    registry
}

/// The OAuth path (#268): the token is acquired and applied, a 401 drops
/// it, and the next call acquires a fresh one — the calls, tokens and
/// answers 1.12's handler made.
#[tokio::test]
async fn a_401_drops_the_managed_token_as_1_12_did() {
    let input = json!({"connector": "partner", "output": "data.r"});
    let (idp, api, issued) = oauth_pair().await;
    let registry = oauth_registry(&api, &idp).await;
    let mut calls = Vec::new();
    for i in 0..3 {
        let seen = twin::v4(
            NAME,
            function(Arc::clone(&registry)),
            input.clone(),
            json!({}),
        )
        .await;
        let seen = upstream_unnamed(seen, &api);
        twin::pinned(&seen, format!("oauth/{i}"));
        calls.push(seen);
    }
    let issued = issued.load(Ordering::SeqCst);
    assert_eq!(
        calls[0].as_ref().err().map(|f| f.code.as_str()),
        Some("HTTP_ERROR")
    );
    assert_eq!(
        calls[1].as_ref().ok().map(|d| &d["r"]["token"]),
        Some(&json!("Bearer tok-2"))
    );
    assert_eq!(
        issued, 2,
        "the 401 dropped the first token, the second was reused"
    );
}

/// With no `output` the response is discarded: nothing is written.
#[tokio::test]
async fn an_unset_output_writes_nothing() {
    let url = echo().await;
    let connectors = [("api", connector(&url))];
    let input = json!({"connector": "api", "path": "/orders"});
    let v4 = run(&connectors, &url, input.clone(), json!({"keep": 1})).await;
    assert_eq!(v4, Ok(json!({"keep": 1})));

    let called = testing::call(function(twin::connectors(&connectors).await), input)
        .exprs(kit::test_exprs())
        .name(NAME)
        .await;
    assert_eq!(called.outcome.expect("the request is sent"), Outcome::Ok);
    assert!(called.writes.is_empty(), "{:?}", called.writes);
}

/// `timeout_ms` bounds one attempt, and a timeout is 1.12's retryable
/// `TIMEOUT_ERROR`, with its text.
#[tokio::test]
async fn timeout_ms_bounds_an_attempt_as_1_12_did() {
    let url = echo().await;
    let mut conn = connector(&url);
    conn["retry"] = json!({"max_retries": 0, "retry_delay_ms": 1});
    let connectors = [("api", conn)];
    for timeout in [json!(100), json!({"+": [50, 50]})] {
        let input = json!({"connector": "api", "path": "/slow", "timeout_ms": timeout,
                           "output": "data.r"});
        let v4 = run(&connectors, &url, input.clone(), json!({})).await;
        twin::pinned(&v4, format!("timeout/{input}"));
        let failure: Failure = v4.expect_err("the attempt times out");
        assert_eq!(failure.code, "TIMEOUT_ERROR");
        assert!(
            failure.retryable && failure.message.contains("timed out"),
            "{failure:?}"
        );
    }
}

/// The run's deadline (`ctx.deadline()`) ends a call still waiting on its
/// upstream, as 1.12's channel timeout ended one: the call ends `TimedOut`, well before its
/// own `timeout_ms`, and
/// nothing is written.
#[tokio::test]
async fn the_runs_deadline_ends_a_waiting_call() {
    let url = echo().await;
    let registry = twin::connectors(&[("api", connector(&url))]).await;
    let started = std::time::Instant::now();
    let called = testing::call(
        function(registry),
        json!({"connector": "api", "path": "/slow", "timeout_ms": 5000, "output": "data.r"}),
    )
    .exprs(kit::test_exprs())
    .name(NAME)
    .deadline(dataflow_rs::run::Instant::now() + Duration::from_millis(150))
    .await;
    let error = called.outcome.expect_err("the deadline passes first");
    assert_eq!(*error.kind(), dataflow_rs::ErrorKind::TimedOut);
    assert!(called.writes.is_empty());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

/// A computed `output` is resolved before anything about the request is
/// read, and one that names no path is refused with the family's
/// `VALIDATION_ERROR` and nothing is sent: there is no path to write the
/// response at. An output that names a path is written there, as 1.12
/// wrote it.
#[tokio::test]
async fn a_computed_output_is_resolved_before_the_request() {
    let data = json!({"n": 3, "word": "result", "at": "data.r"});
    for output in [
        json!({"var": "data.n"}),
        json!({"var": "data.word"}),
        json!({"var": "data.none"}),
    ] {
        let input = json!({"connector": "api", "output": output});

        let (url, sent) = flaky(0).await;
        let registry = twin::connectors(&[("api", connector(&url))]).await;
        let v4 = twin::v4(NAME, function(registry), input.clone(), data.clone()).await;
        let failure = v4.expect_err("refused before the request");
        assert_eq!(failure.code, "VALIDATION_ERROR", "{output}");
        assert!(
            failure
                .message
                .starts_with("Validation error: 'output' must resolve"),
            "{failure:?}"
        );
        assert_eq!(sent.load(Ordering::SeqCst), 0, "{output}: nothing is sent");
    }

    let input = json!({"connector": "api", "output": {"var": "data.at"}});
    let (url, sent) = flaky(0).await;
    let seen = run(&[("api", connector(&url))], &url, input, data).await;
    twin::pinned(&seen, "computed_output/data.at");
    assert_eq!(sent.load(Ordering::SeqCst), 1);
    assert_eq!(
        seen.as_ref().ok().map(|d| &d["r"]),
        Some(&json!({"ok": true}))
    );
}
