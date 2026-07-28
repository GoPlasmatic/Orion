use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use mongodb::bson::Document;
use serde_json::{Value, json};
use sqlx::any::AnyRow;

use super::connector_helpers::{
    ConnectorCall, apply_output, es_request, es_write_error, is_mongo, require_op_allowed,
    resolve_params, send_es, timed_query, to_connect_error, to_exec_error,
};
use super::db_read::rows_to_json;
use super::schema::{FieldKind, FieldSchema};
use crate::connector::mongo_pool::MongoPoolCache;
use crate::connector::pool_cache::SqlPoolCache;
use crate::connector::{ConnectorConfig, ConnectorRegistry, EsConnectorConfig};
use crate::query::backend::es::{EsWrite, bulk_ndjson};
use crate::query::backend::mongo::MongoWrite;
use crate::query::write::{ResolvedWrite, WriteOp};
use crate::query::{self, SqlDialect};
use crate::storage::detect_backend;

/// This handler's name in metrics, profiles and error messages (F48).
const NAME: &str = "data_write";

/// Executes a portable `data_write` — one backend-neutral mutation envelope
/// (`op`/`target`/`values`/`set`/`filter`/`on_conflict`/`returning`) that renders
/// to a native SQL `INSERT`/`UPDATE`/`DELETE`/upsert, a MongoDB write, or an
/// Elasticsearch write — against a SQL, Mongo, or ES connector. The translation
/// — including the write-safety guards (bulk cap, unfiltered mutation; W15) —
/// lives in `src/query/write.rs` and the per-backend renderers; this handler
/// wires it to the connector/pool machinery (ES runs over the HTTP client).
pub struct DataWriteHandler {
    pub pool_cache: Arc<SqlPoolCache>,
    pub mongo_pool_cache: Arc<MongoPoolCache>,
    pub http_client: reqwest::Client,
    pub registry: Arc<ConnectorRegistry>,
    /// Safety bounds (`max_rows` / `allow_unfiltered`) from `[write]`,
    /// enforced inside `resolve_write`.
    pub write_config: crate::config::WriteConfig,
}

#[async_trait]
impl AsyncFunctionHandler for DataWriteHandler {
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // F48/F58: the literal prologue first — a task naming no connector must
        // say so before the message is consulted at all.
        let call = ConnectorCall::begin(NAME, input, ctx)?;

        // Resolve `params` against the message context (the only point the
        // message touches the mutation); it produces literals, not SQL.
        let params = resolve_params(input.get("params"), ctx);

        call.run(&self.registry, async {
            // The op is only known after the envelope is parsed, so the
            // gate is applied below rather than here.
            let connector_config = call.resolve(&self.registry, None).await?;

            // Optional inline schema (privileged config): renames, allowlist, and
            // the per-column `writable` flag.
            let registry = match input.get("schema") {
                Some(s) => query::EntityRegistry::from_json(s)?,
                None => query::EntityRegistry::default(),
            };

            // W7: the mutation envelope is nested under `write`, mirroring
            // `data_query`'s `query`. Before 1.0 it was flat, sharing a
            // namespace with the handler keys (`connector`, `schema`,
            // `params`, `database`, `output`) — so those five names could
            // never be used by the dialect, and there was no single JSON
            // value that *was* the envelope. The flat form is still accepted
            // for one release; since `resolve_write` rejects unknown keys
            // (W6), the handler keys are stripped from it first.
            let legacy_flat: Value;
            let envelope = match input.get("write") {
                Some(w) => w,
                None => {
                    const HANDLER_KEYS: [&str; 5] =
                        ["connector", "schema", "params", "database", "output"];
                    legacy_flat = match input.as_object() {
                        Some(o) => Value::Object(
                            o.iter()
                                .filter(|(k, _)| !HANDLER_KEYS.contains(&k.as_str()))
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect(),
                        ),
                        None => input.clone(),
                    };
                    &legacy_flat
                }
            };

            // One backend-neutral resolution: parse envelope, fold params into
            // values/set, resolve physical names, coerce, lower the filter,
            // and enforce the write-safety guards (W15).
            let resolved =
                query::write::resolve_write(envelope, &params, &registry, &self.write_config)?;

            // Per-connector operation gates: a connector config can disable
            // individual ops (e.g. delete) regardless of what workflows ask for.
            if let Some(gates) = connector_config.operation_gates() {
                require_op_allowed(gates, resolved.op.as_str(), call.connector)?;
            }

            let result = match connector_config.as_ref() {
                ConnectorConfig::Db(db) if is_mongo(&db.connection_string) => {
                    let database = call.require_str(input, "database")?;
                    let mw = query::backend::mongo::render_write(&resolved)?;
                    let client = self
                        .mongo_pool_cache
                        .get_client(call.connector, db)
                        .await
                        .map_err(to_connect_error)?;
                    // F11: bound the write like the SQL branches bound theirs.
                    timed_query(db.query_timeout_ms, call.name, async {
                        execute_mongo(&client, database, mw)
                            .await
                            .map_err(|e| e.to_string())
                    })
                    .await?
                }
                ConnectorConfig::Db(db) => {
                    let dialect: SqlDialect = detect_backend(&db.connection_string)
                        .map_err(to_exec_error)?
                        .into();
                    let (sql, values) = query::backend::sql::render_write(&resolved, dialect)?;
                    let pool = self
                        .pool_cache
                        .get_pool(call.connector, db)
                        .await
                        .map_err(to_connect_error)?;
                    execute_sql(&pool, &sql, values, &resolved, db.query_timeout_ms).await?
                }
                ConnectorConfig::Es(es) => {
                    let ew = query::backend::es::render_write(&resolved)?;
                    run_es_write(&self.http_client, es, ew).await?
                }
                _ => {
                    return Err(DataflowError::Validation(format!(
                        "Connector '{}' is not a db or es connector",
                        call.connector
                    )));
                }
            };

            apply_output(ctx, call.output, result);
            Ok(TaskOutcome::Success)
        })
        .await
    }
}

/// Execute a rendered SQL write. When `returning` is requested the statement
/// returns rows (`fetch_all`); otherwise it is a plain `execute` returning the
/// affected-row count (and `last_insert_id` where the driver reports one).
async fn execute_sql(
    pool: &sqlx::AnyPool,
    sql: &str,
    values: sea_query_binder::SqlxValues,
    w: &ResolvedWrite,
    timeout_ms: Option<u64>,
) -> Result<Value, DataflowError> {
    if w.returning.is_empty() {
        let res = timed_query(
            timeout_ms,
            NAME,
            sqlx::query_with(sql, values).execute(pool),
        )
        .await?;
        let mut out = json!({ "rows_affected": res.rows_affected() });
        if matches!(w.op, WriteOp::Insert | WriteOp::Upsert)
            && let Some(id) = res.last_insert_id()
        {
            out["last_insert_id"] = json!(id);
        }
        Ok(out)
    } else {
        let rows: Vec<AnyRow> = timed_query(
            timeout_ms,
            NAME,
            sqlx::query_with(sql, values).fetch_all(pool),
        )
        .await?;
        let returning = rows_to_json(&rows)?;
        let count = match &returning {
            Value::Array(a) => a.len(),
            _ => 0,
        };
        Ok(json!({ "rows_affected": count, "returning": returning }))
    }
}

/// Execute a rendered MongoDB write and normalise its result to a small object.
async fn execute_mongo(
    client: &mongodb::Client,
    database: &str,
    mw: MongoWrite,
) -> Result<Value, DataflowError> {
    let db = client.database(database);
    match mw {
        MongoWrite::Insert { collection, docs } => {
            if docs.is_empty() {
                return Ok(json!({ "inserted": 0, "ids": [] }));
            }
            let coll = db.collection::<Document>(&collection);
            let res = coll.insert_many(docs).await.map_err(to_exec_error)?;
            // `inserted_ids` is keyed by input index; return in index order.
            let mut pairs: Vec<(usize, mongodb::bson::Bson)> =
                res.inserted_ids.into_iter().collect();
            pairs.sort_by_key(|(i, _)| *i);
            let ids: Vec<Value> = pairs
                .into_iter()
                .filter_map(|(_, b)| serde_json::to_value(b).ok())
                .collect();
            Ok(json!({ "inserted": ids.len(), "ids": ids }))
        }
        MongoWrite::Update {
            collection,
            filter,
            update,
            upsert,
            multi,
        } => {
            let coll = db.collection::<Document>(&collection);
            let res = if multi {
                coll.update_many(filter, update)
                    .await
                    .map_err(to_exec_error)?
            } else {
                coll.update_one(filter, update)
                    .upsert(upsert)
                    .await
                    .map_err(to_exec_error)?
            };
            let mut out = json!({
                "matched": res.matched_count,
                "modified": res.modified_count,
            });
            if let Some(id) = res.upserted_id {
                out["upserted_id"] = serde_json::to_value(id).unwrap_or(Value::Null);
            }
            Ok(out)
        }
        MongoWrite::Delete { collection, filter } => {
            let coll = db.collection::<Document>(&collection);
            let res = coll.delete_many(filter).await.map_err(to_exec_error)?;
            Ok(json!({ "deleted": res.deleted_count }))
        }
    }
}

/// Build `{base}/{segments...}?{query...}` with percent-encoded path segments
/// (the document `_id` travels in the path).
fn es_url(base: &str, segments: &[&str], query: &[(&str, &str)]) -> Result<String, DataflowError> {
    let mut url = url::Url::parse(base).map_err(to_exec_error)?;
    url.path_segments_mut()
        .map_err(|_| DataflowError::Validation("es connector url cannot be a base".to_string()))?
        .pop_if_empty()
        .extend(segments);
    for (k, v) in query {
        url.query_pairs_mut().append_pair(k, v);
    }
    Ok(url.into())
}

/// Execute a rendered Elasticsearch write and normalise its result to the same
/// small object the Mongo path returns.
///
/// Every write requests a refresh (`wait_for` doc-level; `true` on the by-query
/// endpoints, which don't support `wait_for`) so a `data_query` later in the same
/// pipeline reads its own writes — parity with SQL/Mongo visibility, at a
/// throughput cost. A per-envelope knob is future work.
async fn run_es_write(
    client: &reqwest::Client,
    es: &EsConnectorConfig,
    ew: EsWrite,
) -> Result<Value, DataflowError> {
    match ew {
        EsWrite::BulkInsert { index, docs } => {
            if docs.is_empty() {
                return Ok(json!({ "inserted": 0, "ids": [] }));
            }
            let url = es_url(&es.url, &[&index, "_bulk"], &[("refresh", "wait_for")])?;
            let req = es_request(client, es, reqwest::Method::POST, &url)
                .await?
                .header("Content-Type", "application/x-ndjson")
                .body(bulk_ndjson(&docs));
            let (status, body) = send_es(req, es.max_response_size).await?;
            if !status.is_success() {
                return Err(es_write_error(status, &body));
            }
            // `_bulk` is ordered but non-transactional: any item error fails the
            // call (parity with atomic SQL / ordered Mongo), though earlier items
            // may already be applied — the documented §6.6 divergence.
            let items = body.get("items").and_then(|i| i.as_array());
            if body
                .get("errors")
                .and_then(|e| e.as_bool())
                .unwrap_or(false)
            {
                let first_error = items
                    .into_iter()
                    .flatten()
                    .filter_map(|it| it.get("index").or_else(|| it.get("create")))
                    .find_map(|a| a.get("error"))
                    .cloned()
                    .unwrap_or(Value::Null);
                return Err(DataflowError::function_execution(
                    format!("Elasticsearch bulk insert had failures: {first_error}"),
                    None,
                ));
            }
            let ids: Vec<Value> = items
                .into_iter()
                .flatten()
                .filter_map(|it| it.get("index").or_else(|| it.get("create")))
                .filter_map(|a| a.get("_id").cloned())
                .collect();
            Ok(json!({ "inserted": ids.len(), "ids": ids }))
        }
        EsWrite::UpdateByQuery { index, body } => {
            let resp = run_by_query(client, es, &index, "_update_by_query", &body).await?;
            Ok(json!({ "matched": resp["total"], "modified": resp["updated"] }))
        }
        EsWrite::DeleteByQuery { index, body } => {
            let resp = run_by_query(client, es, &index, "_delete_by_query", &body).await?;
            Ok(json!({ "deleted": resp["deleted"] }))
        }
        EsWrite::UpdateDoc { index, id, body } => {
            let url = es_url(
                &es.url,
                &[&index, "_update", &id],
                &[("refresh", "wait_for")],
            )?;
            let req = es_request(client, es, reqwest::Method::POST, &url)
                .await?
                .json(&body);
            let (status, resp) = send_es(req, es.max_response_size).await?;
            if !status.is_success() {
                return Err(es_write_error(status, &resp));
            }
            Ok(match resp.get("result").and_then(|r| r.as_str()) {
                Some("created") => json!({ "matched": 0, "modified": 0, "upserted_id": id }),
                Some("noop") => json!({ "matched": 1, "modified": 0 }),
                _ => json!({ "matched": 1, "modified": 1 }),
            })
        }
        EsWrite::CreateDoc { index, id, doc } => {
            let url = es_url(
                &es.url,
                &[&index, "_doc", &id],
                &[("op_type", "create"), ("refresh", "wait_for")],
            )?;
            let req = es_request(client, es, reqwest::Method::PUT, &url)
                .await?
                .json(&doc);
            let (status, resp) = send_es(req, es.max_response_size).await?;
            if status == reqwest::StatusCode::CONFLICT {
                // The document exists — `action: "nothing"` semantics.
                return Ok(json!({ "matched": 1, "modified": 0 }));
            }
            if !status.is_success() {
                return Err(es_write_error(status, &resp));
            }
            Ok(json!({ "matched": 0, "modified": 0, "upserted_id": id }))
        }
    }
}

/// POST an `_update_by_query` / `_delete_by_query` body (with `refresh=true`) and
/// surface failures/version conflicts as errors — the endpoints run with the
/// default `conflicts=abort`, matching parity-or-error.
async fn run_by_query(
    client: &reqwest::Client,
    es: &EsConnectorConfig,
    index: &str,
    endpoint: &str,
    body: &Value,
) -> Result<Value, DataflowError> {
    let url = es_url(&es.url, &[index, endpoint], &[("refresh", "true")])?;
    let req = es_request(client, es, reqwest::Method::POST, &url)
        .await?
        .json(body);
    let (status, resp) = send_es(req, es.max_response_size).await?;
    if !status.is_success() {
        return Err(es_write_error(status, &resp));
    }
    let failed = resp
        .get("failures")
        .and_then(|f| f.as_array())
        .is_some_and(|f| !f.is_empty())
        || resp
            .get("version_conflicts")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            > 0;
    if failed {
        return Err(DataflowError::function_execution(
            format!("Elasticsearch {endpoint} had failures: {resp}"),
            None,
        ));
    }
    Ok(resp)
}

// -- Input schema (F53) --
//
// The table describing this handler's `function.input` lives next to the
// handler it describes. It used to sit in `schema.rs` with the other nine,
// which is how every schema/handler divergence in the 1.0 audit happened:
// a field was added, renamed or made conditional here and the table saying
// so was in a different file.

pub(super) const DATA_WRITE_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the db (SQL/MongoDB) or es (Elasticsearch) connector to write to.",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
    },
    FieldSchema {
        name: "write",
        description: "Backend-neutral mutation envelope: op/target/values/set/filter/on_conflict/returning/all.",
        kind: FieldKind::Object,
        // Not `required` in the schema: the pre-1.0 flat form (envelope keys at
        // the top level) is still accepted, and `validate_input`'s cross-field
        // rule reports whichever of the two shapes is actually missing.
        required: false,
        resolvable: false,
    },
    FieldSchema {
        name: "database",
        description: "MongoDB database name. Optional here because the same task shape is \
                      valid against SQL and Elasticsearch, which need no database key; \
                      required — and checked at workflow activation — once the referenced \
                      connector is a MongoDB one (F52).",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
    },
    FieldSchema {
        name: "schema",
        description: "Optional inline entity schema (renames, allowlist, writable flag).",
        kind: FieldKind::Object,
        required: false,
        resolvable: false,
    },
    FieldSchema {
        name: "params",
        description: "Object of named values folded into {\"param\": ..} nodes in values/set/filter. \
                      A value of {\"var\": \"path\"} is read from the message context.",
        kind: FieldKind::Object,
        required: false,
        resolvable: true,
    },
    FieldSchema {
        name: "output",
        description: "Dotted path in the message where the write result is written. Defaults to \"data\".",
        kind: FieldKind::String,
        required: false,
        resolvable: false,
    },
];

pub(super) const DATA_WRITE_ENVELOPE_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "op",
        description: "Mutation kind: \"insert\", \"update\", \"delete\", or \"upsert\".",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
    },
    FieldSchema {
        name: "target",
        description: "Logical entity to write to (schema-resolved to a table/collection).",
        kind: FieldKind::String,
        required: true,
        resolvable: false,
    },
    FieldSchema {
        name: "values",
        description: "Row object, or array of row objects (bulk), for insert/upsert.",
        kind: FieldKind::Any,
        required: false,
        resolvable: false,
    },
    FieldSchema {
        name: "set",
        description: "Object of column → value assignments for update/upsert.",
        kind: FieldKind::Object,
        required: false,
        resolvable: false,
    },
    FieldSchema {
        name: "filter",
        description: "Query-dialect filter selecting rows for update/delete. An \
                      update/delete without it is rejected unless \"all\": true is set.",
        kind: FieldKind::Object,
        required: false,
        resolvable: false,
    },
    FieldSchema {
        name: "on_conflict",
        description: "Upsert conflict clause: { \"target\": [cols], \"action\": \"update\"|\"nothing\" }.",
        kind: FieldKind::Object,
        required: false,
        resolvable: false,
    },
    FieldSchema {
        name: "returning",
        description: "Column names to return from mutated rows (Postgres/SQLite only).",
        kind: FieldKind::Array,
        required: false,
        resolvable: false,
    },
    FieldSchema {
        name: "all",
        description: "Acknowledge an intentionally unfiltered update/delete (affects every row).",
        kind: FieldKind::Bool,
        required: false,
        resolvable: false,
    },
];
