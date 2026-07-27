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

/// Resolve a `params` object into concrete values for the query/write dialects.
///
/// A value shaped like `{"var": "path"}` is looked up in the message context
/// (dot-path over `{data, metadata, temp_data}`); anything else is used as a
/// literal. A lookup that does not resolve yields null. Shared by `data_query`
/// and `data_write` (folded into `{"param": ..}` nodes before translation).
pub fn resolve_params(params: Option<&Value>, ctx: &TaskContext<'_>) -> Map<String, Value> {
    let mut out = Map::new();
    let Some(Value::Object(map)) = params else {
        return out;
    };
    for (name, spec) in map {
        let resolved = match spec {
            Value::Object(o) if o.len() == 1 && o.contains_key("var") => {
                match o.get("var").and_then(|v| v.as_str()) {
                    Some(path) => ctx.get(path).map(Value::from).unwrap_or(Value::Null),
                    None => Value::Null,
                }
            }
            other => other.clone(),
        };
        out.insert(name.clone(), resolved);
    }
    out
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
