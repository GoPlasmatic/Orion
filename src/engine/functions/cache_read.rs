use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::Value;

use super::connector_helpers::{
    ConnectorCall, apply_output, require_cache_connector, resolve_required_str, to_connect_error,
    to_exec_error,
};
use super::schema::{FieldKind, FieldSchema};
use crate::connector::ConnectorRegistry;
use crate::connector::cache_backend::CachePool;

/// This handler's name in metrics, profiles and error messages (F48).
const NAME: &str = "cache_read";

/// Workflow function handler for reading values from a cache backend.
pub struct CacheReadHandler {
    pub cache_pool: Arc<CachePool>,
    pub registry: Arc<ConnectorRegistry>,
}

#[async_trait]
impl AsyncFunctionHandler for CacheReadHandler {
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // F48/F58: the literal prologue first — a task naming no connector
        // must say so before anything about the message is consulted.
        let call = ConnectorCall::begin(NAME, input, ctx)?;

        // Resolve the key against the message context before the handler body
        // takes `ctx` mutably — `{"var": "data.id"}` is the whole point of a
        // per-request cache lookup.
        let key = resolve_required_str(input, "key", call.name, ctx)?;

        call.run(&self.registry, async {
            let connector_config = call.resolve(&self.registry, None).await?;
            let cache_config = require_cache_connector(&connector_config, call.connector)?;

            let backend = self
                .cache_pool
                .get_backend(call.connector, cache_config)
                .await
                .map_err(to_connect_error)?;

            let value = backend.get(&key).await.map_err(to_exec_error)?;

            // `cache_write` JSON-encodes everything, so parsing is its exact
            // inverse. The raw-string fallback is kept deliberately: a key
            // written by something other than Orion may hold a bare string,
            // and surfacing that as a string beats failing the task.
            let result = match value {
                Some(v) => serde_json::from_str::<Value>(&v).unwrap_or(Value::String(v)),
                None => Value::Null,
            };

            apply_output(ctx, call.output, result);
            Ok(TaskOutcome::Success)
        })
        .await
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
        resolvable: false,
    },
    FieldSchema {
        name: "key",
        description: "Cache key to look up. Accepts {\"var\": \"path\"} to read the value from the message.",
        kind: FieldKind::String,
        required: true,
        resolvable: true,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path in the message where the result is stored. Defaults to \"data\".",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
    },
];
