//! `http_call` — a request to an HTTP connector, with the connector's retry
//! policy, its auth (managed OAuth2 included) and the circuit breaker.
//!
//! Two implementations, one request path: [`HttpCallHandler`] serves on
//! dataflow-rs 3 over its `HttpCallConfig`, and [`HttpCallFunction`] is the
//! dataflow-rs 4 function over Orion's own [`HttpCallConfig`] (break 10).
//! Both resolve the task's fields in the same order and hand the request to
//! one `send`, so the two cannot send different requests.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{CompileCx, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec};
use dataflow_rs::program::IssueCode;
use dataflow_rs::{Ctx, Expr, Function, Outcome};
use dataflow_rs_v3::engine::error::DataflowError;
use dataflow_rs_v3::engine::functions::HttpCallConfig as V3HttpCallConfig;
use dataflow_rs_v3::engine::task_context::TaskContext;
use serde_json::Value;

use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{ConnectorCall, require_method_allowed};
use super::http_common::{self, BodyFormat, ResponseFormat, build_url};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::Scope;
use crate::connector::{ConnectorRegistry, HttpConnectorConfig};
use crate::engine::kit::{self, read};
use crate::engine::retry::{RetryPolicy, retry_with_attempts};
use crate::engine::{ErrorClass, HandlerError, http_status};

/// This function's name, for the free functions below — a reference to the
/// one place it is written (F48), not a second spelling of it.
const NAME: &str = <HttpCallHandler as ConnectorHandler>::NAME;

/// The methods a task may name, in dataflow-rs 3's `HttpMethod::ALL` order:
/// the vocabulary its `HttpMethod` deserialized, upper case only.
const METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];

/// Whether a retry may resend `method` (RFC 9110): dataflow-rs 3's
/// `HttpMethod::is_idempotent`.
fn is_idempotent(method: &str) -> bool {
    matches!(method, "GET" | "PUT" | "DELETE")
}

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
    body_format: BodyFormat,
    response_format: ResponseFormat,
    timeout: Duration,
    /// Per-task headers, values resolved against the message. Before 3.9 these
    /// were `String`s on the config, so a bearer token or a correlation id had
    /// to be injected by the service layer.
    headers: HashMap<String, String>,
}

/// One request, its every field resolved: what [`send`] needs from the
/// task, read by either engine's implementation.
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

    // F8: retrying a non-idempotent method resends the side effect. A POST
    // that times out is indistinguishable from one the server applied, so
    // re-sending it means double charges and double orders — out of the
    // box, since max_retries defaults to 3. Idempotent methods retry as
    // before; others need an explicit per-connector opt-in (the workflow
    // can carry its own idempotency key in headers).
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
        // F47: the ceiling is the sum of the attempts' own budgets —
        // `timeout_ms × (max_retries + 1)`, backoff included, since the
        // deadline is measured from the first attempt. On shipped defaults
        // (30 s × 4) that is 120 s. What it does is make the loop *bounded*:
        // attempts plus backoff were previously unbounded, and a 60 s channel
        // timeout could sit under a ~127 s retry loop still burning a
        // connection after the caller gave up. The run's own deadline is not
        // this loop's: on dataflow-rs 3 `run_for_channel` wraps the engine
        // call in a timeout, and dataflow-rs 4 races every host call against
        // the run's deadline, so a call cut short by either ends the same
        // way. Non-retryable methods get `max_retries = 0`, so their budget is
        // exactly one `timeout_ms`.
        deadline: Some(req.timeout.saturating_mul(max_retries.saturating_add(1))),
    };

    // #268: resolve the effective auth once for the whole retry loop —
    // static variants pass through; a managed-OAuth2 connector acquires
    // (or reuses) its access token here.
    let auth = crate::connector::oauth::effective_auth(registry.oauth(), connector, http_config)
        .await
        .map_err(http_common::oauth_error_to_dataflow)?;

    // F6: the breaker is applied by the caller's shell, the same one every
    // other egress path uses. `retry_with_attempts` rather than
    // `retry_with_policy`: the loop (dataflow-rs 3.15's, vendored as
    // `engine::retry`) logs each retry at `debug` only. Taking the count
    // back means one warning naming how many attempts a call actually
    // cost, and it says the same thing about a call that eventually
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

/// Executes HTTP requests against named connectors with retry support.
pub struct HttpCallHandler {
    pub registry: Arc<ConnectorRegistry>,
    pub client: reqwest::Client,
}

#[async_trait]
impl ConnectorHandler for HttpCallHandler {
    const NAME: &'static str = "http_call";
    type Kind = crate::connector::kind::Http;
    type Input = V3HttpCallConfig;
    type Parsed = HttpCall;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        _call: &ConnectorCall<'_>,
        input: &V3HttpCallConfig,
        ctx: &Scope<'_, '_>,
    ) -> Result<Self::Parsed, HandlerError> {
        // The format axes are values-as-data on dataflow-rs's config; this
        // parse is the value table that interprets them. Workflow validation
        // checks the same table at authoring time, so these refusals only fire
        // for definitions that bypassed it.
        Ok(HttpCall {
            method: super::to_reqwest_method(&input.method),
            body_format: BodyFormat::parse(input.resolve_body_format(ctx)?.as_deref())
                .map_err(DataflowError::Validation)?,
            response_format: ResponseFormat::parse(input.resolve_response_format(ctx)?.as_deref())
                .map_err(DataflowError::Validation)?,
            timeout: Duration::from_millis(input.resolve_timeout_ms(ctx)?),
            headers: input.resolve_headers(ctx)?,
        })
    }

    fn gate(
        parsed: &Self::Parsed,
        conn: &HttpConnectorConfig,
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
        http_config: &HttpConnectorConfig,
        call: &ConnectorCall<'_>,
        input: &V3HttpCallConfig,
        ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        // `resolve_path` / `resolve_body` are dataflow-rs's own sanctioned read
        // of the (static, logic) pairs: they apply the static fallback, coerce
        // a non-string path to compact JSON, and evaluate on the worker's
        // pooled arena. Resolved here rather than in `parse` so the method gate
        // above still precedes the only message-dependent step (F58).
        let path = input.resolve_path(ctx)?;
        let body = input.resolve_body(ctx)?;
        let response_body = send(
            &self.registry,
            &self.client,
            call.connector,
            http_config,
            Request {
                method: &parsed.method,
                idempotent: input.method.is_idempotent(),
                path: path.as_deref(),
                headers: &parsed.headers,
                body: body.as_ref(),
                body_format: parsed.body_format,
                response_format: parsed.response_format,
                timeout: parsed.timeout,
            },
        )
        .await?;

        // `response_path` is optional: omitting it discards the body.
        Ok(match input.response_path {
            Some(_) => response_body.into(),
            None => Produced::nothing(),
        })
    }
}

/// `http_call`'s input on dataflow-rs 4: Orion's own config, in place of
/// dataflow-rs 3's `HttpCallConfig` (break 10), each field read as 3.15's
/// read it (I183).
///
/// The keys are the canonical ones. `path_logic`, `body_logic` and
/// `response_path`, 3.15's serde aliases, are rewritten by the loader before
/// a definition is parsed (I186), so here they are unknown keys like any
/// other. A computed `connector` is refused here, where 3.15 accepted one at
/// load and refused it on every call. `method` is one of 3.15's five, upper
/// case, as its `HttpMethod` read it; the config's check refuses anything
/// else with that type's own message.
///
/// The string parameters are read with [`read::plain_str`] and `timeout_ms`
/// with [`read::template_u64`], `Template::resolve_string`'s and
/// `resolve_u64`'s readings, and an optional parameter written as `null` is
/// unset ([`read::present`]), as serde read an `Option<Template>`.
#[derive(Debug, FunctionConfig)]
#[config(check = HttpCallConfig::check)]
pub struct HttpCallConfig {
    /// The HTTP connector called.
    connector: String,
    /// The request method.
    #[config(default = "GET")]
    method: String,
    /// The path appended to the connector's base URL.
    path: Option<Expr<OwnedDataValue>>,
    /// Extra request headers, each value computed from the message. A map
    /// with a default, not an `Option`, so `"headers": null` is refused as
    /// 3.15's serde refused it.
    #[config(default_json = "{}")]
    headers: BTreeMap<String, Expr<OwnedDataValue>>,
    /// The request body.
    body: Option<Expr<OwnedDataValue>>,
    /// How the body becomes bytes: `json`, `form` or `text`.
    body_format: Option<Expr<OwnedDataValue>>,
    /// Where the response body is written; unset discards it.
    output: Option<Output>,
    /// How the response becomes a value: `json` or `text`.
    response_format: Option<Expr<OwnedDataValue>>,
    /// The timeout of one attempt, in milliseconds.
    #[config(default = 30000)]
    timeout_ms: Expr<OwnedDataValue>,
}

impl HttpCallConfig {
    /// `method` is one of [`METHODS`]: 3.15 refused anything else when it
    /// deserialized the task, and so does this, with that refusal's text.
    fn check(cfg: &Self, cx: &mut CompileCx<'_>) {
        if !METHODS.contains(&cfg.method.as_str()) {
            cx.at("method", |cx| {
                cx.issue(IssueCode::InputParse, unknown_method(&cfg.method));
            });
        }
    }
}

/// serde's refusal of an unknown `HttpMethod`, as 3.15 gave it.
fn unknown_method(method: &str) -> String {
    let expected: Vec<String> = METHODS.iter().map(|m| format!("`{m}`")).collect();
    format!(
        "unknown variant `{method}`, expected one of {}",
        expected.join(", ")
    )
}

/// `http_call` as a dataflow-rs 4 function: the request `HttpCallHandler`
/// sends, read in the same order.
pub struct HttpCallFunction {
    pub registry: Arc<ConnectorRegistry>,
    pub client: reqwest::Client,
}

/// A refusal the caller can fix, its message kept: dataflow-rs 3's
/// `Validation`.
fn validation(msg: impl std::fmt::Display) -> HandlerError {
    HandlerError::new(ErrorClass::CallerInput, msg)
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

impl Function for HttpCallFunction {
    type Config = HttpCallConfig;

    fn spec(&self) -> FunctionSpec {
        kit::spec(NAME)
    }

    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &HttpCallConfig) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        // `parse`'s half, before the connector is looked up (F58): the
        // formats, the timeout and the headers, in that order.
        let method =
            reqwest::Method::from_bytes(cfg.method.as_bytes()).unwrap_or(reqwest::Method::GET);
        let body_format =
            BodyFormat::parse(plain(ctx, &cfg.body_format)?.as_deref()).map_err(validation)?;
        let response_format = ResponseFormat::parse(plain(ctx, &cfg.response_format)?.as_deref())
            .map_err(validation)?;
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

        let mut response = None;
        kit::guarded(NAME, &self.registry, &cfg.connector, channel, async {
            let config = kit::resolve_connector(&self.registry, &cfg.connector).await?;
            let http_config =
                kit::require_connector::<crate::connector::kind::Http>(&config, &cfg.connector)?;
            kit::require_method_allowed(&http_config.operations, method.as_str(), &cfg.connector)?;
            // The only message-dependent reads after the gate (F58).
            let path = plain(ctx, &cfg.path)?;
            let body = read::present(&cfg.body)
                .map(|expr| ctx.eval(expr).map(|v| Value::from(&*v)))
                .transpose()?;
            response = Some(
                send(
                    &self.registry,
                    &self.client,
                    &cfg.connector,
                    http_config,
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
                .await?,
            );
            Ok(Outcome::Ok)
        })
        .await?;

        // `output` is optional: leaving it unset discards the body.
        if let Some(body) = response.filter(|_| cfg.output.is_some()) {
            ctx.emit_default(OwnedDataValue::from(&body))?;
        }
        Ok(Outcome::Ok)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::functions::connector_handler::Connector;
    use crate::engine::functions::twin::{self, Failure, Seen};
    use dataflow_rs::function::testing;
    use dataflow_rs::program::Severity;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn config(input: Value) -> Result<HttpCallConfig, Vec<dataflow_rs::Issue>> {
        testing::config_with(&kit::test_exprs(), input)
    }

    /// The Rejected issues of a refused config, as `(path, code, message)`.
    fn refused(input: Value) -> Vec<(String, IssueCode, String)> {
        config(input)
            .expect_err("the config is refused")
            .into_iter()
            .filter(|issue| issue.severity == Severity::Rejected)
            .map(|issue| (issue.path.to_string(), issue.code, issue.message))
            .collect()
    }

    // -- The config --

    /// Declaration order is the field table's (I179), which is the docs page's.
    #[test]
    fn the_config_declares_the_field_table_in_its_order() {
        let fields: Vec<&str> = HttpCallConfig::fields().iter().map(|f| &*f.name).collect();
        let table: Vec<&str> = HTTP_CALL_FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(fields, table);
    }

    #[test]
    fn every_field_kind_compiles() {
        let cfg = config(json!({
            "connector": "api",
            "method": "POST",
            "path": {"cat": ["/orders/", {"var": "data.id"}]},
            "headers": {"X-Id": {"var": "data.id"}, "X-Static": "v"},
            "body": {"var": "data.order"},
            "body_format": "form",
            "output": "data.response",
            "response_format": "text",
            "timeout_ms": 500
        }))
        .expect("compiles");
        assert_eq!(cfg.connector, "api");
        assert_eq!(cfg.method, "POST");
        assert!(cfg.path.as_ref().is_some_and(|p| p.constant().is_none()));
        assert_eq!(cfg.headers.len(), 2);
        assert!(cfg.headers["X-Static"].constant().is_some());
        let output = cfg.output.as_ref().and_then(Output::constant);
        assert_eq!(
            output.map(ToString::to_string).as_deref(),
            Some("data.response")
        );
        assert_eq!(
            cfg.timeout_ms.constant(),
            Some(&OwnedDataValue::from(&json!(500)))
        );
        assert!(cfg.primary_output().is_some());

        // 3.15's defaults: GET, 30 s, and no output, which writes nothing.
        let cfg = config(json!({"connector": "api"})).expect("compiles");
        assert_eq!(cfg.method, "GET");
        assert_eq!(
            cfg.timeout_ms.constant(),
            Some(&OwnedDataValue::from(&json!(30000)))
        );
        assert!(cfg.output.is_none() && cfg.primary_output().is_none());
        assert!(cfg.path.is_none() && cfg.body.is_none() && cfg.headers.is_empty());
    }

    /// The config loads what 3.15's `HttpCallConfig` deserialized and
    /// refuses what it refused — a `null` where serde wanted a map or a
    /// method, an unknown key, both spellings of an alias — but for the
    /// named breaks, each refused here where 3.15 loaded it: a computed
    /// `connector` (the shapes' note), an alias on its own (I186), and an
    /// `output` that is not a path (an `Output`).
    #[test]
    fn the_load_verdict_is_3_15s_but_for_the_named_breaks() {
        let loads = [
            json!({"connector": "api"}),
            json!({"connector": "api", "method": "DELETE", "timeout_ms": null, "output": null}),
            json!({"connector": "api", "path": null, "body": null, "body_format": null,
                   "response_format": null}),
            json!({"connector": "api", "headers": {"X": 1, "Y": null, "Z": {"var": "data.z"}}}),
            json!({"connector": "api", "body": {"cat": ["a"]}, "body_format": 7,
                   "timeout_ms": "soon", "output": {"cat": ["data.", "x"]}}),
        ];
        let refused_alike = [
            json!({}),
            json!({"connector": "api", "headers": null}),
            json!({"connector": "api", "headers": [1]}),
            json!({"connector": "api", "method": "get"}),
            json!({"connector": "api", "method": null}),
            json!({"connector": "api", "unknown": 1}),
            json!({"connector": "api", "path": "/x", "path_logic": "/y"}),
            json!({"connector": "api", "output": "data.x", "response_path": "data.y"}),
        ];
        let breaks = [
            json!({"connector": 7}),
            json!({"connector": {"var": "data.api"}}),
            json!({"connector": "api", "path_logic": "/x"}),
            json!({"connector": "api", "body_logic": {"a": 1}}),
            json!({"connector": "api", "response_path": "data.r"}),
            json!({"connector": "api", "output": 7}),
        ];
        let v3 = |input: &Value| serde_json::from_value::<V3HttpCallConfig>(input.clone()).is_ok();
        for input in loads {
            assert!(v3(&input), "3.15 loads {input}");
            assert!(config(input.clone()).is_ok(), "{input}");
        }
        for input in refused_alike {
            assert!(!v3(&input), "3.15 refuses {input}");
            assert!(config(input.clone()).is_err(), "{input}");
        }
        for input in breaks {
            assert!(v3(&input), "3.15 loads {input}");
            assert!(config(input.clone()).is_err(), "{input}");
        }
    }

    /// `connector` is a literal (the shapes' note): a computed one is refused
    /// at load, and a task without one is Rejected (I183).
    #[test]
    fn the_connector_is_a_required_literal() {
        let issues = refused(json!({"connector": {"var": "data.api"}}));
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(
            (issues[0].0.as_str(), issues[0].1),
            ("connector", IssueCode::InputParse)
        );
        let issues = refused(json!({"method": "GET"}));
        assert_eq!(
            issues,
            [(
                "connector".to_string(),
                IssueCode::InputParse,
                "missing field `connector`".to_string()
            )]
        );
    }

    /// Break 2a: an unknown key is refused. 3.15's aliases are among them
    /// here, since the loader rewrites them before the config sees a task
    /// (I186).
    #[test]
    fn an_unknown_key_is_refused_and_the_aliases_are_the_loaders() {
        for key in ["path_logic", "body_logic", "response_path", "retries"] {
            let issues = refused(json!({"connector": "api", key: "x"}));
            assert_eq!(issues.len(), 1, "{key}: {issues:?}");
            assert_eq!(issues[0].0, key);
            assert!(
                matches!(issues[0].1, IssueCode::UnknownKey | IssueCode::LikelyTypo),
                "{key}: {issues:?}"
            );
        }
    }

    /// The method vocabulary is 3.15's `HttpMethod`, idempotency included,
    /// and what it refused is refused with its text.
    #[test]
    fn the_method_is_3_15s_with_its_refusal() {
        let names: Vec<&str> = dataflow_rs_v3::HttpMethod::ALL
            .iter()
            .map(dataflow_rs_v3::HttpMethod::as_str)
            .collect();
        assert_eq!(names, METHODS);
        for method in dataflow_rs_v3::HttpMethod::ALL {
            config(json!({"connector": "api", "method": method.as_str()})).expect("compiles");
            assert_eq!(
                is_idempotent(method.as_str()),
                method.is_idempotent(),
                "{method}"
            );
        }
        for method in ["get", "Post", "HEAD", "OPTIONS", ""] {
            let v3 = serde_json::from_value::<dataflow_rs_v3::HttpMethod>(json!(method))
                .expect_err("3.15 refuses it")
                .to_string();
            let issues = refused(json!({"connector": "api", "method": method}));
            assert_eq!(
                issues,
                [("method".to_string(), IssueCode::InputParse, v3)],
                "{method:?}"
            );
        }
        // Not a string at all, which 3.15 refused too.
        assert!(serde_json::from_value::<dataflow_rs_v3::HttpMethod>(Value::Null).is_err());
        assert_eq!(
            refused(json!({"connector": "api", "method": null}))[0].0,
            "method"
        );
    }

    // -- The call, against 3.15's handler --

    /// An HTTP upstream on a free loopback port.
    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        format!("http://{addr}")
    }

    /// An upstream answering every request with what it received: the
    /// method, the path and query, the `x-` and content-type headers, and the
    /// body as text. `/slow` answers after two seconds, `/teapot` with a 418,
    /// and `/plain` with a text body.
    async fn echo() -> String {
        async fn answer(
            method: axum::http::Method,
            uri: axum::http::Uri,
            headers: axum::http::HeaderMap,
            body: String,
        ) -> axum::response::Response {
            use axum::response::IntoResponse;
            match uri.path() {
                "/slow" => tokio::time::sleep(Duration::from_secs(2)).await,
                "/teapot" => {
                    return (axum::http::StatusCode::IM_A_TEAPOT, "short and stout")
                        .into_response();
                }
                "/plain" => return "plain text".into_response(),
                _ => {}
            }
            let picked: serde_json::Map<String, Value> = headers
                .iter()
                .filter(|(k, _)| k.as_str().starts_with("x-") || k.as_str() == "content-type")
                .map(|(k, v)| (k.to_string(), json!(v.to_str().unwrap_or_default())))
                .collect();
            axum::Json(json!({
                "method": method.as_str(),
                "uri": uri.to_string(),
                "headers": picked,
                "body": body,
            }))
            .into_response()
        }
        serve(axum::Router::new().fallback(answer)).await
    }

    /// An HTTP connector to `url`, retrying twice a millisecond apart.
    fn connector(url: &str) -> Value {
        json!({
            "type": "http",
            "url": url,
            "allow_private_urls": true,
            "retry": {"max_retries": 2, "retry_delay_ms": 1}
        })
    }

    fn handler(registry: Arc<ConnectorRegistry>) -> HttpCallHandler {
        HttpCallHandler {
            registry,
            client: reqwest::Client::new(),
        }
    }

    fn function(registry: Arc<ConnectorRegistry>) -> HttpCallFunction {
        HttpCallFunction {
            registry,
            client: reqwest::Client::new(),
        }
    }

    /// One task through both engines, each over a fresh registry holding
    /// `connectors`: dataflow-rs 3's, then dataflow-rs 4's.
    async fn both(connectors: &[(&str, Value)], input: Value, data: Value) -> (Seen, Seen) {
        let v3 = twin::v3(
            NAME,
            Box::new(Connector(handler(twin::connectors(connectors).await))),
            input.clone(),
            data.clone(),
        )
        .await;
        let v4 = twin::v4(
            NAME,
            function(twin::connectors(connectors).await),
            input,
            data,
        )
        .await;
        (v3, v4)
    }

    /// Every field resolves as 3.15's `HttpCallConfig` resolved it: the path,
    /// the headers and the body over each kind of value, the formats, a
    /// computed output, a `null` read as unset, and the refusals of a bad
    /// format or timeout, each with its code and text.
    #[tokio::test]
    async fn every_field_reads_as_3_15s_config_read_it() {
        let url = echo().await;
        let connectors = [("api", connector(&url))];
        let data = json!({"id": 7, "obj": {"k": [1, 2]}, "order": {"sku": "A1", "qty": 2}});
        let cases = [
            json!({"connector": "api", "output": "data.r"}),
            json!({"connector": "api", "path": {"cat": ["/orders/", {"var": "data.id"}]}, "output": "data.r"}),
            json!({"connector": "api", "path": {"var": "data.id"}, "output": "data.r"}),
            json!({"connector": "api", "path": {"var": "data.obj"}, "output": "data.r"}),
            json!({"connector": "api", "path": "/search?q=1", "output": "data.r"}),
            json!({"connector": "api", "path": null, "body": null, "body_format": null,
                   "response_format": null, "output": "data.r"}),
            json!({"connector": "api", "headers": {"X-Id": {"var": "data.id"}, "X-Num": 7,
                   "X-Null": null, "X-Obj": {"var": "data.obj"}, "X-Missing": {"var": "data.none"}},
                   "output": "data.r"}),
            json!({"connector": "api", "method": "POST", "body": {"var": "data.order"},
                   "output": "data.r"}),
            json!({"connector": "api", "method": "PUT", "body": {"var": "data.order"},
                   "body_format": "form", "output": "data.r"}),
            json!({"connector": "api", "method": "PATCH", "body": "raw text",
                   "body_format": {"cat": ["te", "xt"]}, "output": "data.r"}),
            json!({"connector": "api", "method": "DELETE", "body": [1, 2], "output": "data.r"}),
            json!({"connector": "api", "path": "/plain", "response_format": "text",
                   "output": "data.r"}),
            json!({"connector": "api", "output": {"cat": ["data.", "computed"]}}),
            json!({"connector": "api", "body_format": "xml", "output": "data.r"}),
            json!({"connector": "api", "response_format": "yaml", "output": "data.r"}),
            json!({"connector": "api", "body": 7, "body_format": "text", "output": "data.r"}),
            json!({"connector": "api", "body": [1], "body_format": "form", "output": "data.r"}),
            json!({"connector": "api", "timeout_ms": "soon", "output": "data.r"}),
            json!({"connector": "api", "timeout_ms": -1, "output": "data.r"}),
            json!({"connector": "api", "timeout_ms": {"var": "data.none"}, "output": "data.r"}),
            json!({"connector": "api", "path": "/teapot", "output": "data.r"}),
        ];
        for input in cases {
            let (v3, v4) = both(&connectors, input.clone(), data.clone()).await;
            assert_eq!(v4, v3, "{input}");
        }
    }

    /// The connector lookups and the method gate refuse as 3.15's did.
    #[tokio::test]
    async fn the_connector_and_its_gate_refuse_as_3_15s_did() {
        let url = echo().await;
        let mut gated = connector(&url);
        gated["operations"] = json!({"methods": ["GET"]});
        let connectors = [
            ("api", gated),
            (
                "mail",
                json!({"type": "smtp", "host": "127.0.0.1", "from": "a@b.test"}),
            ),
        ];
        for input in [
            json!({"connector": "missing"}),
            json!({"connector": "mail"}),
            json!({"connector": "api", "method": "POST"}),
            json!({"connector": "api", "method": "GET", "output": "data.r"}),
        ] {
            let (v3, v4) = both(&connectors, input.clone(), json!({})).await;
            assert_eq!(v4, v3, "{input}");
        }
    }

    /// `seen` with the upstream `url` a failure names spelled `<upstream>`,
    /// for comparing two sides that each ran their own upstream.
    fn upstream_unnamed(seen: Seen, url: &str) -> Seen {
        seen.map_err(|failure| Failure {
            message: failure.message.replace(url, "<upstream>"),
            ..failure
        })
    }

    /// An upstream answering 503 `fails` times, then `{"ok": true}`; the
    /// requests it has seen.
    async fn flaky(fails: usize) -> (String, Arc<AtomicUsize>) {
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        let app = axum::Router::new().fallback(move || {
            let counter = Arc::clone(&counter);
            async move {
                use axum::response::IntoResponse;
                if counter.fetch_add(1, Ordering::SeqCst) < fails {
                    (axum::http::StatusCode::SERVICE_UNAVAILABLE, "busy").into_response()
                } else {
                    axum::Json(json!({"ok": true})).into_response()
                }
            }
        });
        (serve(app).await, seen)
    }

    /// The retry: an idempotent method is retried by the connector's policy,
    /// a POST is not unless the connector opts in, and each engine makes the
    /// same number of attempts to the same answer.
    #[tokio::test]
    async fn the_retry_is_3_15s() {
        let cases = [
            // (method, retry_non_idempotent, failures, attempts expected)
            ("GET", false, 2, 3),
            ("PUT", false, 2, 3),
            ("GET", false, 5, 3),
            ("POST", false, 2, 1),
            ("POST", true, 2, 3),
        ];
        for (method, opt_in, fails, attempts) in cases {
            let input = json!({"connector": "api", "method": method, "output": "data.r"});
            let mut sides = Vec::new();
            for v4 in [false, true] {
                let (url, seen) = flaky(fails).await;
                let mut conn = connector(&url);
                conn["retry_non_idempotent"] = json!(opt_in);
                let registry = twin::connectors(&[("api", conn)]).await;
                let result = if v4 {
                    twin::v4(NAME, function(registry), input.clone(), json!({})).await
                } else {
                    let handler = Box::new(Connector(handler(registry)));
                    twin::v3(NAME, handler, input.clone(), json!({})).await
                };
                sides.push((upstream_unnamed(result, &url), seen.load(Ordering::SeqCst)));
            }
            assert_eq!(
                sides[1], sides[0],
                "{method}, opt-in {opt_in}, {fails} failures"
            );
            assert_eq!(
                sides[1].1, attempts,
                "{method}, opt-in {opt_in}, {fails} failures"
            );
        }
    }

    /// A fake IdP issuing `tok-<n>`, and an API that refuses `tok-1` with a
    /// 401 as a revoked token and answers any other; how many tokens the IdP
    /// has issued.
    async fn oauth_pair() -> (String, String, Arc<AtomicUsize>) {
        let issued = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&issued);
        let idp = axum::Router::new().route(
            "/token",
            axum::routing::post(move || {
                let counter = Arc::clone(&counter);
                async move {
                    let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    axum::Json(json!({
                        "access_token": format!("tok-{n}"),
                        "token_type": "Bearer",
                        "expires_in": 3600
                    }))
                }
            }),
        );
        let api = axum::Router::new().fallback(|headers: axum::http::HeaderMap| async move {
            use axum::response::IntoResponse;
            let token = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            if token == "Bearer tok-1" {
                (axum::http::StatusCode::UNAUTHORIZED, "revoked").into_response()
            } else {
                axum::Json(json!({"token": token})).into_response()
            }
        });
        (serve(idp).await, serve(api).await, issued)
    }

    /// An HTTP connector to `api` with managed OAuth2 against `idp`, its
    /// token manager running.
    async fn oauth_registry(api: &str, idp: &str) -> Arc<ConnectorRegistry> {
        let conn = json!({
            "type": "http",
            "url": api,
            "allow_private_urls": true,
            "retry": {"max_retries": 0, "retry_delay_ms": 1},
            "auth": {
                "type": "oauth2",
                "grant": "client_credentials",
                "token_url": format!("{idp}/token"),
                "client_id": "svc",
                "client_secret": "secret"
            }
        });
        let registry = twin::connectors(&[("partner", conn)]).await;
        registry
            .oauth()
            .init(crate::connector::oauth::OAuthRuntimeDeps {
                http_client: reqwest::Client::new(),
                repo: Arc::new(crate::connector::test_support::StubConnectorRepo::with(
                    vec![],
                )),
                lease: None,
            });
        registry
    }

    /// The OAuth path (#268): the token is acquired and applied, a 401 drops
    /// it, and the next call acquires a fresh one — the same calls, tokens
    /// and answers on both engines.
    #[tokio::test]
    async fn a_401_drops_the_managed_token_as_3_15s_handler_did() {
        let input = json!({"connector": "partner", "output": "data.r"});
        let mut sides = Vec::new();
        for v4 in [false, true] {
            let (idp, api, issued) = oauth_pair().await;
            let registry = oauth_registry(&api, &idp).await;
            let mut calls = Vec::new();
            for _ in 0..3 {
                let seen = if v4 {
                    twin::v4(
                        NAME,
                        function(Arc::clone(&registry)),
                        input.clone(),
                        json!({}),
                    )
                    .await
                } else {
                    let handler = Box::new(Connector(handler(Arc::clone(&registry))));
                    twin::v3(NAME, handler, input.clone(), json!({})).await
                };
                calls.push(upstream_unnamed(seen, &api));
            }
            sides.push((calls, issued.load(Ordering::SeqCst)));
        }
        assert_eq!(sides[1], sides[0]);
        let (calls, issued) = &sides[1];
        assert_eq!(
            calls[0].as_ref().err().map(|f| f.code.as_str()),
            Some("HTTP_ERROR")
        );
        assert_eq!(
            calls[1].as_ref().ok().map(|d| &d["r"]["token"]),
            Some(&json!("Bearer tok-2"))
        );
        assert_eq!(
            *issued, 2,
            "the 401 dropped the first token, the second was reused"
        );
    }

    /// With no `output` the response is discarded: nothing is written.
    #[tokio::test]
    async fn an_unset_output_writes_nothing() {
        let url = echo().await;
        let connectors = [("api", connector(&url))];
        let input = json!({"connector": "api", "path": "/orders"});
        let (v3, v4) = both(&connectors, input.clone(), json!({"keep": 1})).await;
        assert_eq!(v4, v3);
        assert_eq!(v4, Ok(json!({"keep": 1})));

        let called = testing::call(function(twin::connectors(&connectors).await), input)
            .exprs(kit::test_exprs())
            .name(NAME)
            .await;
        assert_eq!(called.outcome.expect("the request is sent"), Outcome::Ok);
        assert!(called.writes.is_empty(), "{:?}", called.writes);
    }

    /// `timeout_ms` bounds one attempt, and a timeout is 3.15's retryable
    /// `TIMEOUT_ERROR`, with its text.
    #[tokio::test]
    async fn timeout_ms_bounds_an_attempt_as_3_15s_did() {
        let url = echo().await;
        let mut conn = connector(&url);
        conn["retry"] = json!({"max_retries": 0, "retry_delay_ms": 1});
        let connectors = [("api", conn)];
        for timeout in [json!(100), json!({"+": [50, 50]})] {
            let input = json!({"connector": "api", "path": "/slow", "timeout_ms": timeout,
                               "output": "data.r"});
            let (v3, v4) = both(&connectors, input, json!({})).await;
            assert_eq!(v4, v3);
            let failure: Failure = v4.expect_err("the attempt times out");
            assert_eq!(failure.code, "TIMEOUT_ERROR");
            assert!(
                failure.retryable && failure.message.contains("timed out"),
                "{failure:?}"
            );
        }
    }

    /// The run's deadline (`ctx.deadline()`) ends a call still waiting on its
    /// upstream, as `run_for_channel`'s timeout ended one on dataflow-rs 3:
    /// the call ends `TimedOut`, well before its own `timeout_ms`, and
    /// nothing is written.
    #[tokio::test]
    async fn the_runs_deadline_ends_a_waiting_call() {
        let url = echo().await;
        let registry = twin::connectors(&[("api", connector(&url))]).await;
        let started = std::time::Instant::now();
        let called = testing::call(
            function(registry),
            json!({"connector": "api", "path": "/slow", "timeout_ms": 5000, "output": "data.r"}),
        )
        .exprs(kit::test_exprs())
        .name(NAME)
        .deadline(dataflow_rs::run::Instant::now() + Duration::from_millis(150))
        .await;
        let error = called.outcome.expect_err("the deadline passes first");
        assert_eq!(*error.kind(), dataflow_rs::run::ErrorKind::TimedOut);
        assert!(called.writes.is_empty());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }
}
