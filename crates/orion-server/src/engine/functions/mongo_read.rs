use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::task_context::TaskContext;
use mongodb::bson::{self, Document};
use serde_json::Value;

use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{
    ConnectorCall, require_op_allowed, resolve_value, timed_query, to_connect_error,
};
use super::mongo_common::{
    docs_to_json, drain_capped, require_mongo_backend, resolve_document, resolve_u64,
};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::TemplatedInput;
use crate::config::QueryConfig;
use crate::connector::ConnectorRegistry;
use crate::connector::mongo_pool::MongoPoolCache;
use crate::engine::HandlerError;
use crate::query::QueryError;

/// This handler's name, for the helpers that take it as an argument — a
/// reference to the one place it is written (F48).
const NAME: &str = <MongoReadHandler as ConnectorHandler>::NAME;

/// Workflow function handler for reading documents from MongoDB.
pub struct MongoReadHandler {
    pub pool_cache: Arc<MongoPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
    /// The `[query]` bounds: `max_limit` caps both an explicit `limit` and the
    /// drained result size (F10 — an unbounded `find` must not OOM the
    /// process); `max_skip` bounds `skip` (W12).
    pub limits: QueryConfig,
}

/// The `find` the task describes, with every message-dependent part already
/// folded: `{"var": ..}` nodes may sit at any depth of the filter, so a
/// per-request query is expressible.
pub struct MongoFind {
    database: String,
    collection: String,
    filter: Document,
    projection: Option<Document>,
    sort: Option<Document>,
    limit: Option<u64>,
    skip: Option<u64>,
}

#[async_trait]
impl ConnectorHandler for MongoReadHandler {
    const NAME: &'static str = "mongo_read";
    type Kind = crate::connector::kind::Db;
    type Input = TemplatedInput;
    type Parsed = MongoFind;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        ctx: &TaskContext<'_>,
    ) -> Result<Self::Parsed, HandlerError> {
        let database = call.require_str(input, "database")?.to_string();
        let collection = call.require_str(input, "collection")?.to_string();

        let filter_val = input
            .get("filter")
            .map(|f| resolve_value(f, ctx))
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        let filter = bson::to_document(&filter_val)
            .map_err(|e| DataflowError::Validation(format!("Invalid MongoDB filter: {e}")))?;

        // #263: optional find options — additive; a task naming none behaves
        // exactly as before.
        let limit = resolve_u64(input, "limit", call.name, ctx)?;
        let skip = resolve_u64(input, "skip", call.name, ctx)?;
        // The dialect's reject-never-clamp rule for both bounds. An absent
        // `limit` deliberately does NOT fall back to `default_limit`: the
        // pre-#263 contract is "everything the filter matches, capped", and a
        // silent page-size default would change existing tasks' results.
        if let Some(l) = limit
            && l > self.limits.max_limit
        {
            return Err(DataflowError::from(QueryError::LimitExceeded {
                requested: l,
                max: self.limits.max_limit,
            })
            .into());
        }
        if let Some(s) = skip
            && s > self.limits.max_skip
        {
            return Err(DataflowError::from(QueryError::SkipExceeded {
                requested: s,
                max: self.limits.max_skip,
            })
            .into());
        }

        Ok(MongoFind {
            database,
            collection,
            filter,
            projection: resolve_document(input.raw(), "projection", call.name, ctx)?,
            sort: resolve_document(input.raw(), "sort", call.name, ctx)?,
            limit,
            skip,
        })
    }

    fn gate(
        _parsed: &Self::Parsed,
        conn: &crate::connector::DbConnectorConfig,
        connector: &str,
    ) -> Result<(), HandlerError> {
        require_op_allowed(&conn.operations, "read", connector)?;
        Ok(require_mongo_backend(
            conn,
            <Self as ConnectorHandler>::NAME,
            connector,
        )?)
    }

    async fn run(
        &self,
        find: Self::Parsed,
        db_config: &crate::connector::DbConnectorConfig,
        call: &ConnectorCall<'_>,
        _input: &TemplatedInput,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        let client = self
            .pool_cache
            .get_client(call.connector, db_config)
            .await
            .map_err(to_connect_error)?;

        let coll = client
            .database(&find.database)
            .collection::<Document>(&find.collection);
        let cap = self.limits.max_limit as usize;
        let docs: Vec<Document> = timed_query(db_config.query_timeout_ms, call.name, async {
            // F11: the Mongo driver has no per-query timeout of its own here —
            // timed_query bounds connect + find + drain.
            let mut q = coll.find(find.filter);
            if let Some(p) = find.projection {
                q = q.projection(p);
            }
            if let Some(s) = find.sort {
                q = q.sort(s);
            }
            if let Some(sk) = find.skip {
                q = q.skip(sk);
            }
            if let Some(l) = find.limit {
                q = q.limit(l as i64);
            }
            let cursor = q.await.map_err(|e| e.to_string())?;
            drain_capped(cursor, cap, NAME).await
        })
        .await?;

        Ok(docs_to_json(&docs, NAME)?.into())
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
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "database",
        description: "Mongo database name.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "collection",
        description: "Mongo collection name.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "filter",
        description: "Mongo find() filter document (extended JSON: $oid, $date, ... work). Defaults to {}. Accepts {\"var\": \"path\"} to read the value from the message.",
        kind: FieldKind::Object,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "projection",
        description: "Mongo projection document (e.g. {\"name\": 1, \"_id\": 0}). Accepts {\"var\": \"path\"}.",
        kind: FieldKind::Object,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "sort",
        description: "Mongo sort document (e.g. {\"created_at\": -1}). Accepts {\"var\": \"path\"}.",
        kind: FieldKind::Object,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "limit",
        description: "Maximum documents to return; must not exceed query.max_limit. JSONLogic: a literal, or an expression over the message.",
        kind: FieldKind::Number,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "skip",
        description: "Documents to skip before returning; must not exceed query.max_skip. JSONLogic: a literal, or an expression over the message.",
        kind: FieldKind::Number,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path where matched documents are written.",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];
