use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::task_context::TaskContext;
use serde_json::json;

use super::cache_read::MAX_KEYS;
use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{
    ConnectorCall, output_declared, require_op, resolve_required_str_list, to_connect_error,
    to_exec_error,
};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::TemplatedInput;
use crate::connector::ConnectorRegistry;
use crate::connector::cache_backend::{CachePool, CachePurpose};
use crate::engine::HandlerError;

/// Workflow function handler for deleting exact keys from a cache backend.
///
/// The half of "cache a read until a write invalidates it" that `cache_read`
/// and `cache_write` could not express: a workflow that changes the data can
/// now drop the entries it made stale instead of waiting out their TTL.
/// Exact keys only. A prefix or pattern form would be a `SCAN` over the whole
/// keyspace on Redis; a generation counter (`cache_incr`) is the scalable way
/// to retire a family of keys.
pub struct CacheDeleteHandler {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

#[async_trait]
impl ConnectorHandler for CacheDeleteHandler {
    const NAME: &'static str = "cache_delete";
    type Kind = crate::connector::kind::Cache;
    type Input = TemplatedInput;
    type Parsed = Vec<String>;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        ctx: &TaskContext<'_>,
    ) -> Result<Self::Parsed, HandlerError> {
        Ok(resolve_required_str_list(
            input, "keys", call.name, ctx, MAX_KEYS,
        )?)
    }

    fn gate(
        _keys: &Self::Parsed,
        conn: &crate::connector::CacheConnectorConfig,
        connector: &str,
    ) -> Result<(), HandlerError> {
        // A delete changes what later reads see, so a read-only connector
        // refuses it exactly as it refuses `cache_write`.
        Ok(require_op(conn.operations.write, "write", connector)?)
    }

    async fn run(
        &self,
        keys: Self::Parsed,
        conn: &crate::connector::CacheConnectorConfig,
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        // Workflow-purpose namespace (S19), like `cache_read` and
        // `cache_write`: a memory backend here can never reach the dedup store
        // or the response cache.
        let backend = self
            .cache_pool
            .get_backend(CachePurpose::Workflow, call.connector, conn)
            .await
            .map_err(to_connect_error)?;
        let deleted = backend.remove_many(&keys).await.map_err(to_exec_error)?;
        tracing::debug!(keys = keys.len(), deleted, "Deleted cache keys");

        // Recorded only where the task asks: the delete is the effect, and a
        // default of `data` would overwrite the message with a count.
        Ok(if output_declared(input) {
            json!({ "deleted": deleted }).into()
        } else {
            Produced::nothing()
        })
    }
}

// -- Input schema (F53) --

pub(super) const CACHE_DELETE_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the cache connector to delete from.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "keys",
        description: "Exact keys to delete (at most 1000). A key that is not present is not an error. JSONLogic: an array of literals or expressions, or an expression evaluating to one.",
        kind: FieldKind::Array,
        required: true,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where `{\"deleted\": n}`, the number of keys that existed, is stored. Omit to record nothing.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_connector(write: bool) -> crate::connector::CacheConnectorConfig {
        crate::connector::CacheConnectorConfig {
            backend: "memory".to_string(),
            url: None,
            allow_private_urls: false,
            operations: crate::connector::CacheOperationGates { read: true, write },
        }
    }

    #[test]
    fn a_read_only_connector_refuses_a_delete() {
        let err = <CacheDeleteHandler as ConnectorHandler>::gate(
            &vec!["k".to_string()],
            &memory_connector(false),
            "c",
        )
        .expect_err("a delete must be refused when writes are off");
        assert!(
            err.detail
                .as_deref()
                .unwrap_or_default()
                .contains("operation 'write' is disabled")
        );
    }

    /// The count is of keys that existed, and the output is recorded only
    /// where the task names one.
    #[tokio::test]
    async fn deletes_report_what_existed() {
        let pool = Arc::new(CachePool::new(4, 60, 128));
        let conn = memory_connector(true);
        let backend = pool
            .get_backend(CachePurpose::Workflow, "c", &conn)
            .await
            .expect("test");
        backend.set("a", "1").await.expect("test");
        backend.set("b", "2").await.expect("test");
        let h = CacheDeleteHandler {
            cache_pool: pool,
            registry: Arc::new(ConnectorRegistry::new(Default::default())),
        };
        let datalogic = Arc::new(dataflow_rs::datalogic_rs::Engine::new());
        let mut message = dataflow_rs::Message::from_value(&json!({}));
        let mut ctx = TaskContext::new(&mut message, &datalogic);
        let call = ConnectorCall {
            name: CacheDeleteHandler::NAME,
            connector: "c",
            channel: "ch".to_string(),
            output: "data.del".to_string(),
        };
        let keys = vec!["a".to_string(), "b".to_string(), "absent".to_string()];

        let with_output =
            TemplatedInput::from(json!({"connector": "c", "keys": [], "output": "data.del"}));
        let produced = h
            .run(keys.clone(), &conn, &call, &with_output, &mut ctx)
            .await
            .expect("test");
        assert_eq!(produced.value, Some(json!({"deleted": 2})));
        assert_eq!(backend.get("a").await.expect("test"), None);

        let without = TemplatedInput::from(json!({"connector": "c", "keys": []}));
        let produced = h
            .run(keys, &conn, &call, &without, &mut ctx)
            .await
            .expect("test");
        assert_eq!(produced.value, None, "no output declared, nothing recorded");
    }
}
