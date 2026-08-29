use std::sync::Arc;
use std::time::Duration;

use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::{Map, Value};

use crate::connector::ConnectorTarget;
use crate::connector::{
    ConnectorConfig, ConnectorRegistry, EsConnectorConfig, HttpOperationGates, OperationGates,
};
use crate::engine::{ErrorClass, HandlerError};
use crate::query::EntityRegistry;

/// Build the dialect's `EntityRegistry` for one `data_query` / `data_write`
/// call: parse the task's optional inline `schema`, then apply the connector's
/// operator-owned guards to it (F24).
///
/// Both handlers took the same two lines before this — `from_json`, else
/// `default()` — and neither consulted the connector at all. The order matters:
/// `require_schema` judges what the task actually declared, and the allowlist is
/// installed afterwards so it cannot be part of what is being judged.
pub fn build_entity_registry(
    schema: Option<&Value>,
    connector_config: &ConnectorConfig,
    connector_name: &str,
) -> Result<EntityRegistry, DataflowError> {
    let mut registry = match schema {
        Some(s) => EntityRegistry::from_json(s)?,
        None => EntityRegistry::default(),
    };
    if let Some(guards) = connector_config.dialect_guards() {
        if !guards.schema_is_sufficient(!registry.is_empty(), registry.is_identity_mode()) {
            return Err(crate::errors::connector_detail_error(format!(
                "connector '{connector_name}' requires a declared schema \
                 (dialect.require_schema): supply \"schema\" with an \"entities\" map \
                 and without \"unmapped\": \"identity\""
            )));
        }
        registry.restrict_to(&guards.allowed_entities);
    }
    Ok(registry)
}

/// Reject the call when the connector's operation gates disable `op` — the
/// per-connector en/disable switch for read / insert / update / delete /
/// upsert / raw_write (see [`OperationGates`]).
pub fn require_op_allowed(
    gates: &OperationGates,
    op: &str,
    connector_name: &str,
) -> Result<(), DataflowError> {
    require_op(gates.allows(op), op, connector_name)
}

/// [`require_op_allowed`] for the gates that are not the db/es set: the cache
/// (`read` / `write`) and Kafka (`publish`) gates each have their own struct,
/// because a `raw_write` flag on a cache would be a field nothing reads (F22e).
/// The refusal is worded identically whatever the connector type.
pub fn require_op(allowed: bool, op: &str, connector_name: &str) -> Result<(), DataflowError> {
    if !allowed {
        return Err(crate::errors::connector_detail_error(format!(
            "operation '{op}' is disabled on connector '{connector_name}'"
        )));
    }
    Ok(())
}

/// Reject the call when an HTTP connector's method allow-list excludes
/// `method` (F22e). An empty list allows everything, which is what a connector
/// authored before the gate existed keeps meaning.
pub fn require_method_allowed(
    gates: &HttpOperationGates,
    method: &str,
    connector_name: &str,
) -> Result<(), DataflowError> {
    if !gates.allows_method(method) {
        return Err(crate::errors::connector_detail_error(format!(
            "HTTP method '{method}' is not allowed on connector \
             '{connector_name}' (allowed: {})",
            gates.methods.join(", ")
        )));
    }
    Ok(())
}

/// Build an Elasticsearch HTTP request with the connector's auth and timeout
/// applied. Shared by the `data_query` search path and the `data_write` write
/// path. Enforces the same SSRF pre-check as `execute_request` unless the
/// connector opts out via `allow_private_urls`.
pub async fn es_request(
    client: &reqwest::Client,
    es: &EsConnectorConfig,
    method: reqwest::Method,
    url: &str,
) -> Result<reqwest::RequestBuilder, DataflowError> {
    if !es.allow_private_urls
        && let Err(msg) = crate::validation::validate_url_not_private(url).await
    {
        return Err(DataflowError::function_execution(
            format!("SSRF protection: {msg}"),
            None,
        ));
    }

    let mut req = client.request(method, url);
    if let Some(auth) = &es.auth {
        req = super::http_common::apply_auth(req, auth);
    }
    if let Some(ms) = es.request_timeout_ms {
        req = req.timeout(Duration::from_millis(ms));
    }
    Ok(req)
}

/// Read an ES response body as JSON, enforcing the connector's
/// `max_response_size` (F12) — the same guard `execute_request` applies to
/// `http_call` responses. Without it a large `_search` result was buffered
/// wholesale.
pub async fn read_es_body(
    resp: reqwest::Response,
    max_size: usize,
) -> Result<Value, DataflowError> {
    if let Some(len) = resp.content_length()
        && len as usize > max_size
    {
        return Err(DataflowError::function_execution(
            format!(
                "Elasticsearch response declared Content-Length {len} exceeds \
                 limit of {max_size} bytes"
            ),
            None,
        ));
    }
    let bytes = resp.bytes().await.map_err(to_exec_error)?;
    if bytes.len() > max_size {
        return Err(DataflowError::function_execution(
            format!(
                "Elasticsearch response body is {} bytes, exceeding limit of {max_size} bytes",
                bytes.len()
            ),
            None,
        ));
    }
    serde_json::from_slice(&bytes).map_err(|e| to_exec_error(e).into())
}

/// Send an ES request and parse its JSON body. Returns the status alongside so
/// callers can treat specific non-2xx statuses as semantic (`op_type=create`
/// treats 409 as the "conflict → do nothing" no-op).
pub async fn send_es(
    req: reqwest::RequestBuilder,
    max_response_size: usize,
) -> Result<(reqwest::StatusCode, Value), DataflowError> {
    // `without_url`: a transport failure's `Display` carries the endpoint it
    // was given, and an ES connector URL can hold credentials in userinfo or
    // the query (#281). The connector name is the diagnostic here.
    let resp = req
        .send()
        .await
        .map_err(|e| to_exec_error(e.without_url()))?;
    let status = resp.status();
    let body: Value = read_es_body(resp, max_response_size).await?;
    Ok((status, body))
}

/// Uniform error for a non-2xx Elasticsearch write response.
pub fn es_write_error(status: reqwest::StatusCode, body: &Value) -> DataflowError {
    DataflowError::function_execution(
        format!("Elasticsearch write failed ({status}): {body}"),
        None,
    )
}

/// The prologue every connector handler runs before its own body.
///
/// F48: the same six steps — resolve-before-borrow, the `connector` field, the
/// operation gate, the pool fetch, the output path, the observability shell —
/// were written out in seven handlers, with the handler-name string literal
/// repeated three to six times per file. A typo in one of those copies is
/// invisible to the compiler and shows up as a metric label or an error message
/// naming the wrong function. Now it is named once, in [`ConnectorCall::begin`],
/// and read back off `self.name` everywhere else.
///
/// The split between `begin` and [`ConnectorCall::run`] is not cosmetic: `begin`
/// reads only the handler's *own literal keys* and the message channel, both of
/// which must happen before the body borrows `ctx` mutably.
pub struct ConnectorCall<'a> {
    /// The handler's name, as it appears in metrics, profiles and errors.
    pub name: &'static str,
    /// The connector this call targets, from the task's literal `connector` key.
    pub connector: &'a str,
    /// The channel the message arrived on — read before the body takes `ctx`
    /// mutably, which is why it is owned.
    pub channel: String,
    /// Where the handler's result is written, defaulting to `data`.
    pub output: &'a str,
}

impl<'a> ConnectorCall<'a> {
    /// Read the literal prologue: the `connector` key, the `output` path, and
    /// the message's channel.
    ///
    /// F58: call this **first**, ahead of any message-dependent resolution. The
    /// handlers used to resolve `key` / `filter` / `params` against the message
    /// before checking that a `connector` was even named, so a task missing both
    /// reported the wrong one — the author fixed `key`, re-ran, and only then
    /// learned about `connector`. Cheap literal checks are also the ones whose
    /// failure is unambiguous: nothing about the message can change the answer.
    /// Generic over the input's shape, not over `Value`: `http_call` and
    /// `publish_kafka` take dataflow-rs's typed configs, and the prologue is
    /// the same question for them. See
    /// [`ConnectorInput`](super::connector_handler::ConnectorInput).
    pub fn begin<I: super::connector_handler::ConnectorInput>(
        name: &'static str,
        input: &'a I,
        ctx: &TaskContext<'_>,
    ) -> Result<Self, DataflowError> {
        Ok(Self {
            name,
            connector: input.connector(name)?,
            channel: super::extract_channel(ctx.message()).to_string(),
            output: input.output(),
        })
    }

    /// [`require_str_field`] with the handler name already filled in.
    pub fn require_str<'i>(&self, input: &'i Value, field: &str) -> Result<&'i str, DataflowError> {
        require_str_field(input, field, self.name)
    }

    /// Resolve the target connector and apply its operation gate, if it has
    /// one and the call names an operation.
    ///
    /// `op` is `None` for the handlers with nothing to gate (`http_call`,
    /// `publish_kafka`, the cache pair) and `Some(_)` for the data handlers,
    /// where `data_write` only knows its op after parsing the envelope and so
    /// passes `None` here and gates separately.
    pub async fn resolve(
        &self,
        registry: &ConnectorRegistry,
        op: Option<&str>,
    ) -> Result<Arc<ConnectorConfig>, DataflowError> {
        let config = resolve_connector(registry, self.connector).await?;
        if let Some(op) = op
            && let Some(gates) = config.operation_gates()
        {
            require_op_allowed(gates, op, self.connector)?;
        }
        Ok(config)
    }

    /// Run the handler body inside the shared observability + circuit-breaker
    /// shell ([`guarded_handler`]).
    pub async fn run<F>(
        &self,
        registry: &ConnectorRegistry,
        fut: F,
    ) -> dataflow_rs::Result<TaskOutcome>
    where
        F: std::future::Future<Output = dataflow_rs::Result<TaskOutcome>>,
    {
        guarded_handler(self.name, registry, self.connector, &self.channel, fut).await
    }
}

/// [`observed_handler_named`] plus the circuit breaker (F6).
///
/// The breaker used to wrap exactly one of the nine egress paths — `http_call`.
/// `db_read`, `db_write`, `data_query`, `data_write`, `mongo_read`,
/// `cache_read`, `cache_write` and `publish_kafka` reached their pools
/// directly, so `[engine.circuit_breaker]` read as global resilience while a
/// hung Postgres or Redis pinned every worker.
///
/// **Only retryable failures trip it.** That distinction is what makes this
/// safe to apply to a database: a query the backend *rejected* — a syntax
/// error, a constraint violation, a row-cap breach — says nothing about the
/// dependency's health, and counting it would let one bad workflow trip the
/// breaker on a perfectly healthy database and take down every other channel
/// using it. F42's taxonomy is what makes "retryable" mean "the dependency is
/// in trouble" rather than "something went wrong".
///
/// A no-op when `engine.circuit_breaker.enabled` is false (the default), which
/// leaves the observability in [`observed_handler_named`] unconditional.
pub async fn guarded_handler<F>(
    fn_name: &'static str,
    registry: &ConnectorRegistry,
    connector: &str,
    channel: &str,
    fut: F,
) -> dataflow_rs::Result<TaskOutcome>
where
    F: std::future::Future<Output = dataflow_rs::Result<TaskOutcome>>,
{
    if !registry.circuit_breaker_enabled() {
        return observed_handler_named(fn_name, connector, channel, fut).await;
    }

    // Same key shape as the pre-existing `http_call` path, so an operator's
    // `channel:connector` keys keep meaning what they meant.
    let breaker = registry
        .get_or_create_breaker(&format!("{channel}:{connector}"))
        .await;
    if !breaker.check() {
        crate::metrics::record_circuit_breaker_rejection(connector, channel);
        return Err(crate::errors::circuit_open_dataflow_error(
            connector, channel,
        ));
    }

    let result = observed_handler_named(fn_name, connector, channel, fut).await;
    match &result {
        Ok(_) => breaker.record_success(),
        Err(e) if e.retryable() => {
            if breaker.record_failure() {
                tracing::warn!(
                    connector = connector,
                    channel = channel,
                    "Circuit breaker tripped"
                );
                crate::metrics::record_circuit_breaker_trip(connector, channel);
            }
        }
        // A failure the caller caused is not evidence about the dependency.
        Err(_) => {}
    }
    result
}

/// The profile sample plus the connector request/latency metrics (F40) — the
/// inner layer of the shell, reached through [`guarded_handler`].
///
/// `connector_requests_total` and `connector_request_duration_seconds` used to
/// be emitted from exactly one place — inside the circuit breaker — which
/// `http_call` reaches only when `engine.circuit_breaker.enabled` is true. That
/// defaults to **false**, so a default install emitted **zero** connector-level
/// counts or latencies for *any* of the ten handlers: every external dependency
/// was dark in Prometheus until an operator flipped an unrelated resilience
/// flag.
///
/// Observability is not conditional on resilience config, so every connector
/// handler reaches this unconditionally; the breaker stays an inner, optional
/// layer above it.
///
/// `channel` must be read from the message *before* the handler body takes
/// `ctx` mutably.
pub async fn observed_handler_named<F>(
    fn_name: &'static str,
    connector: &str,
    channel: &str,
    fut: F,
) -> dataflow_rs::Result<TaskOutcome>
where
    F: std::future::Future<Output = dataflow_rs::Result<TaskOutcome>>,
{
    let start = std::time::Instant::now();
    let result = crate::engine::profile::record(fn_name, Some(connector), fut).await;
    let status = if result.is_ok() { "ok" } else { "error" };
    crate::metrics::record_connector_request(connector, channel, status);
    crate::metrics::record_connector_duration(connector, channel, start.elapsed().as_secs_f64());
    result
}

/// Extracts the `output` field from the input JSON, defaulting to `"data"`.
pub fn extract_output_path(input: &Value) -> &str {
    input
        .get("output")
        .and_then(|v| v.as_str())
        .unwrap_or("data")
}

/// The backend was reached and the operation failed.
///
/// Kept as a named constructor because `.map_err(to_exec_error)` reads better
/// at a call site than a struct literal; the variant it becomes, and the retry
/// policy that variant implies, are decided once in [`HandlerError`]'s
/// `Into<DataflowError>`.
pub fn to_exec_error(e: impl std::fmt::Display) -> HandlerError {
    HandlerError::new(ErrorClass::Backend, e)
}

/// A failure to *reach* a backend — pool acquisition, connection setup, DNS.
///
/// `DataflowError::Io` rather than `FunctionExecution` because dataflow-rs
/// classifies the latter (with `source: None`) as **not retryable**, while
/// `Io` is. Before F42 every non-HTTP connector failure went through
/// [`to_exec_error`], so a dead Postgres, Redis or MongoDB was a non-retryable
/// 500 while the *identical* HTTP outage was a retryable `Io` — DLQ retry
/// policy diverged by backend for no principled reason.
///
/// Use for "could not connect"; keep [`to_exec_error`] for "connected, and the
/// query failed", which is genuinely not worth retrying.
pub fn to_connect_error(e: impl std::fmt::Display) -> HandlerError {
    HandlerError::new(ErrorClass::Connector, e)
}

/// A caller-fixable limit or shape problem, e.g. a result set over
/// `query.max_limit`.
///
/// `Validation` maps to 400 with the message preserved. Routing these through
/// [`to_exec_error`] made them 500 `ENGINE_ERROR` with the text replaced, so
/// deliberately helpful guidance — *"add a LIMIT to the query or raise the
/// cap"* — was sanitised away exactly when the caller needed it (F42).
pub fn to_limit_error(message: impl std::fmt::Display) -> HandlerError {
    HandlerError::new(ErrorClass::Limit, message)
}

/// Why a [`timed_query`] operation failed, decided where it is known.
///
/// The distinction matters: a limit the caller can fix is a 400 with its text
/// intact, a backend failure is a 500 with the text sanitised. Only the
/// operation itself can tell them apart, and it used to say so by **prefixing
/// its error string** with a marker the wrapper looked for and stripped
/// (F42) — control flow through a string, in the one place a value under a
/// `Result`'s `Err` was already available to carry it. A message that happened
/// to start with the marker would have been misread as a limit; a limit whose
/// text was reformatted anywhere in between would have silently become a 500.
///
/// `From<String>` maps to [`Self::Backend`], so an operation with nothing to
/// distinguish keeps returning `Result<_, String>` and reads the same.
#[derive(Debug)]
pub enum QueryFailure {
    /// The backend was reached and the operation failed. A 500 with the text
    /// replaced — a driver error names hosts, tables and sometimes values.
    Backend(String),
    /// A limit the caller can fix — a result set over `query.max_limit`, say.
    /// A 400 with the message intact, because the message is the guidance:
    /// *"add a LIMIT to the query or raise the cap"* is useless once
    /// sanitised.
    Limit(String),
}

impl From<String> for QueryFailure {
    fn from(message: String) -> Self {
        Self::Backend(message)
    }
}

/// A driver error is always a backend failure — the query reached the server
/// and the server refused it. Spelled out so `?` keeps working inside an
/// operation, which is the idiom every one of them uses.
impl From<sqlx::Error> for QueryFailure {
    fn from(e: sqlx::Error) -> Self {
        Self::Backend(e.to_string())
    }
}

impl std::fmt::Display for QueryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(m) | Self::Limit(m) => f.write_str(m),
        }
    }
}

/// Extracts a required string field from a JSON value, returning a validation
/// error that names the handler and field on failure.
pub fn require_str_field<'a>(
    input: &'a Value,
    field: &str,
    handler_name: &str,
) -> Result<&'a str, DataflowError> {
    input.get(field).and_then(|v| v.as_str()).ok_or_else(|| {
        DataflowError::Validation(format!("{handler_name} requires '{field}' field"))
    })
}

/// A connector targets MongoDB when its connection string uses a `mongodb`
/// scheme; otherwise it is a SQL connector (dialect from the URL scheme).
pub use crate::connector::is_mongo_url as is_mongo;

/// Refuse a MongoDB connector for a handler that speaks SQL.
///
/// `require_db_connector` only checks the `ConnectorConfig` variant, and both
/// SQL and MongoDB connectors are `Db`. Without this, a `mongodb://` string
/// reached `AnyPool` and surfaced as an opaque driver error rather than a
/// validation one — the mirror of the `mongo_read` gap (proposal F29).
pub fn reject_mongo_connector(
    function: &str,
    connector_name: &str,
    db_config: &crate::connector::DbConnectorConfig,
) -> Result<(), DataflowError> {
    if is_mongo(&db_config.connection_string) {
        return Err(DataflowError::Validation(format!(
            "{function} requires a SQL connector, but '{connector_name}' is a MongoDB \
             connector — use mongo_read or data_query for MongoDB"
        )));
    }
    Ok(())
}

/// Looks up a connector by name in the registry, returning a function-execution
/// error if not found.
pub async fn resolve_connector(
    registry: &ConnectorRegistry,
    name: &str,
) -> Result<Arc<ConnectorConfig>, DataflowError> {
    registry.get(name).await.ok_or_else(|| {
        DataflowError::function_execution(format!("Connector '{name}' not found"), None)
    })
}

/// Borrow the typed config a handler needs, or say which type the connector
/// actually is.
///
/// One generic where there were six functions — `require_db_connector`,
/// `require_http_connector`, and four more — that differed only in the variant
/// they matched and the noun they printed. Both now come from the
/// [`ConnectorTarget`] impl, so an eighth connector type costs nothing here,
/// and the type parameter is what stops a handler asking for one kind and
/// binding another.
///
/// The bound is [`ConnectorTarget`] rather than `ConnectorKind` so that the two
/// handlers accepting more than one variant — the portable dialect's
/// `data_query` and `data_write`, via `DataBackend` — produce their wrong-type
/// refusal here with every other handler's, instead of after resolution in
/// their own words.
pub fn require_connector<'a, K: ConnectorTarget>(
    config: &'a ConnectorConfig,
    name: &str,
) -> Result<&'a K::Config, DataflowError> {
    K::extract(config).ok_or_else(|| {
        crate::errors::connector_detail_error(format!("Connector '{name}' is not {}", K::noun()))
    })
}

/// Writes a value at `output_path` in the message context via `TaskContext::set_json`,
/// which auto-records a `Change` on the audit trail when `capture_changes` is on.
pub fn apply_output(ctx: &mut TaskContext<'_>, output_path: &str, new_value: Value) {
    ctx.set_json(output_path, &new_value);
}

/// Fold `{"var": ..}` nodes in a workflow-authored input against the message
/// context. This is the single convention every connector handler uses to read
/// request data.
///
/// dataflow-rs precompiles a task's `input` once at engine build, so a handler
/// receives the literal workflow JSON rather than anything evaluated per
/// message. Handlers that need message data must therefore resolve it
/// themselves.
///
/// * `{"var": "data.id"}` → the value at that dot-path over the unified
///   `{data, metadata, temp_data}` context, or `null` when it does not resolve.
/// * `{"var": ["data.id", <default>]}` → the same, falling back to `<default>`
///   when the path is absent (JSONLogic's two-argument `var` form).
/// * Objects and arrays are walked recursively, so a `{"var": ..}` node is
///   folded wherever it appears — including inside a positional bind-parameter
///   array or a nested filter document.
/// * Every other value is a literal and is cloned unchanged.
///
/// Values pulled out of the message are **not** re-scanned, so request data can
/// never inject a `{"var": ..}` node of its own.
pub fn resolve_value(value: &Value, ctx: &TaskContext<'_>) -> Value {
    match value {
        Value::Object(o) => {
            if o.len() == 1
                && let Some(spec) = o.get("var")
            {
                return resolve_var(spec, ctx);
            }
            Value::Object(
                o.iter()
                    .map(|(k, v)| (k.clone(), resolve_value(v, ctx)))
                    .collect(),
            )
        }
        Value::Array(a) => Value::Array(a.iter().map(|v| resolve_value(v, ctx)).collect()),
        other => other.clone(),
    }
}

/// Look up the payload of a `{"var": ..}` node. Accepts the string form
/// (`"data.id"`) and the JSONLogic array form (`["data.id", <default>]`).
fn resolve_var(spec: &Value, ctx: &TaskContext<'_>) -> Value {
    let (path, default) = match spec {
        Value::String(p) => (p.as_str(), Value::Null),
        Value::Array(a) => match a.first().and_then(|v| v.as_str()) {
            Some(p) => (p, a.get(1).cloned().unwrap_or(Value::Null)),
            None => return Value::Null,
        },
        _ => return Value::Null,
    };
    ctx.get(path).map(Value::from).unwrap_or(default)
}

/// Resolve a `params` object into concrete values for the query/write dialects.
///
/// Thin wrapper over [`resolve_value`] that requires the result to be an
/// object. Shared by `data_query` and `data_write`, which fold the returned map
/// into the `{"param": ..}` nodes of a filter before translation.
pub fn resolve_params(params: Option<&Value>, ctx: &TaskContext<'_>) -> Map<String, Value> {
    match params.map(|p| resolve_value(p, ctx)) {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// Resolve a required input field and coerce the result to a string.
///
/// Scalars stringify; `null`, objects, and arrays are rejected so an
/// unresolvable `{"var": ..}` surfaces as an error instead of silently becoming
/// the literal key `"null"`.
pub fn resolve_required_str(
    input: &Value,
    field: &str,
    handler_name: &str,
    ctx: &TaskContext<'_>,
) -> Result<String, DataflowError> {
    let Some(raw) = input.get(field) else {
        return Err(DataflowError::Validation(format!(
            "{handler_name} requires '{field}' field"
        )));
    };
    match resolve_value(raw, ctx) {
        Value::String(s) => Ok(s),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        other => Err(DataflowError::Validation(format!(
            "{handler_name} '{field}' must resolve to a string or number, got {}",
            json_type_name(&other)
        ))),
    }
}

/// `"900"`-less duration spelling: `<n>` followed by one of `s m h d`.
pub fn parse_duration_secs(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (number, unit) = s.split_at(s.len().saturating_sub(1));
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => {
            return Err(format!(
                "'{s}' is not a duration — \"<n>s\", \"<n>m\", \"<n>h\" or \"<n>d\""
            ));
        }
    };
    let n: u64 = number.parse().map_err(|_| {
        format!("'{s}' is not a duration — the part before the unit must be a number")
    })?;
    n.checked_mul(multiplier)
        .ok_or_else(|| format!("'{s}' overflows"))
}

/// Seconds from an already-fetched duration value: integer seconds or a
/// [`parse_duration_secs`] string, either possibly a `{"var": ..}` node.
/// Bounds stay with the caller — they differ per field.
pub fn resolve_duration_secs(
    raw: &Value,
    ctx: &TaskContext<'_>,
    handler_name: &str,
    field: &str,
) -> Result<u64, DataflowError> {
    match resolve_value(raw, ctx) {
        Value::Number(n) => n.as_u64().ok_or_else(|| {
            DataflowError::Validation(format!(
                "{handler_name}: '{field}' must be a positive integer"
            ))
        }),
        Value::String(s) => parse_duration_secs(&s)
            .map_err(|e| DataflowError::Validation(format!("{handler_name}: '{field}': {e}"))),
        _ => Err(DataflowError::Validation(format!(
            "{handler_name}: '{field}' must be seconds (integer) or a duration like \"24h\""
        ))),
    }
}

/// Resolve an optional input field to a string: absent, null, or resolving to
/// null → `None`; a resolved non-string is an error naming the field.
pub fn resolve_optional_str(
    input: &Value,
    field: &str,
    handler_name: &str,
    ctx: &TaskContext<'_>,
) -> Result<Option<String>, DataflowError> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(raw) => match resolve_value(raw, ctx) {
            Value::String(s) => Ok(Some(s)),
            Value::Null => Ok(None),
            _ => Err(DataflowError::Validation(format!(
                "{handler_name}: '{field}' must resolve to a string"
            ))),
        },
    }
}

/// Resolve the positional `params` array bound to a raw-SQL statement.
///
/// Absent or null yields no binds. Anything that resolves to a non-array is an
/// error rather than being dropped, which would leave the statement's
/// placeholders unbound.
pub fn resolve_bind_params(
    input: &Value,
    handler_name: &str,
    ctx: &TaskContext<'_>,
) -> Result<Vec<Value>, DataflowError> {
    match input.get("params") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(raw) => match resolve_value(raw, ctx) {
            Value::Array(a) => Ok(a),
            other => Err(DataflowError::Validation(format!(
                "{handler_name} 'params' must resolve to an array of bind values, got {}",
                json_type_name(&other)
            ))),
        },
    }
}

/// Name a JSON value's type for error messages.
pub fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Bind a slice of JSON values to a sqlx query, matching each value type to
/// the appropriate sqlx bind call.  Consolidates the identical loop found in
/// `db_read` and `db_write`.
pub fn bind_json_params<'q>(
    mut query: sqlx::query::Query<'q, sqlx::Any, sqlx::any::AnyArguments<'q>>,
    params: &'q [Value],
) -> sqlx::query::Query<'q, sqlx::Any, sqlx::any::AnyArguments<'q>> {
    for param in params {
        query = match param {
            Value::String(s) => query.bind(s.as_str()),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    query.bind(i)
                } else if let Some(f) = n.as_f64() {
                    query.bind(f)
                } else {
                    query.bind(n.to_string())
                }
            }
            Value::Bool(b) => query.bind(*b),
            Value::Null => query.bind(None::<String>),
            _ => query.bind(param.to_string()),
        };
    }
    query
}

/// The query timeout a connector that declares no `query_timeout_ms` gets.
const DEFAULT_QUERY_TIMEOUT_MS: u64 = 30_000;

/// One wall-clock budget shared by every round trip of a single logical
/// operation (F11, F28).
///
/// `timed_query` bounds *one* future. That is the whole story for a handler
/// that issues one statement, but a `data_write` that opens a transaction
/// issues three round trips — acquire + `BEGIN`, the statement, `COMMIT` — and
/// giving each its own `query_timeout_ms` would silently multiply the
/// connector's configured bound. A budget is started once and every leg runs
/// against the same deadline, so `query_timeout_ms` keeps meaning what the
/// connector's owner set it to.
#[derive(Debug, Clone, Copy)]
pub struct QueryBudget {
    deadline: tokio::time::Instant,
    total_ms: u64,
}

impl QueryBudget {
    /// Start a budget of `timeout_ms` (or the default) from now.
    pub fn start(timeout_ms: Option<u64>) -> Self {
        let total_ms = timeout_ms.unwrap_or(DEFAULT_QUERY_TIMEOUT_MS);
        Self {
            deadline: tokio::time::Instant::now() + Duration::from_millis(total_ms),
            total_ms,
        }
    }

    /// Run one leg of the operation against the shared deadline.
    pub async fn run<F, T, E>(&self, handler_name: &str, operation: F) -> Result<T, DataflowError>
    where
        F: std::future::Future<Output = Result<T, E>>,
        E: Into<QueryFailure>,
    {
        let total_ms = self.total_ms;
        tokio::time::timeout_at(self.deadline, operation)
            .await
            .map_err(|_| {
                HandlerError::new(
                    ErrorClass::Timeout,
                    format!("{handler_name} query timed out after {total_ms}ms"),
                )
            })?
            .map_err(|e| {
                // F42: a limit the caller can fix is a 400 with its text
                // intact, not a 500 with the guidance replaced. Both arms name
                // a class; which `DataflowError` that becomes is decided once.
                match e.into() {
                    QueryFailure::Limit(detail) => to_limit_error(detail),
                    QueryFailure::Backend(text) => {
                        to_exec_error(format!("{handler_name} query failed: {text}"))
                    }
                }
            })
            .map_err(DataflowError::from)
    }
}

/// Execute an async operation with a timeout, mapping errors to
/// `DataflowError::Timeout` and `DataflowError::FunctionExecution`
/// respectively.  Consolidates the repeated timeout + error-mapping pattern
/// in the SQL handler functions. A single-round-trip [`QueryBudget`].
pub async fn timed_query<F, T, E>(
    timeout_ms: Option<u64>,
    handler_name: &str,
    operation: F,
) -> Result<T, DataflowError>
where
    F: std::future::Future<Output = Result<T, E>>,
    E: Into<QueryFailure>,
{
    QueryBudget::start(timeout_ms)
        .run(handler_name, operation)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::DialectGuards;

    fn es_config(allow_private_urls: bool) -> EsConnectorConfig {
        EsConnectorConfig {
            max_response_size: 10 * 1024 * 1024,
            url: "http://127.0.0.1:9200".to_string(),
            auth: None,
            request_timeout_ms: None,
            allow_private_urls,
            operations: OperationGates::default(),
            dialect: DialectGuards::default(),
        }
    }

    #[tokio::test]
    async fn test_es_request_blocks_private_url() {
        let client = reqwest::Client::new();
        let result = es_request(
            &client,
            &es_config(false),
            reqwest::Method::POST,
            "http://127.0.0.1:9200/idx/_search",
        )
        .await;

        let err = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("SSRF protection"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn test_es_request_allows_private_url_when_opted_in() {
        let client = reqwest::Client::new();
        let result = es_request(
            &client,
            &es_config(true),
            reqwest::Method::POST,
            "http://127.0.0.1:9200/idx/_search",
        )
        .await;

        assert!(result.is_ok());
    }
}

#[cfg(test)]
mod observability_tests {
    //! Conformance, asserted against handler *source text*.
    //!
    //! Every test here checks a property that a type can hold instead, and
    //! `connector_handler::ConnectorHandler` now holds them: `NAME` is an
    //! associated const so it cannot be written twice, `parse` runs after
    //! `ConnectorCall::begin` by construction so the F58 ordering cannot be
    //! reversed, and an unwrapped handler is not an `AsyncFunctionHandler` at
    //! all, so it cannot reach the registry without the shell.
    //!
    //! So this list is the handlers **not yet converted**. It shrinks with each
    //! one, and when it empties these tests delete themselves — the property
    //! will be the compiler's. Do not add to it.
    const HANDLERS: [&str; 2] = ["http_call", "publish_kafka"];

    fn handler_source(handler: &str) -> String {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/engine/functions");
        std::fs::read_to_string(format!("{dir}/{handler}.rs")).expect("handler source")
    }

    /// F40/F6: every connector handler must run its body inside the shared
    /// shell — [`guarded_handler`], reached directly or through
    /// [`ConnectorCall::run`]. That shell is the only thing emitting
    /// `connector_requests_total` / `connector_request_duration_seconds`, and
    /// the only place the circuit breaker is applied.
    ///
    /// Asserted against the sources rather than by scraping `/metrics`: the
    /// metrics recorder is process-global, so only the first test app in a
    /// binary installs a real one and a scrape-based test would pass or fail
    /// on test ordering. This checks the property that actually regresses —
    /// a handler added without the wrapper.
    ///
    /// [`ConnectorCall::run`]: super::ConnectorCall::run
    /// [`guarded_handler`]: super::guarded_handler
    #[test]
    fn every_connector_handler_is_wrapped_in_the_observability_shell() {
        let mut unwrapped = Vec::new();
        for handler in HANDLERS {
            let src = handler_source(handler);
            // `guarded_handler` (F6) wraps `observed_handler_named`, and
            // `ConnectorCall::run` (F48) wraps `guarded_handler`; either one
            // means the handler is inside the shell.
            if !["guarded_handler", "call.run("]
                .iter()
                .any(|w| src.contains(w))
            {
                unwrapped.push(handler);
            }
            // The wrapper is useless if the body reaches for the raw profiler,
            // which records no connector metrics.
            assert!(
                !src.contains("profile::record("),
                "{handler} calls the raw profiler; go through ConnectorCall::run \
                 so its connector metrics are not conditional on the circuit breaker"
            );
        }
        assert!(
            unwrapped.is_empty(),
            "these connector handlers emit no connector metrics: {unwrapped:?}"
        );
    }

    /// F48: each handler names itself exactly once, in its `NAME` const.
    ///
    /// The name reaches metric labels, profile samples and every error message,
    /// and it used to be a bare string literal repeated three to six times per
    /// file. A typo in one copy compiles cleanly and surfaces as a metric series
    /// nobody is graphing, or an error naming a function the workflow never
    /// called.
    #[test]
    fn a_handler_names_itself_exactly_once() {
        for handler in HANDLERS {
            let src = handler_source(handler);
            let quoted = format!("\"{handler}\"");
            let occurrences = src.matches(&quoted).count();
            assert_eq!(
                occurrences, 1,
                "{handler}.rs writes \"{handler}\" {occurrences} times; it should \
                 appear only in `const NAME` and be read back from there"
            );
            assert!(
                src.contains(&format!("const NAME: &str = {quoted}")),
                "{handler}.rs has no `const NAME` (proposal F48)"
            );
        }
    }
}

#[cfg(test)]
mod error_taxonomy_tests {
    use super::*;

    /// F42: a dead Postgres/Redis/Mongo used to be a **non-retryable** 500
    /// while the identical HTTP outage was a retryable `Io`, so DLQ retry
    /// policy diverged by backend for no principled reason. dataflow-rs
    /// classifies `FunctionExecution { source: None }` as not retryable and
    /// `Io` as retryable.
    ///
    /// The constructors now name a class rather than a variant, and
    /// `HandlerError`'s `Into<DataflowError>` picks the variant. Asserted
    /// end-to-end — through the conversion — because the retryability of the
    /// error that actually reaches the retry loop is the property that matters.
    #[test]
    fn a_failure_to_connect_is_retryable_but_a_failed_query_is_not() {
        assert!(
            DataflowError::from(to_connect_error("connection refused")).retryable(),
            "an unreachable backend must be retryable, like the HTTP path"
        );
        assert!(
            !DataflowError::from(to_exec_error("syntax error at or near \"SELCT\"")).retryable(),
            "a query the backend rejected is not worth retrying"
        );
    }

    /// A caller-fixable limit is a 400 that keeps its guidance, not a 500 that
    /// loses it to sanitisation.
    #[test]
    fn a_limit_error_is_validation_not_execution() {
        let err = DataflowError::from(to_limit_error(
            "result exceeds query.max_limit — add a LIMIT",
        ));
        assert!(
            matches!(err, DataflowError::Validation(_)),
            "expected Validation, got {err:?}"
        );
        assert!(!err.retryable(), "a limit does not fix itself on retry");
    }

    /// A limit the operation classified reaches the caller as a 400 with its
    /// message intact — the guidance *is* the message.
    ///
    /// This used to be carried by prefixing the error string with a marker
    /// that `timed_query` looked for and stripped. The classification is a
    /// value under the `Err` now, so there is no marker to leak, no message
    /// that can be mistaken for one, and no reformatting in between that can
    /// lose it.
    #[tokio::test]
    async fn timed_query_reports_a_classified_limit_as_validation() {
        let err = timed_query(Some(1_000), "db_read", async {
            Err::<(), _>(QueryFailure::Limit(
                "too many rows — add a LIMIT".to_string(),
            ))
        })
        .await
        .expect_err("the operation failed");
        assert!(
            matches!(err, DataflowError::Validation(ref m) if m == "too many rows — add a LIMIT"),
            "expected the message intact under Validation, got {err:?}"
        );
    }

    /// A message that *looks* like the old marker is just a message now. The
    /// string-prefix scheme could not tell the difference.
    #[tokio::test]
    async fn a_backend_failure_is_never_reclassified_by_its_text() {
        let err = timed_query(Some(1_000), "db_read", async {
            Err::<(), String>("orion.limit: not a limit, just text".to_string())
        })
        .await
        .expect_err("the operation failed");
        assert!(
            matches!(err, DataflowError::FunctionExecution { .. }),
            "text cannot promote a backend failure to a limit: {err:?}"
        );
    }

    /// An unmarked failure keeps the old behaviour: a 500 naming the handler.
    #[tokio::test]
    async fn timed_query_leaves_an_ordinary_failure_as_execution() {
        let err = timed_query(Some(1_000), "db_read", async {
            Err::<(), String>("connection reset".to_string())
        })
        .await
        .expect_err("the operation failed");
        assert!(
            matches!(err, DataflowError::FunctionExecution { .. }),
            "expected FunctionExecution, got {err:?}"
        );
    }
}
