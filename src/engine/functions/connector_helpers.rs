use std::sync::Arc;
use std::time::Duration;

use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::task_context::TaskContext;
use serde_json::{Map, Value};

use crate::connector::{
    AuthConfig, CacheConnectorConfig, ConnectorConfig, ConnectorRegistry, DbConnectorConfig,
    EsConnectorConfig, OperationGates,
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
            "operation '{op}' is disabled on connector '{connector_name}'"
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
        req = match auth {
            AuthConfig::Basic { username, password } => req.basic_auth(username, Some(password)),
            AuthConfig::Bearer { token } => req.bearer_auth(token),
            AuthConfig::ApiKey { header, key } => req.header(header.as_str(), key.as_str()),
        };
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
                "Elasticsearch response declared Content-Length {len} exceeds                  limit of {max_size} bytes"
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
            "Connector '{name}' is not a database connector"
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
            "Connector '{name}' is not a cache connector"
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
            DataflowError::function_execution(format!("{handler_name} query failed: {e}"), None)
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
