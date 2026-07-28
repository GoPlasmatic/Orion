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
use crate::connector::ConnectorRegistry;

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
        // F40: read the channel before the body borrows `ctx` mutably.
        let channel = super::extract_channel(ctx.message()).to_string();

        super::connector_helpers::observed_handler_named(
            "http_call",
            &input.connector,
            &channel,
            async move {
                let connector_config =
                    super::connector_helpers::resolve_connector(&self.registry, &input.connector)
                        .await?;
                let http_config = super::connector_helpers::require_http_connector(
                    connector_config.as_ref(),
                    &input.connector,
                )?;

                let path =
                    super::resolve_path(&input.path, input.compiled_path_logic.as_deref(), ctx)?;
                let url = build_url(&http_config.url, path.as_deref());

                let method = super::to_reqwest_method(&input.method);

                let body = resolve_body(&input.body, input.compiled_body_logic.as_deref(), ctx)?;

                let timeout = Duration::from_millis(input.timeout_ms);

                // F8: retrying a non-idempotent method resends the side effect.
                // A POST that times out is indistinguishable from one the server
                // applied, so re-sending it means double charges and double
                // orders — out of the box, since max_retries defaults to 3.
                // Idempotent methods retry as before; others need an explicit
                // per-connector opt-in (the workflow can carry its own
                // idempotency key in headers).
                let retryable_method = is_idempotent(&method) || http_config.retry_non_idempotent;
                let retry_config = &http_config.retry;
                let policy = super::RetryPolicy {
                    max_retries: if retryable_method {
                        retry_config.max_retries
                    } else {
                        0
                    },
                    retry_delay_ms: retry_config.retry_delay_ms,
                    // Bound the whole loop by the task's own timeout budget, so
                    // one http_call cannot outlive the channel deadline it sits
                    // under (attempts + backoff were previously unbounded).
                    deadline: Some(
                        timeout.saturating_mul(retry_config.max_retries.saturating_add(1)),
                    ),
                };
                let response_body = if self.registry.circuit_breaker_enabled() {
                    let channel = super::extract_channel(ctx.message());
                    let key = format!("{}:{}", channel, input.connector);
                    let breaker = self.registry.get_or_create_breaker(&key).await;
                    super::execute_with_circuit_breaker(
                        &breaker,
                        &input.connector,
                        channel,
                        policy,
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
                    super::retry_with_policy(policy, "HTTP call", || {
                        http_common::execute_request(
                            &self.client,
                            &method,
                            &url,
                            Some(&input.headers),
                            http_config,
                            body.as_ref(),
                            timeout,
                        )
                    })
                    .await?
                };

                if let Some(ref response_path) = input.response_path {
                    ctx.set_json(response_path, &response_body);
                }

                Ok(TaskOutcome::Success)
            },
        )
        .await
    }
}

/// RFC 9110 idempotent methods: re-sending them is safe by definition, so
/// a timeout can be retried without risking a duplicate side effect (F8).
fn is_idempotent(method: &reqwest::Method) -> bool {
    matches!(
        *method,
        reqwest::Method::GET
            | reqwest::Method::HEAD
            | reqwest::Method::PUT
            | reqwest::Method::DELETE
            | reqwest::Method::OPTIONS
            | reqwest::Method::TRACE
    )
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
