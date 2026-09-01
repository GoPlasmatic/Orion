use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::HttpCallConfig;
use dataflow_rs::engine::task_context::TaskContext;

use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{ConnectorCall, require_method_allowed};
use super::http_common::{self, build_url};
use super::schema::{FieldKind, FieldSchema};
use crate::connector::ConnectorRegistry;
use crate::engine::HandlerError;

/// The half of an `http_call` task read before the connector is resolved.
///
/// The refusals here — an unknown `body_format`, a `response_format` naming
/// nothing — must not be reported after one the message can change (F58), so
/// they are decided in `parse`, which runs before the connector lookup. Since
/// dataflow-rs 3.9 every one of these parameters is JSONLogic, so "decided
/// first" no longer means "decided by the task text alone": a statically
/// authored value folds at engine build and costs nothing, and a computed one
/// evaluates here, still ahead of the method gate. The path and body stay in
/// `run`, after the connector's method allow-list has had its say.
pub struct HttpCall {
    method: reqwest::Method,
    body_format: http_common::BodyFormat,
    response_format: http_common::ResponseFormat,
    timeout: Duration,
    /// Per-task headers, values resolved against the message. Before 3.9 these
    /// were `String`s on the config, so a bearer token or a correlation id had
    /// to be injected by the service layer.
    headers: std::collections::HashMap<String, String>,
}

/// Executes HTTP requests against named connectors with retry support.
pub struct HttpCallHandler {
    pub registry: Arc<ConnectorRegistry>,
    pub client: reqwest::Client,
}

#[async_trait]
impl ConnectorHandler for HttpCallHandler {
    const NAME: &'static str = "http_call";
    type Kind = crate::connector::kind::Http;
    type Input = HttpCallConfig;
    type Parsed = HttpCall;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        _call: &ConnectorCall<'_>,
        input: &HttpCallConfig,
        ctx: &TaskContext<'_>,
    ) -> Result<Self::Parsed, HandlerError> {
        // The format axes are values-as-data on dataflow-rs's config; this
        // parse is the value table that interprets them. Workflow validation
        // checks the same table at authoring time, so these refusals only fire
        // for definitions that bypassed it.
        Ok(HttpCall {
            method: super::to_reqwest_method(&input.method),
            body_format: http_common::BodyFormat::parse(input.resolve_body_format(ctx)?.as_deref())
                .map_err(DataflowError::Validation)?,
            response_format: http_common::ResponseFormat::parse(
                input.resolve_response_format(ctx)?.as_deref(),
            )
            .map_err(DataflowError::Validation)?,
            timeout: Duration::from_millis(input.resolve_timeout_ms(ctx)?),
            headers: input.resolve_headers(ctx)?,
        })
    }

    fn gate(
        parsed: &Self::Parsed,
        conn: &crate::connector::HttpConnectorConfig,
        connector: &str,
    ) -> Result<(), HandlerError> {
        // F22e: the connector's method allow-list, empty by default. A
        // connector pointed at a read-only upstream can be `["GET"]` and no
        // workflow can POST through it.
        Ok(require_method_allowed(
            &conn.operations,
            parsed.method.as_str(),
            connector,
        )?)
    }

    async fn run(
        &self,
        parsed: Self::Parsed,
        http_config: &crate::connector::HttpConnectorConfig,
        call: &ConnectorCall<'_>,
        input: &HttpCallConfig,
        ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        // `resolve_path` / `resolve_body` are dataflow-rs's own sanctioned read
        // of the (static, logic) pairs: they apply the static fallback, coerce
        // a non-string path to compact JSON, and evaluate on the worker's
        // pooled arena. Resolved here rather than in `parse` so the method gate
        // above still precedes the only message-dependent step (F58).
        let path = input.resolve_path(ctx)?;
        let url = build_url(&http_config.url, path.as_deref());
        let body = input.resolve_body(ctx)?;

        // F8: retrying a non-idempotent method resends the side effect. A POST
        // that times out is indistinguishable from one the server applied, so
        // re-sending it means double charges and double orders — out of the
        // box, since max_retries defaults to 3. Idempotent methods retry as
        // before; others need an explicit per-connector opt-in (the workflow
        // can carry its own idempotency key in headers).
        let retryable_method = input.method.is_idempotent() || http_config.retry_non_idempotent;
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
            // `timeout_ms × (max_retries + 1)`, backoff included, since the
            // deadline is measured from the first attempt. On shipped defaults
            // (30 s × 4) that is 120 s, so the comment that used to sit here —
            // "cannot outlive the channel deadline it sits under" — described
            // something this does not do. What it does do is make the loop
            // *bounded*: attempts plus backoff were previously unbounded, and a
            // 60 s channel timeout could sit under a ~127 s retry loop still
            // burning a connection after the caller gave up. The channel
            // deadline itself is enforced upstream, by the timeout
            // `run_for_channel` wraps the whole engine call in. Non-retryable
            // methods get `max_retries = 0`, so their budget is exactly one
            // `timeout_ms`.
            deadline: Some(parsed.timeout.saturating_mul(max_retries.saturating_add(1))),
        };

        // #268: resolve the effective auth once for the whole retry loop —
        // static variants pass through; a managed-OAuth2 connector acquires
        // (or reuses) its access token here.
        let auth = crate::connector::oauth::effective_auth(
            self.registry.oauth(),
            call.connector,
            http_config,
        )
        .await
        .map_err(http_common::oauth_error_to_dataflow)?;

        // F6: the breaker is applied by the handler shell, the same one every
        // other egress path uses. This branch used to carry its own copy — the
        // only one in the codebase. `retry_with_attempts` rather than
        // `retry_with_policy`: the loop moved upstream in dataflow-rs 3.7 and
        // logs its retries through the `log` facade, which Orion does not
        // bridge into `tracing`. Taking the count back means one warning naming
        // how many attempts a call actually cost, instead of the per-attempt
        // warnings that used to come out of Orion's own copy of the loop — and
        // it says the same thing about a call that eventually *succeeded*,
        // which the old warnings did too but nothing summarised.
        let (result, attempts) = super::retry_with_attempts(policy, "HTTP call", || {
            http_common::execute_request(
                &self.client,
                http_config,
                http_common::RequestSpec {
                    method: &parsed.method,
                    url: &url,
                    task_headers: Some(&parsed.headers),
                    body: body.as_ref(),
                    body_format: parsed.body_format,
                    response_format: parsed.response_format,
                    timeout: parsed.timeout,
                    auth: auth.as_deref(),
                },
            )
        })
        .await;
        if attempts > 1 {
            tracing::warn!(
                connector = %call.connector,
                method = %parsed.method,
                attempts,
                max_retries,
                outcome = if result.is_ok() { "succeeded" } else { "failed" },
                "HTTP call retried"
            );
        }

        let response_body = match result {
            Ok(body) => body,
            Err(e) => {
                // #268: a 401 on a managed-OAuth2 connector means the cached
                // access token was revoked IdP-side; drop it so the next call
                // refetches instead of failing again for a full refresh margin.
                if matches!(&e, DataflowError::Http { status: 401, .. })
                    && matches!(
                        http_config.auth,
                        Some(crate::connector::AuthConfig::OAuth2(_))
                    )
                {
                    self.registry.oauth().invalidate(call.connector).await;
                }
                return Err(e.into());
            }
        };

        // `response_path` is optional: omitting it discards the body.
        Ok(match input.response_path {
            Some(_) => response_body.into(),
            None => Produced::nothing(),
        })
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
        description: "Name of the HTTP connector to call (JSONLogic; a computed name is \
                      not yet supported).",
        kind: FieldKind::String,
        required: true,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "method",
        description: "HTTP method (GET, POST, PUT, DELETE, PATCH). Defaults to GET. \
                      The one parameter that is not JSONLogic — the connector's method \
                      allow-list is checked before the message is consulted.",
        kind: FieldKind::String,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "path",
        description: "Path appended to the connector's base URL (JSONLogic). \
                      (Was `path_logic`; still accepted, but not alongside `path`.)",
        kind: FieldKind::String,
        template_at: &[""],
        alias: Some("path_logic"),
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "headers",
        description: "Additional request headers. Each value is JSONLogic, so a bearer \
                      token or a correlation id can be computed from the message.",
        kind: FieldKind::Object,
        template_at: &["*"],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "body",
        description: "Request body, any JSON value (JSONLogic). \
                      (Was `body_logic`; still accepted, but not alongside `body`.)",
        kind: FieldKind::Any,
        template_at: &[""],
        alias: Some("body_logic"),
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "body_format",
        description: "How the body becomes request bytes: 'json' (default), 'form' \
                      (URL-encoded key/value pairs), or 'text' (string sent verbatim). \
                      Sets the content-type unless a header names one explicitly. \
                      (JSONLogic.)",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the response body is written (JSONLogic). Omit to discard it. (Was `response_path` before 1.0; still accepted, but not alongside `output`.)",
        kind: FieldKind::String,
        template_at: &[""],
        // A real serde alias on dataflow-rs's `HttpCallConfig` since 3.1 —
        // Orion used to rewrite the key in the storage repository instead.
        alias: Some("response_path"),
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "response_format",
        description: "How the response bytes are captured at `output`: 'json' \
                      (default, parsed) or 'text' (a plain string). (JSONLogic.)",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "timeout_ms",
        description: "Request timeout in milliseconds (JSONLogic). Defaults to 30000.",
        kind: FieldKind::Number,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];
