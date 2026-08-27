use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::Value;

use super::connector_helpers::{
    ConnectorCall, json_type_name, require_cache_connector, require_op, resolve_required_str,
    resolve_value, to_connect_error, to_exec_error,
};
use super::schema::{FieldKind, FieldSchema};
use crate::connector::ConnectorRegistry;
use crate::connector::cache_backend::{CachePool, CachePurpose};

/// This handler's name in metrics, profiles and error messages (F48).
const NAME: &str = "cache_write";

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
        // F48/F58: the literal prologue first — a task naming no connector
        // must say so before anything about the message is consulted.
        let call = ConnectorCall::begin(NAME, input, ctx)?;

        // Key, value, and TTL are all resolved against the message context up
        // front: the handler body below takes `ctx` mutably, and without this
        // the whole task could only ever write one constant.
        let key = resolve_required_str(input, "key", call.name, ctx)?;
        let value = match input.get("value") {
            Some(v) => resolve_value(v, ctx),
            None => {
                return Err(DataflowError::Validation(format!(
                    "{} requires 'value'",
                    call.name
                )));
            }
        };
        let ttl = resolve_ttl_secs(input, ctx)?;

        call.run(&self.registry, async {
            let connector_config = call.resolve(&self.registry, None).await?;
            let cache_config = require_cache_connector(&connector_config, call.connector)?;
            // F22e: a cache connector can be made read-only in its config.
            require_op(cache_config.operations.write, "write", call.connector)?;

            // Workflow-purpose namespace (S19): a memory backend here can
            // never alias the dedup store or response cache, so a crafted
            // `dedup:{channel}:…` key is just an ordinary workflow key.
            let backend = self
                .cache_pool
                .get_backend(CachePurpose::Workflow, call.connector, cache_config)
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
        })
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
                    "{NAME} 'ttl_secs' must be a non-negative whole number of seconds, got {n}"
                )))
            }
        }
        other => Err(DataflowError::Validation(format!(
            "{NAME} 'ttl_secs' must resolve to a number, got {}",
            json_type_name(&other)
        ))),
    }
}

// -- Input schema (F53) --
//
// The table describing this handler's `function.input` lives next to the
// handler it describes. It used to sit in `schema.rs` with the other nine,
// which is how every schema/handler divergence in the 1.0 audit happened:
// a field was added, renamed or made conditional here and the table saying
// so was in a different file.

pub(super) const CACHE_WRITE_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the cache connector to write to.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "key",
        description: "Cache key to set. Accepts {\"var\": \"path\"} to read the value from the message.",
        kind: FieldKind::String,
        required: true,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "value",
        description: "Value to store. May be any JSON value. Accepts {\"var\": \"path\"} to read the value from the message.",
        kind: FieldKind::Any,
        required: true,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "ttl_secs",
        description: "Time-to-live in seconds. Omit for no expiry. Accepts {\"var\": \"path\"} to read the value from the message.",
        kind: FieldKind::Number,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
];
