use std::sync::Arc;

use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::config::FunctionConfig;
use dataflow_rs::engine::message::{Change, Message};
use serde_json::Value;

use crate::connector::{
    CacheConnectorConfig, ConnectorConfig, ConnectorRegistry, DbConnectorConfig,
};

/// Extracts the `input` value from a `FunctionConfig::Custom` variant.
/// Returns a validation error if the config is any other variant.
pub fn extract_custom_input<'a>(
    config: &'a FunctionConfig,
    handler_name: &str,
) -> Result<&'a Value, DataflowError> {
    match config {
        FunctionConfig::Custom { input, .. } => Ok(input),
        _ => Err(DataflowError::Validation(format!(
            "Expected Custom config for {handler_name}"
        ))),
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

/// Sets a value at `output_path` in the message context and returns the
/// corresponding `Change` record.  Consolidates the repeated
/// get_nested → set_nested → invalidate_context_cache → Change pattern.
pub fn apply_output(message: &mut Message, output_path: &str, new_value: Value) -> Vec<Change> {
    let old_value = super::http_common::get_nested(&message.context, output_path);
    super::http_common::set_nested(&mut message.context, output_path, new_value.clone());
    message.invalidate_context_cache();
    vec![Change {
        path: Arc::from(output_path),
        old_value: Arc::new(old_value),
        new_value: Arc::new(new_value),
    }]
}
