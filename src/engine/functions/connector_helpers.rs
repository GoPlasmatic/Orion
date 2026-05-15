use std::sync::Arc;

use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::task_context::TaskContext;
use serde_json::Value;

use crate::connector::{
    CacheConnectorConfig, ConnectorConfig, ConnectorRegistry, DbConnectorConfig,
};

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
