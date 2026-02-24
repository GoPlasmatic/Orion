use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::functions::config::FunctionConfig;
use dataflow_rs::engine::message::{Change, Message};
use datalogic_rs::DataLogic;
use serde_json::Value;

use super::http_common::{self, build_url, get_nested, set_nested};
use crate::connector::{ConnectorConfig, ConnectorRegistry};

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
        let path = super::resolve_path(&input.path, &input.path_logic, message, &datalogic)?;
        let url = build_url(&http_config.url, path.as_deref());

        // Build method
        let method = super::to_reqwest_method(&input.method);

        // Build body
        let body = resolve_body(&input.body, &input.body_logic, message, &datalogic)?;

        // Timeout
        let timeout = Duration::from_millis(input.timeout_ms);

        // Execute with retry
        let retry_config = &http_config.retry;
        let response_body = super::retry_with_backoff(
            retry_config.max_retries,
            retry_config.retry_delay_ms,
            "HTTP call",
            || {
                http_common::execute_request(
                    &self.client,
                    &method,
                    &url,
                    Some(&input.headers),
                    http_config,
                    body.as_ref(),
                    timeout,
                )
            },
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
