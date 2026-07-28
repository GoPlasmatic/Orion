use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use futures::TryStreamExt;
use mongodb::bson::{self, Document};
use serde_json::Value;

use super::connector_helpers::{
    ConnectorCall, apply_output, is_mongo, require_db_connector, resolve_value, timed_query,
    to_connect_error,
};
use super::schema::{FieldKind, FieldSchema};
use crate::connector::ConnectorRegistry;
use crate::connector::mongo_pool::MongoPoolCache;

/// This handler's name in metrics, profiles and error messages (F48).
const NAME: &str = "mongo_read";

/// Workflow function handler for reading documents from MongoDB.
pub struct MongoReadHandler {
    pub pool_cache: Arc<MongoPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
    /// Hard row cap, from `query.max_limit` (F10): an unbounded `find` must
    /// not OOM the process.
    pub max_rows: usize,
}

#[async_trait]
impl AsyncFunctionHandler for MongoReadHandler {
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // F48/F58: the literal prologue first — `connector`, `database` and
        // `collection` are all literal keys, so a task missing any of them must
        // be told before the filter is folded against the message.
        let call = ConnectorCall::begin(NAME, input, ctx)?;
        let database = call.require_str(input, "database")?;
        let collection = call.require_str(input, "collection")?;

        // The filter is folded against the message context before the handler
        // body takes `ctx` mutably; `{"var": ..}` nodes may sit at any depth of
        // the document, so a per-request query is expressible.
        let filter_val = input
            .get("filter")
            .map(|f| resolve_value(f, ctx))
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        let filter_doc = bson::to_document(&filter_val)
            .map_err(|e| DataflowError::Validation(format!("Invalid MongoDB filter: {e}")))?;

        call.run(&self.registry, async {
            let connector_config = call.resolve(&self.registry, Some("read")).await?;
            let db_config = require_db_connector(&connector_config, call.connector)?;
            // `require_db_connector` only checks the ConnectorConfig variant, so
            // a SQL connector reached the Mongo driver and produced an opaque
            // driver error instead of a validation one. `data_query` already
            // makes this distinction; `mongo_read` did not (proposal F29).
            if !is_mongo(&db_config.connection_string) {
                return Err(DataflowError::Validation(format!(
                    "{NAME} requires a MongoDB connector, but '{}' has a non-MongoDB \
                     connection string (expected a mongodb:// or mongodb+srv:// URL)",
                    call.connector
                )));
            }

            let client = self
                .pool_cache
                .get_client(call.connector, db_config)
                .await
                .map_err(to_connect_error)?;

            let coll = client.database(database).collection::<Document>(collection);
            let max_rows = self.max_rows;
            let docs: Vec<Document> = timed_query(db_config.query_timeout_ms, call.name, async {
                // F11: the Mongo driver has no per-query timeout of its
                // own here — timed_query bounds connect + find + drain.
                let mut cursor = coll.find(filter_doc).await.map_err(|e| e.to_string())?;
                let mut docs: Vec<Document> = Vec::new();
                while let Some(doc) = cursor.try_next().await.map_err(|e| e.to_string())? {
                    if docs.len() >= max_rows {
                        return Err(format!(
                            "{NAME} result exceeds query.max_limit ({max_rows} \
                             documents) — add a filter/limit or raise the cap"
                        ));
                    }
                    docs.push(doc);
                }
                Ok(docs)
            })
            .await?;

            let result: Vec<Value> = docs
                .iter()
                .filter_map(|doc| serde_json::to_value(doc).ok())
                .collect();

            apply_output(ctx, call.output, Value::Array(result));
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

pub(super) const MONGO_READ_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the MongoDB connector.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
    },
    FieldSchema {
        name: "database",
        description: "Mongo database name.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
    },
    FieldSchema {
        name: "collection",
        description: "Mongo collection name.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
    },
    FieldSchema {
        name: "filter",
        description: "Mongo find() filter document. Defaults to {}. Accepts {\"var\": \"path\"} to read the value from the message.",
        kind: FieldKind::Object,
        required: false,
        resolvable: true,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where matched documents are written.",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
    },
];
