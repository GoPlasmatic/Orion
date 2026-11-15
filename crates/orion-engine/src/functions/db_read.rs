use super::definition::{Definition, FieldOverlay, OrionFunction};
use std::borrow::Cow;
use std::sync::Arc;

use dataflow_rs::expr::{Interpolated, Output};
use dataflow_rs::function::{FunctionConfig, FunctionSpec, Prepare};
use dataflow_rs::{Ctx, Function, Issue, Outcome};
use serde_json::Value;

use super::connector_helpers::{
    QueryBudget, QueryFailure, decode_failure, require_op_allowed, to_connect_error,
};
use super::schema::{FieldKind, RetrySafety};
use crate::connector::pool_cache::SqlPoolCache;
use crate::connector::sql_decode::RowFormat;
use crate::connector::{ConnectorRegistry, DbConnectorConfig};
use crate::kit::{self, read};
use crate::{ErrorClass, HandlerError};

/// This handler's name, for the row-conversion helpers below — a reference to
/// the one place it is written, not a second spelling of it.
pub(crate) const NAME: &str = "db_read";

/// The statement and its bind values.
///
/// The SQL text is a literal read from the task; only the parameters come from
/// the message, which is what keeps them the sole request-controlled part of
/// the statement. The query is borrowed from the compiled config.
pub struct DbRead<'q> {
    query: Cow<'q, str>,
    params: Vec<Value>,
    format: RowFormat,
}

impl<'q> DbRead<'q> {
    /// The statement a compiled config states, its binds rendered for this
    /// call, the query borrowed.
    pub(super) fn statement(
        ctx: &Ctx<'_>,
        query: &'q str,
        params: Option<&Interpolated>,
        handler: &str,
    ) -> Result<Self, HandlerError> {
        Ok(Self {
            query: Cow::Borrowed(query),
            params: rendered_binds(ctx, params, handler)?,
            format: RowFormat::default(),
        })
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn params(&self) -> &[Value] {
        &self.params
    }

    pub(super) fn format(&self) -> RowFormat {
        self.format
    }
}

/// A `params` document rendered for this call, as `resolve_bind_params` reads
/// it: unset or `null` binds nothing, an array binds its elements, anything
/// else is refused. The rendered document serializes straight into the JSON
/// the reader takes, with no tree of its own.
pub(super) fn rendered_binds(
    ctx: &Ctx<'_>,
    params: Option<&Interpolated>,
    handler: &str,
) -> Result<Vec<Value>, HandlerError> {
    let rendered = params.map(|doc| kit::rendered(ctx, doc)).transpose()?;
    read::bind_params(rendered, handler)
}

/// `numeric_as` and `binary_as` rendered for this call, `numeric_as` read
/// first, as `resolve_row_format` reads them.
pub(super) fn rendered_row_format(
    ctx: &Ctx<'_>,
    numeric_as: Option<&Interpolated>,
    binary_as: Option<&Interpolated>,
    handler: &str,
) -> Result<RowFormat, HandlerError> {
    let numeric = numeric_as.map(|doc| ctx.render(doc).to_owned());
    let binary = binary_as.map(|doc| ctx.render(doc).to_owned());
    read::row_format(numeric.as_ref(), binary.as_ref(), handler)
}

/// The connector's gate for a read: its `read` operation.
fn gate(conn: &DbConnectorConfig, connector: &str) -> Result<(), HandlerError> {
    require_op_allowed(&conn.operations, "read", connector)
}

/// The rows a read returns, as a JSON array, against a resolved and gated
/// connector. `run_deadline` is the deadline of the run the call belongs
/// to ([`QueryBudget::within`]).
#[expect(
    clippy::too_many_arguments,
    reason = "the generation joins the pool key"
)]
async fn read_rows(
    pool_cache: &SqlPoolCache,
    connector: &str,
    db_config: &DbConnectorConfig,
    max_rows: usize,
    max_result_bytes: usize,
    read: &DbRead<'_>,
    run_deadline: Option<dataflow_rs::run::Instant>,
    generation: u64,
) -> Result<Value, HandlerError> {
    let pool = pool_cache
        .get_pool_for_generation(connector, db_config, generation)
        .await
        .map_err(to_connect_error)?;

    let params = read.params();
    let format = read.format();
    let query = read.query();

    // One budget over both legs. `acquire` is a round trip of its own, and
    // giving it a timeout of its own would silently make the bound the
    // connector's owner set into `connect_timeout_ms` plus
    // `query_timeout_ms`.
    let budget = QueryBudget::within(db_config.query_timeout_ms, run_deadline);

    // Bound once, outside the dispatch, because the vocabulary is the same
    // whichever driver runs it.
    let scalars: Vec<crate::connector::sql_encode::Scalar> =
        params.iter().map(Into::into).collect();

    // One body, three drivers: the macro binds the concrete pool, its
    // decoder and its binders, and each arm is type-checked on its own.
    let json = crate::connector::pool_cache::dispatch_sql_pool!(
        &pool, p, rows_to_json, bind, typed_args, _write_result => {
            // One connection, prepared for what the server declared, or
            // bound by value shape when it declared nothing usable (M-04).
            let (mut conn, statement) = crate::functions::connector_helpers::bound_statement!(
                budget,
                NAME,
                p,
                query,
                Some(&scalars),
                typed_args,
                |cache| bind(sqlx::query(sqlx::AssertSqlSafe(query)), params).persistent(cache)
            );
            budget.run(NAME, async {
                use futures::TryStreamExt;
                let mut stream = statement.fetch(&mut *conn);
                let mut json_rows = Vec::new();
                let mut total_bytes = 0usize;
                // Not `.map_err(|e| e.to_string())`: stringifying here
                // converted through `From<String>`, which is
                // unconditionally a backend failure, so a constraint the
                // driver had already classified was thrown away before
                // `QueryFailure` could see it.
                while let Some(row) = stream.try_next().await? {
                    if json_rows.len() >= max_rows {
                        // Classified so it is reported as a 400 with the text
                        // intact rather than a 500 with the guidance sanitised
                        // away. The guidance *is* the message, so losing it
                        // loses the point.
                        return Err(
                            crate::functions::connector_helpers::QueryFailure::Limit(
                                format!(
                                    "{NAME} result exceeds query.max_limit ({max_rows} rows) \
                                     — add a LIMIT to the query or raise the cap"
                                ),
                            ),
                        );
                    }
                    let mut decoded = rows_to_json(std::slice::from_ref(&row), format)
                        .map_err(|e| QueryFailure::Classified(decode_failure(NAME, e)))?;
                    if let Some(decoded_row) = decoded.pop() {
                        if max_result_bytes > 0 {
                            total_bytes = total_bytes.saturating_add(
                                crate::connector::sql_decode::approx_json_value_bytes(&decoded_row),
                            );
                            if total_bytes > max_result_bytes {
                                return Err(
                                    crate::functions::connector_helpers::QueryFailure::Limit(
                                        format!(
                                            "{NAME} result exceeds query.max_result_bytes ({max_result_bytes} bytes) \
                                             — select fewer columns, add a LIMIT, or raise the cap"
                                        ),
                                    ),
                                );
                            }
                        }
                        json_rows.push(decoded_row);
                    }
                }
                Ok(json_rows)
            })
            .await?
        }
    );

    Ok(Value::Array(json))
}

/// The Defect for a literal root: the rows are an array, which no context
/// root takes, so an `output` that is one fails every call.
const ROOT_OUTPUT: &str = "`db_read` writes an array of rows, and a context root takes \
    only an object (break 21): set `output` to a path below it, such as `data.rows`";

/// `db_read`'s input, in its field table's order. The connector and the
/// statement are literals, required at load; the binds and the two rendering
/// choices are documents of `var` holes, folded at execution, read by
/// `kit::read`'s readers.
#[derive(Debug, FunctionConfig)]
pub struct DbReadConfig {
    /// Name of the SQL connector to query.
    connector: String,
    /// Read statement — SELECT, WITH, VALUES or TABLE; a write belongs in db_write, which has
    /// its own 'raw_write' connector gate. Bind placeholders are the backend's own spelling: ?
    /// for SQLite and MySQL, $1, $2, ... for PostgreSQL.
    query: String,
    /// Array of values to bind to query placeholders, in order. Accepts {"var": "path"} to read
    /// the value from the message.
    params: Option<Interpolated>,
    /// How an arbitrary-precision decimal column is rendered: "number" (default) or "string". A
    /// number is computable in JSONLogic and rounds beyond 2^53 or on most decimal fractions; a
    /// string keeps every digit, which is what a money column needs.
    numeric_as: Option<Interpolated>,
    /// How a binary column is rendered: "auto" (default), "hex", "base64" or "text". Auto reads
    /// the bytes as text when they are valid UTF-8 and as hex when they are not, so its result
    /// shape depends on the data; name an encoding for a column that is genuinely binary.
    binary_as: Option<Interpolated>,
    /// Dotted path in the message where rows are written. Defaults to "data".
    #[output(default = "data")]
    output: Output,
}

/// `db_read` as a function: the statement it runs, read in order.
pub struct DbReadFunction {
    pub pool_cache: Arc<SqlPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
    /// Hard row cap, from `query.max_limit`.
    pub max_rows: usize,
    /// Hard decoded byte cap across the result set, from
    /// `query.max_result_bytes` (`0` = unlimited).
    pub max_result_bytes: usize,
}

impl OrionFunction for DbReadFunction {
    type Connector = crate::connector::kind::Db;

    const DEFINITION: Definition = Definition {
        name: NAME,
        description: "Execute a SELECT against a SQL connector.",
        category: "connector",
        retry_safety: RetrySafety::Read,
        fields: &[
            FieldOverlay {
                name: "params",
                kind: Some(FieldKind::Array),
                resolvable: true,
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "numeric_as",
                kind: Some(FieldKind::String),
                resolvable: true,
                ..FieldOverlay::DEFAULT
            },
            FieldOverlay {
                name: "binary_as",
                kind: Some(FieldKind::String),
                resolvable: true,
                ..FieldOverlay::DEFAULT
            },
        ],
        deny_unknown: false,
        stub_target: Some("connector"),
        ..Definition::DEFAULT
    };

    fn from_deps(deps: &crate::HandlerDeps<'_>) -> Self {
        Self {
            pool_cache: deps.sql_pool_cache.clone(),
            registry: deps.registry.clone(),
            max_rows: deps.query_config.max_limit as usize,
            max_result_bytes: deps.query_config.max_result_bytes,
        }
    }
}

impl Function for DbReadFunction {
    type Config = DbReadConfig;

    fn spec(&self) -> FunctionSpec {
        super::definition::spec::<Self>()
    }

    /// An `output` that is a literal context root, the default `data`
    /// included, is a Defect at `output`.
    fn prepare(&self, cfg: &mut DbReadConfig, _p: &Prepare<'_>) -> Result<(), Vec<Issue>> {
        kit::root_output_defect(&cfg.output, ROOT_OUTPUT)
    }

    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &DbReadConfig) -> dataflow_rs::Result<Outcome> {
        let channel = kit::channel(ctx);
        // The prologue's output, then `parse_read`'s order: the rendering
        // choices, the binds, the read-only check.
        let output = Self::DEFINITION.output_path(ctx, &cfg.output)?;
        let format =
            rendered_row_format(ctx, cfg.numeric_as.as_ref(), cfg.binary_as.as_ref(), NAME)?;
        let read = DbRead {
            format,
            ..DbRead::statement(ctx, &cfg.query, cfg.params.as_ref(), NAME)?
        };
        require_read_only(&read.query, NAME)?;
        let run_deadline = ctx.deadline();
        let rows = kit::with_connector::<<Self as OrionFunction>::Connector, _, _, _>(
            NAME,
            &self.registry,
            &cfg.connector,
            channel,
            |conn| async move {
                gate(conn.config(), &cfg.connector)?;
                Ok(read_rows(
                    &self.pool_cache,
                    &cfg.connector,
                    conn.config(),
                    self.max_rows,
                    self.max_result_bytes,
                    &read,
                    run_deadline,
                    self.registry.config_generation(),
                )
                .await?)
            },
        )
        .await?;
        ctx.set(&output, dataflow_rs::datavalue::OwnedDataValue::from(rows))?;
        Ok(Outcome::Ok)
    }
}

fn require_read_only(query: &str, handler_name: &str) -> Result<(), HandlerError> {
    use orion_definitions::sql_lex::{READ_STATEMENTS, ReadOnlyViolation};
    let message = match orion_definitions::sql_lex::read_only_violation(query) {
        None => return Ok(()),
        Some(ReadOnlyViolation::Empty) => format!("{handler_name} 'query' has no statement to run"),
        Some(ReadOnlyViolation::NotARead { keyword }) => format!(
            "{handler_name} runs read statements only, but this one starts with \
             '{keyword}' — use db_write for INSERT/UPDATE/DELETE (it has its own \
             'raw_write' connector gate). Reads start with {}",
            READ_STATEMENTS.join(", ")
        ),
        Some(ReadOnlyViolation::ModifyingCte { keyword }) => format!(
            "{handler_name} runs read statements only, but this one carries a \
             data-modifying '{keyword}' common table expression — use db_write \
             (it has its own 'raw_write' connector gate)"
        ),
    };
    Err(HandlerError::new(ErrorClass::CallerInput, message))
}

#[cfg(test)]
#[path = "db_read_tests.rs"]
mod tests;
