use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::functions::HttpCallConfig;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;

use super::http_common::{self, build_url};
use super::schema::{FieldKind, FieldSchema};
use crate::connector::ConnectorRegistry;

/// This handler's name in metrics, profiles and error messages (F48).
const NAME: &str = "http_call";

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

        super::connector_helpers::guarded_handler(
            NAME,
            &self.registry,
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

                // F22e: the connector's method allow-list, empty by default.
                // A connector pointed at a read-only upstream can be `["GET"]`
                // and no workflow can POST through it. Checked ahead of the
                // path logic, which is the only message-dependent step here:
                // a refusal that no property of the message can change should
                // not be reported after one that can (F58).
                let method = super::to_reqwest_method(&input.method);
                super::connector_helpers::require_method_allowed(
                    &http_config.operations,
                    method.as_str(),
                    &input.connector,
                )?;

                // `resolve_path` / `resolve_body` are dataflow-rs's own
                // sanctioned read of the (static, logic) pairs: they apply the
                // static fallback, coerce a non-string path to compact JSON,
                // and evaluate on the worker's pooled arena.
                let path = input.resolve_path(ctx)?;
                let url = build_url(&http_config.url, path.as_deref());

                let body = input.resolve_body(ctx)?;

                let timeout = Duration::from_millis(input.timeout_ms);

                // F8: retrying a non-idempotent method resends the side effect.
                // A POST that times out is indistinguishable from one the server
                // applied, so re-sending it means double charges and double
                // orders — out of the box, since max_retries defaults to 3.
                // Idempotent methods retry as before; others need an explicit
                // per-connector opt-in (the workflow can carry its own
                // idempotency key in headers).
                let retryable_method =
                    input.method.is_idempotent() || http_config.retry_non_idempotent;
                let retry_config = &http_config.retry;
                let max_retries = if retryable_method {
                    retry_config.max_retries
                } else {
                    0
                };
                let policy = super::RetryPolicy {
                    max_retries,
                    retry_delay_ms: retry_config.retry_delay_ms,
                    // F47: the ceiling is the sum of the attempts' own budgets —
                    // `timeout_ms × (max_retries + 1)`, backoff included, since
                    // the deadline is measured from the first attempt. On
                    // shipped defaults (30 s × 4) that is 120 s, so the comment
                    // that used to sit here — "cannot outlive the channel
                    // deadline it sits under" — described something this does
                    // not do. What it does do is make the loop *bounded*:
                    // attempts plus backoff were previously unbounded, and a
                    // 60 s channel timeout could sit under a ~127 s retry loop
                    // still burning a connection after the caller gave up. The
                    // channel deadline itself is enforced upstream, by the
                    // timeout `run_for_channel` wraps the whole engine call in.
                    // Non-retryable methods get `max_retries = 0`, so their
                    // budget is exactly one `timeout_ms`.
                    deadline: Some(timeout.saturating_mul(max_retries.saturating_add(1))),
                };
                // F6: the breaker is applied by `guarded_handler` above, the
                // same shell every other egress path now uses. This branch used
                // to carry its own copy — the only one in the codebase.
                let response_body = super::retry_with_policy(policy, "HTTP call", || {
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
                .await?;

                if let Some(ref response_path) = input.response_path {
                    ctx.set_json(response_path, &response_body);
                }

                Ok(TaskOutcome::Success)
            },
        )
        .await
    }
}

// -- Input schema (F53) --
//
// The table describing this handler's `function.input` lives next to the
// handler it describes. It used to sit in `schema.rs` with the other nine,
// which is how every schema/handler divergence in the 1.0 audit happened:
// a field was added, renamed or made conditional here and the table saying
// so was in a different file.

pub(super) const HTTP_CALL_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the HTTP connector to call.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "method",
        description: "HTTP method (GET, POST, PUT, DELETE, PATCH). Defaults to GET.",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "path",
        description: "Static path appended to the connector's base URL.",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "path_logic",
        description: "JSONLogic expression evaluated to derive the request path.",
        kind: FieldKind::Any,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "headers",
        description: "Additional request headers.",
        kind: FieldKind::Object,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "body",
        description: "Static request body (any JSON value).",
        kind: FieldKind::Any,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "body_logic",
        description: "JSONLogic expression evaluated to derive the request body.",
        kind: FieldKind::Any,
        required: false,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the response body is written. Omit to discard it. (Was `response_path` before 1.0; still accepted, but not alongside `output`.)",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        // A real serde alias on dataflow-rs's `HttpCallConfig` since 3.1 —
        // Orion used to rewrite the key in the storage repository instead.
        alias: Some("response_path"),
    },
    FieldSchema {
        name: "timeout_ms",
        description: "Request timeout in milliseconds. Defaults to 30000.",
        kind: FieldKind::Number,
        required: false,
        resolvable: false,
        alias: None,
    },
];
