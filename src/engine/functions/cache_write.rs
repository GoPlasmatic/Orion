use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::Value;

use super::connector_helpers::{
    guarded_handler, json_type_name, require_cache_connector, require_str_field, resolve_connector,
    resolve_required_str, resolve_value, to_connect_error, to_exec_error,
};
use crate::connector::ConnectorRegistry;
use crate::connector::cache_backend::CachePool;

/// Workflow function handler for writing values to a cache backend.
pub struct CacheWriteHandler {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

#[async_trait]
impl AsyncFunctionHandler for CacheWriteHandler {
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // Key, value, and TTL are all resolved against the message context up
        // front: the handler body below takes `ctx` mutably, and without this
        // the whole task could only ever write one constant.
        let key = resolve_required_str(input, "key", "cache_write", ctx)?;
        let value = match input.get("value") {
            Some(v) => resolve_value(v, ctx),
            None => {
                return Err(DataflowError::Validation(
                    "cache_write requires 'value'".into(),
                ));
            }
        };
        let ttl = resolve_ttl_secs(input, ctx)?;

        // F40: read the channel before the body borrows `ctx` mutably.
        let channel = super::extract_channel(ctx.message()).to_string();

        // F6: the breaker now wraps every egress path, not just http_call.
        let connector_name = require_str_field(input, "connector", "cache_write")?;

        guarded_handler(
            "cache_write",
            &self.registry,
            connector_name,
            &channel,
            async move {
                let connector_config = resolve_connector(&self.registry, connector_name).await?;
                let cache_config = require_cache_connector(&connector_config, connector_name)?;

                let backend = self
                    .cache_pool
                    .get_backend(connector_name, cache_config)
                    .await
                    .map_err(to_connect_error)?;

                // Always JSON-encode, including strings, so `cache_read` is the
                // exact inverse. Storing strings raw made the round-trip lossy:
                // writing "123" stored `123`, which read back as the *number* 123,
                // and "true"/"null" likewise changed type — silently breaking any
                // downstream JSONLogic comparison (proposal N13).
                let value_str = serde_json::to_string(&value).map_err(|e| {
                    DataflowError::Validation(format!("Failed to serialize value for cache: {e}"))
                })?;

                if let Some(ttl) = ttl {
                    backend
                        .set_ex(&key, &value_str, ttl)
                        .await
                        .map_err(to_exec_error)?;
                } else {
                    backend.set(&key, &value_str).await.map_err(to_exec_error)?;
                }

                tracing::debug!(
                    key = %key,
                    ttl = ?ttl,
                    "Wrote value to cache"
                );

                Ok(TaskOutcome::Success)
            },
        )
        .await
    }
}

/// Resolve `ttl_secs` to whole seconds. Absent or null means "no expiry"; a
/// value that resolves to something uninterpretable is an error rather than a
/// silent fall-through to a key that never expires.
fn resolve_ttl_secs(input: &Value, ctx: &TaskContext<'_>) -> Result<Option<u64>, DataflowError> {
    let Some(raw) = input.get("ttl_secs") else {
        return Ok(None);
    };
    match resolve_value(raw, ctx) {
        Value::Null => Ok(None),
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Ok(Some(u))
            } else if let Some(f) = n.as_f64()
                && f >= 0.0
                && f.fract() == 0.0
            {
                Ok(Some(f as u64))
            } else {
                Err(DataflowError::Validation(format!(
                    "cache_write 'ttl_secs' must be a non-negative whole number of seconds, got {n}"
                )))
            }
        }
        other => Err(DataflowError::Validation(format!(
            "cache_write 'ttl_secs' must resolve to a number, got {}",
            json_type_name(&other)
        ))),
    }
}
