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
            format!("{}/{}", base, path)
        }
        _ => base.to_string(),
    }
}

/// Apply authentication to a request builder.
pub fn apply_auth(req: reqwest::RequestBuilder, auth: &AuthConfig) -> reqwest::RequestBuilder {
    match auth {
        AuthConfig::Bearer { token } => req.header("authorization", format!("Bearer {}", token)),
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

/// Execute an HTTP request with connector config applied.
///
/// Builds the request with connector headers, auth, optional task-level headers,
/// optional body, and timeout. Returns the parsed JSON response.
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
    // SSRF protection: block requests to private/internal IPs unless explicitly allowed
    if !http_config.allow_private_urls
        && let Err(msg) = crate::validation::validate_url_not_private(url).await
    {
        return Err(DataflowError::function_execution(
            format!("SSRF protection: {msg}"),
            None,
        ));
    }

    let mut req = client.request(method.clone(), url).timeout(timeout);

    // Inject W3C trace context headers (traceparent/tracestate) for distributed tracing
    #[cfg(feature = "otel")]
    {
        let mut trace_headers = std::collections::HashMap::new();
        crate::server::trace_context::inject_trace_context(&mut trace_headers);
        for (k, v) in &trace_headers {
            req = req.header(k, v);
        }
    }

    // Apply connector default headers (lowest priority)
    for (k, v) in &http_config.headers {
        req = req.header(k, v);
    }

    // Apply auth headers (override connector defaults)
    if let Some(ref auth) = http_config.auth {
        req = apply_auth(req, auth);
    }

    // Apply default content-type and body
    if let Some(b) = body {
        req = req.header("content-type", "application/json").json(b);
    }

    // Apply task-level headers LAST (highest priority — workflow developer's explicit choice wins)
    if let Some(headers) = task_headers {
        for (k, v) in headers {
            req = req.header(k, v);
        }
    }

    let response = req.send().await.map_err(|e| {
        if e.is_timeout() {
            DataflowError::Timeout(format!("HTTP request to {} timed out", url))
        } else {
            DataflowError::Io(format!("HTTP request to {} failed: {}", url, e))
        }
    })?;

    let max_size = http_config.max_response_size;
    let status = response.status();

    // Check Content-Length hint before reading body
    if let Some(content_length) = response.content_length()
        && content_length as usize > max_size
    {
        return Err(DataflowError::function_execution(
            format!(
                "Response from {} declared Content-Length {} exceeds limit of {} bytes",
                url, content_length, max_size
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
            format!("HTTP {} from {}: {}", status, url, body_text),
        ));
    }

    let body_bytes = response.bytes().await.map_err(|e| {
        DataflowError::function_execution(
            format!("Failed to read response body from {}: {}", url, e),
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
            format!("Failed to parse response from {} as JSON: {}", url, e),
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
        let built = req.build().unwrap();
        assert_eq!(
            built
                .headers()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
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
        let built = req.build().unwrap();
        assert_eq!(
            built.headers().get("x-api-key").unwrap().to_str().unwrap(),
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, mock_app).await.unwrap();
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
        let val = result.unwrap();
        assert_eq!(val["result"], "success");
    }

    #[tokio::test]
    async fn test_execute_request_with_headers_auth_and_body() {
        let mock_app = axum::Router::new().route(
            "/post-test",
            axum::routing::post(|| async { axum::Json(serde_json::json!({"received": true})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, mock_app).await.unwrap();
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, mock_app).await.unwrap();
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
        let err = result.unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn test_execute_request_non_json_response() {
        let mock_app = axum::Router::new().route(
            "/text",
            axum::routing::get(|| async { "plain text response" }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, mock_app).await.unwrap();
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
        assert!(result.unwrap_err().to_string().contains("parse"));
    }

    #[tokio::test]
    async fn test_execute_request_response_too_large() {
        let mock_app = axum::Router::new().route(
            "/large",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({"data": "x".repeat(200)}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, mock_app).await.unwrap();
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
        assert!(result.unwrap_err().to_string().contains("exceed"));
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, mock_app).await.unwrap();
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
        assert!(result.unwrap_err().to_string().contains("timed out"));
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
        assert!(result.unwrap_err().to_string().contains("failed"));
    }

    #[test]
    fn test_apply_auth_basic() {
        let client = reqwest::Client::new();
        let auth = AuthConfig::Basic {
            username: "user".to_string(),
            password: "pass".to_string(),
        };
        let req = apply_auth(client.get("http://localhost"), &auth);
        let built = req.build().unwrap();
        let auth_header = built
            .headers()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(auth_header.starts_with("Basic "));
    }
}
