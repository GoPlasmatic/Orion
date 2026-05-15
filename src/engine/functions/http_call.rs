use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::functions::HttpCallConfig;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::Value;

use super::http_common::{self, build_url};
use crate::connector::{ConnectorConfig, ConnectorRegistry};

/// Executes HTTP requests against named connectors with retry support.
pub struct HttpCallHandler {
    pub registry: Arc<ConnectorRegistry>,
    pub client: reqwest::Client,
}

#[async_trait]
impl AsyncFunctionHandler for HttpCallHandler {
    type Input = HttpCallConfig;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &HttpCallConfig,
    ) -> dataflow_rs::Result<TaskOutcome> {
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

        let path = super::resolve_path(&input.path, input.compiled_path_logic.as_deref(), ctx)?;
        let url = build_url(&http_config.url, path.as_deref());

        let method = super::to_reqwest_method(&input.method);

        let body = resolve_body(&input.body, input.compiled_body_logic.as_deref(), ctx)?;

        let timeout = Duration::from_millis(input.timeout_ms);

        let retry_config = &http_config.retry;
        let response_body = if self.registry.circuit_breaker_enabled() {
            let channel = super::extract_channel(ctx.message());
            let key = format!("{}:{}", channel, input.connector);
            let breaker = self.registry.get_or_create_breaker(&key).await;
            super::execute_with_circuit_breaker(
                &breaker,
                &input.connector,
                channel,
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
            .await?
        } else {
            super::retry_with_backoff(
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
            .await?
        };

        if let Some(ref response_path) = input.response_path {
            ctx.set_json(response_path, &response_body);
        }

        Ok(TaskOutcome::Success)
    }
}

fn resolve_body(
    static_body: &Option<Value>,
    compiled_logic: Option<&datalogic_rs::Logic>,
    ctx: &TaskContext<'_>,
) -> dataflow_rs::Result<Option<Value>> {
    if let Some(compiled) = compiled_logic {
        let result: Value = ctx
            .datalogic()
            .session()
            .eval_into(compiled, &ctx.message().context)
            .map_err(|e| DataflowError::LogicEvaluation(e.to_string()))?;
        Ok(Some(result))
    } else {
        Ok(static_body.clone())
    }
}
