use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::Value;

use super::connector_helpers::{
    ConnectorCall, apply_output, bind_json_params, reject_mongo_connector, require_db_connector,
    resolve_bind_params, timed_query, to_connect_error,
};
use super::schema::{FieldKind, FieldSchema};
use crate::connector::ConnectorRegistry;
use crate::connector::pool_cache::SqlPoolCache;

/// This handler's name in metrics, profiles and error messages (F48).
const NAME: &str = "db_write";

/// Executes SQL write queries (INSERT, UPDATE, DELETE) against external databases
/// configured via connectors.
pub struct DbWriteHandler {
    pub pool_cache: Arc<SqlPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
}

#[async_trait]
impl AsyncFunctionHandler for DbWriteHandler {
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // F48/F58: the literal prologue first — `connector` and `query` are
        // both literal keys, so a task missing either must be told before
        // anything about the message is consulted.
        let call = ConnectorCall::begin(NAME, input, ctx)?;
        let query = call.require_str(input, "query")?;

        // Bind values are resolved against the message context; the SQL text
        // itself is never message-derived, so parameters stay the only
        // request-controlled part of the statement.
        let params = resolve_bind_params(input, call.name, ctx)?;

        call.run(&self.registry, async {
            // Raw SQL cannot be classified per-op; it has its own gate.
            let connector_config = call.resolve(&self.registry, Some("raw_write")).await?;
            let db_config = require_db_connector(&connector_config, call.connector)?;
            reject_mongo_connector(call.name, call.connector, db_config)?;

            let pool = self
                .pool_cache
                .get_pool(call.connector, db_config)
                .await
                .map_err(to_connect_error)?;

            let sqlx_query = bind_json_params(sqlx::query(query), &params);

            let result = timed_query(
                db_config.query_timeout_ms,
                call.name,
                sqlx_query.execute(&pool),
            )
            .await?;

            let output = serde_json::json!({
                "rows_affected": result.rows_affected(),
            });

            apply_output(ctx, call.output, output);
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

pub(super) const DB_WRITE_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the SQL connector to execute against.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
    },
    FieldSchema {
        name: "query",
        description: "INSERT/UPDATE/DELETE statement.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
    },
    FieldSchema {
        name: "params",
        description: "Array of values to bind to query placeholders, in order. Accepts {\"var\": \"path\"} to read the value from the message.",
        kind: FieldKind::Array,
        required: false,
        resolvable: true,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the rows-affected count is written.",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
    },
];
