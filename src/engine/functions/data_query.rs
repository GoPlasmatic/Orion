use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use futures::TryStreamExt;
use mongodb::bson::Document;
use serde_json::Value;
use sqlx::any::AnyRow;

use super::connector_helpers::{
    apply_output, es_request, extract_output_path, is_mongo, observed_handler, require_op_allowed,
    require_str_field, resolve_connector, resolve_params, timed_query, to_exec_error,
};
use super::db_read::rows_to_json;
use crate::connector::mongo_pool::MongoPoolCache;
use crate::connector::pool_cache::SqlPoolCache;
use crate::connector::{ConnectorConfig, ConnectorRegistry, EsConnectorConfig};
use crate::query::{self, SqlDialect};
use crate::storage::detect_backend;

/// Executes a portable `data_query` — one backend-neutral filter + envelope that
/// renders to native SQL, a MongoDB `find`, or an Elasticsearch search — against a
/// SQL, Mongo, or ES connector. The translation lives in `src/query/`; this
/// handler wires it to the connector/pool machinery (ES runs over the HTTP client).
pub struct DataQueryHandler {
    pub pool_cache: Arc<SqlPoolCache>,
    pub mongo_pool_cache: Arc<MongoPoolCache>,
    pub http_client: reqwest::Client,
    pub registry: Arc<ConnectorRegistry>,
    pub default_limit: u64,
    pub max_limit: u64,
}

#[async_trait]
impl AsyncFunctionHandler for DataQueryHandler {
    type Input = Value;

    async fn execute(
        &self,
        ctx: &mut TaskContext<'_>,
        input: &Value,
    ) -> dataflow_rs::Result<TaskOutcome> {
        // Resolve `params` against the message context first — the only point at
        // which the message touches the query. It produces concrete literals, not
        // SQL, which the pure translation path then folds into the filter.
        let params = resolve_params(input.get("params"), ctx);

        // F40: read the channel before the body borrows `ctx` mutably.
        let channel = super::extract_channel(ctx.message()).to_string();

        observed_handler("data_query", input, &channel, async move {
            let connector_name = require_str_field(input, "connector", "data_query")?;
            let query = input.get("query").ok_or_else(|| {
                DataflowError::Validation("data_query requires 'query' field".to_string())
            })?;

            let connector_config = resolve_connector(&self.registry, connector_name).await?;

            // Per-connector operation gates: reads can be disabled too
            // (e.g. a write-only audit sink).
            if let Some(gates) = connector_config.operation_gates() {
                require_op_allowed(gates, "read", connector_name)?;
            }

            // Optional inline schema (privileged config authored alongside the
            // query): renames, type hints, allowlist, and relation declarations.
            let registry = match input.get("schema") {
                Some(s) => query::EntityRegistry::from_json(s)?,
                None => query::EntityRegistry::default(),
            };

            let result = match connector_config.as_ref() {
                ConnectorConfig::Es(es) => {
                    // Elasticsearch: render a search body and POST it via HTTP.
                    let eq = query::translate_es(
                        query,
                        &params,
                        &registry,
                        self.default_limit,
                        self.max_limit,
                    )?;
                    run_es_search(&self.http_client, es, &eq).await?
                }
                ConnectorConfig::Db(db) if is_mongo(&db.connection_string) => {
                    // MongoDB: render a `find` and execute it via the Mongo pool.
                    let database = require_str_field(input, "database", "data_query")?;
                    let mq = query::translate_mongo(
                        query,
                        &params,
                        &registry,
                        self.default_limit,
                        self.max_limit,
                    )?;
                    let client = self
                        .mongo_pool_cache
                        .get_client(connector_name, db)
                        .await
                        .map_err(to_exec_error)?;
                    let coll = client
                        .database(database)
                        .collection::<Document>(&mq.collection);
                    // F11: DbConnectorConfig.query_timeout_ms never applied
                    // to Mongo — an unresponsive server hung the request for
                    // the channel timeout, which is itself optional.
                    let docs: Vec<Document> =
                        timed_query(db.query_timeout_ms, "data_query", async {
                            let mut find = coll.find(mq.filter);
                            if let Some(p) = mq.projection {
                                find = find.projection(p);
                            }
                            if let Some(s) = mq.sort {
                                find = find.sort(s);
                            }
                            if let Some(sk) = mq.skip {
                                find = find.skip(sk);
                            }
                            find = find.limit(mq.limit as i64);
                            let cursor = find.await.map_err(|e| e.to_string())?;
                            cursor.try_collect().await.map_err(|e| e.to_string())
                        })
                        .await?;
                    Value::Array(
                        docs.iter()
                            .filter_map(|d| serde_json::to_value(d).ok())
                            .collect(),
                    )
                }
                ConnectorConfig::Db(db) => {
                    // SQL: dialect from the connection-string scheme (the same
                    // source `AnyPool` uses), so the rendered SQL matches the pool.
                    let dialect: SqlDialect = detect_backend(&db.connection_string)
                        .map_err(to_exec_error)?
                        .into();
                    let plan = query::plan_sql(
                        query,
                        &params,
                        &registry,
                        dialect,
                        self.default_limit,
                        self.max_limit,
                    )?;
                    let pool = self
                        .pool_cache
                        .get_pool(connector_name, db)
                        .await
                        .map_err(to_exec_error)?;
                    run_sql_with_includes(&pool, &plan, dialect, db.query_timeout_ms).await?
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

/// Execute an Elasticsearch search: POST the rendered body to
/// `{url}/{index}/_search` and return the `_source` of each hit as a JSON array.
async fn run_es_search(
    client: &reqwest::Client,
    es: &EsConnectorConfig,
    eq: &query::backend::es::EsQuery,
) -> Result<Value, DataflowError> {
    let url = format!("{}/{}/_search", es.url.trim_end_matches('/'), eq.index);
    let req = es_request(client, es, reqwest::Method::POST, &url)
        .await?
        .json(&eq.body);

    let (status, body) = super::connector_helpers::send_es(req, es.max_response_size).await?;
    if !status.is_success() {
        return Err(DataflowError::function_execution(
            format!("Elasticsearch search failed ({status}): {body}"),
            None,
        ));
    }

    let docs: Vec<Value> = body
        .get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(|h| h.as_array())
        .map(|hits| {
            hits.iter()
                .map(|h| h.get("_source").cloned().unwrap_or(Value::Null))
                .collect()
        })
        .unwrap_or_default();
    Ok(Value::Array(docs))
}

/// Execute the main SQL query, then hydrate each `include` with a per-relation
/// child query (`WHERE fk IN (parent keys)`), grouping children back to their
/// parents in memory and applying the per-parent include limit. One extra query
/// per include relation; no per-dialect JSON functions needed.
async fn run_sql_with_includes(
    pool: &sqlx::AnyPool,
    plan: &query::SqlPlan,
    dialect: SqlDialect,
    timeout_ms: Option<u64>,
) -> Result<Value, DataflowError> {
    let (sql, values) = query::backend::sql::build_for(dialect, &plan.main);
    let rows: Vec<AnyRow> = timed_query(
        timeout_ms,
        "data_query",
        sqlx::query_with(&sql, values).fetch_all(pool),
    )
    .await?;
    let mut parents: Vec<Value> = match rows_to_json(&rows)? {
        Value::Array(a) => a,
        _ => Vec::new(),
    };

    for inc in &plan.includes {
        // Distinct, non-null parent keys to fetch children for.
        let mut seen = HashSet::new();
        let mut keys = Vec::new();
        for p in &parents {
            if let Some(k) = p.get(&inc.local)
                && let Some(sv) = query::backend::sql::json_key_to_sea(k)
                && seen.insert(k.to_string())
            {
                keys.push(sv);
            }
        }

        // Fetch children and group them by their foreign-key value.
        let keep_foreign = inc.fields.is_empty() || inc.fields.iter().any(|f| f == &inc.foreign);
        let mut groups: HashMap<String, Vec<Value>> = HashMap::new();
        if !keys.is_empty() {
            let (csql, cvalues) = query::backend::sql::build_include_select(
                &inc.target_table,
                &inc.foreign,
                &inc.fields,
                &keys,
                dialect,
            );
            let crows: Vec<AnyRow> = timed_query(
                timeout_ms,
                "data_query",
                sqlx::query_with(&csql, cvalues).fetch_all(pool),
            )
            .await?;
            let children = match rows_to_json(&crows)? {
                Value::Array(a) => a,
                _ => Vec::new(),
            };
            for mut child in children {
                let Some(fk) = child.get(&inc.foreign).map(|v| v.to_string()) else {
                    continue;
                };
                if !keep_foreign && let Value::Object(m) = &mut child {
                    m.remove(&inc.foreign);
                }
                groups.entry(fk).or_default().push(child);
            }
        }

        // Attach the (limited) child list to each parent under the relation name.
        for p in &mut parents {
            let key = p.get(&inc.local).map(|k| k.to_string()).unwrap_or_default();
            let mut kids = groups.get(&key).cloned().unwrap_or_default();
            if let Some(lim) = inc.limit {
                kids.truncate(lim as usize);
            }
            if let Value::Object(m) = p {
                m.insert(inc.field.clone(), Value::Array(kids));
            }
        }
    }

    // Remove parent columns that were added only to group children.
    if !plan.strip.is_empty() {
        for p in &mut parents {
            if let Value::Object(m) = p {
                for s in &plan.strip {
                    m.remove(s);
                }
            }
        }
    }

    Ok(Value::Array(parents))
}
