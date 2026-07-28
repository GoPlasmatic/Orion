use std::sync::Arc;
use std::time::Duration;

use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::{Map, Value};

use crate::connector::{
    CacheConnectorConfig, ConnectorConfig, ConnectorRegistry, DbConnectorConfig, EsConnectorConfig,
    OperationGates,
};

/// Reject the call when the connector's operation gates disable `op` — the
/// per-connector en/disable switch for read / insert / update / delete /
/// upsert / raw_write (see [`OperationGates`]).
pub fn require_op_allowed(
    gates: &OperationGates,
    op: &str,
    connector_name: &str,
) -> Result<(), DataflowError> {
    if !gates.allows(op) {
        return Err(DataflowError::Validation(format!(
            "{}operation '{op}' is disabled on connector '{connector_name}'",
            crate::errors::CONNECTOR_DETAIL_MARKER
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
    serde_json::from_slice(&bytes).map_err(to_exec_error)
}

/// Send an ES request and parse its JSON body. Returns the status alongside so
/// callers can treat specific non-2xx statuses as semantic (`op_type=create`
/// treats 409 as the "conflict → do nothing" no-op).
pub async fn send_es(
    req: reqwest::RequestBuilder,
    max_response_size: usize,
) -> Result<(reqwest::StatusCode, Value), DataflowError> {
    let resp = req.send().await.map_err(to_exec_error)?;
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

/// Wrap a handler body with profile recording, peeking the `connector`
/// field from `input` to label the sample. Replaces the
/// `let connector_peek = input.get("connector").and_then(...);
///  crate::engine::profile::record(fn_name, connector_peek, async move {...})`
/// preamble repeated by every `AsyncFunctionHandler::execute` in this module.
pub async fn profile_handler<F, T>(fn_name: &'static str, input: &Value, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let connector_peek = input.get("connector").and_then(|v| v.as_str());
    crate::engine::profile::record(fn_name, connector_peek, fut).await
}

/// [`profile_handler`] plus the connector request/latency metrics (F40).
///
/// `connector_requests_total` and `connector_request_duration_seconds` used to
/// be emitted from exactly one place — inside `execute_with_circuit_breaker` —
/// which `http_call` reaches only when `engine.circuit_breaker.enabled` is
/// true. That defaults to **false**, so a default install emitted **zero**
/// connector-level counts or latencies for *any* of the ten handlers: every
/// external dependency was dark in Prometheus until an operator flipped an
/// unrelated resilience flag.
///
/// Observability is not conditional on resilience config, so every connector
/// handler calls this unconditionally. The circuit breaker stays an inner,
/// optional layer (F6 tracks extending it past `http_call`).
///
/// `channel` must be read from the message *before* the handler body takes
/// `ctx` mutably.
pub async fn observed_handler<F>(
    fn_name: &'static str,
    input: &Value,
    channel: &str,
    fut: F,
) -> dataflow_rs::Result<TaskOutcome>
where
    F: std::future::Future<Output = dataflow_rs::Result<TaskOutcome>>,
{
    // The same peek `profile_handler` does. `unknown` rather than skipping the
    // metric: a handler that failed before resolving its connector is exactly
    // the case an operator needs to see.
    let connector = input
        .get("connector")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    observed_handler_named(fn_name, connector, channel, fut).await
}

/// [`observed_handler`] for the two handlers whose input is a typed struct
/// rather than a `Value`, so the connector name is passed directly.
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

/// Converts any `Display`-able error into a `DataflowError::FunctionExecution`.
pub fn to_exec_error(e: impl std::fmt::Display) -> DataflowError {
    DataflowError::function_execution(e.to_string(), None)
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
pub fn to_connect_error(e: impl std::fmt::Display) -> DataflowError {
    DataflowError::Io(e.to_string())
}

/// A caller-fixable limit or shape problem, e.g. a result set over
/// `query.max_limit`.
///
/// `Validation` maps to 400 with the message preserved. Routing these through
/// [`to_exec_error`] made them 500 `ENGINE_ERROR` with the text replaced, so
/// deliberately helpful guidance — *"add a LIMIT to the query or raise the
/// cap"* — was sanitised away exactly when the caller needed it (F42).
pub fn to_limit_error(message: impl std::fmt::Display) -> DataflowError {
    DataflowError::Validation(message.to_string())
}

/// Prefix a `timed_query` operation puts on a message that is a caller-fixable
/// limit rather than a backend failure, so the wrapper can classify it (F42).
/// Stripped before the message is surfaced.
pub const LIMIT_MARKER: &str = "orion.limit: ";

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
pub fn is_mongo(conn: &str) -> bool {
    conn.starts_with("mongodb://") || conn.starts_with("mongodb+srv://")
}

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

/// Extracts the `DbConnectorConfig` from a `ConnectorConfig`, returning a
/// validation error if the connector is not a database type.
pub fn require_db_connector<'a>(
    config: &'a ConnectorConfig,
    name: &str,
) -> Result<&'a DbConnectorConfig, DataflowError> {
    match config {
        ConnectorConfig::Db(c) => Ok(c),
        _ => Err(DataflowError::Validation(format!(
            "{}Connector '{name}' is not a database connector",
            crate::errors::CONNECTOR_DETAIL_MARKER
        ))),
    }
}

/// Extracts the `HttpConnectorConfig` from a `ConnectorConfig`, returning a
/// validation error if the connector is not an HTTP type.
pub fn require_http_connector<'a>(
    config: &'a ConnectorConfig,
    name: &str,
) -> Result<&'a crate::connector::HttpConnectorConfig, DataflowError> {
    match config {
        ConnectorConfig::Http(c) => Ok(c),
        _ => Err(DataflowError::Validation(format!(
            "{}Connector '{name}' is not an HTTP connector",
            crate::errors::CONNECTOR_DETAIL_MARKER
        ))),
    }
}

/// Extracts the `KafkaConnectorConfig` from a `ConnectorConfig`, returning a
/// validation error if the connector is not a Kafka type.
pub fn require_kafka_connector<'a>(
    config: &'a ConnectorConfig,
    name: &str,
) -> Result<&'a crate::connector::KafkaConnectorConfig, DataflowError> {
    match config {
        ConnectorConfig::Kafka(c) => Ok(c),
        _ => Err(DataflowError::Validation(format!(
            "{}Connector '{name}' is not a Kafka connector",
            crate::errors::CONNECTOR_DETAIL_MARKER
        ))),
    }
}

/// Extracts the `CacheConnectorConfig` from a `ConnectorConfig`, returning a
/// validation error if the connector is not a cache type.
pub fn require_cache_connector<'a>(
    config: &'a ConnectorConfig,
    name: &str,
) -> Result<&'a CacheConnectorConfig, DataflowError> {
    match config {
        ConnectorConfig::Cache(c) => Ok(c),
        _ => Err(DataflowError::Validation(format!(
            "{}Connector '{name}' is not a cache connector",
            crate::errors::CONNECTOR_DETAIL_MARKER
        ))),
    }
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

/// Execute an async operation with a timeout, mapping errors to
/// `DataflowError::Timeout` and `DataflowError::FunctionExecution`
/// respectively.  Consolidates the repeated timeout + error-mapping pattern
/// in the SQL handler functions.
pub async fn timed_query<F, T, E>(
    timeout_ms: Option<u64>,
    handler_name: &str,
    operation: F,
) -> Result<T, DataflowError>
where
    F: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let ms = timeout_ms.unwrap_or(30_000);
    tokio::time::timeout(std::time::Duration::from_millis(ms), operation)
        .await
        .map_err(|_| {
            DataflowError::Timeout(format!("{handler_name} query timed out after {ms}ms"))
        })?
        .map_err(|e| {
            // F42: a limit the caller can fix is a 400 with its text intact,
            // not a 500 with the guidance replaced.
            let text = e.to_string();
            if let Some(detail) = text.strip_prefix(LIMIT_MARKER) {
                return to_limit_error(detail);
            }
            DataflowError::function_execution(format!("{handler_name} query failed: {text}"), None)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn es_config(allow_private_urls: bool) -> EsConnectorConfig {
        EsConnectorConfig {
            max_response_size: 10 * 1024 * 1024,
            url: "http://127.0.0.1:9200".to_string(),
            auth: None,
            request_timeout_ms: None,
            retry: crate::connector::RetryConfig::default(),
            allow_private_urls,
            operations: OperationGates::default(),
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
    /// F40: every connector handler must run its body inside
    /// [`observed_handler`] (or [`observed_handler_named`] for the two with
    /// typed inputs), because that is the only thing emitting
    /// `connector_requests_total` / `connector_request_duration_seconds`.
    ///
    /// Asserted against the sources rather than by scraping `/metrics`: the
    /// metrics recorder is process-global, so only the first test app in a
    /// binary installs a real one and a scrape-based test would pass or fail
    /// on test ordering. This checks the property that actually regresses —
    /// a handler added without the wrapper.
    #[test]
    fn every_connector_handler_is_wrapped_in_the_observability_shell() {
        const HANDLERS: [&str; 9] = [
            "cache_read",
            "cache_write",
            "db_read",
            "db_write",
            "data_query",
            "data_write",
            "mongo_read",
            "http_call",
            "publish_kafka",
        ];
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/engine/functions");
        let mut unwrapped = Vec::new();
        for handler in HANDLERS {
            let path = format!("{dir}/{handler}.rs");
            let src = std::fs::read_to_string(&path).expect("handler source");
            if !src.contains("observed_handler") {
                unwrapped.push(handler);
            }
            // The wrapper is useless if the body still reaches for the raw
            // profiler, which records no connector metrics.
            assert!(
                !src.contains("profile_handler("),
                "{handler} still calls profile_handler; use observed_handler so \
                 its connector metrics are not conditional on the circuit breaker"
            );
        }
        assert!(
            unwrapped.is_empty(),
            "these connector handlers emit no connector metrics: {unwrapped:?}"
        );
    }
}

#[cfg(test)]
mod error_taxonomy_tests {
    use super::*;

    /// F42: a dead Postgres/Redis/Mongo used to be a **non-retryable** 500
    /// while the identical HTTP outage was a retryable `Io`, so DLQ retry
    /// policy diverged by backend for no principled reason. dataflow-rs
    /// classifies `FunctionExecution { source: None }` as not retryable and
    /// `Io` as retryable, so the distinction has to be made at construction.
    #[test]
    fn a_failure_to_connect_is_retryable_but_a_failed_query_is_not() {
        assert!(
            to_connect_error("connection refused").retryable(),
            "an unreachable backend must be retryable, like the HTTP path"
        );
        assert!(
            !to_exec_error("syntax error at or near \"SELCT\"").retryable(),
            "a query the backend rejected is not worth retrying"
        );
    }

    /// A caller-fixable limit is a 400 that keeps its guidance, not a 500 that
    /// loses it to sanitisation.
    #[test]
    fn a_limit_error_is_validation_not_execution() {
        let err = to_limit_error("result exceeds query.max_limit — add a LIMIT");
        assert!(
            matches!(err, DataflowError::Validation(_)),
            "expected Validation, got {err:?}"
        );
        assert!(!err.retryable(), "a limit does not fix itself on retry");
    }

    /// `timed_query` is where the marker is turned back into a classification;
    /// the marker itself must never reach a caller.
    #[tokio::test]
    async fn timed_query_classifies_a_marked_limit_and_strips_the_marker() {
        let err = timed_query(Some(1_000), "db_read", async {
            Err::<(), String>(format!("{LIMIT_MARKER}too many rows — add a LIMIT"))
        })
        .await
        .expect_err("the operation failed");
        assert!(
            matches!(err, DataflowError::Validation(ref m) if m == "too many rows — add a LIMIT"),
            "expected a stripped Validation, got {err:?}"
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
