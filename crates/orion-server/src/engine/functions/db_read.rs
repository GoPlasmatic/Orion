use std::sync::Arc;

use async_trait::async_trait;
use dataflow_rs::engine::error::DataflowError;
use dataflow_rs::engine::task_context::TaskContext;
use serde_json::Value;

use super::connector_handler::{ConnectorHandler, Produced};
use super::connector_helpers::{
    ConnectorCall, decode_failure, reject_mongo_connector, require_op_allowed, resolve_bind_params,
    resolve_row_format, timed_query, to_connect_error,
};
use super::schema::{FieldKind, FieldSchema};
use super::templated_input::TemplatedInput;
use crate::connector::ConnectorRegistry;
use crate::connector::pool_cache::SqlPoolCache;
use crate::engine::HandlerError;

/// This handler's name, for the row-conversion helpers below — a reference to
/// the one place it is written (F48), not a second spelling of it.
const NAME: &str = <DbReadHandler as ConnectorHandler>::NAME;

/// Executes SQL SELECT queries against external databases configured via connectors.
pub struct DbReadHandler {
    pub pool_cache: Arc<SqlPoolCache>,
    pub registry: Arc<ConnectorRegistry>,
    /// Hard row cap, from `query.max_limit` (F10). Raw SQL can't have a
    /// LIMIT injected reliably, so rows are streamed and counted — one
    /// `SELECT * FROM big_table` must not OOM the process.
    pub max_rows: usize,
}

/// The statement and its bind values.
///
/// The SQL text is a literal read from the task; only the parameters come from
/// the message, which is what keeps them the sole request-controlled part of
/// the statement.
pub struct DbRead {
    query: String,
    params: Vec<Value>,
    format: crate::connector::sql_decode::RowFormat,
}

impl DbRead {
    /// The parse both raw-SQL handlers do. Shared rather than copied because
    /// `db_read` and `db_write` differ in what the database does with the
    /// statement, not in what the task says.
    /// The half `db_read` and `db_write` genuinely share: a literal statement
    /// and message-derived binds.
    ///
    /// The rendering choices (`numeric_as`, `binary_as`) are **not** read here.
    /// They govern how a decoded row renders,
    /// and `db_write` decodes no rows — it answers `rows_affected`. Reading it
    /// on both paths meant a wrong value produced `db_write: 'numeric_as' must
    /// be one of number/string`, an error naming a field `DB_WRITE_FIELDS` does
    /// not declare and the schema validator already reports as `UNKNOWN_FIELD`.
    /// One of the two had to go, and the runtime is the one that was wrong.
    pub(super) fn parse_statement(
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        ctx: &TaskContext<'_>,
    ) -> Result<Self, HandlerError> {
        Ok(Self {
            query: call.require_str(input, "query")?.to_string(),
            params: resolve_bind_params(input, call.name, ctx)?,
            format: crate::connector::sql_decode::RowFormat::default(),
        })
    }

    /// [`parse_statement`](Self::parse_statement) plus the read-only rendering
    /// choice — and the check that the statement is actually a read.
    pub(super) fn parse_read(
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        ctx: &TaskContext<'_>,
    ) -> Result<Self, HandlerError> {
        let read = Self {
            format: resolve_row_format(input, call.name, ctx)?,
            ..Self::parse_statement(call, input, ctx)?
        };
        require_read_only(&read.query, call.name)?;
        Ok(read)
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn params(&self) -> &[Value] {
        &self.params
    }

    pub(super) fn format(&self) -> crate::connector::sql_decode::RowFormat {
        self.format
    }
}

#[async_trait]
impl ConnectorHandler for DbReadHandler {
    const NAME: &'static str = "db_read";
    type Kind = crate::connector::kind::Db;
    type Input = TemplatedInput;
    type Parsed = DbRead;

    fn registry(&self) -> &Arc<ConnectorRegistry> {
        &self.registry
    }

    fn parse(
        &self,
        call: &ConnectorCall<'_>,
        input: &TemplatedInput,
        ctx: &TaskContext<'_>,
    ) -> Result<Self::Parsed, HandlerError> {
        DbRead::parse_read(call, input, ctx)
    }

    fn gate(
        _parsed: &Self::Parsed,
        conn: &crate::connector::DbConnectorConfig,
        connector: &str,
    ) -> Result<(), HandlerError> {
        require_op_allowed(&conn.operations, "read", connector)?;
        // A Mongo connection string in a `db` connector is the right type and
        // the wrong backend: both are `ConnectorConfig::Db`, and only the
        // string tells them apart.
        Ok(reject_mongo_connector(
            <Self as ConnectorHandler>::NAME,
            connector,
            conn,
        )?)
    }

    async fn run(
        &self,
        read: Self::Parsed,
        db_config: &crate::connector::DbConnectorConfig,
        call: &ConnectorCall<'_>,
        _input: &TemplatedInput,
        _ctx: &mut TaskContext<'_>,
    ) -> Result<Produced, HandlerError> {
        let pool = self
            .pool_cache
            .get_pool(call.connector, db_config)
            .await
            .map_err(to_connect_error)?;

        let max_rows = self.max_rows;
        let params = read.params();
        let format = read.format();
        let query = read.query();

        // One body, three drivers: the macro binds the concrete pool, its
        // decoder and its binder, and each arm is type-checked on its own.
        let json = crate::connector::pool_cache::dispatch_sql_pool!(
            &pool, p, rows_to_json, bind => {
                let rows = timed_query(db_config.query_timeout_ms, call.name, async {
                    use futures::TryStreamExt;
                    let sqlx_query = bind(sqlx::query(query), params);
                    let mut stream = sqlx_query.fetch(p);
                    let mut rows = Vec::new();
                    // Not `.map_err(|e| e.to_string())`: stringifying here
                    // converted through `From<String>`, which is
                    // unconditionally a backend failure, so a constraint the
                    // driver had already classified was thrown away before
                    // `QueryFailure` could see it.
                    while let Some(row) = stream.try_next().await? {
                        if rows.len() >= max_rows {
                            // F42: classified so `timed_query` reports it as a
                            // 400 with the text intact rather than a 500 with
                            // the guidance sanitised away. The guidance *is*
                            // the message, so losing it loses the point.
                            return Err(
                                crate::engine::functions::connector_helpers::QueryFailure::Limit(
                                    format!(
                                        "{} result exceeds query.max_limit ({max_rows} rows) \
                                         — add a LIMIT to the query or raise the cap",
                                        call.name
                                    ),
                                ),
                            );
                        }
                        rows.push(row);
                    }
                    Ok(rows)
                })
                .await?;
                rows_to_json(&rows, format).map_err(|e| decode_failure(NAME, e))?
            }
        );

        Ok(Value::Array(json).into())
    }
}

// -- Input schema (F53) --
//
// The table describing this handler's `function.input` lives next to the
// handler it describes. It used to sit in `schema.rs` with the other nine,
// which is how every schema/handler divergence in the 1.0 audit happened:
// a field was added, renamed or made conditional here and the table saying
// so was in a different file.

pub(super) const DB_READ_FIELDS: &[FieldSchema] = &[
    FieldSchema {
        name: "connector",
        description: "Name of the SQL connector to query.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "query",
        description: "Read statement — SELECT, WITH, VALUES or TABLE; a write belongs in db_write, which has its own 'raw_write' connector gate. Bind placeholders are the backend's own spelling: ? for SQLite and MySQL, $1, $2, ... for PostgreSQL.",
        kind: FieldKind::String,
        required: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "params",
        description: "Array of values to bind to query placeholders, in order. Accepts {\"var\": \"path\"} to read the value from the message.",
        kind: FieldKind::Array,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "numeric_as",
        description: "How an arbitrary-precision decimal column is rendered: \"number\" (default) or \"string\". A number is computable in JSONLogic and rounds beyond 2^53 or on most decimal fractions; a string keeps every digit, which is what a money column needs.",
        kind: FieldKind::String,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "binary_as",
        description: "How a binary column is rendered: \"auto\" (default), \"hex\", \"base64\" or \"text\". Auto reads the bytes as text when they are valid UTF-8 and as hex when they are not, so its result shape depends on the data; name an encoding for a column that is genuinely binary.",
        kind: FieldKind::String,
        resolvable: true,
        ..FieldSchema::DEFAULT
    },
    FieldSchema {
        name: "output",
        description: "Dotted path in the message where rows are written. Defaults to \"data\".",
        kind: FieldKind::String,
        template_at: &[""],
        ..FieldSchema::DEFAULT
    },
];

// -- Read-only statement check --
//
// `db_read` gates on the connector's `read` operation and then runs whatever
// statement the task carries. `fetch` executes any statement — it merely
// streams whatever rows come back — so `DELETE FROM t RETURNING id` ran here on
// PostgreSQL and SQLite, and a bare `DELETE`/`UPDATE`/`INSERT` ran (returning no
// rows) on all three, while `raw_write: false` was set on the connector.
//
// That made the operation gates advertise more than they enforced. The gate
// table's own claim — "SQL writes are fully bounded by allowed_entities once
// `raw_write: false` leaves `data_write` as the only write path" — is only true
// with this check in place, because `db_read` was the second write path.
//
// The statement is a workflow-authored literal, never caller-supplied, so this
// is not an injection guard; it is what makes "delete-proof connector" a
// property an operator can rely on rather than a convention authors are asked
// to keep.

/// The statement kinds that return rows without modifying them.
///
/// Deliberately short. `EXPLAIN` is **not** here: `EXPLAIN ANALYZE DELETE …`
/// executes the delete on PostgreSQL. Neither is `PRAGMA`, which writes on
/// SQLite (`PRAGMA journal_mode = WAL`). A statement that needs to write
/// belongs in `db_write`, which has its own `raw_write` gate.
const READ_STATEMENTS: [&str; 4] = ["SELECT", "WITH", "VALUES", "TABLE"];

/// The keywords that make a CTE data-modifying.
const MODIFYING_STATEMENTS: [&str; 4] = ["INSERT", "UPDATE", "DELETE", "MERGE"];

/// The only two token shapes this check needs: a bare word, and an opening
/// parenthesis (which is what separates a data-modifying CTE from a column
/// alias — `AS (INSERT …` versus `AS total`).
#[derive(Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Open,
}

/// Split a statement into significant tokens, with comments and every quoted
/// form removed.
///
/// Stripping quoted text first is what keeps the check from reading data as
/// syntax: `WHERE note = 'delete me'` contains the word `delete` and is a
/// perfectly ordinary read. Handled: `--` line comments, `/* */` block comments
/// (nested, as PostgreSQL allows), `'…'` strings with `''` escapes, `"…"` and
/// `` `…` `` quoted identifiers, and PostgreSQL `$tag$…$tag$` dollar quoting.
/// A `$1` placeholder is not a dollar quote — a tag may not start with a digit —
/// so bind parameters survive untouched.
fn scan(sql: &str) -> Vec<Token> {
    let c: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '-' if c.get(i + 1) == Some(&'-') => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            }
            '/' if c.get(i + 1) == Some(&'*') => {
                let mut depth = 1usize;
                i += 2;
                while i < c.len() && depth > 0 {
                    if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                        depth += 1;
                        i += 2;
                    } else if c[i] == '*' && c.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            '\'' => i = skip_quoted(&c, i, '\'', true),
            '"' => i = skip_quoted(&c, i, '"', true),
            '`' => i = skip_quoted(&c, i, '`', false),
            '$' => match dollar_tag(&c, i) {
                Some(tag) => i = skip_dollar_quoted(&c, i, &tag),
                None => i += 1,
            },
            '(' => {
                out.push(Token::Open);
                i += 1;
            }
            ch if ch.is_alphanumeric() || ch == '_' => {
                let start = i;
                while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_') {
                    i += 1;
                }
                out.push(Token::Word(
                    c[start..i].iter().collect::<String>().to_uppercase(),
                ));
            }
            _ => i += 1,
        }
    }
    out
}

/// Advance past a `quote`-delimited run starting at `i`. When `doubled` is set,
/// two quote characters in a row are an escaped quote rather than the end.
fn skip_quoted(c: &[char], mut i: usize, quote: char, doubled: bool) -> usize {
    i += 1;
    while i < c.len() {
        if c[i] == quote {
            if doubled && c.get(i + 1) == Some(&quote) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    i
}

/// The tag of a PostgreSQL dollar quote opening at `i` (`""` for `$$`), or
/// `None` when this `$` starts something else — a `$1` bind placeholder, say.
fn dollar_tag(c: &[char], i: usize) -> Option<String> {
    let mut j = i + 1;
    while j < c.len() && (c[j].is_alphabetic() || c[j] == '_' || (j > i + 1 && c[j].is_numeric())) {
        j += 1;
    }
    (c.get(j) == Some(&'$')).then(|| c[i + 1..j].iter().collect())
}

/// Advance past a dollar-quoted block to just after its closing `$tag$`.
fn skip_dollar_quoted(c: &[char], i: usize, tag: &str) -> usize {
    let close: Vec<char> = format!("${tag}$").chars().collect();
    let mut j = i + close.len();
    while j + close.len() <= c.len() {
        if c[j..j + close.len()] == close[..] {
            return j + close.len();
        }
        j += 1;
    }
    c.len()
}

/// The statement's leading keyword, upper-cased, with comments and quoted text
/// ignored — `None` for a statement with no keyword at all.
///
/// Shared with `db_write`, which needs to know whether the statement is an
/// `INSERT` before it reports a `last_insert_id`.
pub(super) fn leading_keyword(sql: &str) -> Option<String> {
    scan(sql).into_iter().find_map(|t| match t {
        // Leading `(` is ordinary — `(SELECT 1) UNION (SELECT 2)`.
        Token::Word(w) => Some(w),
        Token::Open => None,
    })
}

/// Refuse a `db_read` statement that is not a read.
///
/// # Errors
///
/// [`DataflowError::Validation`] when the statement does not open with one of
/// [`READ_STATEMENTS`], or when it carries a data-modifying CTE.
fn require_read_only(query: &str, handler_name: &str) -> Result<(), HandlerError> {
    let tokens = scan(query);
    let Some(first) = leading_keyword(query) else {
        return Err(DataflowError::Validation(format!(
            "{handler_name} 'query' has no statement to run"
        ))
        .into());
    };
    if !READ_STATEMENTS.contains(&first.as_str()) {
        return Err(DataflowError::Validation(format!(
            "{handler_name} runs read statements only, but this one starts with \
             '{first}' — use db_write for INSERT/UPDATE/DELETE (it has its own \
             'raw_write' connector gate). Reads start with {}",
            READ_STATEMENTS.join(", ")
        ))
        .into());
    }
    // A data-modifying CTE — `WITH moved AS (DELETE … RETURNING …) SELECT …` —
    // opens with `WITH` and writes. It is the one way a statement that passes
    // the check above can still mutate, and it is recognisable by shape: `AS`,
    // an optional `[NOT] MATERIALIZED`, `(`, then the modifying keyword. A
    // column alias (`AS total`) and an ordinary CTE (`AS (SELECT …)`) both fail
    // to match, so neither is caught.
    for (n, token) in tokens.iter().enumerate() {
        if !matches!(token, Token::Word(w) if w == "AS") {
            continue;
        }
        let mut j = n + 1;
        while matches!(tokens.get(j), Some(Token::Word(w)) if w == "NOT" || w == "MATERIALIZED") {
            j += 1;
        }
        if tokens.get(j) != Some(&Token::Open) {
            continue;
        }
        if let Some(Token::Word(w)) = tokens.get(j + 1)
            && MODIFYING_STATEMENTS.contains(&w.as_str())
        {
            return Err(DataflowError::Validation(format!(
                "{handler_name} runs read statements only, but this one carries a \
                 data-modifying '{w}' common table expression — use db_write \
                 (it has its own 'raw_write' connector gate)"
            ))
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(sql: &str) -> Result<(), String> {
        require_read_only(sql, "db_read").map_err(|e| {
            let e: DataflowError = e.into();
            e.to_string()
        })
    }

    #[test]
    fn reads_are_admitted() {
        for sql in [
            "SELECT id FROM users WHERE id = $1",
            "  \n select 1",
            "-- a comment\nSELECT 1",
            "/* block */ SELECT 1",
            "(SELECT 1) UNION (SELECT 2)",
            "WITH recent AS (SELECT * FROM orders) SELECT * FROM recent",
            "VALUES (1), (2)",
            "TABLE users",
            // A locking read is a read: `FOR UPDATE` must not be mistaken for
            // an UPDATE statement.
            "SELECT id FROM jobs ORDER BY id FOR UPDATE SKIP LOCKED",
            // The word only appears inside data.
            "SELECT id FROM notes WHERE body = 'delete from users'",
            "SELECT \"delete\" FROM t",
            "SELECT total AS deleted FROM t",
            "SELECT CAST(a AS text) FROM t",
        ] {
            assert!(
                check(sql).is_ok(),
                "must be admitted: {sql} — {:?}",
                check(sql)
            );
        }
    }

    #[test]
    fn writes_are_refused() {
        for sql in [
            "DELETE FROM audit_log WHERE id > 0 RETURNING id",
            "delete from audit_log",
            "INSERT INTO t (a) VALUES (1)",
            "UPDATE t SET a = 1",
            "TRUNCATE t",
            "DROP TABLE t",
            "PRAGMA journal_mode = WAL",
            // `EXPLAIN ANALYZE` executes the statement it explains.
            "EXPLAIN ANALYZE DELETE FROM t",
            "  -- lead in\n  DELETE FROM t",
        ] {
            let err = check(sql).expect_err(&format!("must be refused: {sql}"));
            assert!(err.contains("read statements only"), "{sql}: {err}");
        }
    }

    #[test]
    fn a_data_modifying_cte_is_refused() {
        for sql in [
            "WITH gone AS (DELETE FROM t RETURNING id) SELECT * FROM gone",
            "WITH added AS (INSERT INTO t (a) VALUES (1) RETURNING id) SELECT * FROM added",
            "with m as materialized (update t set a = 1 returning id) select * from m",
        ] {
            let err = check(sql).expect_err(&format!("must be refused: {sql}"));
            assert!(err.contains("data-modifying"), "{sql}: {err}");
        }
    }

    /// A statement whose text merely *mentions* a modifying keyword inside a
    /// literal, a comment or an identifier stays a read — the check reads
    /// syntax, not data.
    #[test]
    fn quoted_text_is_not_syntax() {
        assert!(check("SELECT 1 /* AS (DELETE */").is_ok());
        assert!(check("SELECT 'x AS (DELETE FROM t)' AS s").is_ok());
        assert!(check("SELECT $tag$ AS (DELETE FROM t) $tag$ AS s").is_ok());
        assert!(check("SELECT * FROM t WHERE a = $1 AND b = $2").is_ok());
    }

    #[test]
    fn an_empty_statement_is_refused() {
        let err = check("   -- nothing here\n").expect_err("empty");
        assert!(err.contains("no statement"), "{err}");
    }
}
