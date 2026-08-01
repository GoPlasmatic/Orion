//! Database abstraction: pool construction, backend detection, migrations,
//! row models and repositories.
//!
//! ## Why there are no foreign keys (D26)
//!
//! `grep REFERENCES migrations/` returns nothing, on any backend, and that is
//! deliberate rather than an oversight:
//!
//! - **`workflows` and `channels` are version-keyed.** Both have a composite
//!   primary key `(id, version)`, and `channels.workflow_id` references a
//!   workflow *id*, never a specific version. There is no key to point an FK
//!   at, and adding one to `(workflow_id, version)` would freeze a channel to
//!   the workflow version current when it was authored — the opposite of the
//!   rollout model.
//! - **`traces` outlive what they reference.** A trace records what ran, so it
//!   must survive the deletion of its channel or workflow. An FK would either
//!   block that delete or cascade away the audit record.
//! - **`audit_logs.resource_id` is polymorphic** — it holds a workflow,
//!   channel, connector or breaker id depending on `resource_type`, which no
//!   single FK can express.
//! - **`trace_dlq.trace_id`** is the one plausible FK. It is omitted so DLQ
//!   rows survive trace retention cleanup, which is exactly when they matter.
//!
//! Referential integrity is enforced in the repositories instead. SQLite is
//! still opened with `PRAGMA foreign_keys = ON` so a future FK takes effect
//! immediately; see `init_sqlite_pool`.

pub mod config_encryption;
pub mod models;
pub mod repositories;
pub mod schema;

use std::sync::OnceLock;
use std::time::Duration;

use sea_query_binder::SqlxBinder;

use crate::config::StorageConfig;
use crate::errors::OrionError;

// ============================================================
// Database backend detection — determined at runtime from URL
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbBackend {
    Sqlite,
    Postgres,
    Mysql,
}

impl std::fmt::Display for DbBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite => write!(f, "sqlite"),
            Self::Postgres => write!(f, "postgres"),
            Self::Mysql => write!(f, "mysql"),
        }
    }
}

static DB_BACKEND: OnceLock<DbBackend> = OnceLock::new();

/// Detect the database backend from a connection URL.
pub fn detect_backend(url: &str) -> Result<DbBackend, OrionError> {
    if url.starts_with("sqlite:") || url.starts_with("file:") {
        Ok(DbBackend::Sqlite)
    } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        Ok(DbBackend::Postgres)
    } else if url.starts_with("mysql://") || url.starts_with("mariadb://") {
        Ok(DbBackend::Mysql)
    } else {
        Err(OrionError::Config {
            message: format!(
                "Unsupported database URL scheme: {url}. Expected sqlite:, postgres://, or mysql://"
            ),
        })
    }
}

/// Get the active database backend. Panics if not yet initialized.
pub fn get_backend() -> DbBackend {
    *DB_BACKEND
        .get()
        .expect("Database backend not initialized. Call init_pool() first.")
}

/// Set the database backend for unit tests that call `build_sqlx` without a pool.
#[cfg(test)]
pub fn set_backend_for_test(backend: DbBackend) {
    DB_BACKEND.set(backend).ok();
}

// ============================================================
// DbPool — enum wrapping concrete pool types
// ============================================================

/// A type [`DbPool`] and [`DbTransaction`] can decode a row into on any of the
/// three backends.
///
/// `DbPool` dispatches over three concrete sqlx pools, so every typed fetch
/// needs `FromRow` for all three row types plus `Send + Unpin`. Spelled out,
/// that is a five-line `where` clause; it was repeated in eight signatures
/// across this file and `repositories/helpers.rs` (D19). The blanket impl
/// below means nothing has to implement this explicitly — deriving
/// `sqlx::FromRow` on a struct in `models::rows` is enough.
pub trait DbRow:
    for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow>
    + for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>
    + for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow>
    + Send
    + Unpin
{
}

impl<T> DbRow for T where
    T: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow>
        + for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>
        + for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow>
        + Send
        + Unpin
{
}

/// Database connection pool that wraps the concrete sqlx pool type,
/// selected at runtime based on the connection URL.
#[derive(Clone)]
pub enum DbPool {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
    Mysql(sqlx::MySqlPool),
}

/// Dispatch a query expression across all pool variants.
///
/// Each arm binds the inner pool to `$p` and evaluates `$body`.
/// The expression is type-checked independently per arm, so the pool type
/// is correctly inferred by sqlx in each case.
macro_rules! dispatch_pool {
    ($self:expr, $p:ident => $body:expr) => {
        match $self {
            DbPool::Sqlite($p) => $body,
            DbPool::Postgres($p) => $body,
            DbPool::Mysql($p) => $body,
        }
    };
}

impl DbPool {
    pub fn size(&self) -> u32 {
        dispatch_pool!(self, p => p.size())
    }

    /// Database connectivity check: `SELECT 1` through the pool.
    ///
    /// D22: this lived on `WorkflowRepository` purely because the health
    /// checks needed *a* pool and that trait happened to hold one. It is a
    /// property of the pool, not of any entity.
    pub async fn ping(&self) -> Result<(), sqlx::Error> {
        crate::metrics::timed_db_op("db.ping", async {
            let (sql, values) =
                build_sqlx(sea_query::Query::select().expr(sea_query::Expr::val(1i32)));
            self.fetch_scalar::<i32>(&sql, values).await?;
            Ok(())
        })
        .await
    }

    pub fn num_idle(&self) -> usize {
        dispatch_pool!(self, p => p.num_idle())
    }

    pub async fn fetch_all_as<T: DbRow>(
        &self,
        sql: &str,
        values: sea_query_binder::SqlxValues,
    ) -> Result<Vec<T>, sqlx::Error> {
        dispatch_pool!(self, p => sqlx::query_as_with::<_, T, _>(sql, values).fetch_all(p).await)
    }

    pub async fn fetch_one_as<T: DbRow>(
        &self,
        sql: &str,
        values: sea_query_binder::SqlxValues,
    ) -> Result<T, sqlx::Error> {
        dispatch_pool!(self, p => sqlx::query_as_with::<_, T, _>(sql, values).fetch_one(p).await)
    }

    pub async fn fetch_optional_as<T: DbRow>(
        &self,
        sql: &str,
        values: sea_query_binder::SqlxValues,
    ) -> Result<Option<T>, sqlx::Error> {
        dispatch_pool!(self, p => sqlx::query_as_with::<_, T, _>(sql, values).fetch_optional(p).await)
    }

    pub async fn execute_query(
        &self,
        sql: &str,
        values: sea_query_binder::SqlxValues,
    ) -> Result<u64, sqlx::Error> {
        dispatch_pool!(self, p => {
            let r = sqlx::query_with(sql, values).execute(p).await?;
            Ok(r.rows_affected())
        })
    }

    pub async fn fetch_scalar<T>(
        &self,
        sql: &str,
        values: sea_query_binder::SqlxValues,
    ) -> Result<T, sqlx::Error>
    where
        T: Send + Unpin + 'static,
        T: sqlx::Type<sqlx::Sqlite> + for<'r> sqlx::Decode<'r, sqlx::Sqlite>,
        T: sqlx::Type<sqlx::Postgres> + for<'r> sqlx::Decode<'r, sqlx::Postgres>,
        T: sqlx::Type<sqlx::MySql> + for<'r> sqlx::Decode<'r, sqlx::MySql>,
        (T,): for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow>,
        (T,): for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>,
        (T,): for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow>,
    {
        dispatch_pool!(self, p => sqlx::query_scalar_with::<_, T, _>(sql, values).fetch_one(p).await)
    }

    pub async fn begin_tx(&self) -> Result<DbTransaction, sqlx::Error> {
        match self {
            DbPool::Sqlite(p) => Ok(DbTransaction::Sqlite(p.begin().await?)),
            DbPool::Postgres(p) => Ok(DbTransaction::Postgres(p.begin().await?)),
            DbPool::Mysql(p) => Ok(DbTransaction::Mysql(p.begin().await?)),
        }
    }
}

// ============================================================
// DbTransaction — enum wrapping concrete transaction types
// ============================================================

pub enum DbTransaction {
    Sqlite(sqlx::Transaction<'static, sqlx::Sqlite>),
    Postgres(sqlx::Transaction<'static, sqlx::Postgres>),
    Mysql(sqlx::Transaction<'static, sqlx::MySql>),
}

macro_rules! dispatch_tx {
    ($self:expr, $tx:ident => $body:expr) => {
        match $self {
            DbTransaction::Sqlite($tx) => $body,
            DbTransaction::Postgres($tx) => $body,
            DbTransaction::Mysql($tx) => $body,
        }
    };
}

impl DbTransaction {
    pub async fn commit(self) -> Result<(), sqlx::Error> {
        match self {
            DbTransaction::Sqlite(tx) => tx.commit().await,
            DbTransaction::Postgres(tx) => tx.commit().await,
            DbTransaction::Mysql(tx) => tx.commit().await,
        }
    }

    pub async fn fetch_all_as<T: DbRow>(
        &mut self,
        sql: &str,
        values: sea_query_binder::SqlxValues,
    ) -> Result<Vec<T>, sqlx::Error> {
        dispatch_tx!(self, tx => sqlx::query_as_with::<_, T, _>(sql, values).fetch_all(&mut **tx).await)
    }

    pub async fn fetch_optional_as<T: DbRow>(
        &mut self,
        sql: &str,
        values: sea_query_binder::SqlxValues,
    ) -> Result<Option<T>, sqlx::Error> {
        dispatch_tx!(self, tx => sqlx::query_as_with::<_, T, _>(sql, values).fetch_optional(&mut **tx).await)
    }

    pub async fn execute_query(
        &mut self,
        sql: &str,
        values: sea_query_binder::SqlxValues,
    ) -> Result<u64, sqlx::Error> {
        dispatch_tx!(self, tx => {
            let r = sqlx::query_with(sql, values).execute(&mut **tx).await?;
            Ok(r.rows_affected())
        })
    }
}

// ============================================================
// Query builder helper — builds SQL + bound values
// ============================================================

/// Build a SQL string and bound values using the runtime-detected backend.
pub fn build_sqlx<S: SqlxBinder>(stmt: &mut S) -> (String, sea_query_binder::SqlxValues) {
    match get_backend() {
        DbBackend::Sqlite => stmt.build_sqlx(sea_query::SqliteQueryBuilder),
        DbBackend::Postgres => stmt.build_sqlx(sea_query::PostgresQueryBuilder),
        DbBackend::Mysql => stmt.build_sqlx(sea_query::MysqlQueryBuilder),
    }
}

// ============================================================
// Embedded migrations — all backends compiled in, selected at runtime
// ============================================================

static MIGRATOR_SQLITE: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/sqlite");
static MIGRATOR_POSTGRES: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");
static MIGRATOR_MYSQL: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/mysql");

/// The embedded migration set for one backend.
///
/// Public so a test can migrate a backend the current process is *not* pinned
/// to — `get_backend()` is a process-wide `OnceLock`, which is exactly why
/// cross-backend checks (`tests/schema_parity.rs`, D10) cannot go through
/// [`run_migrations`].
pub fn migrator_for(backend: DbBackend) -> &'static sqlx::migrate::Migrator {
    match backend {
        DbBackend::Sqlite => &MIGRATOR_SQLITE,
        DbBackend::Postgres => &MIGRATOR_POSTGRES,
        DbBackend::Mysql => &MIGRATOR_MYSQL,
    }
}

fn migrator() -> &'static sqlx::migrate::Migrator {
    migrator_for(get_backend())
}

// ============================================================
// Pool initialization
// ============================================================

/// Initialize the database connection pool and run migrations.
/// Fresh in-memory SQLite pool with migrations applied — the shared
/// constructor for in-file repository tests.
#[cfg(test)]
pub(crate) async fn test_sqlite_pool() -> DbPool {
    init_pool(&crate::config::StorageConfig {
        url: "sqlite::memory:".to_string(),
        max_connections: 1,
        ..Default::default()
    })
    .await
    .expect("test pool")
}

pub async fn init_pool(config: &StorageConfig) -> Result<DbPool, OrionError> {
    let pool = init_pool_no_migrate(config).await?;
    run_migrations(&pool).await?;
    Ok(pool)
}

/// Initialize the pool the way the server does at boot.
///
/// With `auto_migrate = false` (the multi-replica deploy shape) a stale schema
/// is a hard startup error — a replica must never serve against pending
/// migrations. `orion-server migrate` is the deploy step that clears it, and
/// this refusal is what the Helm pre-upgrade Job and the compose `migrate`
/// service rely on. Kept out of `main` so it is reachable from tests (T5).
pub async fn init_pool_for_startup(config: &StorageConfig) -> Result<DbPool, OrionError> {
    if config.auto_migrate {
        return init_pool(config).await;
    }
    let pool = init_pool_no_migrate(config).await?;
    let pending = pending_migrations(&pool).await?;
    if !pending.is_empty() {
        return Err(OrionError::Config {
            message: format!(
                "{} pending migration(s) and storage.auto_migrate = false — \
                 run `orion-server migrate` first",
                pending.len()
            ),
        });
    }
    Ok(pool)
}

/// Initialize the database connection pool without running migrations.
pub async fn init_pool_no_migrate(config: &StorageConfig) -> Result<DbPool, OrionError> {
    let backend = detect_backend(&config.url)?;
    DB_BACKEND.set(backend).ok(); // Ignore if already set (e.g. tests)

    connect_with_retry(config, backend).await
}

// ============================================================
// Startup connect retry (D14)
// ============================================================

/// Backoff between startup connect attempts: 250 ms doubling to a 5 s ceiling.
///
/// Deliberately a constant rather than four more config knobs — the only
/// number an operator has a reason to tune is the total window
/// (`storage.connect_retry_secs`), which is what bounds the whole loop.
const CONNECT_RETRY_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Backoff before the attempt following `failures` failed attempts:
/// 250 ms, 500 ms, 1 s, 2 s, 4 s, then 5 s forever.
fn connect_backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(20);
    Duration::from_millis(250u64 << doublings).min(CONNECT_RETRY_BACKOFF_MAX)
}

/// Open the pool, retrying a failure until `storage.connect_retry_secs`
/// elapses (D14).
///
/// A hard exit on an unreachable database means every replica crash-loops for
/// the duration of a Postgres/MySQL failover, and the container restart
/// backoff outlives the outage; the readiness probe already keeps traffic off
/// a pod that has not finished booting, so failing fast bought nothing.
///
/// Two things stay fail-fast on purpose: SQLite (its failures — bad path, bad
/// permissions, corrupt file — do not heal on their own), and the *migration*
/// check in [`init_pool_for_startup`], which is about schema state rather than
/// reachability.
async fn connect_with_retry(
    config: &StorageConfig,
    backend: DbBackend,
) -> Result<DbPool, OrionError> {
    let window = match backend {
        DbBackend::Sqlite => Duration::ZERO,
        _ => Duration::from_secs(config.connect_retry_secs),
    };
    retry_within(window, |attempt| async move {
        if attempt > 1 {
            tracing::info!(attempt, backend = %backend, "Retrying database connection");
        }
        match backend {
            DbBackend::Sqlite => init_sqlite_pool(config).await,
            DbBackend::Postgres => init_postgres_pool(config).await,
            DbBackend::Mysql => init_mysql_pool(config).await,
        }
    })
    .await
}

/// Run `attempt` until it succeeds or the next backoff would land past
/// `window`, returning the last error.
///
/// Generic over the attempt so the retry *policy* is unit-testable without a
/// database outage — see `retry_within_*` in this module's tests.
async fn retry_within<T, F, Fut>(window: Duration, mut attempt: F) -> Result<T, OrionError>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T, OrionError>>,
{
    let started = tokio::time::Instant::now();
    let deadline = started + window;
    let mut failures = 0u32;
    loop {
        match attempt(failures + 1).await {
            Ok(value) => {
                if failures > 0 {
                    tracing::info!(
                        attempts = failures + 1,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "Database connection established after retrying"
                    );
                }
                return Ok(value);
            }
            Err(err) => {
                failures += 1;
                let backoff = connect_backoff(failures);
                if tokio::time::Instant::now() + backoff > deadline {
                    if failures > 1 {
                        tracing::error!(
                            attempts = failures,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "Giving up on the database connection — retry window exhausted"
                        );
                    }
                    return Err(err);
                }
                tracing::warn!(
                    attempt = failures,
                    retry_in_ms = backoff.as_millis() as u64,
                    error = %err,
                    "Database unavailable at startup; retrying \
                     (bounded by storage.connect_retry_secs)"
                );
                tokio::time::sleep(backoff).await;
            }
        }
    }
}

/// Run pending database migrations.
pub async fn run_migrations(pool: &DbPool) -> Result<(), OrionError> {
    let m = migrator();
    match pool {
        DbPool::Sqlite(p) => m.run(p).await,
        DbPool::Postgres(p) => m.run(p).await,
        DbPool::Mysql(p) => m.run(p).await,
    }
    .map_err(|e| OrionError::Internal {
        context: "Failed to run migrations".to_string(),
        source: Some(Box::new(e)),
    })
}

/// List pending migrations that have not yet been applied.
pub async fn pending_migrations(pool: &DbPool) -> Result<Vec<(i64, String)>, OrionError> {
    let applied: std::collections::HashSet<i64> = {
        let sql = "SELECT version FROM _sqlx_migrations ORDER BY version";
        let result: Result<Vec<i64>, _> = match pool {
            DbPool::Sqlite(p) => sqlx::query_scalar::<_, i64>(sql).fetch_all(p).await,
            DbPool::Postgres(p) => sqlx::query_scalar::<_, i64>(sql).fetch_all(p).await,
            DbPool::Mysql(p) => sqlx::query_scalar::<_, i64>(sql).fetch_all(p).await,
        };
        match result {
            Ok(versions) => versions.into_iter().collect(),
            Err(_) => std::collections::HashSet::new(),
        }
    };

    let pending: Vec<(i64, String)> = migrator()
        .iter()
        .filter(|m| !applied.contains(&m.version))
        .map(|m| (m.version, m.description.to_string()))
        .collect();

    Ok(pending)
}

// ============================================================
// Backend-specific pool initialization
// ============================================================

async fn init_sqlite_pool(config: &StorageConfig) -> Result<DbPool, OrionError> {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    let busy_timeout = config.busy_timeout_ms.to_string();
    let options = SqliteConnectOptions::from_str(&config.url)
        .map_err(|e| OrionError::Internal {
            context: "Invalid DB path".to_string(),
            source: Some(Box::new(e)),
        })?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        // D26: currently a no-op — the schema declares no foreign keys at all
        // (`grep REFERENCES migrations/` is empty), by design. See the module
        // doc comment. Kept on so that any FK added later is enforced from the
        // first connection rather than depending on someone remembering this.
        .pragma("foreign_keys", "ON")
        .pragma("busy_timeout", busy_timeout)
        .pragma("synchronous", "NORMAL")
        .pragma("cache_size", "-20000");

    let mut pool_opts = SqlitePoolOptions::new()
        .max_connections(config.max_connections)
        .min_connections(config.min_connections)
        .acquire_timeout(Duration::from_secs(config.acquire_timeout_secs));
    if config.idle_timeout_secs > 0 {
        pool_opts = pool_opts.idle_timeout(Duration::from_secs(config.idle_timeout_secs));
    }
    let pool = pool_opts
        .connect_with(options)
        .await
        .map_err(|e| OrionError::Internal {
            context: "Failed to connect to database".to_string(),
            source: Some(Box::new(e)),
        })?;
    Ok(DbPool::Sqlite(pool))
}

async fn init_postgres_pool(config: &StorageConfig) -> Result<DbPool, OrionError> {
    use sqlx::postgres::PgPoolOptions;

    let mut pool_opts = PgPoolOptions::new()
        .max_connections(config.max_connections)
        .min_connections(config.min_connections)
        .acquire_timeout(Duration::from_secs(config.acquire_timeout_secs));
    if config.idle_timeout_secs > 0 {
        pool_opts = pool_opts.idle_timeout(Duration::from_secs(config.idle_timeout_secs));
    }
    let pool = pool_opts
        .connect(&config.url)
        .await
        .map_err(|e| OrionError::Internal {
            context: "Failed to connect to database".to_string(),
            source: Some(Box::new(e)),
        })?;
    Ok(DbPool::Postgres(pool))
}

async fn init_mysql_pool(config: &StorageConfig) -> Result<DbPool, OrionError> {
    use sqlx::mysql::MySqlPoolOptions;

    let mut pool_opts = MySqlPoolOptions::new()
        .max_connections(config.max_connections)
        .min_connections(config.min_connections)
        .acquire_timeout(Duration::from_secs(config.acquire_timeout_secs));
    if config.idle_timeout_secs > 0 {
        pool_opts = pool_opts.idle_timeout(Duration::from_secs(config.idle_timeout_secs));
    }
    let pool = pool_opts
        .connect(&config.url)
        .await
        .map_err(|e| OrionError::Internal {
            context: "Failed to connect to database".to_string(),
            source: Some(Box::new(e)),
        })?;
    Ok(DbPool::Mysql(pool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_backend_sqlite() {
        assert_eq!(
            detect_backend("sqlite:orion.db").expect("test"),
            DbBackend::Sqlite
        );
        assert_eq!(
            detect_backend("sqlite::memory:").expect("test"),
            DbBackend::Sqlite
        );
    }

    #[test]
    fn test_detect_backend_postgres() {
        assert_eq!(
            detect_backend("postgres://user:pass@localhost/db").expect("test"),
            DbBackend::Postgres
        );
        assert_eq!(
            detect_backend("postgresql://user:pass@localhost/db").expect("test"),
            DbBackend::Postgres
        );
    }

    #[test]
    fn test_detect_backend_mysql() {
        assert_eq!(
            detect_backend("mysql://user:pass@localhost/db").expect("test"),
            DbBackend::Mysql
        );
    }

    #[test]
    fn test_detect_backend_unsupported() {
        assert!(detect_backend("mssql://localhost").is_err());
    }

    // ------------------------------------------------------------------
    // D14: startup connect retry. The policy is tested through
    // `retry_within` rather than a real outage — a paused clock makes the
    // backoff schedule observable without sleeping through it.
    // ------------------------------------------------------------------

    fn unavailable() -> OrionError {
        OrionError::Config {
            message: "connection refused".to_string(),
        }
    }

    #[test]
    fn connect_backoff_doubles_then_caps() {
        assert_eq!(connect_backoff(1), Duration::from_millis(250));
        assert_eq!(connect_backoff(2), Duration::from_millis(500));
        assert_eq!(connect_backoff(3), Duration::from_millis(1000));
        assert_eq!(connect_backoff(4), Duration::from_millis(2000));
        assert_eq!(connect_backoff(5), Duration::from_millis(4000));
        assert_eq!(connect_backoff(6), CONNECT_RETRY_BACKOFF_MAX);
        assert_eq!(connect_backoff(60), CONNECT_RETRY_BACKOFF_MAX);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_within_recovers_when_the_database_comes_back() {
        let mut seen = Vec::new();
        let started = tokio::time::Instant::now();
        let pool: Result<&str, OrionError> = retry_within(Duration::from_secs(60), |attempt| {
            seen.push(attempt);
            async move {
                if attempt < 4 {
                    Err(unavailable())
                } else {
                    Ok("pool")
                }
            }
        })
        .await;

        assert_eq!(pool.expect("test"), "pool");
        assert_eq!(seen, vec![1, 2, 3, 4], "every attempt must be numbered");
        // 250 + 500 + 1000 ms of backoff between the four attempts.
        assert_eq!(started.elapsed(), Duration::from_millis(1750));
    }

    #[tokio::test(start_paused = true)]
    async fn retry_within_gives_up_at_the_window_and_returns_the_last_error() {
        let mut attempts = 0u32;
        let started = tokio::time::Instant::now();
        let pool: Result<&str, OrionError> = retry_within(Duration::from_secs(2), |_| {
            attempts += 1;
            async { Err(unavailable()) }
        })
        .await;

        assert!(pool.is_err(), "an unreachable database must still fail");
        // 250 + 500 + 1000 ms fits in the 2 s window; the fourth backoff
        // (2000 ms) would land past it, so attempt 4 is where it stops.
        assert_eq!(attempts, 4);
        assert!(
            started.elapsed() <= Duration::from_secs(2),
            "the retry window is a hard bound, elapsed {:?}",
            started.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_within_zero_window_is_fail_fast() {
        let mut attempts = 0u32;
        let pool: Result<&str, OrionError> = retry_within(Duration::ZERO, |_| {
            attempts += 1;
            async { Err(unavailable()) }
        })
        .await;

        assert!(pool.is_err());
        assert_eq!(attempts, 1, "connect_retry_secs = 0 must not retry");
    }

    /// The wiring, not just the policy: a Postgres URL pointing at a closed
    /// port must be retried (SQLite must not). Uses a port that nothing is
    /// listening on, so no database — or outage — is needed.
    #[tokio::test]
    async fn postgres_connect_retries_but_sqlite_fails_fast() {
        // Bind and immediately drop, so the port is free and refuses.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("test");
        let port = listener.local_addr().expect("test").port();
        drop(listener);

        // One attempt costs ~acquire_timeout_secs (sqlx retries a refused
        // connection internally until that deadline), so a run longer than
        // two of them is only reachable by retrying.
        let config = StorageConfig {
            url: format!("postgres://orion:orion@127.0.0.1:{port}/orion"),
            acquire_timeout_secs: 1,
            connect_retry_secs: 2,
            min_connections: 0,
            max_connections: 1,
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let err = connect_with_retry(&config, DbBackend::Postgres).await;
        assert!(err.is_err(), "a closed port cannot yield a pool");
        assert!(
            started.elapsed() >= Duration::from_secs(2),
            "the first failure must be retried, not propagated immediately \
             (elapsed {:?})",
            started.elapsed()
        );

        // Same window, SQLite: not retried, because its failures are not
        // transient. A missing parent directory is never created for you.
        let missing = std::env::temp_dir().join("orion-d14-missing/nested/orion.db");
        let sqlite = StorageConfig {
            url: format!("sqlite:{}", missing.display()),
            connect_retry_secs: 30,
            min_connections: 0,
            max_connections: 1,
            ..Default::default()
        };
        let started = std::time::Instant::now();
        assert!(
            connect_with_retry(&sqlite, DbBackend::Sqlite)
                .await
                .is_err(),
            "an unwritable SQLite path must fail"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "SQLite must fail fast regardless of connect_retry_secs (elapsed {:?})",
            started.elapsed()
        );
    }
}
