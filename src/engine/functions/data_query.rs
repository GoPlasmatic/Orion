use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use futures::TryStreamExt;
use mongodb::bson::Document;
use serde_json::{Map, Value};
use sqlx::any::AnyRow;

use super::connector_helpers::{
    apply_output, extract_output_path, profile_handler, require_db_connector, require_str_field,
    resolve_connector, timed_query, to_exec_error,
};
use super::db_read::rows_to_json;
use crate::connector::ConnectorRegistry;
use crate::connector::mongo_pool::MongoPoolCache;
use crate::connector::pool_cache::SqlPoolCache;
use crate::query::{self, SqlDialect};
use crate::storage::detect_backend;

/// Executes a portable `data_query` — one backend-neutral filter + envelope that
/// renders to native SQL or a MongoDB `find` — against a SQL or Mongo connector
/// (chosen by the connection-string scheme). The translation lives in
/// `src/query/`; this handler wires it to the existing connector/pool machinery.
pub struct DataQueryHandler {
    pub pool_cache: Arc<SqlPoolCache>,
    pub mongo_pool_cache: Arc<MongoPoolCache>,
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

        profile_handler("data_query", input, async move {
            let connector_name = require_str_field(input, "connector", "data_query")?;
            let query = input.get("query").ok_or_else(|| {
                DataflowError::Validation("data_query requires 'query' field".to_string())
            })?;

            let connector_config = resolve_connector(&self.registry, connector_name).await?;
            let db_config = require_db_connector(&connector_config, connector_name)?;

            // Optional inline schema (privileged config authored alongside the
            // query): renames, type hints, allowlist, and relation declarations.
            let registry = match input.get("schema") {
                Some(s) => query::EntityRegistry::from_json(s)?,
                None => query::EntityRegistry::default(),
            };

            let result = if is_mongo(&db_config.connection_string) {
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
                    .get_client(connector_name, db_config)
                    .await
                    .map_err(to_exec_error)?;
                let coll = client
                    .database(database)
                    .collection::<Document>(&mq.collection);
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
                let cursor = find.await.map_err(to_exec_error)?;
                let docs: Vec<Document> = cursor.try_collect().await.map_err(to_exec_error)?;
                Value::Array(
                    docs.iter()
                        .filter_map(|d| serde_json::to_value(d).ok())
                        .collect(),
                )
            } else {
                // SQL: dialect from the connection-string scheme (the same source
                // `AnyPool` uses), so the rendered SQL matches the pool.
                let dialect: SqlDialect = detect_backend(&db_config.connection_string)
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
                    .get_pool(connector_name, db_config)
                    .await
                    .map_err(to_exec_error)?;
                run_sql_with_includes(&pool, &plan, dialect, db_config.query_timeout_ms).await?
            };

            apply_output(ctx, extract_output_path(input), result);
            Ok(TaskOutcome::Success)
        })
        .await
    }
}

/// A connector targets MongoDB when its connection string uses a `mongodb`
/// scheme; otherwise it is a SQL connector (dialect from the URL scheme).
fn is_mongo(conn: &str) -> bool {
    conn.starts_with("mongodb://") || conn.starts_with("mongodb+srv://")
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
    let mut parents: Vec<Value> = match rows_to_json(&rows) {
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
            let children = match rows_to_json(&crows) {
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

/// Resolve the `params` object into concrete values. A value shaped like
/// `{"var": "path"}` is looked up in the message context (dot-path over
/// `{data, metadata, temp_data}`); anything else is used as a literal. A lookup
/// that does not resolve yields null.
fn resolve_params(params: Option<&Value>, ctx: &TaskContext<'_>) -> Map<String, Value> {
    let mut out = Map::new();
    let Some(Value::Object(map)) = params else {
        return out;
    };
    for (name, spec) in map {
        let resolved = match spec {
            Value::Object(o) if o.len() == 1 && o.contains_key("var") => {
                match o.get("var").and_then(|v| v.as_str()) {
                    Some(path) => ctx.get(path).map(Value::from).unwrap_or(Value::Null),
                    None => Value::Null,
                }
            }
            other => other.clone(),
        };
        out.insert(name.clone(), resolved);
    }
    out
}
