use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use mongodb::bson::{self, Document};
use serde_json::Value;

use super::connector_helpers::{
    ConnectorCall, apply_output, resolve_value, timed_query, to_connect_error,
};
use super::mongo_common::{
    docs_to_json, drain_capped, require_mongo_connector, resolve_document, resolve_u64,
};
use super::schema::{FieldKind, FieldSchema};
use crate::config::QueryConfig;
use crate::connector::ConnectorRegistry;
use crate::connector::mongo_pool::MongoPoolCache;
use crate::query::QueryError;

/// This handler's name in metrics, profiles and error messages (F48).
const NAME: &str = "mongo_read";

/// Workflow function handler for reading documents from MongoDB.
pub struct MongoReadHandler {
    pub pool_cache: Arc<MongoPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
    /// The `[query]` bounds: `max_limit` caps both an explicit `limit` and the
    /// drained result size (F10 — an unbounded `find` must not OOM the
    /// process); `max_skip` bounds `skip` (W12).
    pub limits: QueryConfig,
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

        // #263: optional find options — additive; a task naming none behaves
        // exactly as before.
        let projection = resolve_document(input, "projection", NAME, ctx)?;
        let sort = resolve_document(input, "sort", NAME, ctx)?;
        let limit = resolve_u64(input, "limit", NAME, ctx)?;
        let skip = resolve_u64(input, "skip", NAME, ctx)?;
        // The dialect's reject-never-clamp rule for both bounds. An absent
        // `limit` deliberately does NOT fall back to `default_limit`: the
        // pre-#263 contract is "everything the filter matches, capped", and a
        // silent page-size default would change existing tasks' results.
        if let Some(l) = limit
            && l > self.limits.max_limit
        {
            return Err(QueryError::LimitExceeded {
                requested: l,
                max: self.limits.max_limit,
            }
            .into());
        }
        if let Some(s) = skip
            && s > self.limits.max_skip
        {
            return Err(QueryError::SkipExceeded {
                requested: s,
                max: self.limits.max_skip,
            }
            .into());
        }

        call.run(&self.registry, async {
            let connector_config = call.resolve(&self.registry, Some("read")).await?;
            let db_config = require_mongo_connector(&connector_config, NAME, call.connector)?;

            let client = self
                .pool_cache
                .get_client(call.connector, db_config)
                .await
                .map_err(to_connect_error)?;

            let coll = client.database(database).collection::<Document>(collection);
            let cap = self.limits.max_limit as usize;
            let docs: Vec<Document> = timed_query(db_config.query_timeout_ms, call.name, async {
                // F11: the Mongo driver has no per-query timeout of its
                // own here — timed_query bounds connect + find + drain.
                let mut find = coll.find(filter_doc);
                if let Some(p) = projection {
                    find = find.projection(p);
                }
                if let Some(s) = sort {
                    find = find.sort(s);
                }
                if let Some(sk) = skip {
                    find = find.skip(sk);
                }
                if let Some(l) = limit {
                    find = find.limit(l as i64);
                }
                let cursor = find.await.map_err(|e| e.to_string())?;
                drain_capped(cursor, cap, NAME).await
            })
            .await?;

            apply_output(ctx, call.output, docs_to_json(&docs));
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
        alias: None,
    },
    FieldSchema {
        name: "database",
        description: "Mongo database name.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "collection",
        description: "Mongo collection name.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
        alias: None,
    },
    FieldSchema {
        name: "filter",
        description: "Mongo find() filter document (extended JSON: $oid, $date, ... work). Defaults to {}. Accepts {\"var\": \"path\"} to read the value from the message.",
        kind: FieldKind::Object,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "projection",
        description: "Mongo projection document (e.g. {\"name\": 1, \"_id\": 0}). Accepts {\"var\": \"path\"}.",
        kind: FieldKind::Object,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "sort",
        description: "Mongo sort document (e.g. {\"created_at\": -1}). Accepts {\"var\": \"path\"}.",
        kind: FieldKind::Object,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "limit",
        description: "Maximum documents to return; must not exceed query.max_limit. Accepts {\"var\": \"path\"}.",
        kind: FieldKind::Number,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "skip",
        description: "Documents to skip before returning; must not exceed query.max_skip. Accepts {\"var\": \"path\"}.",
        kind: FieldKind::Number,
        required: false,
        resolvable: true,
        alias: None,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where matched documents are written.",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
        alias: None,
    },
];
