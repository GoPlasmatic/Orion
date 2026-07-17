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
    apply_output, es_request, extract_output_path, profile_handler, require_op_allowed,
    require_str_field, resolve_connector, resolve_params, timed_query, to_exec_error,
};
use super::db_read::rows_to_json;
use crate::connector::mongo_pool::MongoPoolCache;
use crate::connector::pool_cache::SqlPoolCache;
use crate::connector::{ConnectorConfig, ConnectorRegistry, EsConnectorConfig};
use crate::query::backend::es::{EsWrite, bulk_ndjson};
use crate::query::backend::mongo::MongoWrite;
use crate::query::write::{ResolvedWrite, WriteError, WriteOp};
use crate::query::{self, SqlDialect};
use crate::storage::detect_backend;

/// Executes a portable `data_write` — one backend-neutral mutation envelope
/// (`op`/`target`/`values`/`set`/`filter`/`on_conflict`/`returning`) that renders
/// to a native SQL `INSERT`/`UPDATE`/`DELETE`/upsert, a MongoDB write, or an
/// Elasticsearch write — against a SQL, Mongo, or ES connector. The translation
/// lives in `src/query/write.rs` and the per-backend renderers; this handler wires
/// it to the connector/pool machinery (ES runs over the HTTP client) and enforces
/// the write-safety guards (bulk cap, unfiltered mutation).
pub struct DataWriteHandler {
    pub pool_cache: Arc<SqlPoolCache>,
    pub mongo_pool_cache: Arc<MongoPoolCache>,
    pub http_client: reqwest::Client,
    pub registry: Arc<ConnectorRegistry>,
    pub max_rows: u64,
    pub allow_unfiltered: bool,
}

#[async_trait]
impl AsyncFunctionHandler for DataWriteHandler {
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // Resolve `params` against the message context first (the only point the
        // message touches the mutation); it produces literals, not SQL.
        let params = resolve_params(input.get("params"), ctx);

        profile_handler("data_write", input, async move {
            let connector_name = require_str_field(input, "connector", "data_write")?;
            let connector_config = resolve_connector(&self.registry, connector_name).await?;

            // Optional inline schema (privileged config): renames, allowlist, and
            // the per-column `writable` flag.
            let registry = match input.get("schema") {
                Some(s) => query::EntityRegistry::from_json(s)?,
                None => query::EntityRegistry::default(),
            };

            // One backend-neutral resolution: parse envelope, fold params into
            // values/set, resolve physical names, coerce, and lower the filter.
            let resolved = query::write::resolve_write(input, &params, &registry)?;
            self.check_guards(&resolved)?;

            // Per-connector operation gates: a connector config can disable
            // individual ops (e.g. delete) regardless of what workflows ask for.
            if let Some(gates) = connector_config.operation_gates() {
                require_op_allowed(gates, resolved.op.as_str(), connector_name)?;
            }

            let result = match connector_config.as_ref() {
                ConnectorConfig::Db(db) if is_mongo(&db.connection_string) => {
                    let database = require_str_field(input, "database", "data_write")?;
                    let mw = query::backend::mongo::render_write(&resolved)?;
                    let client = self
                        .mongo_pool_cache
                        .get_client(connector_name, db)
                        .await
                        .map_err(to_exec_error)?;
                    execute_mongo(&client, database, mw).await?
                }
                ConnectorConfig::Db(db) => {
                    let dialect: SqlDialect = detect_backend(&db.connection_string)
                        .map_err(to_exec_error)?
                        .into();
                    let (sql, values) = query::backend::sql::render_write(&resolved, dialect)?;
                    let pool = self
                        .pool_cache
                        .get_pool(connector_name, db)
                        .await
                        .map_err(to_exec_error)?;
                    execute_sql(&pool, &sql, values, &resolved, db.query_timeout_ms).await?
                }
                ConnectorConfig::Es(es) => {
                    let ew = query::backend::es::render_write(&resolved)?;
                    run_es_write(&self.http_client, es, ew).await?
                }
                _ => {
                    return Err(DataflowError::Validation(format!(
                        "Connector '{connector_name}' is not a db or es connector"
                    )));
                }
            };

            apply_output(ctx, extract_output_path(input), result);
            Ok(TaskOutcome::Success)
        })
        .await
    }
}

impl DataWriteHandler {
    /// Enforce the write-safety guards: the bulk-insert cap and the
    /// unfiltered-mutation guard (an `update`/`delete` with no filter is rejected
    /// unless both `"all": true` is set and `write.allow_unfiltered` is enabled).
    fn check_guards(&self, w: &ResolvedWrite) -> Result<(), DataflowError> {
        if matches!(w.op, WriteOp::Insert | WriteOp::Upsert) && w.rows.len() as u64 > self.max_rows
        {
            return Err(WriteError::TooManyRows {
                requested: w.rows.len(),
                max: self.max_rows,
            }
            .into());
        }
        if matches!(w.op, WriteOp::Update | WriteOp::Delete) && !w.filter_present {
            if !w.all {
                return Err(WriteError::UnfilteredMutation {
                    op: w.op.as_str().to_string(),
                }
                .into());
            }
            if !self.allow_unfiltered {
                return Err(WriteError::UnfilteredNotAllowed {
                    op: w.op.as_str().to_string(),
                }
                .into());
            }
        }
        Ok(())
    }
}

/// A connector targets MongoDB when its connection string uses a `mongodb` scheme.
fn is_mongo(conn: &str) -> bool {
    conn.starts_with("mongodb://") || conn.starts_with("mongodb+srv://")
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
            "data_write",
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
            "data_write",
            sqlx::query_with(sql, values).fetch_all(pool),
        )
        .await?;
        let returning = rows_to_json(&rows);
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

/// Send an ES request and parse its JSON body. Returns the status alongside so
/// callers can treat specific non-2xx statuses as semantic (`op_type=create`
/// treats 409 as the "conflict → do nothing" no-op).
async fn send_es(
    req: reqwest::RequestBuilder,
) -> Result<(reqwest::StatusCode, Value), DataflowError> {
    let resp = req.send().await.map_err(to_exec_error)?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(to_exec_error)?;
    Ok((status, body))
}

fn es_write_error(status: reqwest::StatusCode, body: &Value) -> DataflowError {
    DataflowError::function_execution(
        format!("Elasticsearch write failed ({status}): {body}"),
        None,
    )
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
                .header("Content-Type", "application/x-ndjson")
                .body(bulk_ndjson(&docs));
            let (status, body) = send_es(req).await?;
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
            let req = es_request(client, es, reqwest::Method::POST, &url).json(&body);
            let (status, resp) = send_es(req).await?;
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
            let req = es_request(client, es, reqwest::Method::PUT, &url).json(&doc);
            let (status, resp) = send_es(req).await?;
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
    let req = es_request(client, es, reqwest::Method::POST, &url).json(body);
    let (status, resp) = send_es(req).await?;
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
