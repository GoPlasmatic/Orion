use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::task_context::TaskContext;
use serde_json::Value;

use super::connector_handler::ConnectorHandler;
use super::connector_helpers::{
    ConnectorCall, require_op, resolve_required_str, resolve_required_str_list, to_connect_error,
    to_exec_error,
};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::TemplatedInput;
use crate::connector::ConnectorRegistry;
use crate::connector::cache_backend::{CachePool, CachePurpose};
use dataflow_rs::engine::error::DataflowError;

/// Workflow function handler for reading values from a cache backend.
pub struct CacheReadHandler {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

#[async_trait]
impl ConnectorHandler for CacheReadHandler {
    const NAME: &'static str = "cache_read";
    type Kind = crate::connector::kind::Cache;
    type Input = TemplatedInput;
    /// The key or keys, resolved against the message. `{"var": "data.id"}` is
    /// the whole point of a per-request cache lookup, so it has to be folded
    /// before the body takes `ctx` mutably.
    type Parsed = CacheReadKeys;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        ctx: &TaskContext<'_>,
    ) -> Result<Self::Parsed, crate::engine::HandlerError> {
        // Exactly one of the two; `validate_static_input` says so at authoring
        // time, and this is the same rule for a hand-built input.
        match (input.get("key").is_some(), input.get("keys").is_some()) {
            (true, false) => Ok(CacheReadKeys::One(resolve_required_str(
                input, "key", call.name, ctx,
            )?)),
            (false, true) => Ok(CacheReadKeys::Many(resolve_required_str_list(
                input, "keys", call.name, ctx, MAX_KEYS,
            )?)),
            (true, true) => Err(DataflowError::Validation(format!(
                "{} takes 'key' or 'keys', not both",
                call.name
            ))
            .into()),
            (false, false) => Err(DataflowError::Validation(format!(
                "{} requires 'key' or 'keys'",
                call.name
            ))
            .into()),
        }
    }

    fn gate(
        _keys: &CacheReadKeys,
        conn: &crate::connector::CacheConnectorConfig,
        connector: &str,
    ) -> Result<(), crate::engine::HandlerError> {
        // F22e: a cache connector can be made write-only in its config.
        Ok(require_op(conn.operations.read, "read", connector)?)
    }

    async fn run(
        &self,
        keys: CacheReadKeys,
        conn: &crate::connector::CacheConnectorConfig,
        call: &ConnectorCall<'_>,
        _input: &TemplatedInput,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<super::connector_handler::Produced, crate::engine::HandlerError> {
        // Workflow-purpose namespace (S19) — the mirror of `cache_write`, so a
        // workflow reads exactly what workflows wrote and never the dedup store
        // or response cache.
        let backend = self
            .cache_pool
            .get_backend(CachePurpose::Workflow, call.connector, conn)
            .await
            .map_err(to_connect_error)?;

        match keys {
            CacheReadKeys::One(key) => {
                let value = backend.get(&key).await.map_err(to_exec_error)?;
                Ok(decode(value).into())
            }
            // One `MGET`: a route reading two generation counters pays one
            // round trip, not two.
            CacheReadKeys::Many(keys) => {
                let values = backend.get_many(&keys).await.map_err(to_exec_error)?;
                Ok(Value::Array(values.into_iter().map(decode).collect()).into())
            }
        }
    }
}

/// Most keys one `cache_read` or `cache_delete` may name.
pub(super) const MAX_KEYS: usize = 1000;

/// What a `cache_read` looks up: one `key`, answered as a value, or `keys`,
/// answered as an array in the same order.
pub enum CacheReadKeys {
    One(String),
    Many(Vec<String>),
}

/// A stored string back to the value it was written as.
///
/// `cache_write` JSON-encodes everything, so parsing is its exact inverse. The
/// raw-string fallback is kept deliberately: a key written by something other
/// than Orion may hold a bare string, and surfacing that as a string beats
/// failing the task. A miss is a result — `null` — not an absence of one.
fn decode(value: Option<String>) -> Value {
    match value {
        Some(v) => serde_json::from_str::<Value>(&v).unwrap_or(Value::String(v)),
        None => Value::Null,
    }
}

// -- Authoring-time validation (shared with schema::validate_input) --

/// `key` and `keys` are each optional in the field table, because either one
/// does; exactly one of them is the rule.
pub(super) fn validate_static_input(
    obj: &serde_json::Map<String, Value>,
) -> Vec<(&'static str, &'static str, String)> {
    match (obj.contains_key("key"), obj.contains_key("keys")) {
        (true, true) => vec![(
            "keys",
            "INVALID",
            "cache_read takes 'key' (one value) or 'keys' (an array of values), not both"
                .to_string(),
        )],
        (false, false) => vec![(
            "key",
            "REQUIRED",
            "cache_read requires 'key', or 'keys' to read several at once".to_string(),
        )],
        _ => Vec::new(),
    }
}

// -- Input schema (F53) --
//
// The table describing this handler's `function.input` lives next to the
// handler it describes. It used to sit in `schema.rs` with the other nine,
// which is how every schema/handler divergence in the 1.0 audit happened:
// a field was added, renamed or made conditional here and the table saying
// so was in a different file.

pub(super) const CACHE_READ_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the cache connector to read from.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "key",
        description: "Cache key to look up. JSONLogic: a literal, or an expression over the message. One of `key` and `keys` is required.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "keys",
        description: "Several keys to look up in one round trip (at most 1000). The result is an array in the same order, null for a miss. JSONLogic: an array of literals or expressions, or an expression evaluating to one.",
        kind: FieldKind::Array,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path in the message where the result is stored. Defaults to \"data\".",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_connector(read: bool) -> crate::connector::CacheConnectorConfig {
        crate::connector::CacheConnectorConfig {
            backend: "memory".to_string(),
            url: None,
            allow_private_urls: false,
            operations: crate::connector::CacheOperationGates { read, write: true },
        }
    }

    fn handler() -> CacheReadHandler {
        CacheReadHandler {
            cache_pool: Arc::new(CachePool::new(4, 60, 128)),
            registry: Arc::new(ConnectorRegistry::new(Default::default())),
        }
    }

    /// The seam the trait exists for.
    ///
    /// `run` receives a connector that is already resolved, already
    /// type-checked and already gated, so a handler body can be exercised
    /// in-process against a real backend — no registry entry, no engine, no
    /// workflow. The same shape is what would let the Mongo, Redis, SMTP and ES
    /// bodies out of their container-gated `#[ignore]`s, which is the reason to
    /// prefer the trait over a cheaper conformance fix.
    #[tokio::test]
    async fn the_run_seam_is_reachable_without_an_engine() {
        let h = handler();
        let datalogic = std::sync::Arc::new(dataflow_rs::datalogic_rs::Engine::new());
        let mut message = dataflow_rs::Message::from_value(&serde_json::json!({}));
        let mut ctx = dataflow_rs::engine::task_context::TaskContext::new(&mut message, &datalogic);
        let call = ConnectorCall {
            name: CacheReadHandler::NAME,
            connector: "c",
            channel: "ch".to_string(),
            output: "data".to_string(),
        };

        let value = h
            .run(
                CacheReadKeys::One("absent-key".to_string()),
                &memory_connector(true),
                &call,
                &TemplatedInput::from(serde_json::json!({"connector": "c", "key": "absent-key"})),
                &mut ctx,
            )
            .await
            .expect("a miss is not an error");
        assert_eq!(
            value.value,
            Some(Value::Null),
            "a cache miss reads as null, not as a failure"
        );
    }

    /// The gate is the connector's answer rather than the backend's, which is
    /// why it is a separate method: it has to be decided before anything is
    /// dialled. It is also now callable on its own.
    #[test]
    fn a_write_only_connector_refuses_a_read() {
        let err = <CacheReadHandler as ConnectorHandler>::gate(
            &CacheReadKeys::One("k".to_string()),
            &memory_connector(false),
            "c",
        )
        .expect_err("a read must be refused when the gate is off");
        // The specific refusal is the operator-facing detail; `msg` is the
        // caller-safe half. Both must survive the conversion.
        assert_eq!(err.msg, "Request validation failed");
        let detail = err.detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("operation 'read' is disabled"),
            "the refusal must name the gate it hit: {detail:?}"
        );
    }
}
