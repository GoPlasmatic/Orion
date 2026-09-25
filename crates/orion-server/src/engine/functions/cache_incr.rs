use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::task_context::TaskContext;
use serde_json::Value;

use super::cache_write::resolve_ttl_secs;
use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{
    ConnectorCall, json_type_name, require_op, resolve_required_str, to_connect_error,
    to_exec_error,
};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::TemplatedInput;
use crate::connector::ConnectorRegistry;
use crate::connector::cache_backend::{CachePool, CachePurpose};
use crate::engine::HandlerError;

/// Workflow function handler for an atomic increment on a cache backend.
///
/// The primitive a generation counter needs: each bump yields a value no other
/// writer can be handed, with no clock or random suffix to improvise, and a
/// cached key that embeds the generation it read retires itself the moment the
/// counter moves. It also serves plain counters and idle markers.
pub struct CacheIncrHandler {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

/// The resolved increment.
pub struct CacheIncr {
    key: String,
    by: i64,
    ttl: Option<u64>,
}

#[async_trait]
impl ConnectorHandler for CacheIncrHandler {
    const NAME: &'static str = "cache_incr";
    type Kind = crate::connector::kind::Cache;
    type Input = TemplatedInput;
    type Parsed = CacheIncr;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        ctx: &TaskContext<'_>,
    ) -> Result<Self::Parsed, HandlerError> {
        let key = resolve_required_str(input, "key", call.name, ctx)?;
        let by = match input.value_of("by", call.name, ctx).transpose()? {
            None | Some(Value::Null) => 1,
            Some(Value::Number(n)) => n.as_i64().ok_or_else(|| {
                DataflowError::Validation(format!(
                    "{} 'by' must be a whole number that fits in 64 bits, got {n}",
                    call.name
                ))
            })?,
            Some(other) => {
                return Err(DataflowError::Validation(format!(
                    "{} 'by' must resolve to a number, got {}",
                    call.name,
                    json_type_name(&other)
                ))
                .into());
            }
        };
        Ok(CacheIncr {
            key,
            by,
            ttl: resolve_ttl_secs(input, call.name, ctx)?,
        })
    }

    fn gate(
        _parsed: &Self::Parsed,
        conn: &crate::connector::CacheConnectorConfig,
        connector: &str,
    ) -> Result<(), HandlerError> {
        Ok(require_op(conn.operations.write, "write", connector)?)
    }

    async fn run(
        &self,
        incr: Self::Parsed,
        conn: &crate::connector::CacheConnectorConfig,
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        let backend = self
            .cache_pool
            .get_backend(CachePurpose::Workflow, call.connector, conn)
            .await
            .map_err(to_connect_error)?;
        let value = backend
            .incr_by(&incr.key, incr.by, incr.ttl)
            .await
            .map_err(to_exec_error)?;
        tracing::debug!(key = %incr.key, by = incr.by, value, "Incremented cache key");

        // As `cache_delete`: recorded only where the task asks, so a bare
        // counter bump never overwrites `data` with a number.
        Ok(if input.get("output").is_some_and(|v| !v.is_null()) {
            Value::from(value).into()
        } else {
            Produced::nothing()
        })
    }
}

// -- Input schema (F53) --

pub(super) const CACHE_INCR_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the cache connector holding the counter.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "key",
        description: "Counter key. A missing key counts as 0 and is created. JSONLogic: a literal, or an expression over the message.",
        kind: FieldKind::String,
        required: true,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "by",
        description: "Amount to add, a whole number; negative decrements. Defaults to 1. JSONLogic: a literal, or an expression over the message.",
        kind: FieldKind::Number,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "ttl_secs",
        description: "Time-to-live in seconds, applied only when this call creates the key; later bumps keep its expiry. Omit for no expiry. JSONLogic: a literal, or an expression over the message.",
        kind: FieldKind::Number,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the new value is stored. Omit to record nothing.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];
