use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::functions::config::FunctionConfig;
use dataflow_rs::engine::message::{Change, Message};
use datalogic_rs::DataLogic;
use serde_json::Value;

use crate::connector::{ConnectorConfig, ConnectorRegistry};
use crate::engine::functions::http_call;

/// Fetches external data and merges it into the message context.
pub struct EnrichHandler {
    pub registry: Arc<ConnectorRegistry>,
    pub client: reqwest::Client,
}

#[async_trait]
impl AsyncFunctionHandler for EnrichHandler {
    async fn execute(
        &self,
        message: &mut Message,
        config: &FunctionConfig,
        datalogic: Arc<DataLogic>,
    ) -> dataflow_rs::Result<(usize, Vec<Change>)> {
        let input = match config {
            FunctionConfig::Enrich { input, .. } => input,
            _ => return Err(DataflowError::Validation("Expected Enrich config".into())),
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
        let url = http_call::build_url_pub(&http_config.url, path.as_deref());

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

        let timeout = Duration::from_millis(input.timeout_ms);

        // Execute HTTP request with retry
        let result = execute_enrich(&self.client, &method, &url, http_config, timeout).await;

        match result {
            Ok(response_body) => {
                let old_value = http_call::get_nested_pub(&message.context, &input.merge_path);
                http_call::set_nested_pub(
                    &mut message.context,
                    &input.merge_path,
                    response_body.clone(),
                );
                message.invalidate_context_cache();

                Ok((
                    200,
                    vec![Change {
                        path: Arc::from(input.merge_path.as_str()),
                        old_value: Arc::new(old_value),
                        new_value: Arc::new(response_body),
                    }],
                ))
            }
            Err(e) => match input.on_error {
                dataflow_rs::engine::functions::integration::EnrichErrorAction::Skip => {
                    tracing::warn!(
                        connector = %input.connector,
                        error = %e,
                        "Enrichment failed, skipping per on_error=skip"
                    );
                    Ok((200, vec![]))
                }
                dataflow_rs::engine::functions::integration::EnrichErrorAction::Fail => Err(e),
            },
        }
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

async fn execute_enrich(
    client: &reqwest::Client,
    method: &reqwest::Method,
    url: &str,
    http_config: &crate::connector::HttpConnectorConfig,
    timeout: Duration,
) -> dataflow_rs::Result<Value> {
    let retry = &http_config.retry;
    let mut last_error = None;

    for attempt in 0..=retry.max_retries {
        if attempt > 0 {
            let delay = retry.retry_delay_ms * 2u64.pow(attempt - 1);
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }

        let mut req = client.request(method.clone(), url).timeout(timeout);

        for (k, v) in &http_config.headers {
            req = req.header(k, v);
        }

        if let Some(ref auth) = http_config.auth {
            req = http_call::apply_auth_pub(req, auth);
        }

        match req.send().await {
            Ok(response) => {
                let status = response.status();
                if !status.is_success() {
                    let err = DataflowError::http(
                        status.as_u16(),
                        format!("Enrich HTTP {} from {}", status, url),
                    );
                    if err.retryable() && attempt < retry.max_retries {
                        last_error = Some(err);
                        continue;
                    }
                    return Err(err);
                }
                return response.json::<Value>().await.map_err(|e| {
                    DataflowError::Io(format!("Failed to parse enrich response: {}", e))
                });
            }
            Err(e) => {
                let err = if e.is_timeout() {
                    DataflowError::Timeout(format!("Enrich request to {} timed out", url))
                } else {
                    DataflowError::Io(format!("Enrich request to {} failed: {}", url, e))
                };
                if err.retryable() && attempt < retry.max_retries {
                    last_error = Some(err);
                    continue;
                }
                return Err(err);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| DataflowError::Unknown("Retry loop exhausted".into())))
}
