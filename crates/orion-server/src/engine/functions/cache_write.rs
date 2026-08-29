use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::task_context::TaskContext;
use serde_json::Value;

use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{
    ConnectorCall, json_type_name, require_op, resolve_required_str, resolve_value,
    to_connect_error, to_exec_error,
};
use super::schema::{FieldKind, FieldSchema};
use crate::connector::ConnectorRegistry;
use crate::connector::cache_backend::{CachePool, CachePurpose};
use crate::engine::HandlerError;

/// Workflow function handler for writing values to a cache backend.
pub struct CacheWriteHandler {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

/// Everything the write needs from the task and the message, folded before the
/// body takes `ctx` mutably — without which the task could only ever write one
/// constant, to one constant key.
pub struct CacheWrite {
    key: String,
    value: Value,
    ttl: Option<u64>,
}

#[async_trait]
impl ConnectorHandler for CacheWriteHandler {
    const NAME: &'static str = "cache_write";
    type Kind = crate::connector::kind::Cache;
    type Input = Value;
    type Parsed = CacheWrite;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        call: &ConnectorCall<'_>,
        input: &Value,
        ctx: &TaskContext<'_>,
    ) -> Result<Self::Parsed, HandlerError> {
        let key = resolve_required_str(input, "key", call.name, ctx)?;
        let value = match input.get("value") {
            Some(v) => resolve_value(v, ctx),
            None => {
                return Err(
                    DataflowError::Validation(format!("{} requires 'value'", call.name)).into(),
                );
            }
        };
        Ok(CacheWrite {
            key,
            value,
            ttl: resolve_ttl_secs(input, call.name, ctx)?,
        })
    }

    fn gate(
        _parsed: &Self::Parsed,
        conn: &crate::connector::CacheConnectorConfig,
        connector: &str,
    ) -> Result<(), HandlerError> {
        // F22e: a cache connector can be made read-only in its config.
        Ok(require_op(conn.operations.write, "write", connector)?)
    }

    async fn run(
        &self,
        write: Self::Parsed,
        conn: &crate::connector::CacheConnectorConfig,
        call: &ConnectorCall<'_>,
        _input: &Value,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        // Workflow-purpose namespace (S19): a memory backend here can never
        // alias the dedup store or response cache, so a crafted
        // `dedup:{channel}:…` key is just an ordinary workflow key.
        let backend = self
            .cache_pool
            .get_backend(CachePurpose::Workflow, call.connector, conn)
            .await
            .map_err(to_connect_error)?;

        // Always JSON-encode, including strings, so `cache_read` is the exact
        // inverse. Storing strings raw made the round-trip lossy: writing "123"
        // stored `123`, which read back as the *number* 123, and "true"/"null"
        // likewise changed type — silently breaking any downstream JSONLogic
        // comparison (proposal N13).
        let value_str = serde_json::to_string(&write.value).map_err(|e| {
            DataflowError::Validation(format!("Failed to serialize value for cache: {e}"))
        })?;

        match write.ttl {
            Some(ttl) => backend
                .set_ex(&write.key, &value_str, ttl)
                .await
                .map_err(to_exec_error)?,
            None => backend
                .set(&write.key, &value_str)
                .await
                .map_err(to_exec_error)?,
        }

        tracing::debug!(key = %write.key, ttl = ?write.ttl, "Wrote value to cache");

        // The write is the whole effect: this handler declares no `output`.
        Ok(Produced::nothing())
    }
}

/// Resolve `ttl_secs` to whole seconds. Absent or null means "no expiry"; a
/// value that resolves to something uninterpretable is an error rather than a
/// silent fall-through to a key that never expires.
fn resolve_ttl_secs(
    input: &Value,
    name: &str,
    ctx: &TaskContext<'_>,
) -> Result<Option<u64>, DataflowError> {
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
                    "{name} 'ttl_secs' must be a non-negative whole number of seconds, got {n}"
                )))
            }
        }
        other => Err(DataflowError::Validation(format!(
            "{name} 'ttl_secs' must resolve to a number, got {}",
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
