use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::connector::ConnectorTarget;
use crate::connector::{
    ConnectorConfig, ConnectorRegistry, EsConnectorConfig, HttpOperationGates, OperationGates,
};
use crate::engine::{ErrorClass, HandlerError};
use crate::query::EntityRegistry;

/// Build the dialect's `EntityRegistry` for one `data_query` / `data_write`
/// call: parse the task's optional inline `schema`, then apply the connector's
/// operator-owned guards to it (F24).
///
/// Both handlers took the same two lines before this — `from_json`, else
/// `default()` — and neither consulted the connector at all. The order matters:
/// `require_schema` judges what the task actually declared, and the allowlist is
/// installed afterwards so it cannot be part of what is being judged.
pub fn build_entity_registry(
    schema: Option<&Value>,
    connector_config: &ConnectorConfig,
    connector_name: &str,
) -> Result<EntityRegistry, HandlerError> {
    let mut registry = match schema {
        Some(s) => EntityRegistry::from_json(s)?,
        None => EntityRegistry::default(),
    };
    if let Some(guards) = connector_config.dialect_guards() {
        if !guards.schema_is_sufficient(!registry.is_empty(), registry.is_identity_mode()) {
            return Err(connector_detail(format!(
                "connector '{connector_name}' requires a declared schema \
                 (dialect.require_schema): supply \"schema\" with an \"entities\" map \
                 and without \"unmapped\": \"identity\""
            )));
        }
        registry.restrict_to(&guards.allowed_entities);
    }
    Ok(registry)
}

/// A refusal whose text names connector topology: the caller is told the
/// request failed validation, and `detail` rides as the operator-only text.
fn connector_detail(detail: impl std::fmt::Display) -> HandlerError {
    HandlerError::from(crate::errors::service::connector_detail(detail))
}

/// Reject the call when the connector's operation gates disable `op` — the
/// per-connector en/disable switch for read / insert / update / delete /
/// upsert / raw_write (see [`OperationGates`]).
pub fn require_op_allowed(
    gates: &OperationGates,
    op: &str,
    connector_name: &str,
) -> Result<(), HandlerError> {
    require_op(gates.allows(op), op, connector_name)
}

/// [`require_op_allowed`] for the gates that are not the db/es set: the cache
/// (`read` / `write`) and Kafka (`publish`) gates each have their own struct,
/// because a `raw_write` flag on a cache would be a field nothing reads (F22e).
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
/// `method` (F22e). An empty list allows everything, which is what a connector
/// authored before the gate existed keeps meaning.
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

/// Build an Elasticsearch HTTP request with the connector's auth and timeout
/// applied. Shared by the `data_query` search path and the `data_write` write
/// path. Enforces the same SSRF pre-check as `execute_request` unless the
/// connector opts out via `allow_private_urls`.
pub async fn es_request(
    client: &reqwest::Client,
    es: &EsConnectorConfig,
    method: reqwest::Method,
    url: &str,
) -> Result<reqwest::RequestBuilder, HandlerError> {
    if !es.allow_private_urls
        && let Err(msg) = crate::validation::validate_url_not_private(url).await
    {
        return Err(to_exec_error(format!("SSRF protection: {msg}")));
    }

    let mut req = client.request(method, url);
    if let Some(auth) = &es.auth {
        req = super::http_common::apply_auth(req, auth);
    }
    if let Some(ms) = es.request_timeout_ms {
        req = req.timeout(Duration::from_millis(ms));
    }
    Ok(req)
}

/// Read an ES response body as JSON, enforcing the connector's
/// `max_response_size` (F12) — the same guard `execute_request` applies to
/// `http_call` responses. Without it a large `_search` result was buffered
/// wholesale.
pub async fn read_es_body(resp: reqwest::Response, max_size: usize) -> Result<Value, HandlerError> {
    // Bounded *while streaming* (`http_body`): a `_search` that answers with
    // no `Content-Length` used to be buffered whole and measured afterwards,
    // so the cap bounded what the workflow received and not what the node
    // held. `max_size` is per connector, so the exposure scaled with the
    // largest configured limit times the in-flight ES calls.
    let bytes = crate::http_body::read_bounded(resp, max_size)
        .await
        .map_err(|e| to_exec_error(format!("Elasticsearch response {e}")))?;
    serde_json::from_slice(&bytes).map_err(to_exec_error)
}

/// Send an ES request and parse its JSON body. Returns the status alongside so
/// callers can treat specific non-2xx statuses as semantic (`op_type=create`
/// treats 409 as the "conflict → do nothing" no-op).
pub async fn send_es(
    req: reqwest::RequestBuilder,
    max_response_size: usize,
) -> Result<(reqwest::StatusCode, Value), HandlerError> {
    // A send that fails never reached Elasticsearch: retryable, and what the
    // breaker counts, as a SQL, Redis or MongoDB connect failure is (F42).
    // `without_url`: a transport failure's `Display` carries the endpoint it
    // was given, and an ES connector URL can hold credentials in userinfo or
    // the query (#281). The connector name is the diagnostic here.
    let resp = req.send().await.map_err(|e| {
        if e.is_timeout() {
            HandlerError::new(ErrorClass::Timeout, e.without_url())
        } else {
            to_connect_error(e.without_url())
        }
    })?;
    let status = resp.status();
    let body: Value = read_es_body(resp, max_response_size).await?;
    Ok((status, body))
}

/// Redact secret-bearing keys and URL credentials/query secrets from an
/// upstream JSON error body and format a bounded preview capped at
/// `http_body::ERROR_BODY_PREVIEW_LIMIT` (512 bytes) (`TD-12`).
pub fn format_upstream_error_preview(body: &Value) -> String {
    let redacted = redact_error_json(body, 0);
    let raw = redacted.to_string();
    truncate_error_preview(&raw, crate::http_body::ERROR_BODY_PREVIEW_LIMIT)
}

fn redact_error_json(val: &Value, depth: usize) -> Value {
    if depth > 16 {
        return Value::String("...(nested)...".to_string());
    }
    match val {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                if crate::connector::is_secret_key(k) {
                    out.insert(k.clone(), Value::String(crate::connector::MASK.to_string()));
                } else {
                    out.insert(k.clone(), redact_error_json(v, depth + 1));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(32)
                .map(|v| redact_error_json(v, depth + 1))
                .collect(),
        ),
        Value::String(s) => Value::String(crate::connector::redact_url_secrets_or_raw(s)),
        other => other.clone(),
    }
}

fn truncate_error_preview(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... (truncated)", &s[..end])
}

/// Uniform error for a non-2xx Elasticsearch write response.
pub fn es_write_error(status: reqwest::StatusCode, body: &Value) -> HandlerError {
    let preview = format_upstream_error_preview(body);
    to_exec_error(format!("Elasticsearch write failed ({status}): {preview}"))
}

/// The backend was reached and the operation failed.
///
/// Kept as a named constructor because `.map_err(to_exec_error)` reads better
/// at a call site than a struct literal; the variant it becomes, and the retry
/// policy that variant implies, are decided once in [`HandlerError`]'s
/// `Into<DataflowError>`.
pub fn to_exec_error(e: impl std::fmt::Display) -> HandlerError {
    HandlerError::new(ErrorClass::Backend, e)
}

/// A failure to *reach* a backend — pool acquisition, connection setup, DNS.
///
/// `DataflowError::Io` rather than `FunctionExecution` because dataflow-rs
/// classifies the latter (with `source: None`) as **not retryable**, while
/// `Io` is. Before F42 every non-HTTP connector failure went through
/// [`to_exec_error`], so a dead Postgres, Redis or MongoDB was a non-retryable
/// 500 while the *identical* HTTP outage was a retryable `Io` — DLQ retry
/// policy diverged by backend for no principled reason.
///
/// Use for "could not connect"; keep [`to_exec_error`] for "connected, and the
/// query failed", which is genuinely not worth retrying.
pub fn to_connect_error(e: impl std::fmt::Display) -> HandlerError {
    HandlerError::new(ErrorClass::Connector, e)
}

/// A caller-fixable limit or shape problem, e.g. a result set over
/// `query.max_limit`.
///
/// `Validation` maps to 400 with the message preserved. Routing these through
/// [`to_exec_error`] made them 500 `ENGINE_ERROR` with the text replaced, so
/// deliberately helpful guidance — *"add a LIMIT to the query or raise the
/// cap"* — was sanitised away exactly when the caller needed it (F42).
pub fn to_limit_error(message: impl std::fmt::Display) -> HandlerError {
    HandlerError::new(ErrorClass::Limit, message)
}

/// Why a [`timed_query`] operation failed, decided where it is known.
///
/// The distinction matters: a limit the caller can fix is a 400 with its text
/// intact, a backend failure is a 500 with the text sanitised. Only the
/// operation itself can tell them apart, and it used to say so by **prefixing
/// its error string** with a marker the wrapper looked for and stripped
/// (F42) — control flow through a string, in the one place a value under a
/// `Result`'s `Err` was already available to carry it. A message that happened
/// to start with the marker would have been misread as a limit; a limit whose
/// text was reformatted anywhere in between would have silently become a 500.
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
    Integrity(crate::errors::IntegrityKind, String),
    /// The operation classified the failure itself and handed back a
    /// finished [`HandlerError`].
    ///
    /// Some operations are not one driver call: `data_write`'s MongoDB branch
    /// decides between a partial-bulk 207, a duplicate-key 409 and a plain
    /// backend failure *before* the timeout wrapper ever sees a result. Those
    /// used to be flattened with `.map_err(|e| e.to_string())` on the way in,
    /// which routed them through [`From<String>`] — unconditionally
    /// [`Self::Backend`] — so the classification was built and then discarded
    /// one line later, and the caller got a double-prefixed message with the
    /// wrong status. Passing the error through is the whole variant.
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
            use crate::errors::IntegrityKind as K;
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

/// A connector targets MongoDB when its connection string uses a `mongodb`
/// scheme; otherwise it is a SQL connector (dialect from the URL scheme).
pub use crate::connector::is_mongo_url as is_mongo;

/// Refuse a MongoDB connector for a handler that speaks SQL.
///
/// `require_db_connector` only checks the `ConnectorConfig` variant, and both
/// SQL and MongoDB connectors are `Db`. Without this, a `mongodb://` string
/// reached `AnyPool` and surfaced as an opaque driver error rather than a
/// validation one — the mirror of the `mongo_read` gap (proposal F29).
pub fn reject_mongo_connector(
    function: &str,
    connector_name: &str,
    db_config: &crate::connector::DbConnectorConfig,
) -> Result<(), HandlerError> {
    if is_mongo(&db_config.connection_string) {
        return Err(HandlerError::new(
            ErrorClass::CallerInput,
            format!(
                "{function} requires a SQL connector, but '{connector_name}' is a MongoDB \
             connector — use mongo_read or data_query for MongoDB"
            ),
        ));
    }
    Ok(())
}

/// Looks up a connector by name in the registry, returning a function-execution
/// error if not found.
pub async fn resolve_connector(
    registry: &ConnectorRegistry,
    name: &str,
) -> Result<Arc<ConnectorConfig>, HandlerError> {
    registry
        .get(name)
        .await
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
pub fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Turn a decode failure into a task error the author can act on (#309).
///
/// Classified as a *client* error, because it is one: the query asked for a
/// column whose type has no JSON form here, and no amount of retrying changes
/// that. Before native decoding this surfaced as an internal `500` plus an
/// `unhandled DataflowError variant` log line, which told the caller nothing
/// and the author less.
/// The bind-side twin of [`decode_failure`].
///
/// `Validation` for the same reason: a parameter whose value will not convert
/// to the type the query declares for it is an authoring problem, and a 400
/// naming the placeholder is worth more than a 500 with the reason in the log.
pub fn encode_failure(
    function: &str,
    e: crate::connector::sql_encode::EncodeError,
) -> HandlerError {
    HandlerError::new(ErrorClass::CallerInput, e.message(function))
}

pub fn decode_failure(
    function: &str,
    e: crate::connector::sql_decode::DecodeError,
) -> HandlerError {
    HandlerError::new(ErrorClass::CallerInput, e.message(function))
}

/// The query timeout a connector that declares no `query_timeout_ms` gets.
const DEFAULT_QUERY_TIMEOUT_MS: u64 = 30_000;

/// One wall-clock budget shared by every round trip of a single logical
/// operation (F11, F28).
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
    /// (`Ctx::deadline`): the earlier of the two bounds the legs (E10).
    ///
    /// When the connector's `query_timeout_ms` ends first, a leg it cuts
    /// short fails as it always has, `"{handler} query timed out after
    /// {ms}ms"`. When the run's deadline ends first, the budget arms no timer
    /// of its own: dataflow-rs 4 races every host call against the run's
    /// deadline and drops the call when it passes, ending the run `TimedOut`,
    /// as 1.12's channel timeout dropped the run. A timer of the budget's own
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
                // F42: a limit the caller can fix is a 400 with its text
                // intact, not a 500 with the guidance replaced. Both arms name
                // a class; which `DataflowError` that becomes is decided once.
                match e.into() {
                    QueryFailure::Limit(detail) => to_limit_error(detail),
                    // Built as a service error rather than through an
                    // `ErrorClass`: no class carries a code of its own, and
                    // the kind is what a workflow branches on. `HandlerError`
                    // keeps it as its original, so either engine gets back
                    // exactly this error.
                    //
                    // The handler name goes on the operator-only side; what
                    // the caller sees is the generic sentence the builder
                    // picks by kind.
                    QueryFailure::Integrity(integrity, text) => {
                        HandlerError::from(crate::errors::service::integrity(
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
mod tests {
    use super::*;
    use crate::connector::DialectGuards;

    /// An Elasticsearch node that streams a `_search` result without
    /// declaring a length cannot make this buffer past `max_response_size`.
    ///
    /// The cap used to run after `bytes()` had read the body to its end, so a
    /// hit set larger than the limit was held in full before being refused —
    /// and `max_response_size` is per connector, so the exposure was that
    /// number times the ES calls in flight.
    #[tokio::test]
    async fn a_flooding_es_response_is_cut_off_at_the_cap() {
        const CHUNK: usize = 64 * 1024;
        const CHUNKS: usize = 128; // 8 MiB against the 4 KiB cap below
        let (url, server) = crate::http_body::flood_server(CHUNK, CHUNKS).await;

        let response = reqwest::Client::new()
            .get(url)
            .send()
            .await
            .expect("response head");
        let err = read_es_body(response, 4096)
            .await
            .expect_err("must refuse an oversized hit set");

        let msg = err.to_string();
        assert!(
            msg.contains("Elasticsearch response") && msg.contains("4096"),
            "the refusal must name the connector's limit: {msg}"
        );
        crate::http_body::assert_stopped_early(
            server.await.expect("test server"),
            CHUNK * CHUNKS,
            "read_es_body",
        );
    }

    fn es_config(allow_private_urls: bool) -> EsConnectorConfig {
        EsConnectorConfig {
            max_response_size: 10 * 1024 * 1024,
            url: "http://127.0.0.1:9200".to_string(),
            auth: None,
            request_timeout_ms: None,
            allow_private_urls,
            operations: OperationGates::default(),
            dialect: DialectGuards::default(),
        }
    }

    #[tokio::test]
    async fn test_es_request_blocks_private_url() {
        let client = reqwest::Client::new();
        let result = es_request(
            &client,
            &es_config(false),
            reqwest::Method::POST,
            "http://127.0.0.1:9200/idx/_search",
        )
        .await;

        let err = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("SSRF protection"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn test_es_request_allows_private_url_when_opted_in() {
        let client = reqwest::Client::new();
        let result = es_request(
            &client,
            &es_config(true),
            reqwest::Method::POST,
            "http://127.0.0.1:9200/idx/_search",
        )
        .await;

        assert!(result.is_ok());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod error_taxonomy_tests {
    use super::*;

    /// An Elasticsearch outage is retryable, as every other connector's is.
    #[tokio::test]
    async fn an_unreachable_elasticsearch_is_retryable() {
        let client = reqwest::Client::new();
        let err = send_es(client.get("http://127.0.0.1:1/_search"), 1024)
            .await
            .expect_err("nothing listens on port 1");
        assert!(dataflow_rs::Error::from(err).is_retryable());
    }

    type DataflowError = dataflow_rs::Error;

    /// F42: a dead Postgres/Redis/Mongo used to be a **non-retryable** 500
    /// while the identical HTTP outage was a retryable `Io`, so DLQ retry
    /// policy diverged by backend for no principled reason.
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
    /// This used to be carried by prefixing the error string with a marker
    /// that `timed_query` looked for and stripped. The classification is a
    /// value under the `Err` now, so there is no marker to leak, no message
    /// that can be mistaken for one, and no reformatting in between that can
    /// lose it.
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

    /// A message that *looks* like the old marker is just a message now. The
    /// string-prefix scheme could not tell the difference.
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

    /// An unmarked failure keeps the old behaviour: a 500 naming the handler.
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
        use crate::errors::IntegrityKind;

        let err: DataflowError = crate::errors::service::integrity(
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

    #[test]
    fn upstream_error_preview_redacts_secrets_and_truncates_to_512_bytes() {
        let long_reason = "x".repeat(1000);
        let body = serde_json::json!({
            "error": {
                "type": "mapper_parsing_exception",
                "reason": long_reason,
                "api_key": "super-secret-api-key",
                "authorization": "Bearer secret-jwt-token",
                "endpoint": "https://admin:hunter2@es.internal:9200/orders/_doc?token=my-secret"
            }
        });
        let preview = format_upstream_error_preview(&body);
        assert!(
            !preview.contains("super-secret-api-key"),
            "secret key value must be redacted: {preview}"
        );
        assert!(
            !preview.contains("secret-jwt-token"),
            "authorization value must be redacted: {preview}"
        );
        assert!(
            !preview.contains("hunter2"),
            "URL userinfo password must be redacted: {preview}"
        );
        assert!(
            !preview.contains("my-secret"),
            "URL query secret must be redacted: {preview}"
        );
        assert!(
            preview.ends_with("... (truncated)"),
            "oversized body must be truncated: {preview}"
        );
        let prefix = preview.strip_suffix("... (truncated)").unwrap();
        assert!(
            prefix.len() <= crate::http_body::ERROR_BODY_PREVIEW_LIMIT,
            "preview prefix length {} exceeds limit {}",
            prefix.len(),
            crate::http_body::ERROR_BODY_PREVIEW_LIMIT
        );
    }
}
