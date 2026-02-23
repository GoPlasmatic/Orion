use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::functions::config::FunctionConfig;
use dataflow_rs::engine::message::{Change, Message};
use datalogic_rs::DataLogic;
use serde_json::Value;

use crate::connector::{ConnectorConfig, ConnectorRegistry, HttpConnectorConfig};

/// Executes HTTP requests against named connectors with retry support.
pub struct HttpCallHandler {
    pub registry: Arc<ConnectorRegistry>,
    pub client: reqwest::Client,
}

#[async_trait]
impl AsyncFunctionHandler for HttpCallHandler {
    async fn execute(
        &self,
        message: &mut Message,
        config: &FunctionConfig,
        datalogic: Arc<DataLogic>,
    ) -> dataflow_rs::Result<(usize, Vec<Change>)> {
        let input = match config {
            FunctionConfig::HttpCall { input, .. } => input,
            _ => return Err(DataflowError::Validation("Expected HttpCall config".into())),
        };

        // Resolve connector
        let connector_config = self.registry.get(&input.connector).await.ok_or_else(|| {
            DataflowError::function_execution(
                format!("Connector '{}' not found", input.connector),
                None,
            )
        })?;

        let http_config = match connector_config.as_ref() {
            ConnectorConfig::Http(c) => c,
            _ => {
                return Err(DataflowError::Validation(format!(
                    "Connector '{}' is not an HTTP connector",
                    input.connector
                )));
            }
        };

        // Build URL
        let path = resolve_path(&input.path, &input.path_logic, message, &datalogic)?;
        let url = build_url(&http_config.url, path.as_deref());

        // Build method
        let method = match input.method {
            dataflow_rs::engine::functions::integration::HttpMethod::Get => reqwest::Method::GET,
            dataflow_rs::engine::functions::integration::HttpMethod::Post => reqwest::Method::POST,
            dataflow_rs::engine::functions::integration::HttpMethod::Put => reqwest::Method::PUT,
            dataflow_rs::engine::functions::integration::HttpMethod::Patch => {
                reqwest::Method::PATCH
            }
            dataflow_rs::engine::functions::integration::HttpMethod::Delete => {
                reqwest::Method::DELETE
            }
        };

        // Build body
        let body = resolve_body(&input.body, &input.body_logic, message, &datalogic)?;

        // Timeout
        let timeout = Duration::from_millis(input.timeout_ms);

        // Execute with retry
        let response_body = execute_with_retry(
            &self.client,
            &method,
            &url,
            &input.headers,
            http_config,
            body.as_ref(),
            timeout,
        )
        .await?;

        // Store response at response_path
        let mut changes = Vec::new();
        if let Some(ref response_path) = input.response_path {
            let old_value = get_nested(&message.context, response_path);
            set_nested(&mut message.context, response_path, response_body.clone());
            message.invalidate_context_cache();

            changes.push(Change {
                path: Arc::from(response_path.as_str()),
                old_value: Arc::new(old_value),
                new_value: Arc::new(response_body),
            });
        }

        Ok((200, changes))
    }
}

/// Build a full URL from a base URL and optional path.
pub fn build_url_pub(base: &str, path: Option<&str>) -> String {
    build_url(base, path)
}

/// Get a nested value from a JSON object using dot-notation path.
pub fn get_nested_pub(value: &Value, path: &str) -> Value {
    get_nested(value, path)
}

/// Set a nested value in a JSON object using dot-notation path.
pub fn set_nested_pub(value: &mut Value, path: &str, new_val: Value) {
    set_nested(value, path, new_val)
}

/// Apply authentication to a request builder.
pub fn apply_auth_pub(
    req: reqwest::RequestBuilder,
    auth: &crate::connector::AuthConfig,
) -> reqwest::RequestBuilder {
    apply_auth(req, auth)
}

fn build_url(base: &str, path: Option<&str>) -> String {
    match path {
        Some(p) if !p.is_empty() => {
            let base = base.trim_end_matches('/');
            let path = p.trim_start_matches('/');
            format!("{}/{}", base, path)
        }
        _ => base.to_string(),
    }
}

fn resolve_path(
    static_path: &Option<String>,
    path_logic: &Option<Value>,
    message: &mut Message,
    datalogic: &DataLogic,
) -> dataflow_rs::Result<Option<String>> {
    if let Some(logic) = path_logic {
        let context = message.get_context_arc();
        let compiled = datalogic
            .compile(logic)
            .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?;
        let result = datalogic
            .evaluate(&compiled, context)
            .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?;
        Ok(Some(result.as_str().map(|s| s.to_string()).unwrap_or_else(
            || serde_json::to_string(&result).unwrap_or_default(),
        )))
    } else {
        Ok(static_path.clone())
    }
}

fn resolve_body(
    static_body: &Option<Value>,
    body_logic: &Option<Value>,
    message: &mut Message,
    datalogic: &DataLogic,
) -> dataflow_rs::Result<Option<Value>> {
    if let Some(logic) = body_logic {
        let context = message.get_context_arc();
        let compiled = datalogic
            .compile(logic)
            .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?;
        let result = datalogic
            .evaluate(&compiled, context)
            .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?;
        Ok(Some(result))
    } else {
        Ok(static_body.clone())
    }
}

async fn execute_with_retry(
    client: &reqwest::Client,
    method: &reqwest::Method,
    url: &str,
    task_headers: &std::collections::HashMap<String, String>,
    http_config: &HttpConnectorConfig,
    body: Option<&Value>,
    timeout: Duration,
) -> dataflow_rs::Result<Value> {
    let retry_config = &http_config.retry;
    let mut last_error = None;

    for attempt in 0..=retry_config.max_retries {
        if attempt > 0 {
            let delay = retry_config.retry_delay_ms * 2u64.pow(attempt - 1);
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }

        match execute_once(
            client,
            method,
            url,
            task_headers,
            http_config,
            body,
            timeout,
        )
        .await
        {
            Ok(val) => return Ok(val),
            Err(e) => {
                if e.retryable() && attempt < retry_config.max_retries {
                    tracing::warn!(
                        attempt = attempt + 1,
                        max = retry_config.max_retries,
                        error = %e,
                        "HTTP call failed, retrying"
                    );
                    last_error = Some(e);
                    continue;
                }
                return Err(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| DataflowError::Unknown("Retry loop exhausted".into())))
}

async fn execute_once(
    client: &reqwest::Client,
    method: &reqwest::Method,
    url: &str,
    task_headers: &std::collections::HashMap<String, String>,
    http_config: &HttpConnectorConfig,
    body: Option<&Value>,
    timeout: Duration,
) -> dataflow_rs::Result<Value> {
    let mut req = client.request(method.clone(), url).timeout(timeout);

    // Apply connector default headers
    for (k, v) in &http_config.headers {
        req = req.header(k, v);
    }

    // Apply task-level headers (override connector defaults)
    for (k, v) in task_headers {
        req = req.header(k, v);
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

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        return Err(DataflowError::http(
            status.as_u16(),
            format!("HTTP {} from {}: {}", status, url, body_text),
        ));
    }

    let response_body: Value = response.json().await.unwrap_or(Value::Null);
    Ok(response_body)
}

fn apply_auth(
    req: reqwest::RequestBuilder,
    auth: &crate::connector::AuthConfig,
) -> reqwest::RequestBuilder {
    match auth {
        crate::connector::AuthConfig::Bearer { token } => {
            req.header("authorization", format!("Bearer {}", token))
        }
        crate::connector::AuthConfig::Basic { username, password } => {
            req.basic_auth(username, Some(password))
        }
        crate::connector::AuthConfig::ApiKey { header, key } => req.header(header, key),
    }
}

/// Get a nested value from a JSON object using dot-notation path.
fn get_nested(value: &Value, path: &str) -> Value {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = value;
    for part in &parts {
        current = &current[*part];
    }
    current.clone()
}

/// Set a nested value in a JSON object using dot-notation path.
fn set_nested(value: &mut Value, path: &str, new_val: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = value;
    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            current[*part] = new_val;
            return;
        }
        if !current[*part].is_object() {
            current[*part] = Value::Object(serde_json::Map::new());
        }
        current = &mut current[*part];
    }
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
