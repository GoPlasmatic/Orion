//! `orion-server sql check` — every SQL statement a definition set ships,
//! prepared against a real database as the connector's own role (#335).
//!
//! `lint` sees a statement's shape and never the schema, so a migration that
//! drops a column, or a role without the grant a statement needs, passes
//! every gate and fails on the next request. This prepares each `db_read`
//! and `db_write` statement where it will run: PostgreSQL resolves every
//! relation and column and, from 16, checks the role's grants through
//! `EXPLAIN (GENERIC_PLAN)`; MySQL and SQLite prepare, which proves the
//! schema and not the grants, and the report says so.
//!
//! Nothing is executed. PostgreSQL sessions are `READ ONLY` and rolled back;
//! a SQLite file is opened read-only; `--schema` builds a scratch schema in a
//! transaction that is always rolled back. Connection strings are never
//! printed.

use std::collections::BTreeMap;
use std::time::Duration;

use orion::definitions::clippy::Diagnostic;
use orion::definitions::statements::{SqlStatement, sql_statements};
use orion::storage::DbBackend;
use sqlx::{Connection, Executor, Statement};

use crate::cli::ClippyFormat;

type CliError = Box<dyn std::error::Error>;

/// How long a connection may take before the connector is reported
/// unreachable.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// `sql check`'s flags.
pub(crate) struct SqlCheckRequest<'a> {
    pub(crate) path: &'a str,
    /// `name=url`: a connector's connection string, overriding the set's.
    pub(crate) connectors: &'a [String],
    /// Connectors whose statements are listed as unchecked.
    pub(crate) skip: &'a [String],
    /// A directory of migrations to build a scratch schema from.
    pub(crate) schema: Option<&'a str>,
    /// The PostgreSQL server that scratch schema is built on.
    pub(crate) database: Option<&'a str>,
    /// `name=role`: the role a connector's statements run as, with `--schema`.
    pub(crate) roles: &'a [String],
    pub(crate) format: ClippyFormat,
    pub(crate) boundary: orion::definitions::Boundary,
    pub(crate) plugin_dirs: &'a [String],
    pub(crate) model_dirs: &'a [String],
    pub(crate) config: &'a orion::config::AppConfig,
}

/// Whether a connector's grants were proven, or only its schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Proof {
    Grants,
    SchemaOnly,
}

/// One connector's result.
struct ConnectorReport {
    name: String,
    backend: &'static str,
    version: String,
    role: Option<String>,
    ok: usize,
    failed: usize,
    unchecked: usize,
    proof: Option<Proof>,
}

/// An open session on the database a connector's statements are checked on.
enum Session {
    Postgres(sqlx::PgConnection),
    MySql(sqlx::MySqlConnection),
    Sqlite(sqlx::SqliteConnection),
}

pub(crate) async fn run_sql_check(req: SqlCheckRequest<'_>) -> Result<i32, CliError> {
    let overrides = match name_values(req.connectors, "--connector", "name=<url>") {
        Ok(map) => map,
        Err(e) => return usage(&e),
    };
    let roles = match name_values(req.roles, "--role", "name=<role>") {
        Ok(map) => map,
        Err(e) => return usage(&e),
    };
    if req.database.is_some() && req.schema.is_none() {
        return usage("--database needs --schema: it is the server the scratch schema is built on");
    }
    if !roles.is_empty() && req.schema.is_none() {
        return usage(
            "--role applies with --schema; without it each connector runs as its own URL's user",
        );
    }

    // The gate `lint <dir>` runs: a statement of a set the API would refuse
    // is second-order noise.
    let path = std::path::Path::new(req.path);
    if !path.is_dir() {
        return usage(&format!("'{}' is not a directory of definitions", req.path));
    }
    let report = orion::definitions::gate_directory(
        path,
        &req.boundary,
        orion::definitions::GateOpts::default(),
        req.plugin_dirs,
        req.model_dirs,
    )?;
    for notice in report.notices() {
        eprintln!("{notice}");
    }
    if report.set.is_empty() {
        eprintln!("error: no definitions found under '{}'", req.path);
        return Ok(2);
    }
    let lint_errors: Vec<&Diagnostic> = report.findings.iter().filter(|f| f.is_error()).collect();
    if !lint_errors.is_empty() {
        for finding in &lint_errors {
            eprintln!("{}", finding.render_text());
        }
        println!(
            "{}: {} lint error(s) — fix those first; no statement was checked",
            req.path,
            lint_errors.len()
        );
        return Ok(2);
    }

    let statements = sql_statements(&report.set);
    let mut by_connector: BTreeMap<&str, Vec<&SqlStatement<'_>>> = BTreeMap::new();
    for statement in &statements {
        by_connector
            .entry(statement.connector)
            .or_default()
            .push(statement);
    }
    let connector_docs: BTreeMap<&str, &orion::definitions::Definition> = report
        .set
        .iter(orion::definitions::Entity::Connector)
        .filter_map(|def| Some((def.doc.get("name")?.as_str()?, def)))
        .collect();

    let mut findings: Vec<Diagnostic> = Vec::new();
    let mut reports: Vec<ConnectorReport> = Vec::new();

    // `--schema`: one scratch schema every connector is checked against.
    let mut scratch = match req.schema {
        Some(dir) => match Scratch::build(dir, req.database, &mut findings).await? {
            Some(scratch) => Some(scratch),
            None => return finish(&req, &statements, &findings, &reports),
        },
        None => None,
    };

    for (name, group) in &by_connector {
        let mut report_for = ConnectorReport {
            name: name.to_string(),
            backend: "",
            version: String::new(),
            role: None,
            ok: 0,
            failed: 0,
            unchecked: 0,
            proof: None,
        };
        if req.skip.iter().any(|s| s == name) {
            report_for.unchecked = group.len();
            findings.push(Diagnostic::warning(
                "sql.unchecked",
                format!("connector '{name}'"),
                format!(
                    "{} statement(s) on '{name}' were not checked (--skip-connector)",
                    group.len()
                ),
            ));
            reports.push(report_for);
            continue;
        }
        // The connector's URL: `--connector`, else the set's document,
        // resolved as the server resolves it.
        let url = match overrides.get(*name) {
            Some(url) => Ok(url.to_string()),
            None => connector_url(name, connector_docs.get(name).copied(), req.config).await,
        };
        let url = match (url, &scratch) {
            (Ok(url), _) => Some(url),
            // With a scratch schema the connector's own URL is only where its
            // role comes from.
            (Err(_), Some(_)) => None,
            (Err(problem), None) => {
                report_for.unchecked = group.len();
                findings.push(Diagnostic::error(
                    "sql.connector",
                    format!("connector '{name}'"),
                    problem,
                ));
                reports.push(report_for);
                continue;
            }
        };

        if let Some(scratch) = scratch.as_mut() {
            let role = roles
                .get(*name)
                .map(|r| r.to_string())
                .or_else(|| url.as_deref().and_then(url_user));
            scratch
                .check(
                    name,
                    role.as_deref(),
                    group,
                    &report.set,
                    &mut report_for,
                    &mut findings,
                )
                .await?;
            reports.push(report_for);
            continue;
        }
        let url = url.unwrap_or_default();
        match open(&url, name).await {
            Ok(mut session) => {
                check_group(
                    &mut session,
                    name,
                    group,
                    &report.set,
                    &mut report_for,
                    &mut findings,
                )
                .await?;
            }
            Err(problem) => {
                report_for.unchecked = group.len();
                findings.push(Diagnostic::error(
                    "sql.connector",
                    format!("connector '{name}'"),
                    problem,
                ));
            }
        }
        reports.push(report_for);
    }
    if let Some(scratch) = scratch {
        scratch.discard().await;
    }
    finish(&req, &statements, &findings, &reports)
}

fn usage(message: &str) -> Result<i32, CliError> {
    eprintln!("error: {message}");
    Ok(2)
}

/// `name=value` flags as a map; the first malformed one is the error.
fn name_values<'a>(
    items: &'a [String],
    flag: &str,
    shape: &str,
) -> Result<BTreeMap<&'a str, &'a str>, String> {
    let mut out = BTreeMap::new();
    for item in items {
        let Some((name, value)) = item.split_once('=') else {
            return Err(format!("{flag} '{item}' must be {shape}"));
        };
        if name.is_empty() || value.is_empty() {
            return Err(format!("{flag} '{item}' must be {shape}"));
        }
        out.insert(name, value);
    }
    Ok(out)
}

/// A connector's connection string, from its document in the set, resolved
/// exactly as the registry resolves it and held to the same endpoint rules
/// — a connector the server would refuse to dial is a finding here.
async fn connector_url(
    name: &str,
    doc: Option<&orion::definitions::Definition>,
    config: &orion::config::AppConfig,
) -> Result<String, String> {
    let Some(doc) = doc else {
        return Err(format!(
            "connector '{name}' is not in this set — pass --connector {name}=<url> or \
             --skip-connector {name}"
        ));
    };
    let connector_type = doc
        .doc
        .get("connector_type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let config_json = doc
        .doc
        .get("config")
        .map(ToString::to_string)
        .unwrap_or_else(|| "{}".to_string());
    let vars = config.vars.to_json();
    let resolved = orion::connector::resolve_connector_config(
        name,
        name,
        connector_type,
        &config_json,
        vars.as_ref(),
        orion::connector::secrets::default_resolvers(),
    )
    .await
    .map_err(|e| {
        format!(
            "connector '{name}' does not resolve ({}): {}",
            e.stage, e.reason
        )
    })?;
    let orion::connector::ConnectorConfig::Db(db) = resolved else {
        return Err(format!("connector '{name}' is not a db connector"));
    };
    orion::validation::endpoints::check_db_endpoint(name, &db)
        .await
        .map_err(|e| format!("connector '{name}': {}", e.client_message()))?;
    Ok(db.connection_string)
}

/// The user a PostgreSQL URL connects as, when it names one.
fn url_user(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))?;
    let authority = rest.split(['/', '?']).next()?;
    let (userinfo, _) = authority.rsplit_once('@')?;
    let user = userinfo.split(':').next()?;
    (!user.is_empty()).then(|| user.to_string())
}

/// Open one connection — a single named session, not a pool, so the
/// read-only transaction and the role mean something. A SQLite file is
/// opened read-only and never created.
async fn open(url: &str, name: &str) -> Result<Session, String> {
    let backend = orion::storage::detect_backend(url)
        .map_err(|_| format!("connector '{name}' is not a PostgreSQL, MySQL or SQLite URL"))?;
    let connect = async {
        match backend {
            DbBackend::Postgres => sqlx::PgConnection::connect(url)
                .await
                .map(Session::Postgres),
            DbBackend::Mysql => sqlx::MySqlConnection::connect(url)
                .await
                .map(Session::MySql),
            DbBackend::Sqlite => {
                let options: sqlx::sqlite::SqliteConnectOptions = url
                    .parse::<sqlx::sqlite::SqliteConnectOptions>()?
                    .read_only(true)
                    .create_if_missing(false);
                sqlx::SqliteConnection::connect_with(&options)
                    .await
                    .map(Session::Sqlite)
            }
        }
    };
    match tokio::time::timeout(DEFAULT_CONNECT_TIMEOUT, connect).await {
        Ok(Ok(session)) => Ok(session),
        Ok(Err(e)) => Err(format!(
            "could not connect to connector '{name}': {}",
            redact_error(&e)
        )),
        Err(_) => Err(format!(
            "could not connect to connector '{name}' within {}s",
            DEFAULT_CONNECT_TIMEOUT.as_secs()
        )),
    }
}

/// A driver error without a connection string in it.
fn redact_error(e: &sqlx::Error) -> String {
    orion::connector::redact_url_secrets_or_raw(&e.to_string())
}

/// Check every statement of one connector on its own session.
async fn check_group(
    session: &mut Session,
    name: &str,
    group: &[&SqlStatement<'_>],
    set: &orion::definitions::DefinitionSet,
    report: &mut ConnectorReport,
    findings: &mut Vec<Diagnostic>,
) -> Result<(), CliError> {
    match session {
        Session::Postgres(conn) => {
            let (user, version_num, version) = pg_identity(conn).await?;
            report.backend = "postgres";
            report.version = version;
            report.role = Some(user.clone());
            sqlx::raw_sql("BEGIN READ ONLY").execute(&mut *conn).await?;
            let grants = version_num >= 160000;
            for statement in group {
                let outcome = pg_check(
                    conn,
                    statement,
                    grants,
                    &format!("on '{name}' as role '{user}'"),
                )
                .await?;
                record(outcome, statement, set, report, findings);
            }
            sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await?;
            report.proof = Some(if grants {
                Proof::Grants
            } else {
                Proof::SchemaOnly
            });
            if !grants {
                findings.push(Diagnostic::note(
                    "sql.proof",
                    format!("connector '{name}'"),
                    format!(
                        "PostgreSQL {}: statements were prepared, grants were not proven \
                         (EXPLAIN (GENERIC_PLAN) needs 16+)",
                        report.version
                    ),
                ));
            }
        }
        Session::MySql(conn) => {
            report.backend = "mysql";
            report.version = sqlx::query_scalar::<_, String>("SELECT VERSION()")
                .fetch_one(&mut *conn)
                .await
                .unwrap_or_default();
            for statement in group {
                let outcome =
                    prepare_check(&mut *conn, statement, false, &format!("on '{name}'")).await;
                record(outcome, statement, set, report, findings);
            }
            report.proof = Some(Proof::SchemaOnly);
            findings.push(Diagnostic::note(
                "sql.proof",
                format!("connector '{name}'"),
                "MySQL: statements were prepared, grants were not proven",
            ));
        }
        Session::Sqlite(conn) => {
            report.backend = "sqlite";
            report.version = sqlx::query_scalar::<_, String>("SELECT sqlite_version()")
                .fetch_one(&mut *conn)
                .await
                .unwrap_or_default();
            for statement in group {
                let outcome =
                    prepare_check(&mut *conn, statement, true, &format!("on '{name}'")).await;
                record(outcome, statement, set, report, findings);
            }
            report.proof = Some(Proof::SchemaOnly);
        }
    }
    Ok(())
}

async fn pg_identity(conn: &mut sqlx::PgConnection) -> Result<(String, i32, String), sqlx::Error> {
    sqlx::query_as::<_, (String, i32, String)>(
        "SELECT current_user::text, current_setting('server_version_num')::int, \
         current_setting('server_version')",
    )
    .fetch_one(&mut *conn)
    .await
}

/// What checking one statement found.
enum Outcome {
    Ok,
    /// Findings about it; any error among them counts it failed.
    Findings(Vec<(Severity, &'static str, String)>),
}

#[derive(Clone, Copy)]
enum Severity {
    Error,
    Warning,
    Note,
}

/// One PostgreSQL statement: prepared first — Parse and Describe only, and a
/// multi-command string is refused there, which is what makes the `EXPLAIN`
/// that follows safe — then, on 16+, `EXPLAIN (GENERIC_PLAN)`, which runs
/// executor start-up, where the relation and column grants are checked,
/// and executes nothing. Each statement sits in a savepoint, so one failure
/// does not abort the transaction for the rest.
async fn pg_check(
    conn: &mut sqlx::PgConnection,
    statement: &SqlStatement<'_>,
    grants: bool,
    context: &str,
) -> Result<Outcome, sqlx::Error> {
    sqlx::raw_sql("SAVEPOINT orion_sql_check")
        .execute(&mut *conn)
        .await?;
    let mut found = Vec::new();
    let prepared = conn
        .prepare(sqlx::SqlSafeStr::into_sql_str(sqlx::AssertSqlSafe(
            statement.query.to_string(),
        )))
        .await;
    match prepared {
        Err(e) => {
            let (code, message) = db_error(&e);
            if code.as_deref() == Some("42P18") {
                found.push((
                    Severity::Warning,
                    "sql.check",
                    format!(
                        "{context}: {message} — at run time Orion falls back to value-shaped \
                         binding with the statement cache off"
                    ),
                ));
            } else {
                found.push((
                    Severity::Error,
                    "sql.check",
                    sqlstate(context, &message, code),
                ));
            }
        }
        Ok(prepared) => {
            let declared = match prepared.parameters() {
                Some(sqlx::Either::Left(types)) => Some(types.len()),
                Some(sqlx::Either::Right(n)) => Some(n),
                None => None,
            };
            if let (Some(declared), Some(bound)) = (declared, statement.bound)
                && declared != bound
            {
                found.push((
                    Severity::Error,
                    "sql.params",
                    format!(
                        "the statement takes {declared} parameter(s) but `params` binds {bound}"
                    ),
                ));
            }
            if grants {
                let explain = format!("EXPLAIN (GENERIC_PLAN) {}", statement.query);
                if let Err(e) = sqlx::raw_sql(sqlx::AssertSqlSafe(explain))
                    .execute(&mut *conn)
                    .await
                {
                    let (code, message) = db_error(&e);
                    if code.as_deref() == Some("42601") {
                        found.push((
                            Severity::Note,
                            "sql.check",
                            format!(
                                "{context}: prepared; EXPLAIN does not accept this statement \
                                 kind, so its grants were not proven"
                            ),
                        ));
                    } else {
                        found.push((
                            Severity::Error,
                            "sql.check",
                            sqlstate(context, &message, code),
                        ));
                    }
                }
            }
        }
    }
    sqlx::raw_sql("ROLLBACK TO SAVEPOINT orion_sql_check")
        .execute(&mut *conn)
        .await?;
    Ok(if found.is_empty() {
        Outcome::Ok
    } else {
        Outcome::Findings(found)
    })
}

/// One MySQL or SQLite statement: a server-side (or engine) prepare, which
/// resolves the schema and executes nothing. A SQLite parameter-count
/// mismatch is a warning: the driver binds a missing value as NULL.
async fn prepare_check<'c, E>(
    conn: E,
    statement: &SqlStatement<'_>,
    lenient_count: bool,
    context: &str,
) -> Outcome
where
    E: Executor<'c>,
{
    match conn
        .prepare(sqlx::SqlSafeStr::into_sql_str(sqlx::AssertSqlSafe(
            statement.query.to_string(),
        )))
        .await
    {
        Err(e) => {
            let (code, message) = db_error(&e);
            // SQLite's number is its own result code, not a SQLSTATE.
            let text = match (lenient_count, code) {
                (true, Some(code)) => format!("{context}: {message} (SQLite code {code})"),
                (_, code) => sqlstate(context, &message, code),
            };
            Outcome::Findings(vec![(Severity::Error, "sql.check", text)])
        }
        Ok(prepared) => {
            let declared = match prepared.parameters() {
                Some(sqlx::Either::Left(types)) => Some(types.len()),
                Some(sqlx::Either::Right(n)) => Some(n),
                None => None,
            };
            match (declared, statement.bound) {
                (Some(declared), Some(bound)) if declared != bound => Outcome::Findings(vec![(
                    if lenient_count {
                        Severity::Warning
                    } else {
                        Severity::Error
                    },
                    "sql.params",
                    if lenient_count {
                        format!(
                            "the statement takes {declared} parameter(s) but `params` binds \
                                 {bound} — SQLite binds a missing value as NULL rather than failing"
                        )
                    } else {
                        format!(
                            "the statement takes {declared} parameter(s) but `params` binds \
                                 {bound}"
                        )
                    },
                )]),
                _ => Outcome::Ok,
            }
        }
    }
}

/// A database error's SQLSTATE and message; anything else's text.
fn db_error(e: &sqlx::Error) -> (Option<String>, String) {
    match e {
        sqlx::Error::Database(db) => (db.code().map(|c| c.to_string()), db.message().to_string()),
        other => (None, redact_error(other)),
    }
}

fn sqlstate(context: &str, message: &str, code: Option<String>) -> String {
    match code {
        Some(code) => format!("{context}: {message} (SQLSTATE {code})"),
        None => format!("{context}: {message}"),
    }
}

/// Count one statement's outcome and turn its findings into diagnostics,
/// located where the set can say.
fn record(
    outcome: Outcome,
    statement: &SqlStatement<'_>,
    set: &orion::definitions::DefinitionSet,
    report: &mut ConnectorReport,
    findings: &mut Vec<Diagnostic>,
) {
    let found = match outcome {
        Outcome::Ok => {
            report.ok += 1;
            return;
        }
        Outcome::Findings(found) => found,
    };
    if found.iter().any(|(s, _, _)| matches!(s, Severity::Error)) {
        report.failed += 1;
    } else {
        report.ok += 1;
    }
    let def = set
        .definitions
        .iter()
        .find(|d| d.origin == statement.origin);
    let line = def.and_then(|d| d.locate(&statement.path));
    let via = def.and_then(|d| d.source_of(&statement.path).describe());
    for (severity, check, message) in found {
        let entity = format!(
            "workflow '{}' task '{}'",
            statement.workflow, statement.task_id
        );
        let diagnostic = match severity {
            Severity::Error => Diagnostic::error(check, entity, message),
            Severity::Warning => Diagnostic::warning(check, entity, message),
            Severity::Note => Diagnostic::note(check, entity, message),
        };
        findings.push(
            diagnostic
                .with_location(statement.origin, Some(&statement.path), line)
                .with_via(via.clone()),
        );
    }
}

// ------------------------------------------------------------
// --schema
// ------------------------------------------------------------

/// The statement kinds a migration may not carry into the one transaction a
/// scratch schema is built in: transaction control would end it — and
/// persist everything before it into `--database` — and the rest PostgreSQL
/// refuses inside a transaction block.
const REFUSED_IN_SCRATCH: &[&str] = &[
    "BEGIN", "START", "COMMIT", "END", "ROLLBACK", "ABORT", "VACUUM",
];

/// A scratch schema: the migrations applied in one transaction that is
/// always rolled back (PostgreSQL), or to an in-memory database (SQLite).
enum Scratch {
    Postgres {
        conn: sqlx::PgConnection,
        version_num: i32,
        version: String,
        owner: String,
    },
    Sqlite(sqlx::SqliteConnection),
}

impl Scratch {
    /// Build the scratch schema from `dir`. `None` — with a finding — when
    /// a migration is refused or fails; nothing has persisted either way.
    async fn build(
        dir: &str,
        database: Option<&str>,
        findings: &mut Vec<Diagnostic>,
    ) -> Result<Option<Self>, CliError> {
        let files = migration_files(dir)?;
        let backend = match database {
            Some(url) => orion::storage::detect_backend(url).map_err(|e| e.to_string())?,
            None => DbBackend::Sqlite,
        };
        match backend {
            DbBackend::Mysql => {
                findings.push(Diagnostic::error(
                    "sql.schema",
                    "--schema",
                    "MySQL commits DDL implicitly, so a scratch schema cannot be rolled back — \
                     point --connector at a prepared database instead",
                ));
                Ok(None)
            }
            DbBackend::Sqlite => {
                if database.is_some() {
                    findings.push(Diagnostic::error(
                        "sql.schema",
                        "--schema",
                        "a SQLite scratch schema is built in memory — drop --database",
                    ));
                    return Ok(None);
                }
                let mut conn = sqlx::SqliteConnection::connect("sqlite::memory:").await?;
                for (file, text) in &files {
                    if let Err(e) = sqlx::raw_sql(sqlx::AssertSqlSafe(text.clone()))
                        .execute(&mut conn)
                        .await
                    {
                        findings.push(schema_failure(file, &e));
                        return Ok(None);
                    }
                }
                Ok(Some(Scratch::Sqlite(conn)))
            }
            DbBackend::Postgres => {
                // Refused offline, before anything is sent: a migration that
                // ended the transaction would persist what came before it.
                for (file, text) in &files {
                    if let Some(problem) = refused_statement(text) {
                        findings.push(Diagnostic::error(
                            "sql.schema",
                            file.as_str(),
                            format!(
                                "{problem} — a scratch schema is built in one transaction that \
                                 is rolled back, and this statement cannot run inside it"
                            ),
                        ));
                        return Ok(None);
                    }
                }
                let url = database.unwrap_or_default();
                let mut conn = sqlx::PgConnection::connect(url).await.map_err(|e| {
                    format!("could not connect to --database: {}", redact_error(&e))
                })?;
                let (owner, version_num, version) = pg_identity(&mut conn).await?;
                sqlx::raw_sql("BEGIN").execute(&mut conn).await?;
                let before: i64 = sqlx::query_scalar("SELECT txid_current()")
                    .fetch_one(&mut conn)
                    .await?;
                for (file, text) in &files {
                    if let Err(e) = sqlx::raw_sql(sqlx::AssertSqlSafe(text.clone()))
                        .execute(&mut conn)
                        .await
                    {
                        findings.push(schema_failure(file, &e));
                        let _ = sqlx::raw_sql("ROLLBACK").execute(&mut conn).await;
                        return Ok(None);
                    }
                }
                let after: i64 = sqlx::query_scalar("SELECT txid_current()")
                    .fetch_one(&mut conn)
                    .await?;
                if after != before {
                    let _ = sqlx::raw_sql("ROLLBACK").execute(&mut conn).await;
                    return Err(
                        "a migration ended the scratch transaction; changes before it \
                                may have been committed to --database"
                            .into(),
                    );
                }
                // Everything after the schema is provably non-mutating.
                sqlx::raw_sql("SET TRANSACTION READ ONLY")
                    .execute(&mut conn)
                    .await?;
                Ok(Some(Scratch::Postgres {
                    conn,
                    version_num,
                    version,
                    owner,
                }))
            }
        }
    }

    /// Check one connector's statements on the scratch schema, as `role`.
    async fn check(
        &mut self,
        name: &str,
        role: Option<&str>,
        group: &[&SqlStatement<'_>],
        set: &orion::definitions::DefinitionSet,
        report: &mut ConnectorReport,
        findings: &mut Vec<Diagnostic>,
    ) -> Result<(), CliError> {
        match self {
            Scratch::Sqlite(conn) => {
                report.backend = "sqlite";
                report.version = "scratch".to_string();
                for statement in group {
                    let outcome =
                        prepare_check(&mut *conn, statement, true, &format!("on '{name}'")).await;
                    record(outcome, statement, set, report, findings);
                }
                report.proof = Some(Proof::SchemaOnly);
            }
            Scratch::Postgres {
                conn,
                version_num,
                version,
                owner,
            } => {
                report.backend = "postgres";
                report.version = version.clone();
                let grants = *version_num >= 160000 && role.is_some();
                // The role lives inside a savepoint: rolling back to it undoes
                // the switch, and a switch that fails does not abort the
                // scratch transaction for the connectors after this one.
                sqlx::raw_sql("SAVEPOINT orion_sql_role")
                    .execute(&mut *conn)
                    .await?;
                if let Some(role) = role {
                    let set_role = format!("SET LOCAL ROLE {}", quote_identifier(role));
                    if let Err(e) = sqlx::raw_sql(sqlx::AssertSqlSafe(set_role))
                        .execute(&mut *conn)
                        .await
                    {
                        sqlx::raw_sql("ROLLBACK TO SAVEPOINT orion_sql_role")
                            .execute(&mut *conn)
                            .await?;
                        let (_, message) = db_error(&e);
                        report.unchecked = group.len();
                        findings.push(Diagnostic::error(
                            "sql.connector",
                            format!("connector '{name}'"),
                            format!(
                                "cannot run as role '{role}' on the --database server ({message}) \
                                 — create it in a migration or pass --role {name}=<existing role>"
                            ),
                        ));
                        return Ok(());
                    }
                }
                let as_role = role.unwrap_or(owner.as_str()).to_string();
                report.role = Some(as_role.clone());
                for statement in group {
                    let outcome = pg_check(
                        conn,
                        statement,
                        grants,
                        &format!("on '{name}' as role '{as_role}'"),
                    )
                    .await?;
                    record(outcome, statement, set, report, findings);
                }
                sqlx::raw_sql("ROLLBACK TO SAVEPOINT orion_sql_role")
                    .execute(&mut *conn)
                    .await?;
                report.proof = Some(if grants {
                    Proof::Grants
                } else {
                    Proof::SchemaOnly
                });
                if !grants {
                    findings.push(Diagnostic::note(
                        "sql.proof",
                        format!("connector '{name}'"),
                        if role.is_none() {
                            format!(
                                "no role for '{name}' — its statements ran as the --database \
                                 user, so grants were not proven (pass --role {name}=<role>)"
                            )
                        } else {
                            format!(
                                "PostgreSQL {version}: statements were prepared, grants were not \
                                 proven (EXPLAIN (GENERIC_PLAN) needs 16+)"
                            )
                        },
                    ));
                }
            }
        }
        Ok(())
    }

    /// Roll the scratch schema back. Always: nothing it built persists.
    async fn discard(self) {
        if let Scratch::Postgres { mut conn, .. } = self {
            let _ = sqlx::raw_sql("ROLLBACK").execute(&mut conn).await;
        }
    }
}

/// The `*.sql` files of `dir`, in byte-wise filename order — the order a
/// `NNN_name.sql` sequence is applied in.
fn migration_files(dir: &str) -> Result<Vec<(String, String)>, CliError> {
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("read --schema '{dir}': {e}"))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "sql"))
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("read '{}': {e}", path.display()))?;
            Ok((path.display().to_string(), text))
        })
        .collect()
}

/// The first statement of a migration a scratch transaction cannot hold,
/// described, or `None`.
fn refused_statement(text: &str) -> Option<String> {
    let statements = match orion::sql_lex::statements(text) {
        Ok(statements) => statements,
        Err(e) => return Some(format!("cannot be pre-flighted: {e}")),
    };
    for statement in statements {
        let Some(keyword) = orion::sql_lex::leading_keyword(statement) else {
            continue;
        };
        let upper = keyword.to_ascii_uppercase();
        let normalized = orion::sql_lex::normalize(statement)
            .map(|n| n.text.to_ascii_uppercase())
            .unwrap_or_else(|_| statement.to_ascii_uppercase());
        if REFUSED_IN_SCRATCH.contains(&upper.as_str()) && !normalized.starts_with("BEGIN ATOMIC") {
            return Some(format!(
                "'{}' ends or controls the transaction",
                first_words(statement)
            ));
        }
        if normalized.contains(" CONCURRENTLY")
            || normalized.starts_with("CREATE DATABASE")
            || normalized.starts_with("DROP DATABASE")
            || normalized.starts_with("CREATE TABLESPACE")
            || normalized.starts_with("DROP TABLESPACE")
            || normalized.starts_with("ALTER SYSTEM")
            || normalized.starts_with("PREPARE TRANSACTION")
        {
            return Some(format!(
                "'{}' cannot run inside a transaction block",
                first_words(statement)
            ));
        }
    }
    None
}

fn first_words(statement: &str) -> String {
    statement
        .split_whitespace()
        .take(4)
        .collect::<Vec<_>>()
        .join(" ")
}

fn schema_failure(file: &str, e: &sqlx::Error) -> Diagnostic {
    let (code, message) = db_error(e);
    Diagnostic::error(
        "sql.schema",
        file,
        sqlstate("the migration failed", &message, code),
    )
}

/// A role name as a quoted identifier, `"` doubled — never interpolated raw.
fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

// ------------------------------------------------------------
// Output
// ------------------------------------------------------------

fn finish(
    req: &SqlCheckRequest<'_>,
    statements: &[SqlStatement<'_>],
    findings: &[Diagnostic],
    reports: &[ConnectorReport],
) -> Result<i32, CliError> {
    let mut findings: Vec<&Diagnostic> = findings.iter().collect();
    findings.sort_by(|a, b| (&a.file, &a.path).cmp(&(&b.file, &b.path)));
    let failed: usize = reports.iter().map(|r| r.failed).sum();
    // Every statement no connector counted — its connector was skipped or
    // unreachable, or the scratch schema was refused before any check.
    let checked: usize = reports.iter().map(|r| r.ok + r.failed).sum();
    let unchecked = statements.len().saturating_sub(checked);
    let errors = findings.iter().filter(|f| f.is_error()).count();
    match req.format {
        ClippyFormat::Json => {
            for finding in &findings {
                println!("{}", finding.render_json());
            }
            println!(
                "{}",
                serde_json::json!({"summary": {
                    "statements": statements.len(),
                    "failed": failed,
                    "unchecked": unchecked,
                    "connectors": reports.iter().map(|r| serde_json::json!({
                        "name": r.name,
                        "backend": r.backend,
                        "version": r.version,
                        "role": r.role,
                        "ok": r.ok,
                        "failed": r.failed,
                        "unchecked": r.unchecked,
                        "grants_proven": r.proof == Some(Proof::Grants),
                    })).collect::<Vec<_>>(),
                }})
            );
        }
        ClippyFormat::Text => {
            println!(
                "checking {} statement(s) on {} connector(s)",
                statements.len(),
                reports.len()
            );
            let width = reports.iter().map(|r| r.name.len()).max().unwrap_or(0);
            for r in reports {
                let mut counts = format!("{} ok", r.ok);
                if r.failed > 0 {
                    counts.push_str(&format!(", {} failed", r.failed));
                }
                if r.unchecked > 0 {
                    counts.push_str(&format!(", {} unchecked", r.unchecked));
                }
                let proof = match r.proof {
                    Some(Proof::Grants) => "grants proven",
                    Some(Proof::SchemaOnly) => "grants not proven",
                    None => "",
                };
                let role = r
                    .role
                    .as_deref()
                    .map(|role| format!("role {role}"))
                    .unwrap_or_default();
                println!(
                    "  {:<width$}  {} {}  {role}  {counts}  {proof}",
                    r.name, r.backend, r.version
                );
            }
            for finding in &findings {
                eprintln!("{}", finding.render_text());
            }
            let unchecked_note = if unchecked > 0 {
                format!(", {unchecked} not checked")
            } else {
                String::new()
            };
            if errors > 0 {
                println!(
                    "{failed} of {} statement(s) failed{unchecked_note}",
                    statements.len()
                );
            } else if unchecked > 0 {
                println!(
                    "{checked} of {} statement(s) checked{unchecked_note}",
                    statements.len()
                );
            } else {
                println!("every statement checked ({} total)", statements.len());
            }
        }
    }
    Ok(if errors > 0 { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_value_flags_parse_and_name_the_bad_one() {
        let ok = vec!["orders=postgres://u@h/db".to_string(), "b=x".to_string()];
        let map = name_values(&ok, "--connector", "name=<url>").expect("parses");
        assert_eq!(map["orders"], "postgres://u@h/db");
        let bad = vec!["orders".to_string()];
        let err = name_values(&bad, "--connector", "name=<url>").expect_err("refused");
        assert!(
            err.contains("--connector 'orders' must be name=<url>"),
            "{err}"
        );
    }

    #[test]
    fn a_role_is_quoted_never_interpolated() {
        assert_eq!(quote_identifier("gate"), "\"gate\"");
        assert_eq!(quote_identifier("a\"; DROP"), "\"a\"\"; DROP\"");
    }

    #[test]
    fn the_url_user_is_the_default_role() {
        assert_eq!(
            url_user("postgres://gate:pw@db:5432/orders").as_deref(),
            Some("gate")
        );
        assert_eq!(
            url_user("postgresql://owner@db/orders").as_deref(),
            Some("owner")
        );
        assert_eq!(url_user("postgres://db/orders"), None);
        assert_eq!(url_user("sqlite:orders.db"), None);
    }

    #[test]
    fn a_migration_that_would_leave_the_transaction_is_refused_offline() {
        assert!(refused_statement("CREATE TABLE t (x int);\nCOMMIT;").is_some());
        assert!(refused_statement("CREATE INDEX CONCURRENTLY i ON t (x);").is_some());
        assert!(refused_statement("VACUUM t;").is_some());
        assert!(
            refused_statement("DO $$ BEGIN RAISE NOTICE 'x'; END $$;\nSELECT 'commit';").is_none()
        );
        assert!(refused_statement("CREATE TABLE t (x int);\nGRANT SELECT ON t TO gate;").is_none());
    }
}
