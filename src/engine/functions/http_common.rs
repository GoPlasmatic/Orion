use std::time::Duration;

use dataflow_rs::engine::error::DataflowError;
use serde_json::Value;

use crate::connector::{AuthConfig, HttpConnectorConfig};

/// Build a URL from a base URL and optional path segment.
pub fn build_url(base: &str, path: Option<&str>) -> String {
    match path {
        Some(p) if !p.is_empty() => {
            let base = base.trim_end_matches('/');
            let path = p.trim_start_matches('/');
            format!("{base}/{path}")
        }
        _ => base.to_string(),
    }
}

/// Apply authentication to a request builder.
pub fn apply_auth(req: reqwest::RequestBuilder, auth: &AuthConfig) -> reqwest::RequestBuilder {
    match auth {
        AuthConfig::Bearer { token } => req.header("authorization", format!("Bearer {token}")),
        AuthConfig::Basic { username, password } => req.basic_auth(username, Some(password)),
        AuthConfig::ApiKey { header, key } => req.header(header, key),
    }
}

/// Get a nested value from a JSON object using dot-notation path.
pub fn get_nested(value: &Value, path: &str) -> Value {
    let mut current = value;
    for part in path.split('.') {
        match current.get(part) {
            Some(v) => current = v,
            None => return Value::Null,
        }
    }
    current.clone()
}

/// Set a nested value in a JSON object using dot-notation path.
pub fn set_nested(value: &mut Value, path: &str, new_val: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = value;
    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            current[*part] = new_val;
            return;
        }
        if !current.get(*part).is_some_and(|v| v.is_object()) {
            current[*part] = Value::Object(serde_json::Map::new());
        }
        current = &mut current[*part];
    }
}

/// Redirect-hop cap for the manual follower in [`execute_request`]. The shared
/// client is built with `redirect::Policy::none()` (see main.rs), so every hop
/// returns here and passes SSRF validation before being followed.
const MAX_REDIRECTS: usize = 5;

/// Execute an HTTP request with connector config applied.
///
/// Builds the request with connector headers, auth, optional task-level headers,
/// optional body, and timeout. Returns the parsed JSON response. Redirects are
/// followed manually (up to [`MAX_REDIRECTS`]) with SSRF re-validation per hop.
#[tracing::instrument(skip(client, task_headers, http_config, body))]
pub async fn execute_request(
    client: &reqwest::Client,
    method: &reqwest::Method,
    url: &str,
    task_headers: Option<&std::collections::HashMap<String, String>>,
    http_config: &HttpConnectorConfig,
    body: Option<&Value>,
    timeout: Duration,
) -> dataflow_rs::Result<Value> {
    let original = url::Url::parse(url)
        .map_err(|e| DataflowError::Validation(format!("Invalid URL '{url}': {e}")))?;
    let mut current = original.clone();
    let mut method = method.clone();
    let mut body = body;

    for _ in 0..=MAX_REDIRECTS {
        // SSRF protection: block requests to private/internal IPs.
        // `allow_private_urls` exempts only the connector's own endpoint — a
        // redirect is server-controlled data, so any hop leaving the original
        // host:port is validated even for private-allowed connectors.
        let own_endpoint = same_endpoint(&current, &original);
        if !(http_config.allow_private_urls && own_endpoint)
            && let Err(msg) = crate::validation::validate_url_not_private(current.as_str()).await
        {
            return Err(DataflowError::function_execution(
                format!("SSRF protection: {msg}"),
                None,
            ));
        }

        let mut req = client
            .request(method.clone(), current.clone())
            .timeout(timeout);

        // Inject W3C trace context headers (traceparent/tracestate) for distributed tracing
        {
            let mut trace_headers = std::collections::HashMap::new();
            crate::server::trace_context::inject_trace_context(&mut trace_headers);
            for (k, v) in &trace_headers {
                req = req.header(k, v);
            }
        }

        // Connector headers, auth, and task headers can carry credentials —
        // they go only to the connector's own endpoint, never on a cross-host hop.
        if own_endpoint {
            // Apply connector default headers (lowest priority)
            for (k, v) in &http_config.headers {
                req = req.header(k, v);
            }

            // Apply auth headers (override connector defaults)
            if let Some(ref auth) = http_config.auth {
                req = apply_auth(req, auth);
            }
        }

        // Apply default content-type and body
        if let Some(b) = body {
            req = req.header("content-type", "application/json").json(b);
        }

        // Apply task-level headers LAST (highest priority — workflow developer's explicit choice wins)
        if own_endpoint && let Some(headers) = task_headers {
            for (k, v) in headers {
                req = req.header(k, v);
            }
        }

        let response = req.send().await.map_err(|e| {
            if e.is_timeout() {
                DataflowError::Timeout(format!("HTTP request to {current} timed out"))
            } else {
                DataflowError::Io(format!("HTTP request to {current} failed: {e}"))
            }
        })?;

        if let Some(next) = redirect_target(&response, &current)? {
            // Mirror reqwest's default policy: 301/302/303 turn a non-GET/HEAD
            // request into a bodyless GET; 307/308 re-send method and body.
            if matches!(response.status().as_u16(), 301..=303)
                && method != reqwest::Method::GET
                && method != reqwest::Method::HEAD
            {
                method = reqwest::Method::GET;
                body = None;
            }
            current = next;
            continue;
        }

        return read_json_response(response, &current, http_config.max_response_size).await;
    }

    Err(DataflowError::function_execution(
        format!("Stopped after {MAX_REDIRECTS} redirects requesting {url}"),
        None,
    ))
}

/// Same scheme-default-aware host:port — the boundary within which connector
/// credentials and the `allow_private_urls` exemption apply.
fn same_endpoint(a: &url::Url, b: &url::Url) -> bool {
    a.host_str().is_some()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// The target of a redirect response, resolved against the current URL.
/// `None` when the response is not a followable redirect.
fn redirect_target(
    response: &reqwest::Response,
    current: &url::Url,
) -> dataflow_rs::Result<Option<url::Url>> {
    if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
        return Ok(None);
    }
    let Some(location) = response.headers().get(reqwest::header::LOCATION) else {
        return Ok(None);
    };
    let location = location.to_str().map_err(|_| {
        DataflowError::function_execution(
            format!("Redirect from {current} has a non-ASCII Location header"),
            None,
        )
    })?;
    let next = current.join(location).map_err(|e| {
        DataflowError::function_execution(
            format!("Redirect from {current} has invalid Location '{location}': {e}"),
            None,
        )
    })?;
    if !matches!(next.scheme(), "http" | "https") {
        return Err(DataflowError::function_execution(
            format!(
                "Redirect from {current} targets unsupported scheme '{}'",
                next.scheme()
            ),
            None,
        ));
    }
    Ok(Some(next))
}

/// Read a (non-redirect) response body as JSON, enforcing `max_size`.
async fn read_json_response(
    response: reqwest::Response,
    url: &url::Url,
    max_size: usize,
) -> dataflow_rs::Result<Value> {
    let status = response.status();

    // Check Content-Length hint before reading body
    if let Some(content_length) = response.content_length()
        && content_length as usize > max_size
    {
        return Err(DataflowError::function_execution(
            format!(
                "Response from {url} declared Content-Length {content_length} exceeds limit of {max_size} bytes"
            ),
            None,
        ));
    }

    if !status.is_success() {
        let body_bytes = response.bytes().await.unwrap_or_default();
        // Truncate error body to max_size
        let body_text = String::from_utf8_lossy(&body_bytes[..body_bytes.len().min(max_size)]);
        return Err(DataflowError::http(
            status.as_u16(),
            format!("HTTP {status} from {url}: {body_text}"),
        ));
    }

    let body_bytes = response.bytes().await.map_err(|e| {
        DataflowError::function_execution(
            format!("Failed to read response body from {url}: {e}"),
            None,
        )
    })?;

    if body_bytes.len() > max_size {
        return Err(DataflowError::function_execution(
            format!(
                "Response body from {} is {} bytes, exceeding limit of {} bytes",
                url,
                body_bytes.len(),
                max_size
            ),
            None,
        ));
    }

    let response_body: Value = serde_json::from_slice(&body_bytes).map_err(|e| {
        DataflowError::function_execution(
            format!("Failed to parse response from {url} as JSON: {e}"),
            None,
        )
    })?;
    Ok(response_body)
}

#[cfg(test)]
mod tests {
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
    fn test_set_get_nested() {
        let mut val = serde_json::json!({"data": {"x": 1}});
        set_nested(&mut val, "data.result", serde_json::json!("hello"));
        assert_eq!(get_nested(&val, "data.result"), serde_json::json!("hello"));
        assert_eq!(get_nested(&val, "data.x"), serde_json::json!(1));
    }

    #[test]
    fn test_get_nested_missing_key() {
        let val = serde_json::json!({"data": {"x": 1}});
        assert_eq!(get_nested(&val, "data.y"), Value::Null);
    }

    #[test]
    fn test_get_nested_deeply_missing() {
        let val = serde_json::json!({"a": 1});
        assert_eq!(get_nested(&val, "a.b.c"), Value::Null);
    }

    #[test]
    fn test_get_nested_single_key() {
        let val = serde_json::json!({"key": "value"});
        assert_eq!(get_nested(&val, "key"), serde_json::json!("value"));
    }

    #[test]
    fn test_set_nested_creates_intermediate_objects() {
        let mut val = serde_json::json!({});
        set_nested(&mut val, "a.b.c", serde_json::json!(42));
        assert_eq!(get_nested(&val, "a.b.c"), serde_json::json!(42));
    }

    #[test]
    fn test_set_nested_overwrites_existing() {
        let mut val = serde_json::json!({"a": {"b": "old"}});
        set_nested(&mut val, "a.b", serde_json::json!("new"));
        assert_eq!(get_nested(&val, "a.b"), serde_json::json!("new"));
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
            url: format!("http://{}", addr),
            method: String::new(),
            headers: std::collections::HashMap::new(),
            auth: None,
            retry: crate::connector::RetryConfig::default(),
            max_response_size: 10 * 1024 * 1024,
            allow_private_urls: true, // Tests use localhost
        };

        let result = execute_request(
            &client,
            &reqwest::Method::GET,
            &format!("http://{}/test", addr),
            None,
            &http_config,
            None,
            std::time::Duration::from_secs(5),
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
            url: format!("http://{}", addr),
            method: String::new(),
            headers: std::collections::HashMap::from([(
                "x-connector-header".to_string(),
                "conn-val".to_string(),
            )]),
            auth: Some(AuthConfig::Bearer {
                token: "test-token".to_string(),
            }),
            retry: crate::connector::RetryConfig::default(),
            max_response_size: 10 * 1024 * 1024,
            allow_private_urls: true, // Tests use localhost
        };

        let body = serde_json::json!({"data": "payload"});

        let result = execute_request(
            &client,
            &reqwest::Method::POST,
            &format!("http://{}/post-test", addr),
            Some(&headers),
            &http_config,
            Some(&body),
            std::time::Duration::from_secs(5),
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
            url: format!("http://{}", addr),
            method: String::new(),
            headers: std::collections::HashMap::new(),
            auth: None,
            retry: crate::connector::RetryConfig::default(),
            max_response_size: 10 * 1024 * 1024,
            allow_private_urls: true, // Tests use localhost
        };

        let result = execute_request(
            &client,
            &reqwest::Method::GET,
            &format!("http://{}/error", addr),
            None,
            &http_config,
            None,
            std::time::Duration::from_secs(5),
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
            url: format!("http://{}", addr),
            method: String::new(),
            headers: std::collections::HashMap::new(),
            auth: None,
            retry: crate::connector::RetryConfig::default(),
            max_response_size: 10 * 1024 * 1024,
            allow_private_urls: true, // Tests use localhost
        };

        let result = execute_request(
            &client,
            &reqwest::Method::GET,
            &format!("http://{}/text", addr),
            None,
            &http_config,
            None,
            std::time::Duration::from_secs(5),
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
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({"data": "x".repeat(200)}))
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
            url: format!("http://{}", addr),
            method: String::new(),
            headers: std::collections::HashMap::new(),
            auth: None,
            retry: crate::connector::RetryConfig::default(),
            max_response_size: 10,    // Very small limit
            allow_private_urls: true, // Tests use localhost
        };

        let result = execute_request(
            &client,
            &reqwest::Method::GET,
            &format!("http://{}/large", addr),
            None,
            &http_config,
            None,
            std::time::Duration::from_secs(5),
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
            url: format!("http://{}", addr),
            method: String::new(),
            headers: std::collections::HashMap::new(),
            auth: None,
            retry: crate::connector::RetryConfig::default(),
            max_response_size: 10 * 1024 * 1024,
            allow_private_urls: true, // Tests use localhost
        };

        let result = execute_request(
            &client,
            &reqwest::Method::GET,
            &format!("http://{}/slow", addr),
            None,
            &http_config,
            None,
            std::time::Duration::from_millis(100), // Very short timeout
        )
        .await;

        assert!(result.is_err());
        assert!(result.expect_err("test").to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn test_execute_request_connection_refused() {
        let client = reqwest::Client::new();
        let http_config = HttpConnectorConfig {
            url: "http://127.0.0.1:1".to_string(),
            method: String::new(),
            headers: std::collections::HashMap::new(),
            auth: None,
            retry: crate::connector::RetryConfig::default(),
            max_response_size: 10 * 1024 * 1024,
            allow_private_urls: true, // Tests use localhost
        };

        let result = execute_request(
            &client,
            &reqwest::Method::GET,
            "http://127.0.0.1:1/test",
            None,
            &http_config,
            None,
            std::time::Duration::from_secs(1),
        )
        .await;

        assert!(result.is_err());
        assert!(result.expect_err("test").to_string().contains("failed"));
    }

    /// Mirrors the production client (main.rs): the manual follower in
    /// `execute_request` only sees 3xx responses when reqwest doesn't follow.
    fn redirectless_client() -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("test")
    }

    fn localhost_config(addr: std::net::SocketAddr) -> HttpConnectorConfig {
        HttpConnectorConfig {
            url: format!("http://{}", addr),
            method: String::new(),
            headers: std::collections::HashMap::new(),
            auth: None,
            retry: crate::connector::RetryConfig::default(),
            max_response_size: 10 * 1024 * 1024,
            allow_private_urls: true, // Tests use localhost
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
            &reqwest::Method::GET,
            &format!("http://{}/redirect", addr),
            None,
            &localhost_config(addr),
            None,
            std::time::Duration::from_secs(5),
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
            &reqwest::Method::GET,
            &format!("http://{}/a", addr),
            None,
            &localhost_config(addr),
            None,
            std::time::Duration::from_secs(5),
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
            &reqwest::Method::GET,
            &format!("http://{}/loop", addr),
            None,
            &localhost_config(addr),
            None,
            std::time::Duration::from_secs(5),
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
            &reqwest::Method::POST,
            &format!("http://{}/submit", addr),
            None,
            &localhost_config(addr),
            Some(&body),
            std::time::Duration::from_secs(5),
        )
        .await;

        assert_eq!(result.expect("test")["done"], true);
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
}
