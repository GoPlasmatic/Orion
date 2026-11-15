//! `http_call` — a request to an HTTP connector, with the connector's retry
//! policy, its auth (managed OAuth2 included) and the circuit breaker.
//!
//! [`HttpCallFunction`] reads the task through Orion's own
//! [`HttpCallConfig`], resolving its fields in order, and hands the request
//! to one `send`.

use super::definition::{Definition, FieldOverlay, NullOutput, OrionFunction};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{CompileCx, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::program::IssueCode;
use dataflow_rs::{Ctx, Expr, Function, Outcome};
use serde_json::Value;

use super::http_common::{self, BodyFormat, ResponseFormat, build_url};
use super::schema::{FieldKind, RetrySafety};
use crate::connector::{ConnectorRegistry, HttpConnectorConfig};
use crate::kit::{self, read};
use crate::retry::{RetryPolicy, retry_with_attempts};
use crate::{HandlerError, http_status};

/// This function's name, for the free functions below — a reference to the
/// one place it is written, not a second spelling of it.
pub(crate) const NAME: &str = "http_call";

/// The methods a task may name, in the order of the `HttpMethod` vocabulary
/// it deserializes: upper case only.
const METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];

/// Whether a retry may resend `method` (RFC 9110): the idempotent methods
/// of the `HttpMethod` vocabulary.
fn is_idempotent(method: &str) -> bool {
    matches!(method, "GET" | "PUT" | "DELETE")
}

/// One request, its every field resolved: what [`send`] needs from the
/// task.
struct Request<'a> {
    method: &'a reqwest::Method,
    /// Whether a retry may resend it, by the task's method.
    idempotent: bool,
    path: Option<&'a str>,
    headers: &'a HashMap<String, String>,
    body: Option<&'a Value>,
    body_format: BodyFormat,
    response_format: ResponseFormat,
    timeout: Duration,
}

/// The request, against a resolved, type-checked and gated connector: the
/// retry policy, the effective auth, the retry loop and its summary line, and
/// a 401's token invalidation. The response body, as `response_format`
/// captured it.
async fn send(
    registry: &ConnectorRegistry,
    client: &reqwest::Client,
    connector: &str,
    http_config: &HttpConnectorConfig,
    req: Request<'_>,
) -> dataflow_rs::Result<Value> {
    let url = build_url(&http_config.url, req.path);

    // Retrying a non-idempotent method resends the side effect. A POST that
    // times out is indistinguishable from one the server applied, so
    // re-sending it means double charges and double orders — out of the box,
    // since max_retries defaults to 3. Idempotent methods retry; others need
    // an explicit per-connector opt-in (the workflow can carry its own
    // idempotency key in headers).
    let retryable_method = req.idempotent || http_config.retry_non_idempotent;
    let retry_config = &http_config.retry;
    let max_retries = if retryable_method {
        retry_config.max_retries
    } else {
        0
    };
    let policy = RetryPolicy {
        max_retries,
        retry_delay_ms: retry_config.retry_delay_ms,
        // The ceiling is the sum of the attempts' own budgets —
        // `timeout_ms × (max_retries + 1)`, backoff included, since the
        // deadline is measured from the first attempt. On shipped defaults
        // (30 s × 4) that is 120 s. It makes the loop *bounded*: with attempts
        // plus backoff unbounded, a 60 s channel timeout could sit under a
        // ~127 s retry loop still burning a connection after the caller gave
        // up. The run's own deadline is not this loop's: the engine races
        // every host call against the run's deadline. Non-retryable methods
        // get `max_retries = 0`, so their budget is exactly one `timeout_ms`.
        deadline: Some(req.timeout.saturating_mul(max_retries.saturating_add(1))),
    };

    // #268: resolve the effective auth once for the whole retry loop —
    // static variants pass through; a managed-OAuth2 connector acquires
    // (or reuses) its access token here.
    let auth = crate::connector::oauth::effective_auth(registry.oauth(), connector, http_config)
        .await
        .map_err(http_common::oauth_error_to_dataflow)?;

    // The breaker is applied by the caller's shell, the same one every other
    // egress path uses. `retry_with_attempts` rather than `retry_with_policy`:
    // the loop (vendored as `engine::retry`) logs each retry at `debug` only.
    // Taking the count back means one warning naming how many attempts a call
    // actually cost, and it says the same thing about a call that eventually
    // *succeeded*.
    let (result, attempts) = retry_with_attempts(policy, "HTTP call", || {
        http_common::execute_request(
            client,
            http_config,
            http_common::RequestSpec {
                method: req.method,
                url: &url,
                task_headers: Some(req.headers),
                body: req.body,
                body_format: req.body_format,
                response_format: req.response_format,
                timeout: req.timeout,
                auth: auth.as_deref(),
            },
        )
    })
    .await;
    if attempts > 1 {
        tracing::warn!(
            connector = %connector,
            method = %req.method,
            attempts,
            max_retries,
            outcome = if result.is_ok() { "succeeded" } else { "failed" },
            "HTTP call retried"
        );
    }

    // #268: a 401 on a managed-OAuth2 connector means the cached access
    // token was revoked IdP-side; drop it so the next call refetches instead
    // of failing again for a full refresh margin.
    if let Err(e) = &result
        && http_status(e) == Some(401)
        && matches!(
            http_config.auth,
            Some(crate::connector::AuthConfig::OAuth2(_))
        )
    {
        registry.oauth().invalidate(connector).await;
    }
    result
}

/// `http_call`'s input: Orion's own config, each field read in the order it
/// is read at execution.
///
/// The keys are the canonical ones. `path_logic`, `body_logic` and
/// `response_path` are unknown keys like any other: the loader reads a
/// definition as stored. A computed `connector` is refused at load. `method`
/// is one of the five methods the `HttpMethod` vocabulary reads, upper case;
/// the config's check refuses anything else with that type's own message.
///
/// The string parameters are read with [`read::plain_str`] and `timeout_ms`
/// with [`read::template_u64`], `Template::resolve_string`'s and
/// `resolve_u64`'s readings, and an optional parameter written as `null` is
/// unset ([`read::present`]), as serde reads an `Option<Template>`.
#[derive(Debug, FunctionConfig)]
#[config(check = HttpCallConfig::check)]
pub struct HttpCallConfig {
    /// Name of the HTTP connector to call (JSONLogic; a computed name is not yet supported).
    connector: String,
    /// HTTP method (GET, POST, PUT, DELETE, PATCH). Defaults to GET. The one parameter that is
    /// not JSONLogic — the connector's method allow-list is checked before the message is
    /// consulted.
    #[config(default = "GET")]
    method: String,
    /// Path appended to the connector's base URL (JSONLogic).
    path: Option<Expr<OwnedDataValue>>,
    /// Additional request headers. Each value is JSONLogic, so a bearer token or a correlation
    /// id can be computed from the message.
    #[config(default_json = "{}")]
    headers: BTreeMap<String, Expr<OwnedDataValue>>,
    /// Request body, any JSON value (JSONLogic).
    body: Option<Expr<OwnedDataValue>>,
    /// How the body becomes request bytes: 'json' (default), 'form' (URL-encoded key/value
    /// pairs), or 'text' (string sent verbatim). Sets the content-type unless a header names
    /// one explicitly. (JSONLogic.)
    body_format: Option<Expr<OwnedDataValue>>,
    /// Dotted path where the response body is written (JSONLogic). Omit to discard it.
    output: Option<Output>,
    /// How the response bytes are captured at `output`: 'json' (default, parsed) or 'text' (a
    /// plain string). (JSONLogic.)
    response_format: Option<Expr<OwnedDataValue>>,
    /// Request timeout in milliseconds (JSONLogic). Defaults to 30000.
    #[config(default = 30000)]
    timeout_ms: Expr<OwnedDataValue>,
}

impl HttpCallConfig {
    /// `method` is one of [`METHODS`]: anything else is refused at load, with
    /// serde's text for an unknown variant.
    fn check(cfg: &Self, cx: &mut CompileCx<'_>) {
        if !METHODS.contains(&cfg.method.as_str()) {
            cx.at("method", |cx| {
                cx.issue(IssueCode::InputParse, unknown_method(&cfg.method));
            });
        }
    }
}

/// The message serde gives for an unknown `HttpMethod` variant.
fn unknown_method(method: &str) -> String {
    let expected: Vec<String> = METHODS.iter().map(|m| format!("`{m}`")).collect();
    format!(
        "unknown variant `{method}`, expected one of {}",
        expected.join(", ")
    )
}

/// `http_call` as a function: the request it sends, each field read in
/// order.
pub struct HttpCallFunction {
    pub registry: Arc<ConnectorRegistry>,
    pub client: reqwest::Client,
}

/// A string parameter's value for this call, by `Template::resolve_string`'s
/// reading, or `None` when it is unset or written as `null`.
fn plain(
    ctx: &Ctx<'_>,
    field: &Option<Expr<OwnedDataValue>>,
) -> dataflow_rs::Result<Option<String>> {
    let Some(expr) = read::present(field) else {
        return Ok(None);
    };
    let value = ctx.eval(expr)?;
    Ok(Some(read::plain_str(&value).into_owned()))
}

impl OrionFunction for HttpCallFunction {
    type Connector = crate::connector::kind::Http;

    const DEFINITION: Definition = Definition {
        name: NAME,
        description: "HTTP request to an HTTP connector with retry + circuit breaker.",
        category: "connector",
        retry_safety: RetrySafety::DependsOn { input: "method" },
        fields: &[
            FieldOverlay {
                name: "path",
                kind: Some(FieldKind::String),
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "body_format",
                kind: Some(FieldKind::String),
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "response_format",
                kind: Some(FieldKind::String),
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "timeout_ms",
                kind: Some(FieldKind::Number),
                ..FieldOverlay::DEFAULT
            },
        ],
        null_output: NullOutput::Reject,
        stub_target: Some("connector"),
        ..Definition::DEFAULT
    };

    fn from_deps(deps: &crate::HandlerDeps<'_>) -> Self {
        Self {
            registry: deps.registry.clone(),
            client: deps.client.clone(),
        }
    }
}

impl Function for HttpCallFunction {
    type Config = HttpCallConfig;

    fn spec(&self) -> FunctionSpec {
        super::definition::spec::<Self>()
    }

    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &HttpCallConfig) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        // The prologue's output, before anything about the request is read:
        // an output that names no path sends nothing.
        let output = cfg
            .output
            .as_ref()
            .map(|out| Self::DEFINITION.output_path(ctx, out))
            .transpose()?;
        // `parse`'s half, before the connector is looked up: the formats,
        // the timeout and the headers, in that order.
        let method =
            reqwest::Method::from_bytes(cfg.method.as_bytes()).unwrap_or(reqwest::Method::GET);
        let body_format = BodyFormat::parse(plain(ctx, &cfg.body_format)?.as_deref())
            .map_err(HandlerError::caller_input)?;
        let response_format = ResponseFormat::parse(plain(ctx, &cfg.response_format)?.as_deref())
            .map_err(HandlerError::caller_input)?;
        let timeout = ctx.eval(&cfg.timeout_ms)?;
        let timeout = Duration::from_millis(read::template_u64(&timeout, "http_call timeout_ms")?);
        let headers = cfg
            .headers
            .iter()
            .map(|(name, value)| {
                let value = ctx.eval(value)?;
                Ok((name.clone(), read::plain_str(&value).into_owned()))
            })
            .collect::<dataflow_rs::Result<HashMap<_, _>>>()?;

        let response = kit::with_connector::<<Self as OrionFunction>::Connector, _, _, _>(
            NAME,
            &self.registry,
            &cfg.connector,
            channel,
            |conn| {
                let ctx = &mut *ctx;
                async move {
                    kit::require_method_allowed(
                        &conn.config().operations,
                        method.as_str(),
                        &cfg.connector,
                    )?;
                    // The only message-dependent reads after the gate.
                    let path = plain(ctx, &cfg.path)?;
                    let body = read::present(&cfg.body)
                        .map(|expr| ctx.eval(expr).map(|v| Value::from(&*v)))
                        .transpose()?;
                    send(
                        &self.registry,
                        &self.client,
                        &cfg.connector,
                        conn.config(),
                        Request {
                            method: &method,
                            idempotent: is_idempotent(&cfg.method),
                            path: path.as_deref(),
                            headers: &headers,
                            body: body.as_ref(),
                            body_format,
                            response_format,
                            timeout,
                        },
                    )
                    .await
                }
            },
        )
        .await?;

        // `output` is optional: leaving it unset discards the body.
        if let Some(path) = &output {
            ctx.set(path, OwnedDataValue::from(response))?;
        }
        Ok(Outcome::Ok)
    }
}

#[cfg(test)]
#[path = "http_call_tests.rs"]
mod tests;
