use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::functions::config::FunctionConfig;
use dataflow_rs::engine::message::{Change, Message};
use datalogic_rs::DataLogic;

use super::http_common::{self, build_url, get_nested, set_nested};
use crate::connector::{ConnectorConfig, ConnectorRegistry};

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
        let path = super::resolve_path(&input.path, &input.path_logic, message, &datalogic)?;
        let url = build_url(&http_config.url, path.as_deref());

        // Build method
        let method = super::to_reqwest_method(&input.method);

        let timeout = Duration::from_millis(input.timeout_ms);

        // Execute HTTP request with retry
        let retry = &http_config.retry;
        let result =
            super::retry_with_backoff(retry.max_retries, retry.retry_delay_ms, "Enrich", || {
                http_common::execute_request(
                    &self.client,
                    &method,
                    &url,
                    None,
                    http_config,
                    None,
                    timeout,
                )
            })
            .await;

        match result {
            Ok(response_body) => {
                let old_value = get_nested(&message.context, &input.merge_path);
                set_nested(
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
