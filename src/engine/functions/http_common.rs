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

    // Apply connector default headers
    for (k, v) in &http_config.headers {
        req = req.header(k, v);
    }

    // Apply task-level headers (override connector defaults)
    if let Some(headers) = task_headers {
        for (k, v) in headers {
            req = req.header(k, v);
        }
    }

    // Apply auth
    if let Some(ref auth) = http_config.auth {
        req = apply_auth(req, auth);
    }

    // Apply body
    if let Some(b) = body {
        req = req.header("content-type", "application/json").json(b);
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
    if let Some(content_length) = response.content_length() {
        if content_length as usize > max_size {
            return Err(DataflowError::function_execution(
                format!(
                    "Response from {} declared Content-Length {} exceeds limit of {} bytes",
                    url, content_length, max_size
                ),
                None,
            ));
        }
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
}
