use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::functions::config::FunctionConfig;
use dataflow_rs::engine::message::{Change, Message};
use datalogic_rs::DataLogic;
use serde_json::Value;

use crate::connector::cache_backend::CachePool;
use crate::connector::{ConnectorConfig, ConnectorRegistry};

/// Workflow function handler for reading values from a cache backend.
pub struct CacheReadHandler {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

#[async_trait]
impl AsyncFunctionHandler for CacheReadHandler {
    async fn execute(
        &self,
        message: &mut Message,
        config: &FunctionConfig,
        _datalogic: Arc<DataLogic>,
    ) -> dataflow_rs::Result<(usize, Vec<Change>)> {
        let input = match config {
            FunctionConfig::Custom { input, .. } => input,
            _ => {
                return Err(DataflowError::Validation(
                    "Expected Custom config for cache_read".into(),
                ));
            }
        };

        let connector_name = input
            .get("connector")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DataflowError::Validation("cache_read requires 'connector'".into()))?;
        let key = input
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DataflowError::Validation("cache_read requires 'key'".into()))?;

        let connector_config = self.registry.get(connector_name).await.ok_or_else(|| {
            DataflowError::function_execution(
                format!("Connector '{}' not found", connector_name),
                None,
            )
        })?;
        let cache_config = match connector_config.as_ref() {
            ConnectorConfig::Cache(c) => c,
            _ => {
                return Err(DataflowError::Validation(format!(
                    "Connector '{}' is not a cache connector",
                    connector_name
                )));
            }
        };

        let backend = self
            .cache_pool
            .get_backend(connector_name, cache_config)
            .await
            .map_err(|e| DataflowError::function_execution(e.to_string(), None))?;

        let value = backend
            .get(key)
            .await
            .map_err(|e| DataflowError::function_execution(e.to_string(), None))?;

        let result = match value {
            Some(v) => {
                // Try to parse as JSON, fall back to string
                serde_json::from_str::<Value>(&v).unwrap_or(Value::String(v))
            }
            None => Value::Null,
        };

        let output_path = input
            .get("output")
            .and_then(|v| v.as_str())
            .unwrap_or("data");

        let old_value = super::http_common::get_nested(&message.context, output_path);
        super::http_common::set_nested(&mut message.context, output_path, result.clone());
        message.invalidate_context_cache();

        let changes = vec![Change {
            path: Arc::from(output_path),
            old_value: Arc::new(old_value),
            new_value: Arc::new(result),
        }];
        Ok((1, changes))
    }
}
