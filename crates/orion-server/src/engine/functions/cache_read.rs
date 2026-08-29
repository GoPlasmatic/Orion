use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::task_context::TaskContext;
use serde_json::Value;

use super::connector_handler::ConnectorHandler;
use super::connector_helpers::{
    ConnectorCall, require_op, resolve_required_str, to_connect_error, to_exec_error,
};
use super::schema::{FieldKind, FieldSchema};
use crate::connector::ConnectorRegistry;
use crate::connector::cache_backend::{CachePool, CachePurpose};

/// Workflow function handler for reading values from a cache backend.
pub struct CacheReadHandler {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

#[async_trait]
impl ConnectorHandler for CacheReadHandler {
    const NAME: &'static str = "cache_read";
    type Kind = crate::connector::kind::Cache;
    /// The key, resolved against the message. `{"var": "data.id"}` is the
    /// whole point of a per-request cache lookup, so it has to be folded
    /// before the body takes `ctx` mutably.
    type Parsed = String;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        call: &ConnectorCall<'_>,
        input: &Value,
        ctx: &TaskContext<'_>,
    ) -> Result<Self::Parsed, crate::engine::HandlerError> {
        Ok(resolve_required_str(input, "key", call.name, ctx)?)
    }

    fn gate(
        conn: &crate::connector::CacheConnectorConfig,
        connector: &str,
    ) -> Result<(), crate::engine::HandlerError> {
        // F22e: a cache connector can be made write-only in its config.
        Ok(require_op(conn.operations.read, "read", connector)?)
    }

    async fn run(
        &self,
        key: String,
        conn: &crate::connector::CacheConnectorConfig,
        call: &ConnectorCall<'_>,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<Value, crate::engine::HandlerError> {
        // Workflow-purpose namespace (S19) — the mirror of `cache_write`, so a
        // workflow reads exactly what workflows wrote and never the dedup store
        // or response cache.
        let backend = self
            .cache_pool
            .get_backend(CachePurpose::Workflow, call.connector, conn)
            .await
            .map_err(to_connect_error)?;

        let value = backend.get(&key).await.map_err(to_exec_error)?;

        // `cache_write` JSON-encodes everything, so parsing is its exact
        // inverse. The raw-string fallback is kept deliberately: a key written
        // by something other than Orion may hold a bare string, and surfacing
        // that as a string beats failing the task.
        Ok(match value {
            Some(v) => serde_json::from_str::<Value>(&v).unwrap_or(Value::String(v)),
            None => Value::Null,
        })
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
        description: "Cache key to look up. Accepts {\"var\": \"path\"} to read the value from the message.",
        kind: FieldKind::String,
        required: true,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path in the message where the result is stored. Defaults to \"data\".",
        kind: FieldKind::String,
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
            output: "data",
        };

        let value = h
            .run(
                "absent-key".to_string(),
                &memory_connector(true),
                &call,
                &mut ctx,
            )
            .await
            .expect("a miss is not an error");
        assert_eq!(
            value,
            Value::Null,
            "a cache miss reads as null, not as a failure"
        );
    }

    /// The gate is the connector's answer rather than the backend's, which is
    /// why it is a separate method: it has to be decided before anything is
    /// dialled. It is also now callable on its own.
    #[test]
    fn a_write_only_connector_refuses_a_read() {
        let err = <CacheReadHandler as ConnectorHandler>::gate(&memory_connector(false), "c")
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
