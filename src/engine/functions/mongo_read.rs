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
    apply_output, extract_output_path, is_mongo, profile_handler, require_db_connector,
    require_op_allowed, require_str_field, resolve_connector, resolve_value, timed_query,
    to_exec_error,
};
use crate::connector::ConnectorRegistry;
use crate::connector::mongo_pool::MongoPoolCache;

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
        // The filter is folded against the message context before the handler
        // body takes `ctx` mutably; `{"var": ..}` nodes may sit at any depth of
        // the document, so a per-request query is expressible.
        let filter_val = input
            .get("filter")
            .map(|f| resolve_value(f, ctx))
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        let filter_doc = bson::to_document(&filter_val)
            .map_err(|e| DataflowError::Validation(format!("Invalid MongoDB filter: {e}")))?;

        profile_handler("mongo_read", input, async move {
            let connector_name = require_str_field(input, "connector", "mongo_read")?;
            let database = require_str_field(input, "database", "mongo_read")?;
            let collection = require_str_field(input, "collection", "mongo_read")?;

            let connector_config = resolve_connector(&self.registry, connector_name).await?;
            let db_config = require_db_connector(&connector_config, connector_name)?;
            // `require_db_connector` only checks the ConnectorConfig variant, so
            // a SQL connector reached the Mongo driver and produced an opaque
            // driver error instead of a validation one. `data_query` already
            // makes this distinction; `mongo_read` did not (proposal F29).
            if !is_mongo(&db_config.connection_string) {
                return Err(DataflowError::Validation(format!(
                    "mongo_read requires a MongoDB connector, but '{connector_name}' has a \
                     non-MongoDB connection string (expected a mongodb:// or mongodb+srv:// URL)"
                )));
            }
            require_op_allowed(&db_config.operations, "read", connector_name)?;

            let client = self
                .pool_cache
                .get_client(connector_name, db_config)
                .await
                .map_err(to_exec_error)?;

            let coll = client.database(database).collection::<Document>(collection);
            let max_rows = self.max_rows;
            let docs: Vec<Document> =
                timed_query(db_config.query_timeout_ms, "mongo_read", async {
                    // F11: the Mongo driver has no per-query timeout of its
                    // own here — timed_query bounds connect + find + drain.
                    let mut cursor = coll.find(filter_doc).await.map_err(|e| e.to_string())?;
                    let mut docs: Vec<Document> = Vec::new();
                    while let Some(doc) = cursor.try_next().await.map_err(|e| e.to_string())? {
                        if docs.len() >= max_rows {
                            return Err(format!(
                                "mongo_read result exceeds query.max_limit ({max_rows} \
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

            let output_path = extract_output_path(input);
            apply_output(ctx, output_path, Value::Array(result));
            Ok(TaskOutcome::Success)
        })
        .await
    }
}
