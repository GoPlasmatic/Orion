use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::functions::AsyncFunctionHandler;
use dataflow_rs::engine::task_context::TaskContext;
use dataflow_rs::engine::task_outcome::TaskOutcome;
use serde_json::{Map, Value};
use sqlx::any::AnyRow;

use super::connector_helpers::{
    apply_output, extract_output_path, profile_handler, require_db_connector, require_str_field,
    resolve_connector, timed_query, to_exec_error,
};
use super::db_read::rows_to_json;
use crate::connector::ConnectorRegistry;
use crate::connector::pool_cache::SqlPoolCache;
use crate::query::{self, SqlDialect};
use crate::storage::detect_backend;

/// Executes a portable `data_query` — one backend-neutral filter + envelope that
/// renders to native SQL — against a SQL connector. Phase 1 is scalar SQL in
/// identity mode; the translation lives in `src/query/`, and this handler only
/// wires it to the existing connector/pool machinery (mirroring `db_read`).
pub struct DataQueryHandler {
    pub pool_cache: Arc<SqlPoolCache>,
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

            // Dialect from the connector's connection-string scheme — the same
            // source `AnyPool` uses — so the rendered SQL matches the pool.
            let dialect: SqlDialect = detect_backend(&db_config.connection_string)
                .map_err(to_exec_error)?
                .into();

            let stmt =
                query::translate_sql(query, &params, dialect, self.default_limit, self.max_limit)?;
            let (sql, values) = query::backend::sql::build_for(dialect, &stmt);

            let pool = self
                .pool_cache
                .get_pool(connector_name, db_config)
                .await
                .map_err(to_exec_error)?;

            let rows: Vec<AnyRow> = timed_query(
                db_config.query_timeout_ms,
                "data_query",
                sqlx::query_with(&sql, values).fetch_all(&pool),
            )
            .await?;

            let result = rows_to_json(&rows);
            apply_output(ctx, extract_output_path(input), result);
            Ok(TaskOutcome::Success)
        })
        .await
    }
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
