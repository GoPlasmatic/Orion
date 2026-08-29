use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::task_context::TaskContext;
use serde_json::Value;

use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{
    ConnectorCall, bind_json_params, reject_mongo_connector, require_op_allowed, timed_query,
    to_connect_error,
};
use super::db_read::DbRead;
use super::schema::{FieldKind, FieldSchema};
use crate::connector::ConnectorRegistry;
use crate::connector::pool_cache::SqlPoolCache;
use crate::engine::HandlerError;

/// Executes SQL write queries (INSERT, UPDATE, DELETE) against external databases
/// configured via connectors.
pub struct DbWriteHandler {
    pub pool_cache: Arc<SqlPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
}

#[async_trait]
impl ConnectorHandler for DbWriteHandler {
    const NAME: &'static str = "db_write";
    type Kind = crate::connector::kind::Db;
    type Input = Value;
    /// The same shape `db_read` parses — a literal statement and message-derived
    /// binds — because the two differ in what the database does with it, not in
    /// what the task says.
    type Parsed = DbRead;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        call: &ConnectorCall<'_>,
        input: &Value,
        ctx: &TaskContext<'_>,
    ) -> Result<Self::Parsed, HandlerError> {
        DbRead::parse_statement(call, input, ctx)
    }

    fn gate(
        _parsed: &Self::Parsed,
        conn: &crate::connector::DbConnectorConfig,
        connector: &str,
    ) -> Result<(), HandlerError> {
        // Raw SQL cannot be classified per-op; it has its own gate.
        require_op_allowed(&conn.operations, "raw_write", connector)?;
        Ok(reject_mongo_connector(
            <Self as ConnectorHandler>::NAME,
            connector,
            conn,
        )?)
    }

    async fn run(
        &self,
        write: Self::Parsed,
        db_config: &crate::connector::DbConnectorConfig,
        call: &ConnectorCall<'_>,
        _input: &Value,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        let pool = self
            .pool_cache
            .get_pool(call.connector, db_config)
            .await
            .map_err(to_connect_error)?;

        let sqlx_query = bind_json_params(sqlx::query(write.query()), write.params());
        let result = timed_query(
            db_config.query_timeout_ms,
            call.name,
            sqlx_query.execute(&pool),
        )
        .await?;

        Ok(serde_json::json!({
            "rows_affected": result.rows_affected(),
        })
        .into())
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
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "query",
        description: "INSERT/UPDATE/DELETE statement.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "params",
        description: "Array of values to bind to query placeholders, in order. Accepts {\"var\": \"path\"} to read the value from the message.",
        kind: FieldKind::Array,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where the rows-affected count is written.",
        kind: FieldKind::String,
        ..FieldSchema::DEFAULT
    },
];
