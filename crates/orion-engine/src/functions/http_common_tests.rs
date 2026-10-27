use super::*;

#[test]
fn test_build_url() {
    assert_eq!(
        build_url("https://api.example.com", Some("/users")),
        "https://api.example.com/users"
    );
    assert_eq!(
        build_url("https://api.example.com/", Some("/users")),
        "https://api.example.com/users"
    );
    assert_eq!(
        build_url("https://api.example.com", None),
        "https://api.example.com"
    );
}

#[test]
fn test_build_url_no_path() {
    assert_eq!(
        build_url("https://api.example.com", None),
        "https://api.example.com"
    );
}

#[test]
fn test_build_url_empty_path() {
    assert_eq!(
        build_url("https://api.example.com", Some("")),
        "https://api.example.com"
    );
}

#[test]
fn test_build_url_trims_slashes() {
    assert_eq!(
        build_url("https://api.example.com///", Some("///path")),
        "https://api.example.com/path"
    );
}

/// #277: a base URL carrying a query keeps the path out of the query value —
/// `https://h/api?a=1` + `/orders` must not produce
/// `https://h/api?a=1/orders`. The tests above use no base with a query.
#[test]
fn test_build_url_keeps_base_and_task_queries_apart() {
    assert_eq!(
        build_url("https://h/api?a=1", Some("/orders")),
        "https://h/api/orders?a=1"
    );
    assert_eq!(
        build_url("https://h/api?a=1", Some("/orders?b=2")),
        "https://h/api/orders?a=1&b=2",
        "base query first, task query appended"
    );
    assert_eq!(
        build_url("https://h/api?a=1", None),
        "https://h/api?a=1",
        "no path leaves the base untouched"
    );
    assert_eq!(
        build_url("https://h/api", Some("/orders?b=2")),
        "https://h/api/orders?b=2"
    );
    // A trailing `?` carries no parameters and must not produce one.
    assert_eq!(build_url("https://h/api?", Some("/x")), "https://h/api/x");
}

#[test]
fn test_apply_auth_bearer() {
    let client = reqwest::Client::new();
    let auth = AuthConfig::Bearer {
        token: "tok123".to_string(),
    };
    let req = apply_auth(client.get("http://localhost"), &auth);
    let built = req.build().expect("test");
    assert_eq!(
        built
            .headers()
            .get("authorization")
            .expect("test")
            .to_str()
            .expect("test"),
        "Bearer tok123"
    );
}

#[test]
fn test_apply_auth_api_key() {
    let client = reqwest::Client::new();
    let auth = AuthConfig::ApiKey {
        header: "x-api-key".to_string(),
        key: "secret123".to_string(),
    };
    let req = apply_auth(client.get("http://localhost"), &auth);
    let built = req.build().expect("test");
    assert_eq!(
        built
            .headers()
            .get("x-api-key")
            .expect("test")
            .to_str()
            .expect("test"),
        "secret123"
    );
}

#[tokio::test]
async fn test_execute_request_success() {
    // Start a mock server using axum
    let mock_app = axum::Router::new().route(
        "/test",
        axum::routing::get(|| async { axum::Json(serde_json::json!({"result": "success"})) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test");
    let addr = listener.local_addr().expect("test");
    tokio::spawn(async move {
        axum::serve(listener, mock_app).await.expect("test");
    });

    let client = reqwest::Client::new();
    let http_config = HttpConnectorConfig {
        retry_non_idempotent: false,
        url: format!("http://{}", addr),
        method: String::new(),
        headers: std::collections::HashMap::new(),
        query_params: Default::default(),
        auth: None,
        retry: crate::connector::RetryConfig::default(),
        max_response_size: 10 * 1024 * 1024,
        allow_private_urls: true, // Tests use localhost
        operations: Default::default(),
    };

    let result = execute_request(
        &client,
        &http_config,
        RequestSpec {
            auth: http_config.auth.as_ref(),
            method: &reqwest::Method::GET,
            url: &format!("http://{}/test", addr),
            task_headers: None,
            body: None,
            body_format: BodyFormat::default(),
            response_format: ResponseFormat::default(),
            timeout: std::time::Duration::from_secs(5),
        },
    )
    .await;

    assert!(result.is_ok());
    let val = result.expect("test");
    assert_eq!(val["result"], "success");
}

#[tokio::test]
async fn test_execute_request_with_headers_auth_and_body() {
    let mock_app = axum::Router::new().route(
        "/post-test",
        axum::routing::post(|| async { axum::Json(serde_json::json!({"received": true})) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test");
    let addr = listener.local_addr().expect("test");
    tokio::spawn(async move {
        axum::serve(listener, mock_app).await.expect("test");
    });

    let client = reqwest::Client::new();
    let mut headers = std::collections::HashMap::new();
    headers.insert("x-custom".to_string(), "custom-value".to_string());

    let http_config = HttpConnectorConfig {
        retry_non_idempotent: false,
        url: format!("http://{}", addr),
        method: String::new(),
        headers: std::collections::HashMap::from([(
            "x-connector-header".to_string(),
            "conn-val".to_string(),
        )]),
        query_params: Default::default(),
        auth: Some(AuthConfig::Bearer {
            token: "test-token".to_string(),
        }),
        retry: crate::connector::RetryConfig::default(),
        max_response_size: 10 * 1024 * 1024,
        allow_private_urls: true, // Tests use localhost
        operations: Default::default(),
    };

    let body = serde_json::json!({"data": "payload"});

    let result = execute_request(
        &client,
        &http_config,
        RequestSpec {
            auth: http_config.auth.as_ref(),
            method: &reqwest::Method::POST,
            url: &format!("http://{}/post-test", addr),
            task_headers: Some(&headers),
            body: Some(&body),
            body_format: BodyFormat::default(),
            response_format: ResponseFormat::default(),
            timeout: std::time::Duration::from_secs(5),
        },
    )
    .await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_execute_request_non_success_status() {
    let mock_app = axum::Router::new().route(
        "/error",
        axum::routing::get(|| async { (axum::http::StatusCode::BAD_REQUEST, "Bad Request") }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test");
    let addr = listener.local_addr().expect("test");
    tokio::spawn(async move {
        axum::serve(listener, mock_app).await.expect("test");
    });

    let client = reqwest::Client::new();
    let http_config = HttpConnectorConfig {
        retry_non_idempotent: false,
        url: format!("http://{}", addr),
        method: String::new(),
        headers: std::collections::HashMap::new(),
        query_params: Default::default(),
        auth: None,
        retry: crate::connector::RetryConfig::default(),
        max_response_size: 10 * 1024 * 1024,
        allow_private_urls: true, // Tests use localhost
        operations: Default::default(),
    };

    let result = execute_request(
        &client,
        &http_config,
        RequestSpec {
            auth: http_config.auth.as_ref(),
            method: &reqwest::Method::GET,
            url: &format!("http://{}/error", addr),
            task_headers: None,
            body: None,
            body_format: BodyFormat::default(),
            response_format: ResponseFormat::default(),
            timeout: std::time::Duration::from_secs(5),
        },
    )
    .await;

    assert!(result.is_err());
    let err = result.expect_err("test");
    assert!(err.to_string().contains("400"));
}

#[tokio::test]
async fn test_execute_request_non_json_response() {
    let mock_app = axum::Router::new().route(
        "/text",
        axum::routing::get(|| async { "plain text response" }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test");
    let addr = listener.local_addr().expect("test");
    tokio::spawn(async move {
        axum::serve(listener, mock_app).await.expect("test");
    });

    let client = reqwest::Client::new();
    let http_config = HttpConnectorConfig {
        retry_non_idempotent: false,
        url: format!("http://{}", addr),
        method: String::new(),
        headers: std::collections::HashMap::new(),
        query_params: Default::default(),
        auth: None,
        retry: crate::connector::RetryConfig::default(),
        max_response_size: 10 * 1024 * 1024,
        allow_private_urls: true, // Tests use localhost
        operations: Default::default(),
    };

    let result = execute_request(
        &client,
        &http_config,
        RequestSpec {
            auth: http_config.auth.as_ref(),
            method: &reqwest::Method::GET,
            url: &format!("http://{}/text", addr),
            task_headers: None,
            body: None,
            body_format: BodyFormat::default(),
            response_format: ResponseFormat::default(),
            timeout: std::time::Duration::from_secs(5),
        },
    )
    .await;

    // Should fail to parse as JSON
    assert!(result.is_err());
    assert!(result.expect_err("test").to_string().contains("parse"));
}

#[tokio::test]
async fn test_execute_request_response_too_large() {
    let mock_app = axum::Router::new().route(
        "/large",
        axum::routing::get(|| async { axum::Json(serde_json::json!({"data": "x".repeat(200)})) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test");
    let addr = listener.local_addr().expect("test");
    tokio::spawn(async move {
        axum::serve(listener, mock_app).await.expect("test");
    });

    let client = reqwest::Client::new();
    let http_config = HttpConnectorConfig {
        retry_non_idempotent: false,
        url: format!("http://{}", addr),
        method: String::new(),
        headers: std::collections::HashMap::new(),
        query_params: Default::default(),
        auth: None,
        retry: crate::connector::RetryConfig::default(),
        max_response_size: 10,    // Very small limit
        allow_private_urls: true, // Tests use localhost
        operations: Default::default(),
    };

    let result = execute_request(
        &client,
        &http_config,
        RequestSpec {
            auth: http_config.auth.as_ref(),
            method: &reqwest::Method::GET,
            url: &format!("http://{}/large", addr),
            task_headers: None,
            body: None,
            body_format: BodyFormat::default(),
            response_format: ResponseFormat::default(),
            timeout: std::time::Duration::from_secs(5),
        },
    )
    .await;

    assert!(result.is_err());
    assert!(result.expect_err("test").to_string().contains("exceed"));
}

#[tokio::test]
async fn test_execute_request_timeout() {
    let mock_app = axum::Router::new().route(
        "/slow",
        axum::routing::get(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            axum::Json(serde_json::json!({"slow": true}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test");
    let addr = listener.local_addr().expect("test");
    tokio::spawn(async move {
        axum::serve(listener, mock_app).await.expect("test");
    });

    let client = reqwest::Client::new();
    let http_config = HttpConnectorConfig {
        retry_non_idempotent: false,
        url: format!("http://{}", addr),
        method: String::new(),
        headers: std::collections::HashMap::new(),
        query_params: Default::default(),
        auth: None,
        retry: crate::connector::RetryConfig::default(),
        max_response_size: 10 * 1024 * 1024,
        allow_private_urls: true, // Tests use localhost
        operations: Default::default(),
    };

    let result = execute_request(
        &client,
        &http_config,
        RequestSpec {
            auth: http_config.auth.as_ref(),
            method: &reqwest::Method::GET,
            url: &format!("http://{}/slow", addr),
            task_headers: None,
            body: None,
            body_format: BodyFormat::default(),
            response_format: ResponseFormat::default(),
            timeout: std::time::Duration::from_millis(100), // Very short timeout,
        },
    )
    .await;

    assert!(result.is_err());
    assert!(result.expect_err("test").to_string().contains("timed out"));
}

#[tokio::test]
async fn test_execute_request_connection_refused() {
    let client = reqwest::Client::new();
    let http_config = HttpConnectorConfig {
        retry_non_idempotent: false,
        url: "http://127.0.0.1:1".to_string(),
        method: String::new(),
        headers: std::collections::HashMap::new(),
        query_params: Default::default(),
        auth: None,
        retry: crate::connector::RetryConfig::default(),
        max_response_size: 10 * 1024 * 1024,
        allow_private_urls: true, // Tests use localhost
        operations: Default::default(),
    };

    let result = execute_request(
        &client,
        &http_config,
        RequestSpec {
            auth: http_config.auth.as_ref(),
            method: &reqwest::Method::GET,
            url: "http://127.0.0.1:1/test",
            task_headers: None,
            body: None,
            body_format: BodyFormat::default(),
            response_format: ResponseFormat::default(),
            timeout: std::time::Duration::from_secs(1),
        },
    )
    .await;

    assert!(result.is_err());
    assert!(result.expect_err("test").to_string().contains("failed"));
}

/// Mirrors the production client's redirect policy (`bootstrap`): the
/// manual follower in `execute_request` only sees 3xx responses when
/// reqwest doesn't follow.
fn redirectless_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("test")
}

fn localhost_config(addr: std::net::SocketAddr) -> HttpConnectorConfig {
    HttpConnectorConfig {
        retry_non_idempotent: false,
        url: format!("http://{}", addr),
        method: String::new(),
        headers: std::collections::HashMap::new(),
        query_params: Default::default(),
        auth: None,
        retry: crate::connector::RetryConfig::default(),
        max_response_size: 10 * 1024 * 1024,
        allow_private_urls: true, // Tests use localhost
        operations: Default::default(),
    }
}

async fn spawn_mock(app: axum::Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test");
    let addr = listener.local_addr().expect("test");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("test");
    });
    addr
}

#[tokio::test]
async fn test_redirect_to_private_target_refused() {
    // allow_private_urls covers only the connector's own endpoint; a
    // redirect pointing anywhere else must still pass SSRF validation.
    let mock_app = axum::Router::new().route(
        "/redirect",
        axum::routing::get(|| async {
            (
                axum::http::StatusCode::FOUND,
                [(
                    axum::http::header::LOCATION,
                    "http://169.254.169.254/latest/meta-data",
                )],
            )
        }),
    );
    let addr = spawn_mock(mock_app).await;

    let result = execute_request(
        &redirectless_client(),
        &localhost_config(addr),
        RequestSpec {
            auth: None,
            method: &reqwest::Method::GET,
            url: &format!("http://{}/redirect", addr),
            task_headers: None,
            body: None,
            body_format: BodyFormat::default(),
            response_format: ResponseFormat::default(),
            timeout: std::time::Duration::from_secs(5),
        },
    )
    .await;

    let err = result.expect_err("test").to_string();
    assert!(err.contains("SSRF protection"), "unexpected error: {err}");
}

#[tokio::test]
async fn test_redirect_followed_within_own_endpoint() {
    let mock_app = axum::Router::new()
        .route(
            "/a",
            axum::routing::get(|| async {
                (
                    axum::http::StatusCode::FOUND,
                    [(axum::http::header::LOCATION, "/b")],
                )
            }),
        )
        .route(
            "/b",
            axum::routing::get(|| async { axum::Json(serde_json::json!({"hop": "b"})) }),
        );
    let addr = spawn_mock(mock_app).await;

    let result = execute_request(
        &redirectless_client(),
        &localhost_config(addr),
        RequestSpec {
            auth: None,
            method: &reqwest::Method::GET,
            url: &format!("http://{}/a", addr),
            task_headers: None,
            body: None,
            body_format: BodyFormat::default(),
            response_format: ResponseFormat::default(),
            timeout: std::time::Duration::from_secs(5),
        },
    )
    .await;

    assert_eq!(result.expect("test")["hop"], "b");
}

#[tokio::test]
async fn test_redirect_loop_is_capped() {
    let mock_app = axum::Router::new().route(
        "/loop",
        axum::routing::get(|| async {
            (
                axum::http::StatusCode::FOUND,
                [(axum::http::header::LOCATION, "/loop")],
            )
        }),
    );
    let addr = spawn_mock(mock_app).await;

    let result = execute_request(
        &redirectless_client(),
        &localhost_config(addr),
        RequestSpec {
            auth: None,
            method: &reqwest::Method::GET,
            url: &format!("http://{}/loop", addr),
            task_headers: None,
            body: None,
            body_format: BodyFormat::default(),
            response_format: ResponseFormat::default(),
            timeout: std::time::Duration::from_secs(5),
        },
    )
    .await;

    let err = result.expect_err("test").to_string();
    assert!(err.contains("redirects"), "unexpected error: {err}");
}

#[tokio::test]
async fn test_redirect_303_downgrades_post_to_get() {
    let mock_app = axum::Router::new()
        .route(
            "/submit",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::SEE_OTHER,
                    [(axum::http::header::LOCATION, "/done")],
                )
            }),
        )
        .route(
            "/done",
            // GET-only: the hop only succeeds if the method was downgraded
            axum::routing::get(|| async { axum::Json(serde_json::json!({"done": true})) }),
        );
    let addr = spawn_mock(mock_app).await;

    let body = serde_json::json!({"data": "payload"});
    let result = execute_request(
        &redirectless_client(),
        &localhost_config(addr),
        RequestSpec {
            auth: None,
            method: &reqwest::Method::POST,
            url: &format!("http://{}/submit", addr),
            task_headers: None,
            body: Some(&body),
            body_format: BodyFormat::default(),
            response_format: ResponseFormat::default(),
            timeout: std::time::Duration::from_secs(5),
        },
    )
    .await;

    assert_eq!(result.expect("test")["done"], true);
}

// -- Format axes (#261) --

#[test]
fn test_format_value_tables() {
    // Absent and "json" are today's behavior; unknown values are refused
    // with the allowed set named — the same table validate_input enforces
    // at authoring time.
    assert_eq!(BodyFormat::parse(None).expect("test"), BodyFormat::Json);
    assert_eq!(
        BodyFormat::parse(Some("json")).expect("test"),
        BodyFormat::Json
    );
    assert_eq!(
        BodyFormat::parse(Some("form")).expect("test"),
        BodyFormat::Form
    );
    assert_eq!(
        BodyFormat::parse(Some("text")).expect("test"),
        BodyFormat::Text
    );
    let err = BodyFormat::parse(Some("multipart")).expect_err("test");
    assert!(err.contains("'json', 'form', 'text'"), "{err}");

    assert_eq!(
        ResponseFormat::parse(None).expect("test"),
        ResponseFormat::Json
    );
    assert_eq!(
        ResponseFormat::parse(Some("text")).expect("test"),
        ResponseFormat::Text
    );
    let err = ResponseFormat::parse(Some("base64")).expect_err("test");
    assert!(err.contains("'json', 'text'"), "{err}");
}

#[test]
fn test_encode_form_scalars_arrays_and_null() {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "retries": 3,
        "dry_run": false,
        "to": ["+15551111111", "+15552222222"],
        "optional": null,
    });
    let enc = encode_body(&body, BodyFormat::Form).expect("test");
    assert_eq!(enc.content_type, "application/x-www-form-urlencoded");
    let encoded = String::from_utf8(enc.bytes.to_vec()).expect("test");
    // Scalars canonical, arrays as repeated keys, nulls skipped, reserved
    // characters percent-encoded.
    assert_eq!(
        encoded,
        "grant_type=refresh_token&retries=3&dry_run=false\
             &to=%2B15551111111&to=%2B15552222222"
    );
}

#[test]
fn test_encode_form_bracket_keys_need_no_special_support() {
    // Stripe-style nesting is an ordinary top-level key.
    let body = serde_json::json!({"metadata[order_id]": "6735"});
    let enc = encode_body(&body, BodyFormat::Form).expect("test");
    assert_eq!(
        String::from_utf8(enc.bytes.to_vec()).expect("test"),
        "metadata%5Border_id%5D=6735"
    );
}

#[test]
fn test_encode_form_rejects_nesting_naming_the_key() {
    for body in [
        serde_json::json!({"ok": "v", "bad": {"nested": true}}),
        serde_json::json!({"ok": "v", "bad": [["nested"]]}),
        serde_json::json!({"ok": "v", "bad": [null]}),
    ] {
        let err = encode_body(&body, BodyFormat::Form)
            .expect_err("test")
            .to_string();
        assert!(err.contains("'bad'"), "should name the key: {err}");
    }
    // And a body that is not an object at all.
    let err = encode_body(&serde_json::json!(["a", "b"]), BodyFormat::Form)
        .expect_err("test")
        .to_string();
    assert!(err.contains("requires the body to be an object"), "{err}");
}

#[test]
fn test_encode_text_requires_string() {
    let enc = encode_body(&serde_json::json!("<doc/>"), BodyFormat::Text).expect("test");
    assert_eq!(enc.content_type, "text/plain; charset=utf-8");
    assert_eq!(enc.bytes.as_ref(), b"<doc/>");

    let err = encode_body(&serde_json::json!({"a": 1}), BodyFormat::Text)
        .expect_err("test")
        .to_string();
    assert!(err.contains("requires the body to be a string"), "{err}");
}

/// Echoes back the request's body and every `content-type` header value,
/// so wire tests can assert what actually left the client.
fn echo_app() -> axum::Router {
    axum::Router::new().route(
        "/echo",
        axum::routing::post(|headers: axum::http::HeaderMap, body: String| async move {
            let content_types: Vec<String> = headers
                .get_all("content-type")
                .iter()
                .map(|v| v.to_str().unwrap_or_default().to_string())
                .collect();
            axum::Json(serde_json::json!({
                "content_types": content_types,
                "body": body,
            }))
        }),
    )
}

/// #277: connector query parameters reach the wire, percent-encoded and
/// deterministically ordered.
#[tokio::test]
async fn test_connector_query_params_reach_the_wire() {
    let app = axum::Router::new().route(
        "/api",
        axum::routing::get(|uri: axum::http::Uri| async move {
            axum::Json(serde_json::json!({ "query": uri.query().unwrap_or_default() }))
        }),
    );
    let addr = spawn_mock(app).await;
    let mut config = localhost_config(addr);
    config.query_params = [
        ("uid".to_string(), "svc-user".to_string()),
        // A value needing encoding: URL interpolation would break this
        // silently, `RequestBuilder::query` handles it.
        ("pwd".to_string(), "p@ss w&rd=x".to_string()),
    ]
    .into_iter()
    .collect();

    let result = execute_request(
        &reqwest::Client::new(),
        &config,
        RequestSpec {
            auth: None,
            method: &reqwest::Method::GET,
            url: &format!("http://{}/api", addr),
            task_headers: None,
            body: None,
            body_format: BodyFormat::default(),
            response_format: ResponseFormat::default(),
            timeout: std::time::Duration::from_secs(5),
        },
    )
    .await
    .expect("test");

    // BTreeMap ordering: `pwd` before `uid`, every call. Signature-based
    // gateways depend on a stable order, and HashMap could not give one.
    assert_eq!(
        result["query"], "pwd=p%40ss+w%26rd%3Dx&uid=svc-user",
        "params are percent-encoded and sorted"
    );
}

/// The regression that fires if someone later "simplifies" `query_params`
/// back into `build_url`. The whole point of applying them at the request
/// builder is that `current` — interpolated into every timeout/failure
/// message, and into the `url` OTel field — never carries the credential.
#[tokio::test]
async fn test_connector_query_params_never_appear_in_an_error() {
    // A listener that accepts and never answers, so the request times out
    // and the error string is built from `current`.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test");
    let addr = listener.local_addr().expect("test");
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });

    let mut config = localhost_config(addr);
    config.query_params = [("pwd".to_string(), "super-secret-value".to_string())]
        .into_iter()
        .collect();

    let err = execute_request(
        &reqwest::Client::new(),
        &config,
        RequestSpec {
            auth: None,
            method: &reqwest::Method::GET,
            url: &format!("http://{}/api", addr),
            task_headers: None,
            body: None,
            body_format: BodyFormat::default(),
            response_format: ResponseFormat::default(),
            timeout: std::time::Duration::from_millis(150),
        },
    )
    .await
    .expect_err("must time out");

    let text = format!("{err:?}");
    assert!(
        !text.contains("super-secret-value") && !text.contains("pwd"),
        "the error must carry neither the parameter name nor its value: {text}"
    );
}

#[tokio::test]
async fn test_form_body_on_the_wire() {
    let addr = spawn_mock(echo_app()).await;
    let body = serde_json::json!({"grant_type": "client_credentials", "scope": "a b"});

    let result = execute_request(
        &reqwest::Client::new(),
        &localhost_config(addr),
        RequestSpec {
            auth: None,
            method: &reqwest::Method::POST,
            url: &format!("http://{}/echo", addr),
            task_headers: None,
            body: Some(&body),
            body_format: BodyFormat::Form,
            response_format: ResponseFormat::default(),
            timeout: std::time::Duration::from_secs(5),
        },
    )
    .await
    .expect("test");

    assert_eq!(result["body"], "grant_type=client_credentials&scope=a+b");
    assert_eq!(
        result["content_types"],
        serde_json::json!(["application/x-www-form-urlencoded"])
    );
}

#[tokio::test]
async fn test_text_body_with_explicit_content_type_wins_alone() {
    // The task's explicit content-type replaces the stamp rather than
    // joining it — exactly one content-type header may reach the wire.
    let addr = spawn_mock(echo_app()).await;
    let body = serde_json::json!("<Envelope/>");
    let headers = std::collections::HashMap::from([(
        "Content-Type".to_string(),
        "application/xml".to_string(),
    )]);

    let result = execute_request(
        &reqwest::Client::new(),
        &localhost_config(addr),
        RequestSpec {
            auth: None,
            method: &reqwest::Method::POST,
            url: &format!("http://{}/echo", addr),
            task_headers: Some(&headers),
            body: Some(&body),
            body_format: BodyFormat::Text,
            response_format: ResponseFormat::default(),
            timeout: std::time::Duration::from_secs(5),
        },
    )
    .await
    .expect("test");

    assert_eq!(result["body"], "<Envelope/>");
    assert_eq!(
        result["content_types"],
        serde_json::json!(["application/xml"])
    );
}

#[tokio::test]
async fn test_response_format_text_captures_plain_body() {
    let mock_app =
        axum::Router::new().route("/text", axum::routing::get(|| async { "OK id=12345" }));
    let addr = spawn_mock(mock_app).await;

    let result = execute_request(
        &reqwest::Client::new(),
        &localhost_config(addr),
        RequestSpec {
            auth: None,
            method: &reqwest::Method::GET,
            url: &format!("http://{}/text", addr),
            task_headers: None,
            body: None,
            body_format: BodyFormat::default(),
            response_format: ResponseFormat::Text,
            timeout: std::time::Duration::from_secs(5),
        },
    )
    .await
    .expect("test");

    assert_eq!(result, serde_json::json!("OK id=12345"));
}

#[tokio::test]
async fn test_response_format_text_keeps_error_and_size_paths() {
    // Only the final parse is format-dispatched: a non-2xx still errors,
    // and the streaming size cap still applies.
    let mock_app = axum::Router::new()
        .route(
            "/fail",
            axum::routing::get(|| async {
                (axum::http::StatusCode::BAD_GATEWAY, "upstream said no")
            }),
        )
        .route("/large", axum::routing::get(|| async { "x".repeat(200) }));
    let addr = spawn_mock(mock_app).await;
    let mut config = localhost_config(addr);
    config.max_response_size = 50;

    let err = execute_request(
        &reqwest::Client::new(),
        &config,
        RequestSpec {
            auth: None,
            method: &reqwest::Method::GET,
            url: &format!("http://{}/fail", addr),
            task_headers: None,
            body: None,
            body_format: BodyFormat::default(),
            response_format: ResponseFormat::Text,
            timeout: std::time::Duration::from_secs(5),
        },
    )
    .await
    .expect_err("test")
    .to_string();
    assert!(err.contains("502"), "{err}");

    let err = execute_request(
        &reqwest::Client::new(),
        &config,
        RequestSpec {
            auth: None,
            method: &reqwest::Method::GET,
            url: &format!("http://{}/large", addr),
            task_headers: None,
            body: None,
            body_format: BodyFormat::default(),
            response_format: ResponseFormat::Text,
            timeout: std::time::Duration::from_secs(5),
        },
    )
    .await
    .expect_err("test")
    .to_string();
    assert!(err.contains("exceed"), "{err}");
}

#[test]
fn test_apply_auth_basic() {
    let client = reqwest::Client::new();
    let auth = AuthConfig::Basic {
        username: "user".to_string(),
        password: "pass".to_string(),
    };
    let req = apply_auth(client.get("http://localhost"), &auth);
    let built = req.build().expect("test");
    let auth_header = built
        .headers()
        .get("authorization")
        .expect("test")
        .to_str()
        .expect("test");
    assert!(auth_header.starts_with("Basic "));
}

// ---- #281: URLs and upstream bodies in error text ----

fn redaction_config(url: &str) -> HttpConnectorConfig {
    HttpConnectorConfig {
        retry_non_idempotent: false,
        url: url.to_string(),
        method: String::new(),
        headers: std::collections::HashMap::new(),
        query_params: Default::default(),
        auth: None,
        retry: crate::connector::RetryConfig::default(),
        max_response_size: 10 * 1024 * 1024,
        allow_private_urls: true,
        operations: Default::default(),
    }
}

async fn redaction_error(url: &str, timeout_ms: u64) -> String {
    let http_config = redaction_config(url);
    execute_request(
        &reqwest::Client::new(),
        &http_config,
        RequestSpec {
            auth: http_config.auth.as_ref(),
            method: &reqwest::Method::GET,
            url,
            task_headers: None,
            body: None,
            body_format: BodyFormat::default(),
            response_format: ResponseFormat::default(),
            timeout: std::time::Duration::from_millis(timeout_ms),
        },
    )
    .await
    .expect_err("expected the request to fail")
    .to_string()
}

/// A credential-free URL must come through byte-for-byte — the redaction
/// helper returns its input unchanged when there is nothing to mask, and
/// this pins that so the change is a no-op for the common case.
#[test]
fn safe_url_leaves_a_credential_free_url_alone() {
    let url = "https://api.example.com/v1/orders?page=2";
    assert_eq!(safe_url(url), url);
}

#[test]
fn safe_url_masks_userinfo_and_secret_query_values() {
    let masked = safe_url("https://svc:hunter2@api.example.com/v1?pwd=s3cret&page=2");
    assert!(!masked.contains("hunter2"), "userinfo survived: {masked}");
    assert!(
        !masked.contains("s3cret"),
        "query secret survived: {masked}"
    );
    assert!(masked.contains("page=2"), "non-secret dropped: {masked}");
}

/// The timeout arm is the one that reaches an unauthenticated caller:
/// `errors.rs` maps `DataflowError::Timeout` to a 504 with the message
/// verbatim and does not gate it on `verbose_errors`.
#[tokio::test]
async fn a_timeout_message_masks_a_credential_in_the_url() {
    let mock_app = axum::Router::new().route(
        "/slow",
        axum::routing::get(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            axum::Json(serde_json::json!({"slow": true}))
        }),
    );
    let addr = spawn_mock(mock_app).await;

    let msg = redaction_error(&format!("http://{addr}/slow?pwd=s3cret"), 100).await;
    assert!(msg.contains("timed out"), "{msg}");
    assert!(
        !msg.contains("s3cret"),
        "credential survived the timeout: {msg}"
    );
}

/// The transport arm additionally has to drop reqwest's own copy of the
/// URL, which its `Display` appends unredacted.
#[tokio::test]
async fn a_transport_failure_masks_the_url_reqwest_would_append() {
    let msg = redaction_error("http://127.0.0.1:1/test?pwd=s3cret", 1_000).await;
    assert!(msg.contains("failed"), "{msg}");
    assert!(
        !msg.contains("s3cret"),
        "credential survived the transport error: {msg}"
    );
}

/// An upstream error body is a preview, not a copy: it is someone else's
/// text headed for a persisted trace.
#[tokio::test]
async fn a_non_2xx_body_is_truncated_to_a_preview() {
    let big = "x".repeat(4096);
    let mock_app = axum::Router::new().route(
        "/boom",
        axum::routing::get(move || {
            let big = big.clone();
            async move { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, big) }
        }),
    );
    let addr = spawn_mock(mock_app).await;

    let msg = redaction_error(&format!("http://{addr}/boom"), 5_000).await;
    assert!(msg.contains("… (truncated)"), "no truncation marker: {msg}");
    let body_len = msg.matches('x').count();
    assert_eq!(
        body_len, ERROR_BODY_PREVIEW,
        "expected exactly the preview cap, got {body_len}"
    );
}

/// A body that fits is reported whole, with no marker to mislead whoever
/// reads the trace.
#[tokio::test]
async fn a_short_non_2xx_body_is_not_marked_truncated() {
    let mock_app = axum::Router::new().route(
        "/boom",
        axum::routing::get(|| async {
            (
                axum::http::StatusCode::BAD_REQUEST,
                r#"{"error":"missing field 'id'"}"#,
            )
        }),
    );
    let addr = spawn_mock(mock_app).await;

    let msg = redaction_error(&format!("http://{addr}/boom"), 5_000).await;
    assert!(msg.contains("missing field 'id'"), "{msg}");
    assert!(
        !msg.contains("truncated"),
        "short body marked truncated: {msg}"
    );
}

/// A server that streams without declaring a length cannot make
/// `http_call` buffer past the connector's `max_response_size`.
///
/// The streaming loop lives in `http_body`, shared with other callers; this
/// pins that `http_call` still enforces the cap through it.
/// Driven through `execute_request` rather than `read_response` so the
/// connector's configured limit is proved to reach it.
#[tokio::test]
async fn a_flooding_endpoint_is_cut_off_at_the_connector_limit() {
    const CHUNK: usize = 64 * 1024;
    const CHUNKS: usize = 128; // 8 MiB against the 4 KiB limit below
    let (url, server) = crate::http_body::flood_server(CHUNK, CHUNKS).await;

    let http_config = HttpConnectorConfig {
        max_response_size: 4096,
        ..redaction_config(&url)
    };
    let err = execute_request(
        &reqwest::Client::new(),
        &http_config,
        RequestSpec {
            auth: None,
            method: &reqwest::Method::GET,
            url: &url,
            task_headers: None,
            body: None,
            body_format: BodyFormat::default(),
            response_format: ResponseFormat::default(),
            timeout: std::time::Duration::from_secs(10),
        },
    )
    .await
    .expect_err("must refuse an oversized response")
    .to_string();

    assert!(
        err.contains("4096"),
        "the refusal must name the connector's limit: {err}"
    );
    crate::http_body::assert_stopped_early(
        server.await.expect("test server"),
        CHUNK * CHUNKS,
        "http_call",
    );
}
