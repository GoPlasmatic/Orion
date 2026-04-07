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

/// Workflow function handler for writing values to a cache backend.
pub struct CacheWriteHandler {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

#[async_trait]
impl AsyncFunctionHandler for CacheWriteHandler {
    async fn execute(
        &self,
        _message: &mut Message,
        config: &FunctionConfig,
        _datalogic: Arc<DataLogic>,
    ) -> dataflow_rs::Result<(usize, Vec<Change>)> {
        let input = match config {
            FunctionConfig::Custom { input, .. } => input,
            _ => {
                return Err(DataflowError::Validation(
                    "Expected Custom config for cache_write".into(),
                ));
            }
        };

        let connector_name = input
            .get("connector")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DataflowError::Validation("cache_write requires 'connector'".into()))?;
        let key = input
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| DataflowError::Validation("cache_write requires 'key'".into()))?;

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

        // Serialize the value to a string for storage
        let value_str = match input.get("value") {
            Some(Value::String(s)) => s.clone(),
            Some(v) => serde_json::to_string(v).map_err(|e| {
                DataflowError::Validation(format!("Failed to serialize value for cache: {}", e))
            })?,
            None => {
                return Err(DataflowError::Validation(
                    "cache_write requires 'value'".into(),
                ));
            }
        };

        // Optional TTL in seconds
        let ttl = input.get("ttl_secs").and_then(|v| v.as_u64());

        if let Some(ttl) = ttl {
            backend
                .set_ex(key, &value_str, ttl)
                .await
                .map_err(|e| DataflowError::function_execution(e.to_string(), None))?;
        } else {
            backend
                .set(key, &value_str)
                .await
                .map_err(|e| DataflowError::function_execution(e.to_string(), None))?;
        }

        tracing::debug!(
            key = %key,
            ttl = ?ttl,
            "Wrote value to cache"
        );

        Ok((1, vec![]))
    }
}
