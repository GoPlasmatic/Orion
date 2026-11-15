use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::connector::ConnectorTarget;
use crate::connector::{ConnectorConfig, ConnectorRegistry, HttpOperationGates, OperationGates};
use crate::query::EntityRegistry;
use crate::{ErrorClass, HandlerError};

/// Build the dialect's `EntityRegistry` for one `data_query` / `data_write`
/// call: parse the task's optional inline `schema`, then apply the connector's
/// operator-owned guards to it.
///
/// The order matters: `require_schema` judges what the task actually
/// declared, and the allowlist is installed afterwards so it cannot be part
/// of what is being judged.
pub(crate) fn build_entity_registry(
    schema: Option<&Value>,
    connector_config: &crate::connector::DbConnectorConfig,
    connector_name: &str,
) -> Result<Arc<EntityRegistry>, HandlerError> {
    // The registry is a pure function of the inline schema and the
    // connector's dialect guards, which are fixed for a generation. Parsing it
    // per execution would be a cost per call; a small bounded cache makes the
    // request path a hash and a lookup.
    let cache_key = {
        use sha2::Digest as _;
        let serialized = serde_json::to_vec(&(schema, &connector_config.dialect))
            .map_err(|e| HandlerError::caller_input(format!("schema does not serialize: {e}")))?;
        format!(
            "{connector_name}:{}",
            hex::encode(sha2::Sha256::digest(&serialized))
        )
    };
    if let Some((_, registry)) = registries()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&cache_key)
    {
        return Ok(registry.clone());
    }

    let mut registry = match schema {
        Some(s) => EntityRegistry::from_json(s)?,
        None => EntityRegistry::default(),
    };
    let guards = &connector_config.dialect;
    if !guards.schema_is_sufficient(!registry.is_empty(), registry.is_identity_mode()) {
        return Err(connector_detail(format!(
            "connector '{connector_name}' requires a declared schema \
             (dialect.require_schema): supply \"schema\" with an \"entities\" map \
             and without \"unmapped\": \"identity\""
        )));
    }
    registry.restrict_to(&guards.allowed_entities);
    let registry = Arc::new(registry);
    let mut cache = registries()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cache.len() >= MAX_CACHED_REGISTRIES
        && let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(k, _)| k.clone())
    {
        cache.remove(&oldest);
    }
    cache.insert(cache_key, (std::time::Instant::now(), registry.clone()));
    Ok(registry)
}

type RegistryCache =
    std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Arc<EntityRegistry>)>>;

fn registries() -> &'static RegistryCache {
    static REGISTRIES: std::sync::OnceLock<RegistryCache> = std::sync::OnceLock::new();
    REGISTRIES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

const MAX_CACHED_REGISTRIES: usize = 64;

/// A refusal whose text names connector topology: the caller is told the
/// request failed validation, and `detail` rides as the operator-only text.
fn connector_detail(detail: impl std::fmt::Display) -> HandlerError {
    HandlerError::from(crate::service::connector_detail(detail))
}

/// Reject the call when the connector's operation gates disable `op` — the
/// per-connector en/disable switch for read / insert / update / delete /
/// upsert / raw_write (see [`OperationGates`]).
pub(crate) fn require_op_allowed(
    gates: &OperationGates,
    op: &str,
    connector_name: &str,
) -> Result<(), HandlerError> {
    require_op(gates.allows(op), op, connector_name)
}

/// [`require_op_allowed`] for the gates that are not the db set: the cache
/// (`read` / `write`) and storage (`presign_get` / `presign_put` / `head`)
/// gates each have their own struct,
/// because a `raw_write` flag on a cache would be a field nothing reads.
/// The refusal is worded identically whatever the connector type.
pub fn require_op(allowed: bool, op: &str, connector_name: &str) -> Result<(), HandlerError> {
    if !allowed {
        return Err(connector_detail(format!(
            "operation '{op}' is disabled on connector '{connector_name}'"
        )));
    }
    Ok(())
}

/// Reject the call when an HTTP connector's method allow-list excludes
/// `method`. An empty list allows everything, so a connector that sets no
/// allow-list keeps every method available.
pub fn require_method_allowed(
    gates: &HttpOperationGates,
    method: &str,
    connector_name: &str,
) -> Result<(), HandlerError> {
    if !gates.allows_method(method) {
        return Err(connector_detail(format!(
            "HTTP method '{method}' is not allowed on connector \
             '{connector_name}' (allowed: {})",
            gates.methods.join(", ")
        )));
    }
    Ok(())
}

/// The backend was reached and the operation failed.
///
/// Kept as a named constructor because `.map_err(to_exec_error)` reads better
/// at a call site than a struct literal; the code it becomes, and its
/// retryability, are decided once, by [`HandlerError`]'s class.
pub(crate) fn to_exec_error(e: impl std::fmt::Display) -> HandlerError {
    HandlerError::new(ErrorClass::Backend, e)
}

/// A failure to *reach* a backend — pool acquisition, connection setup, DNS.
///
/// [`ErrorClass::Connector`] rather than [`ErrorClass::Backend`] because the
/// first is retryable and the second is not. A dead Postgres or MySQL is the
/// *identical* outage to an unreachable HTTP host, and DLQ retry policy has no
/// principled reason to diverge by backend.
///
/// Use for "could not connect"; keep [`to_exec_error`] for "connected, and the
/// query failed", which is genuinely not worth retrying.
pub(crate) fn to_connect_error(e: impl std::fmt::Display) -> HandlerError {
    HandlerError::new(ErrorClass::Connector, e)
}

/// A caller-fixable limit or shape problem, e.g. a result set over
/// `query.max_limit`.
///
/// A 400 with the message preserved. Through [`to_exec_error`] it would be a
/// 500 with the text replaced, so deliberately helpful guidance — *"add a
/// LIMIT to the query or raise the cap"* — would be sanitised away exactly
/// when the caller needs it.
pub(crate) fn to_limit_error(message: impl std::fmt::Display) -> HandlerError {
    HandlerError::new(ErrorClass::Limit, message)
}

/// Why a [`timed_query`] operation failed, decided where it is known.
///
/// The distinction matters: a limit the caller can fix is a 400 with its text
/// intact, a backend failure is a 500 with the text sanitised. Only the
/// operation itself can tell them apart, so it says so in the value under the
/// `Err`, not in the text: with a marker prefixed to the error string, a
/// message that happened to start with the marker would be misread as a
/// limit, and a limit whose text was reformatted anywhere in between would
/// silently become a 500.
///
/// `From<String>` maps to [`Self::Backend`], so an operation with nothing to
/// distinguish keeps returning `Result<_, String>` and reads the same.
#[derive(Debug)]
pub enum QueryFailure {
    /// The backend was reached and the operation failed. A 500 with the text
    /// replaced — a driver error names hosts, tables and sometimes values.
    Backend(String),
    /// A limit the caller can fix — a result set over `query.max_limit`, say.
    /// A 400 with the message intact, because the message is the guidance:
    /// *"add a LIMIT to the query or raise the cap"* is useless once
    /// sanitised.
    Limit(String),
    /// The backend was reached and refused the statement against a rule the
    /// schema declares — a unique index, a foreign key, a NOT NULL, a CHECK.
    ///
    /// Its own variant because it is none of the others: not the operator's
    /// problem (unlike [`Self::Backend`]), not transient (unlike a connect
    /// failure), and fixable by the caller — a duplicate submission is a
    /// routine 409, not a 500. The `String` is the driver's text and reaches
    /// the operator-only `detail` alone, never a caller.
    Integrity(crate::service::IntegrityKind, String),
    /// The operation classified the failure itself and handed back a
    /// finished [`HandlerError`].
    ///
    /// Some operations are not one driver call: `data_write` classifies a
    /// parameter that will not bind (a 400 naming the placeholder) *before*
    /// the timeout wrapper ever sees a result. Flattened with
    /// `.map_err(|e| e.to_string())` on the way in, it would route through
    /// [`From<String>`] — unconditionally [`Self::Backend`] — so the
    /// classification would be built and then discarded one line later.
    /// Passing the error through is the whole variant.
    Classified(HandlerError),
}

/// An error that already knows what it is keeps its own classification.
impl From<HandlerError> for QueryFailure {
    fn from(e: HandlerError) -> Self {
        Self::Classified(e)
    }
}

impl From<String> for QueryFailure {
    fn from(message: String) -> Self {
        Self::Backend(message)
    }
}

/// A driver error is a backend failure unless the driver says the server
/// refused it against a declared constraint. Spelled out so `?` keeps working
/// inside an operation, which is the idiom every one of them uses.
impl From<sqlx::Error> for QueryFailure {
    fn from(e: sqlx::Error) -> Self {
        // `kind()` is implemented by all three drivers over their own SQLSTATE
        // / errno tables, so this stays one match rather than becoming a
        // per-backend table here. It also survives `sqlx::Any`, which
        // `data_query` and `data_write` run on: `Error::Database` carries the
        // *concrete* driver's `DatabaseError` and the `any` module never
        // re-boxes it.
        //
        // Matched rather than asked through `is_unique_violation()` and
        // friends: those are thin wrappers over `kind()` and there is no
        // predicate for a NOT NULL violation, which every driver reports and
        // which is as caller-fixable as the other three.
        if let Some(db) = e.as_database_error() {
            use crate::service::IntegrityKind as K;
            let integrity = match db.kind() {
                sqlx::error::ErrorKind::UniqueViolation => Some(K::Unique),
                sqlx::error::ErrorKind::ForeignKeyViolation => Some(K::ForeignKey),
                sqlx::error::ErrorKind::NotNullViolation => Some(K::NotNull),
                sqlx::error::ErrorKind::CheckViolation => Some(K::Check),
                // `ErrorKind` is `#[non_exhaustive]`. A kind this build does
                // not know stays a backend failure — the conservative reading
                // of a classification whose meaning it cannot see.
                _ => None,
            };
            if let Some(integrity) = integrity {
                return Self::Integrity(integrity, e.to_string());
            }
        }
        Self::Backend(e.to_string())
    }
}

impl std::fmt::Display for QueryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(m) | Self::Limit(m) | Self::Integrity(_, m) => f.write_str(m),
            Self::Classified(e) => write!(f, "{e}"),
        }
    }
}

/// Looks up a connector by name in the registry, returning a function-execution
/// error if not found.
pub async fn resolve_connector(
    registry: &ConnectorRegistry,
    name: &str,
) -> Result<Arc<ConnectorConfig>, HandlerError> {
    registry
        .get(name)
        .ok_or_else(|| to_exec_error(format!("Connector '{name}' not found")))
}

/// Borrow the typed config a handler needs, or say which type the connector
/// actually is.
///
/// One generic where there were six functions — `require_db_connector`,
/// `require_http_connector`, and four more — that differed only in the variant
/// they matched and the noun they printed. Both now come from the
/// [`ConnectorTarget`] impl, so an eighth connector type costs nothing here,
/// and the type parameter is what stops a handler asking for one kind and
/// binding another.
///
/// The bound is [`ConnectorTarget`] rather than `ConnectorKind` so that the two
/// handlers accepting more than one variant — the portable dialect's
/// `data_query` and `data_write`, via `DataBackend` — produce their wrong-type
/// refusal here with every other handler's, instead of after resolution in
/// their own words.
pub fn require_connector<'a, K: ConnectorTarget>(
    config: &'a ConnectorConfig,
    name: &str,
) -> Result<&'a K::Config, HandlerError> {
    K::extract(config)
        .ok_or_else(|| connector_detail(format!("Connector '{name}' is not {}", K::noun())))
}

/// A resolved connector that has already been checked against a handler's
/// target type.
///
/// `with_connector` hands one to the work, so a handler reads its typed config
/// without writing the resolve-and-check prologue itself. [`Self::config`]
/// re-extracts the variant, which the constructor proved is there.
pub struct Typed<K: ConnectorTarget> {
    config: Arc<ConnectorConfig>,
    _kind: std::marker::PhantomData<K>,
}

impl<K: ConnectorTarget> Typed<K> {
    pub fn config(&self) -> &K::Config {
        K::extract(&self.config).expect("with_connector checked the variant")
    }
}

/// [`crate::kit::guarded`] plus the prologue every connector function writes:
/// resolve the named connector, require it to be this handler's type, then run
/// `work` on it.
///
/// The resolve and the type check stay inside the guarded future, so their
/// failures are counted and can trip the breaker exactly as the work's do.
pub async fn with_connector<K, T, F, Fut>(
    name: &'static str,
    registry: &ConnectorRegistry,
    connector: &str,
    channel: &str,
    work: F,
) -> dataflow_rs::Result<T>
where
    K: ConnectorTarget,
    F: FnOnce(Typed<K>) -> Fut,
    Fut: std::future::Future<Output = dataflow_rs::Result<T>>,
{
    crate::kit::guarded(name, registry, connector, channel, async move {
        let config = resolve_connector(registry, connector).await?;
        require_connector::<K>(&config, connector)?;
        work(Typed {
            config,
            _kind: std::marker::PhantomData,
        })
        .await
    })
    .await
}

/// `"900"`-less duration spelling: `<n>` followed by one of `s m h d`.
pub fn parse_duration_secs(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (number, unit) = s.split_at(s.len().saturating_sub(1));
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => {
            return Err(format!(
                "'{s}' is not a duration — \"<n>s\", \"<n>m\", \"<n>h\" or \"<n>d\""
            ));
        }
    };
    let n: u64 = number.parse().map_err(|_| {
        format!("'{s}' is not a duration — the part before the unit must be a number")
    })?;
    n.checked_mul(multiplier)
        .ok_or_else(|| format!("'{s}' overflows"))
}

/// Name a JSON value's type for error messages.
pub(crate) fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The bind-side twin of [`decode_failure`].
///
/// `Validation` for the same reason: a parameter whose value will not convert
/// to the type the query declares for it is an authoring problem, and a 400
/// naming the placeholder is worth more than a 500 with the reason in the log.
pub(crate) fn encode_failure(
    function: &str,
    e: crate::connector::sql_encode::EncodeError,
) -> HandlerError {
    HandlerError::new(ErrorClass::CallerInput, e.message(function))
}

/// Turn a decode failure into a task error the author can act on.
///
/// Classified as a *client* error, because it is one: the query asked for a
/// column whose type has no JSON form here, and no amount of retrying changes
/// that. An internal `500` would tell the caller nothing and the author less.
pub(crate) fn decode_failure(
    function: &str,
    e: crate::connector::sql_decode::DecodeError,
) -> HandlerError {
    HandlerError::new(ErrorClass::CallerInput, e.message(function))
}

/// The query timeout a connector that declares no `query_timeout_ms` gets.
const DEFAULT_QUERY_TIMEOUT_MS: u64 = 30_000;

/// One wall-clock budget shared by every round trip of a single logical
/// operation.
///
/// `timed_query` bounds *one* future. That is the whole story for a handler
/// that issues one statement, but a `data_write` that opens a transaction
/// issues three round trips — acquire + `BEGIN`, the statement, `COMMIT` — and
/// giving each its own `query_timeout_ms` would silently multiply the
/// connector's configured bound. A budget is started once and every leg runs
/// against the same deadline, so `query_timeout_ms` keeps meaning what the
/// connector's owner set it to.
#[derive(Debug, Clone, Copy)]
pub struct QueryBudget {
    /// When the legs time out: `None` when the run's own deadline comes
    /// first, which the engine enforces itself (see [`Self::within`]).
    deadline: Option<tokio::time::Instant>,
    total_ms: u64,
}

impl QueryBudget {
    /// Start a budget of `timeout_ms` (or the default) from now.
    pub fn start(timeout_ms: Option<u64>) -> Self {
        Self::within(timeout_ms, None)
    }

    /// [`Self::start`], inside the deadline of the run the call belongs to
    /// (`Ctx::deadline`): the earlier of the two bounds the legs.
    ///
    /// When the connector's `query_timeout_ms` ends first, a leg it cuts
    /// short fails as it always has, `"{handler} query timed out after
    /// {ms}ms"`. When the run's deadline ends first, the budget arms no timer
    /// of its own: dataflow-rs 4 races every host call against the run's
    /// deadline and drops the call when it passes, ending the run `TimedOut`.
    /// A timer of the budget's own
    /// at the same instant would win that race (the engine polls the call
    /// first) and turn the run's timeout into the task's error.
    pub fn within(timeout_ms: Option<u64>, run: Option<dataflow_rs::run::Instant>) -> Self {
        let total_ms = timeout_ms.unwrap_or(DEFAULT_QUERY_TIMEOUT_MS);
        let own = tokio::time::Instant::now() + Duration::from_millis(total_ms);
        let deadline = match run.map(tokio::time::Instant::from_std) {
            Some(run) if run <= own => None,
            _ => Some(own),
        };
        Self { deadline, total_ms }
    }

    /// Run one leg of the operation against the shared deadline.
    pub async fn run<F, T, E>(&self, handler_name: &str, operation: F) -> Result<T, HandlerError>
    where
        F: std::future::Future<Output = Result<T, E>>,
        E: Into<QueryFailure>,
    {
        let total_ms = self.total_ms;
        let finished = match self.deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, operation).await,
            None => Ok(operation.await),
        };
        finished
            .map_err(|_| {
                HandlerError::new(
                    ErrorClass::Timeout,
                    format!("{handler_name} query timed out after {total_ms}ms"),
                )
            })?
            .map_err(|e| {
                // A limit the caller can fix is a 400 with its text
                // intact, not a 500 with the guidance replaced. Both arms name
                // a class; which engine error that becomes is decided once.
                match e.into() {
                    QueryFailure::Limit(detail) => to_limit_error(detail),
                    // Built as a service error rather than through an
                    // `ErrorClass`: no class carries a code of its own, and
                    // the kind is what a workflow branches on. `HandlerError`
                    // keeps it as its original, so the engine gets back
                    // exactly this error.
                    //
                    // The handler name goes on the operator-only side; what
                    // the caller sees is the generic sentence the builder
                    // picks by kind.
                    QueryFailure::Integrity(integrity, text) => {
                        HandlerError::from(crate::service::integrity(
                            integrity,
                            format!("{handler_name} query failed: {text}"),
                        ))
                    }
                    QueryFailure::Backend(text) => {
                        to_exec_error(format!("{handler_name} query failed: {text}"))
                    }
                    // Already classified by the operation, which knew more than
                    // this wrapper can. Re-wrapping would only re-prefix a
                    // message that already names its own failure.
                    QueryFailure::Classified(e) => e,
                }
            })
    }
}

/// Acquire a pooled connection as one leg of `budget`.
///
/// Explicit, rather than handing sqlx the pool and letting it acquire per
/// statement, because PostgreSQL caches a prepared statement's parameter types
/// **per connection** — so learning what the server declared and then binding
/// against it only means anything if both happen on the same one.
///
/// The classification is the part worth stating. A failure here is "could not
/// connect", which is [`ErrorClass::Connector`]: retryable, and what the
/// circuit breaker counts. Left to `QueryFailure`'s blanket `From<sqlx::Error>`
/// it would arrive as a backend failure instead — not retryable, and invisible
/// to the breaker, so pool exhaustion would quietly stop opening it.
pub async fn acquire_conn<DB>(
    budget: &QueryBudget,
    handler_name: &str,
    pool: &sqlx::Pool<DB>,
) -> Result<sqlx::pool::PoolConnection<DB>, HandlerError>
where
    DB: sqlx::Database,
{
    budget
        .run(handler_name, async {
            pool.acquire()
                .await
                .map_err(|e| QueryFailure::Classified(to_connect_error(e)))
        })
        .await
}

/// Bind `$sql` for execution on an already-open connection, and settle which
/// arguments to send (M-04).
///
/// `acquire_conn`, the typed-arguments attempt on that same connection (a
/// prepared statement's parameter types are read per connection, so the
/// declaration and the binding have to happen on one), and then either those
/// arguments or the caller's value-shaped fallback — the step where sqlx 0.9's
/// `AssertSqlSafe`, and PostgreSQL's cache-statement rule, have to stay
/// consistent. Written once here rather than six times.
///
/// A macro rather than a function because `$typed_args` is one `async fn` per
/// driver, chosen by [`crate::connector::pool_cache::dispatch_sql_pool`]: each
/// returns its own borrow-carrying future, which no single `FnOnce` bound can
/// name. It expands where the arm's concrete `typed_args` is in scope, which
/// is the same reason the dispatcher itself is a macro.
///
/// `$fallback` is called with `cache` — whether the prepared statement may be
/// kept. It is `false` only for PostgreSQL, where leaving a value-shaped bind
/// cached is the very defect the typed path removes; only the caller knows
/// which driver its fallback belongs to. `Typed` is always cacheable: its
/// types are the ones the server inferred, so re-preparing under them declares
/// the same statement again.
macro_rules! prepare_statement {
    ($budget:expr, $name:expr, $conn:expr, $sql:expr, $scalars:expr, $typed_args:ident, $fallback:expr) => {{
        // A leg of the same budget: on a connection that has already seen this
        // SQL the prepare is a cache hit and no round trip at all, but the
        // first time through it is a real one.
        let bound = $budget
            .run($name, async {
                $typed_args($conn, $sql, $scalars).await.map_err(|e| {
                    $crate::functions::connector_helpers::QueryFailure::Classified(
                        $crate::functions::connector_helpers::encode_failure($name, e),
                    )
                })
            })
            .await?;
        // `AssertSqlSafe` states what these handlers are: the raw-SQL escape
        // hatch, whose statement is authored in the workflow rather than
        // assembled here. sqlx 0.9 asks the caller to own that, and the answer
        // is the same as it was before it asked — the text comes from the
        // definition, the author-supplied *values* travel as bind parameters
        // beside it, and a definition reaches the runtime only through the
        // admin API. Authors who want values checked use the portable dialect
        // (`data_query`/`data_write`) instead.
        match bound {
            $crate::connector::sql_encode::Bound::Typed(args) => {
                sqlx::query_with(sqlx::AssertSqlSafe($sql), args)
            }
            $crate::connector::sql_encode::Bound::Fallback { cache } => ($fallback)(cache),
        }
    }};
}
pub(crate) use prepare_statement;

/// [`prepare_statement!`] on a connection acquired as one leg of the budget:
/// the pair, so the caller executes on the connection the statement was
/// prepared on.
macro_rules! bound_statement {
    ($budget:expr, $name:expr, $pool:expr, $sql:expr, $scalars:expr, $typed_args:ident, $fallback:expr) => {{
        let mut conn =
            $crate::functions::connector_helpers::acquire_conn(&$budget, $name, $pool).await?;
        let statement = $crate::functions::connector_helpers::prepare_statement!(
            $budget,
            $name,
            &mut conn,
            $sql,
            $scalars,
            $typed_args,
            $fallback
        );
        (conn, statement)
    }};
}
pub(crate) use bound_statement;

/// Execute an async operation with a timeout, classifying a timeout as
/// [`ErrorClass::Timeout`] and a failure as [`ErrorClass::Backend`].
/// Consolidates the repeated timeout + error-mapping pattern
/// in the SQL handler functions. A single-round-trip [`QueryBudget`].
pub async fn timed_query<F, T, E>(
    timeout_ms: Option<u64>,
    handler_name: &str,
    operation: F,
) -> Result<T, HandlerError>
where
    F: std::future::Future<Output = Result<T, E>>,
    E: Into<QueryFailure>,
{
    QueryBudget::start(timeout_ms)
        .run(handler_name, operation)
        .await
}

#[cfg(test)]
mod error_taxonomy_tests {
    use super::*;

    type DataflowError = dataflow_rs::Error;

    /// F42: a dead Postgres/MySQL is as retryable as the identical HTTP
    /// outage; DLQ retry policy has no principled reason to diverge by
    /// backend.
    ///
    /// The constructors name a class, and `HandlerError`'s conversion picks
    /// the code and the retryability. Asserted
    /// end-to-end — through the conversion — because the retryability of the
    /// error that actually reaches the retry loop is the property that matters.
    #[test]
    fn a_failure_to_connect_is_retryable_but_a_failed_query_is_not() {
        assert!(
            DataflowError::from(to_connect_error("connection refused")).is_retryable(),
            "an unreachable backend must be retryable, like the HTTP path"
        );
        assert!(
            !DataflowError::from(to_exec_error("syntax error at or near \"SELCT\"")).is_retryable(),
            "a query the backend rejected is not worth retrying"
        );
    }

    /// A caller-fixable limit is a 400 that keeps its guidance, not a 500 that
    /// loses it to sanitisation.
    #[test]
    fn a_limit_error_is_validation_not_execution() {
        let err = DataflowError::from(to_limit_error(
            "result exceeds query.max_limit — add a LIMIT",
        ));
        assert_eq!(err.code(), "VALIDATION_ERROR", "{err:?}");
        assert!(!err.is_retryable(), "a limit does not fix itself on retry");
    }

    /// A limit the operation classified reaches the caller as a 400 with its
    /// message intact — the guidance *is* the message.
    ///
    /// The classification is a value under the `Err`, not a marker on the
    /// error string, so there is no marker to leak, no message that can be
    /// mistaken for one, and no reformatting in between that can lose it.
    #[tokio::test]
    async fn timed_query_reports_a_classified_limit_as_validation() {
        let err = DataflowError::from(
            timed_query(Some(1_000), "db_read", async {
                Err::<(), _>(QueryFailure::Limit(
                    "too many rows — add a LIMIT".to_string(),
                ))
            })
            .await
            .expect_err("the operation failed"),
        );
        assert_eq!(err.code(), "VALIDATION_ERROR", "{err:?}");
        assert_eq!(
            err.message(),
            "Validation error: too many rows — add a LIMIT",
            "the message intact under its label"
        );
    }

    /// A message that *looks* like a limit marker is just a message: the
    /// classification is read from the value, never from the text.
    #[tokio::test]
    async fn a_backend_failure_is_never_reclassified_by_its_text() {
        let err = DataflowError::from(
            timed_query(Some(1_000), "db_read", async {
                Err::<(), String>("orion.limit: not a limit, just text".to_string())
            })
            .await
            .expect_err("the operation failed"),
        );
        assert_eq!(
            err.code(),
            "FUNCTION_ERROR",
            "text cannot promote a backend failure to a limit: {err:?}"
        );
    }

    /// E10: a budget inside a run's deadline that comes first arms no timer
    /// of its own, and a leg runs on past that deadline, which the engine's
    /// race enforces; one whose `query_timeout_ms` comes first keeps its
    /// own, as with no run deadline at all.
    #[tokio::test]
    async fn a_budget_inside_an_earlier_run_deadline_arms_no_timer_of_its_own() {
        let soon = dataflow_rs::run::Instant::now() + Duration::from_millis(10);
        let late = dataflow_rs::run::Instant::now() + Duration::from_secs(3_600);
        assert!(
            QueryBudget::within(Some(60_000), Some(soon))
                .deadline
                .is_none()
        );
        assert!(QueryBudget::within(Some(10), Some(late)).deadline.is_some());
        assert!(QueryBudget::within(Some(10), None).deadline.is_some());
        assert!(QueryBudget::start(None).deadline.is_some());

        let leg = QueryBudget::within(Some(60_000), Some(soon))
            .run("db_read", async {
                tokio::time::sleep(Duration::from_millis(40)).await;
                Ok::<_, String>(7)
            })
            .await;
        assert_eq!(
            leg.ok(),
            Some(7),
            "the run's deadline is not the budget's to enforce"
        );

        let err = DataflowError::from(
            QueryBudget::within(Some(10), Some(late))
                .run("db_read", std::future::pending::<Result<(), String>>())
                .await
                .expect_err("the query timeout ends the leg"),
        );
        assert_eq!(
            err.to_string(),
            "Timeout error: db_read query timed out after 10ms"
        );
    }

    /// A plain `String` failure is a backend failure: a 500 naming the handler.
    #[tokio::test]
    async fn timed_query_leaves_an_ordinary_failure_as_execution() {
        let err = DataflowError::from(
            timed_query(Some(1_000), "db_read", async {
                Err::<(), String>("connection reset".to_string())
            })
            .await
            .expect_err("the operation failed"),
        );
        assert_eq!(err.code(), "FUNCTION_ERROR", "{err:?}");
    }

    /// An `sqlx::Error` that never reached a database has no classification to
    /// read, so it stays a backend failure.
    ///
    /// The arm this guards is the one an integrity check could most easily
    /// break: `as_database_error()` returning `None` must fall through, not
    /// panic and not guess.
    #[test]
    fn a_driver_error_with_no_database_error_stays_backend() {
        let failure = QueryFailure::from(sqlx::Error::RowNotFound);
        assert!(
            matches!(failure, QueryFailure::Backend(_)),
            "expected Backend, got {failure:?}"
        );
    }

    /// The classification survives the trip to an engine error and back —
    /// the property `QueryBudget::run` depends on, since it converts through
    /// `HandlerError` on the way out.
    #[test]
    fn an_integrity_failure_keeps_its_kind_through_the_conversion() {
        use crate::service::IntegrityKind;

        let err: DataflowError = crate::service::integrity(
            IntegrityKind::Unique,
            "db_write query failed: UNIQUE constraint failed: models.id",
        );
        let back: DataflowError = HandlerError::from(err).into();

        assert_eq!(
            back.code(),
            crate::errors::kind::INTEGRITY_UNIQUE,
            "the service code is what a workflow branches on: {back:?}"
        );
        assert!(
            !back.is_retryable(),
            "an integrity failure must not be retried, or the circuit breaker \
             counts it: {back:?}"
        );
        assert_eq!(
            back.message(),
            "The request conflicts with an existing record",
            "Display is the caller-safe half and must not carry the driver text"
        );
    }
}
