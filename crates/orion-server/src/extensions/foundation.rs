//! Community Edition foundation Direct Rust connector backends (`V2-14.2`–`V2-14.4`).
//!
//! Implements the four foundational Direct Rust (`ImplementationKind::Rust`)
//! connector providers maintained in the public Community repository per
//! `DEC-005`:
//! - [`HttpProviderBackend`] (`"org.orion.http"`)
//! - [`PostgresProviderBackend`] (`"org.orion.postgresql"`)
//! - [`SqliteProviderBackend`] (`"org.orion.sqlite"`) — enforces approved-root
//!   path sandboxing and rejects path traversal or Orion's own config DB (`V2-14.3`)
//! - [`MemoryCacheProviderBackend`] (`"org.orion.memory-cache"`) — bounded,
//!   TTL-aware, connection-partitioned in-memory cache (`V2-14.4`)

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::context::AttemptContext;
use super::contract::{
    ConnectorError, ConnectorOutput, EffectStatus, EffectiveConnectionIdentity, ImplementationKind,
    codes,
};
use super::provider_backend::{
    BoxProviderFuture, ProviderBackend, ProviderRegistry, SourceBatchItem, SourceConnectorFacet,
    SourcePollBatch, SourceSettlement,
};
use super::resource_pool::{
    BoxPoolFuture, ConnectionPoolCache, ResourcePoolRegistry, VersionedSessionPool,
};

/// Canonical driver IDs for the four Community foundation Direct Rust providers.
pub const DRIVER_HTTP: &str = "org.orion.http";
pub const DRIVER_POSTGRESQL: &str = "org.orion.postgresql";
pub const DRIVER_SQLITE: &str = "org.orion.sqlite";
pub const DRIVER_MEMORY_CACHE: &str = "org.orion.memory-cache";

/// All Community foundation Direct Rust driver identifiers.
pub const COMMUNITY_RUST_DRIVERS: &[&str] = &[
    DRIVER_HTTP,
    DRIVER_POSTGRESQL,
    DRIVER_SQLITE,
    DRIVER_MEMORY_CACHE,
];

/// The contract generation the built-in drivers' identities name. It moves
/// only when a driver's operations change incompatibly, never with the
/// build, so a connection revision naming a built-in provider stays valid
/// across upgrades.
const BUILTIN_PROVIDER_CONTRACT: u32 = 1;

/// The package digest a built-in driver answers to: a connection revision
/// names a provider by digest, and a built-in driver has no uploaded
/// package, so this stable digest of its id and contract stands in for one.
/// `None` for an id this build does not ship.
pub fn builtin_provider_digest(driver_id: &str) -> Option<String> {
    COMMUNITY_RUST_DRIVERS
        .contains(&driver_id)
        .then(|| builtin_digest_of(driver_id))
}

/// The built-in driver whose [`builtin_provider_digest`] is `digest`.
pub fn builtin_driver(digest: &str) -> Option<&'static str> {
    COMMUNITY_RUST_DRIVERS
        .iter()
        .copied()
        .find(|driver| builtin_digest_of(driver) == digest)
}

fn builtin_digest_of(driver_id: &str) -> String {
    super::package_identity::sha256_digest(
        format!("orion-builtin-provider:{driver_id}@{BUILTIN_PROVIDER_CONTRACT}").as_bytes(),
    )
}

/// What the foundation providers are bounded by. Built from the node's
/// config at boot; [`Default`] is that config's defaults.
#[derive(Debug, Clone)]
pub struct FoundationOptions {
    /// The client every outbound HTTP call is made on. The node passes its
    /// shared one: SSRF-pinned DNS, so the address the guard vetted is the
    /// one connected to, redirects off, and the global timeout.
    pub http_client: reqwest::Client,
    /// Cap on one HTTP response body, read while streaming.
    pub http_max_response_bytes: usize,
    /// The row and byte caps a SQL query is held to.
    pub sql: SqlLimits,
    /// The directory file-backed SQLite connections resolve beneath.
    pub sqlite_root: PathBuf,
}

impl Default for FoundationOptions {
    fn default() -> Self {
        Self {
            http_client: default_http_client(),
            http_max_response_bytes: 10 * 1024 * 1024,
            sql: SqlLimits::default(),
            sqlite_root: PathBuf::from("./data/connectors/sqlite"),
        }
    }
}

/// A client built the way the node builds its shared one, for a provider
/// constructed without the node's.
fn default_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(crate::version::USER_AGENT)
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(Arc::new(crate::validation::PinnedDnsResolver))
        .build()
        .unwrap_or_default()
}

/// The caps one SQL query is held to, as `db_read` is: `query.max_limit`
/// rows and `query.max_result_bytes` of decoded result (`0` = unlimited).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlLimits {
    pub max_rows: usize,
    pub max_result_bytes: usize,
}

impl Default for SqlLimits {
    fn default() -> Self {
        Self {
            max_rows: 1000,
            max_result_bytes: 16 * 1024 * 1024,
        }
    }
}

/// The input keys a SQL provider reads its statement from. They must be
/// authored as literal strings (`connector_call` refuses an expression at
/// load), so a caller's values reach the database only through `params`,
/// bound beside the statement.
const SQL_STATEMENT_KEYS: &[&str] = &["sql", "source", "target"];

/// Refuse `input` naming a connection target. A connection's target, and
/// any credential it needs, come from its governed connection revision; a
/// target in the input would let a workflow, or the message it renders,
/// aim the provider anywhere.
fn refuse_input_target(input: &Value, keys: &[&str]) -> Result<(), ConnectorError> {
    match keys.iter().find(|key| input.get(**key).is_some()) {
        Some(key) => Err(ConnectorError::forbidden(
            codes::CONNECTION_FORBIDDEN,
            "validation",
            format!("input.{key} is not accepted: the target comes from the connection revision"),
            EffectStatus::NotStarted,
        )),
        None => Ok(()),
    }
}

/// The connection revision `identity` names, or the active one for its id.
fn governed_connection(
    store: Option<&super::deployment_store::ImmutableDeploymentStore>,
    identity: &EffectiveConnectionIdentity,
) -> Option<super::deployment_store::ConnectionRevisionRow> {
    let store = store?;
    store
        .get_connection(&identity.connection_id, identity.revision)
        .or_else(|| store.get_active_connection(&identity.connection_id))
}

/// The connection string a governed revision names. A credential may live
/// only behind the revision's `secret_locator`, which must be a reference
/// (`env://`, `vault://`, …) and resolves to the whole connection string;
/// without one, `public_config.connection_string` is used as written.
async fn governed_connection_string(
    store: Option<&super::deployment_store::ImmutableDeploymentStore>,
    identity: &EffectiveConnectionIdentity,
    driver: &str,
) -> Result<Option<String>, ConnectorError> {
    let Some(conn) = governed_connection(store, identity) else {
        return Ok(None);
    };
    match governed_secret(&conn, identity, driver).await? {
        Some(secret) => Ok(Some(secret)),
        None => Ok(conn
            .public_config
            .get("connection_string")
            .and_then(Value::as_str)
            .map(str::to_string)),
    }
}

/// What a governed revision's `secret_locator` resolves to, or `None` when
/// it has none. The locator must be a reference (`env://`, `vault://`, …):
/// a literal there would be a credential stored in the clear.
async fn governed_secret(
    conn: &super::deployment_store::ConnectionRevisionRow,
    identity: &EffectiveConnectionIdentity,
    driver: &str,
) -> Result<Option<String>, ConnectorError> {
    let Some(locator) = conn.secret_locator.as_deref() else {
        return Ok(None);
    };
    let is_reference =
        crate::connector::secrets::parse_reference(locator).is_some_and(|(scheme, _)| {
            crate::connector::secrets::default_resolvers()
                .iter()
                .any(|r| r.scheme() == scheme)
        });
    if !is_reference {
        return Err(ConnectorError::invalid_input(
            format!(
                "{driver}: connection '{}' has a secret_locator that is not a secret reference \
                 (such as env://VAR)",
                identity.connection_id
            ),
            None,
        ));
    }
    crate::connector::secrets::resolve_secret_string(locator, "secret_locator")
        .await
        .map(Some)
        .map_err(|e| {
            ConnectorError::unavailable(
                format!(
                    "{driver}: connection '{}' secret_locator did not resolve: {e}",
                    identity.connection_id
                ),
                EffectStatus::NotStarted,
            )
        })
}

/// The statement of a SQL operation: the first of `keys` present, as a string.
fn sql_statement<'a>(
    input: &'a Value,
    keys: &[&str],
    driver: &str,
) -> Result<&'a str, ConnectorError> {
    keys.iter()
        .find_map(|key| input.get(*key))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ConnectorError::invalid_input(
                format!(
                    "{driver} operation requires a string '{}'",
                    keys.join("' or '")
                ),
                Some(json!({"pointer": format!("/{}", keys[0])})),
            )
        })
}

/// The values bound to the statement's placeholders: `input.params`, an array.
fn sql_params(input: &Value) -> Result<Vec<Value>, ConnectorError> {
    match input.get("params") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(_) => Err(ConnectorError::invalid_input(
            "'params' must be an array of the values bound to the statement's placeholders",
            Some(json!({"pointer": "/params"})),
        )),
    }
}

/// A read or a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SqlOperation {
    Query,
    Execute,
}

impl SqlOperation {
    fn parse(operation: &str) -> Option<Self> {
        match operation {
            "query" | "data_query" => Some(Self::Query),
            "execute" | "data_write" => Some(Self::Execute),
            _ => None,
        }
    }

    /// The input keys the statement is read from.
    fn statement_keys(self) -> &'static [&'static str] {
        match self {
            Self::Query => &["sql", "source"],
            Self::Execute => &["sql", "target"],
        }
    }
}

/// A driver failure, classified the way `db_read` classifies one. A
/// transport failure is retryable; so is a serialization failure or a
/// deadlock, which is the database asking for the retry. Anything else the
/// database raised is about the statement, and retrying will not change it.
/// `in_flight` is what the call may have done when the transport failed.
fn sql_error(driver: &str, e: sqlx::Error, in_flight: EffectStatus) -> ConnectorError {
    match &e {
        sqlx::Error::Database(db) if matches!(db.code().as_deref(), Some("40001" | "40P01")) => {
            ConnectorError::unavailable(format!("{driver}: {db}"), EffectStatus::NotStarted)
        }
        sqlx::Error::Database(db) => {
            let mut err = ConnectorError::invalid_input(format!("{driver}: {db}"), None);
            err.origin = super::contract::ErrorOrigin::provider("statement");
            err
        }
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::Protocol(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::WorkerCrashed => {
            ConnectorError::unavailable(format!("{driver}: {e}"), in_flight)
        }
        _ => ConnectorError::invalid_input(format!("{driver}: {e}"), None),
    }
}

/// Run one statement on `pool` with `params` bound beside it, through the
/// same binders and row decoders `db_read` uses. A query's rows are counted
/// and sized as they stream, and the call fails as soon as either cap is
/// crossed, before the rest is held in memory.
async fn run_sql(
    pool: &crate::connector::pool_cache::SqlPool,
    driver: &str,
    operation: SqlOperation,
    statement: &str,
    params: &[Value],
    limits: SqlLimits,
) -> Result<Value, ConnectorError> {
    use crate::connector::sql_encode::{Bound, Scalar};

    let in_flight = match operation {
        SqlOperation::Query => EffectStatus::NotStarted,
        SqlOperation::Execute => EffectStatus::Unknown,
    };
    let scalars: Vec<Scalar> = params.iter().map(Into::into).collect();
    crate::connector::pool_cache::dispatch_sql_pool!(
        pool, p, rows_to_json, bind, typed_args, write_result => {
            let mut conn = p
                .acquire()
                .await
                .map_err(|e| sql_error(driver, e, EffectStatus::NotStarted))?;
            let bound = typed_args(&mut conn, statement, Some(&scalars))
                .await
                .map_err(|e| {
                    ConnectorError::invalid_input(
                        e.message(driver),
                        Some(json!({"pointer": format!("/params/{}", e.param)})),
                    )
                })?;
            // The statement is authored in the definition (`connector_call`
            // refuses one built from the message at load); the message's
            // values travel as bind parameters beside it.
            let query = match bound {
                Bound::Typed(args) => sqlx::query_with(sqlx::AssertSqlSafe(statement), args),
                Bound::Fallback { cache } => {
                    bind(sqlx::query(sqlx::AssertSqlSafe(statement)), params).persistent(cache)
                }
            };
            match operation {
                SqlOperation::Query => {
                    use futures::TryStreamExt;
                    let mut stream = query.fetch(&mut *conn);
                    let mut rows = Vec::new();
                    let mut total_bytes = 0usize;
                    while let Some(row) = stream
                        .try_next()
                        .await
                        .map_err(|e| sql_error(driver, e, in_flight))?
                    {
                        if rows.len() >= limits.max_rows {
                            return Err(ConnectorError::invalid_input(
                                format!(
                                    "{driver}: result exceeds {} rows; add a LIMIT to the query",
                                    limits.max_rows
                                ),
                                None,
                            ));
                        }
                        let Some(decoded) = rows_to_json(
                            std::slice::from_ref(&row),
                            crate::connector::sql_decode::RowFormat::default(),
                        )
                        .map_err(|e| ConnectorError::invalid_input(e.message(driver), None))?
                        .pop() else {
                            continue;
                        };
                        if limits.max_result_bytes > 0 {
                            total_bytes = total_bytes.saturating_add(
                                crate::connector::sql_decode::approx_json_value_bytes(&decoded),
                            );
                            if total_bytes > limits.max_result_bytes {
                                return Err(ConnectorError::invalid_input(
                                    format!(
                                        "{driver}: result exceeds {} bytes; select fewer \
                                         columns or add a LIMIT",
                                        limits.max_result_bytes
                                    ),
                                    None,
                                ));
                            }
                        }
                        rows.push(decoded);
                    }
                    Ok(json!({
                        "count": rows.len(),
                        "rows": rows,
                        "statement": statement,
                    }))
                }
                SqlOperation::Execute => {
                    let result = query
                        .execute(&mut *conn)
                        .await
                        .map_err(|e| sql_error(driver, e, in_flight))?;
                    let (rows_affected, last_insert_id) = write_result(&result);
                    let mut out = json!({
                        "rows_affected": rows_affected,
                        "statement": statement,
                    });
                    if let Some(id) = last_insert_id {
                        out["last_insert_id"] = json!(id);
                    }
                    Ok(out)
                }
            }
        }
    )
}

// ============================================================================
// 1. org.orion.http (V2-14.2)
// ============================================================================

/// Community foundation HTTP/REST connector provider (`org.orion.http`, `V2-14.2`).
#[derive(Default)]
pub struct HttpProviderBackend {
    deployments: Option<super::deployment_store::ImmutableDeploymentStore>,
    options: FoundationOptions,
}

impl std::fmt::Debug for HttpProviderBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpProviderBackend").finish()
    }
}

impl HttpProviderBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_deployment_store(
        deployments: super::deployment_store::ImmutableDeploymentStore,
    ) -> Self {
        Self {
            deployments: Some(deployments),
            options: FoundationOptions::default(),
        }
    }

    /// The same provider, making its calls on `options.http_client`.
    pub fn with_options(mut self, options: &FoundationOptions) -> Self {
        self.options = options.clone();
        self
    }
}

/// How an HTTP connection presents its credential, from its
/// `public_config.auth`. The credential itself is never here: it is what
/// the revision's `secret_locator` resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HttpAuth {
    /// `{"type": "bearer"}`: `Authorization: Bearer <secret>`.
    Bearer,
    /// `{"type": "header", "name": "X-API-Key"}`: the named header carries
    /// the secret.
    Header(reqwest::header::HeaderName),
    /// `{"type": "basic", "username": "svc"}`: HTTP Basic with the secret as
    /// the password.
    Basic { username: String },
}

/// Headers an `auth` of type `header` may not name: the request's framing
/// and routing are Orion's, not the credential's.
const RESERVED_AUTH_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "content-type",
    "transfer-encoding",
];

/// The `auth` object of an HTTP connection's `public_config`, read strictly:
/// a known `type`, the fields that type needs, and nothing else.
fn http_auth(config: &Value) -> Result<Option<HttpAuth>, ConnectorError> {
    let Some(auth) = config.get("auth") else {
        return Ok(None);
    };
    let invalid = |message: String, pointer: &str| {
        ConnectorError::invalid_input(message, Some(json!({"pointer": pointer})))
    };
    let Some(object) = auth.as_object() else {
        return Err(invalid(
            "HTTP 'auth' must be an object".to_string(),
            "/auth",
        ));
    };
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let allowed: &[&str] = match kind {
        "bearer" => &["type"],
        "header" => &["type", "name"],
        "basic" => &["type", "username"],
        _ => {
            return Err(invalid(
                format!("HTTP 'auth.type' must be 'bearer', 'header' or 'basic' (got '{kind}')"),
                "/auth/type",
            ));
        }
    };
    if let Some(extra) = object.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(invalid(
            format!("HTTP 'auth' of type '{kind}' does not take '{extra}'"),
            &format!("/auth/{extra}"),
        ));
    }
    let field = |name: &str| {
        object
            .get(name)
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| {
                invalid(
                    format!("HTTP 'auth' of type '{kind}' requires a string '{name}'"),
                    &format!("/auth/{name}"),
                )
            })
    };
    Ok(Some(match kind {
        "bearer" => HttpAuth::Bearer,
        "header" => {
            let name = field("name")?;
            let header = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| invalid(format!("'{name}' is not a header name"), "/auth/name"))?;
            if RESERVED_AUTH_HEADERS.contains(&header.as_str()) {
                return Err(invalid(
                    format!("HTTP 'auth' may not carry its secret in '{name}'"),
                    "/auth/name",
                ));
            }
            HttpAuth::Header(header)
        }
        _ => HttpAuth::Basic {
            username: field("username")?.to_string(),
        },
    }))
}

/// `request` carrying `secret` the way `auth` says, the header marked
/// sensitive so it never reaches a debug log.
fn authorize(
    request: reqwest::RequestBuilder,
    auth: &HttpAuth,
    secret: &str,
) -> Result<reqwest::RequestBuilder, ConnectorError> {
    Ok(match auth {
        HttpAuth::Bearer => request.bearer_auth(secret),
        HttpAuth::Basic { username } => request.basic_auth(username, Some(secret)),
        HttpAuth::Header(name) => {
            let mut value = reqwest::header::HeaderValue::from_str(secret).map_err(|_| {
                ConnectorError::invalid_input(
                    "HTTP connection secret is not a valid header value",
                    None,
                )
            })?;
            value.set_sensitive(true);
            request.header(name.clone(), value)
        }
    })
}

impl ProviderBackend for HttpProviderBackend {
    fn kind(&self) -> ImplementationKind {
        ImplementationKind::Rust
    }

    fn driver_id(&self) -> &str {
        DRIVER_HTTP
    }

    fn validate_config(&self, config: &Value) -> Result<(), ConnectorError> {
        let Some(url) = config.get("url").and_then(Value::as_str) else {
            return Err(ConnectorError::invalid_input(
                "HTTP connector config requires a string 'url'",
                Some(json!({"pointer": "/url"})),
            ));
        };
        let trimmed = url.trim();
        if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
            return Err(ConnectorError::invalid_input(
                "HTTP connector 'url' must start with http:// or https://",
                Some(json!({"pointer": "/url"})),
            ));
        }
        http_auth(config)?;
        if let Some(methods) = config
            .get("operations")
            .and_then(|o| o.get("methods"))
            .and_then(Value::as_array)
        {
            for (idx, m) in methods.iter().enumerate() {
                let Some(verb) = m.as_str() else {
                    return Err(ConnectorError::invalid_input(
                        "HTTP operation method must be a string",
                        Some(json!({"pointer": format!("/operations/methods/{idx}")})),
                    ));
                };
                if !crate::connector::VALID_HTTP_METHODS
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(verb))
                {
                    return Err(ConnectorError::invalid_input(
                        format!("Unsupported HTTP method '{verb}' in operations.methods"),
                        Some(json!({"pointer": format!("/operations/methods/{idx}")})),
                    ));
                }
            }
        }
        Ok(())
    }

    fn invoke<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        operation: &'a str,
        input: Value,
    ) -> BoxProviderFuture<'a, Result<ConnectorOutput, ConnectorError>> {
        Box::pin(async move {
            ctx.run_context()
                .check_live(ctx.invocation().effect_status())?;

            let method = match operation.to_ascii_lowercase().as_str() {
                "get" | "post" | "put" | "patch" | "delete" => operation.to_ascii_uppercase(),
                "request" => input
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("GET")
                    .to_ascii_uppercase(),
                other => {
                    return Err(ConnectorError::forbidden(
                        codes::OPERATION_FORBIDDEN,
                        "dispatch",
                        format!("Unsupported HTTP operation '{other}'"),
                        ctx.invocation().effect_status(),
                    ));
                }
            };

            if !crate::connector::VALID_HTTP_METHODS.contains(&method.as_str()) {
                return Err(ConnectorError::invalid_input(
                    format!("Unsupported HTTP method '{method}'"),
                    Some(json!({"pointer": "/method"})),
                ));
            }

            refuse_input_target(&input, &["url", "base_url"])?;
            let path = input.get("path").and_then(Value::as_str).unwrap_or("/");
            if !path.starts_with('/') {
                return Err(ConnectorError::invalid_input(
                    "HTTP request 'path' must start with '/'",
                    Some(json!({"pointer": "/path"})),
                ));
            }

            let identity = ctx.invocation().effective_identity();
            let conn = governed_connection(self.deployments.as_ref(), identity);
            let url = conn.as_ref().and_then(|conn| {
                conn.public_config
                    .get("url")
                    .or_else(|| conn.public_config.get("base_url"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
            let Some((url, conn)) = url.zip(conn) else {
                return Err(ConnectorError::invalid_input(
                    format!(
                        "HTTP connector invocation on '{}' requires a connection revision with a 'url'",
                        identity.connection_id
                    ),
                    Some(json!({"pointer": "/url"})),
                ));
            };

            // The credential is settled before anything leaves the node: a
            // revision whose `auth` and `secret_locator` disagree is refused
            // without a DNS lookup.
            let credential = match (http_auth(&conn.public_config)?, &conn.secret_locator) {
                (Some(auth), Some(_)) => governed_secret(&conn, identity, DRIVER_HTTP)
                    .await?
                    .map(|secret| (auth, secret)),
                (None, None) => None,
                (Some(_), None) => {
                    return Err(ConnectorError::invalid_input(
                        format!(
                            "HTTP connection '{}' declares 'auth' but has no secret_locator",
                            identity.connection_id
                        ),
                        Some(json!({"pointer": "/auth"})),
                    ));
                }
                (None, Some(_)) => {
                    return Err(ConnectorError::invalid_input(
                        format!(
                            "HTTP connection '{}' has a secret_locator but no 'auth' saying how \
                             to send it",
                            identity.connection_id
                        ),
                        Some(json!({"pointer": "/auth"})),
                    ));
                }
            };

            if let Err(ssrf_reason) = crate::validation::ssrf::validate_url_not_private(&url).await
            {
                return Err(ConnectorError::forbidden(
                    codes::CAPABILITY_INVALID,
                    "ssrf_validation",
                    format!("Outbound HTTP target blocked by SSRF guard: {ssrf_reason}"),
                    ctx.invocation().effect_status(),
                ));
            }

            let base = url.trim_end_matches('/');
            let full_url = format!("{base}{path}");
            let req_method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| {
                ConnectorError::invalid_input(
                    format!("Invalid HTTP method '{method}': {e}"),
                    Some(json!({"pointer": "/method"})),
                )
            })?;
            let mut req_builder = self.options.http_client.request(req_method, &full_url);
            if let Some((auth, secret)) = &credential {
                req_builder = authorize(req_builder, auth, secret)?;
            }
            if let Some(body_val) = input.get("body")
                && !body_val.is_null()
            {
                req_builder = req_builder.json(body_val);
            }
            ctx.invocation().record_effect(EffectStatus::Unknown);
            let resp = req_builder.send().await.map_err(|e| {
                ConnectorError::unavailable(
                    format!("HTTP request failed: {}", e.without_url()),
                    EffectStatus::Unknown,
                )
            })?;
            let status_code = resp.status().as_u16();
            let bytes = crate::http_body::read_bounded(resp, self.options.http_max_response_bytes)
                .await
                .map_err(|e| {
                    ConnectorError::unavailable(
                        format!("Failed to read HTTP response body: {e}"),
                        EffectStatus::Completed,
                    )
                })?;
            ctx.invocation().record_effect(EffectStatus::Completed);
            let parsed_body: Value = serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
            let mut metadata = BTreeMap::new();
            metadata.insert("driver".to_string(), json!(DRIVER_HTTP));
            metadata.insert("attempt".to_string(), json!(ctx.attempt_number()));
            Ok(ConnectorOutput::with_metadata(
                json!({
                    "status_code": status_code,
                    "method": method,
                    "path": path,
                    "body": parsed_body,
                }),
                metadata,
            ))
        })
    }
}

// ============================================================================
// 2. org.orion.postgresql (V2-14.3)
// ============================================================================

/// Community foundation PostgreSQL connector provider (`org.orion.postgresql`, `V2-14.3`).
pub struct PostgresProviderBackend {
    sessions: Arc<VersionedSessionPool<String>>,
    live_pools: crate::connector::lru_cache::LruCache<sqlx::PgPool>,
    deployments: Option<super::deployment_store::ImmutableDeploymentStore>,
    limits: SqlLimits,
}

impl PostgresProviderBackend {
    pub fn new(max_capacity_per_connection: usize) -> Self {
        Self {
            sessions: Arc::new(VersionedSessionPool::new(
                DRIVER_POSTGRESQL,
                max_capacity_per_connection,
            )),
            live_pools: crate::connector::lru_cache::LruCache::with_evict_handler(
                max_capacity_per_connection.max(4),
                "v2_postgres",
                |pool: sqlx::PgPool| {
                    tokio::spawn(async move {
                        pool.close().await;
                    });
                },
            ),
            deployments: None,
            limits: SqlLimits::default(),
        }
    }

    pub fn with_deployment_store(
        max_capacity_per_connection: usize,
        deployments: super::deployment_store::ImmutableDeploymentStore,
    ) -> Self {
        let mut backend = Self::new(max_capacity_per_connection);
        backend.deployments = Some(deployments);
        backend
    }

    /// The same provider, holding every query to `limits`.
    pub fn with_limits(mut self, limits: SqlLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn session_pool(&self) -> Arc<VersionedSessionPool<String>> {
        self.sessions.clone()
    }

    async fn resolve_connection_string(
        &self,
        identity: &EffectiveConnectionIdentity,
    ) -> Result<String, ConnectorError> {
        let conn_str =
            governed_connection_string(self.deployments.as_ref(), identity, DRIVER_POSTGRESQL)
                .await?;
        let Some(conn_str) = conn_str else {
            return Err(ConnectorError::invalid_input(
                format!(
                    "PostgreSQL operation on '{}' requires a configured 'connection_string'",
                    identity.connection_id
                ),
                Some(json!({"pointer": "/connection_string"})),
            ));
        };
        self.validate_config(&json!({"connection_string": conn_str}))?;
        Ok(conn_str)
    }

    async fn execute_live_postgres(
        &self,
        conn_key: &str,
        conn_str: &str,
        operation: SqlOperation,
        statement: &str,
        params: &[Value],
    ) -> Result<Value, ConnectorError> {
        // Keyed by a digest of the target, never the text: a resolved
        // connection string carries its credential.
        let cache_key = format!(
            "{conn_key}::{}",
            super::package_identity::sha256_digest(conn_str.as_bytes())
        );
        let pool = self
            .live_pools
            .get_or_create(&cache_key, || {
                let target_url = conn_str.to_string();
                async move {
                    sqlx::postgres::PgPoolOptions::new()
                        .max_connections(4)
                        .acquire_timeout(Duration::from_secs(5))
                        .connect(&target_url)
                        .await
                        .map_err(|e| sql_error(DRIVER_POSTGRESQL, e, EffectStatus::NotStarted))
                }
            })
            .await?;
        run_sql(
            &crate::connector::pool_cache::SqlPool::Postgres(pool),
            DRIVER_POSTGRESQL,
            operation,
            statement,
            params,
            self.limits,
        )
        .await
    }
}

impl Default for PostgresProviderBackend {
    fn default() -> Self {
        Self::new(16)
    }
}

impl ProviderBackend for PostgresProviderBackend {
    fn kind(&self) -> ImplementationKind {
        ImplementationKind::Rust
    }

    fn driver_id(&self) -> &str {
        DRIVER_POSTGRESQL
    }

    fn validate_config(&self, config: &Value) -> Result<(), ConnectorError> {
        let Some(conn_str) = config.get("connection_string").and_then(Value::as_str) else {
            return Err(ConnectorError::invalid_input(
                "PostgreSQL connector requires 'connection_string'",
                Some(json!({"pointer": "/connection_string"})),
            ));
        };
        let lower = conn_str.trim().to_ascii_lowercase();
        if !lower.starts_with("postgres://") && !lower.starts_with("postgresql://") {
            return Err(ConnectorError::invalid_input(
                "org.orion.postgresql requires a postgres:// or postgresql:// connection_string",
                Some(json!({"pointer": "/connection_string"})),
            ));
        }
        Ok(())
    }

    fn invoke<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        operation: &'a str,
        input: Value,
    ) -> BoxProviderFuture<'a, Result<ConnectorOutput, ConnectorError>> {
        Box::pin(async move {
            ctx.run_context()
                .check_live(ctx.invocation().effect_status())?;

            let identity = ctx.invocation().effective_identity();
            let handle = self
                .sessions
                .acquire(identity, || async move {
                    Ok(format!(
                        "pg-session:{}:r{}",
                        identity.connection_id, identity.revision
                    ))
                })
                .await?;

            let conn_key = format!("{}:r{}", identity.connection_id, identity.revision);
            let Some(sql_op) = SqlOperation::parse(operation) else {
                return Err(ConnectorError::forbidden(
                    codes::OPERATION_FORBIDDEN,
                    "dispatch",
                    format!("Unsupported PostgreSQL operation '{operation}'"),
                    ctx.invocation().effect_status(),
                ));
            };
            refuse_input_target(&input, &["connection_string"])?;
            let statement = sql_statement(&input, sql_op.statement_keys(), DRIVER_POSTGRESQL)?;
            let params = sql_params(&input)?;
            let conn_str = self.resolve_connection_string(identity).await?;
            ctx.invocation().record_effect(EffectStatus::Unknown);
            let data = self
                .execute_live_postgres(&conn_key, &conn_str, sql_op, statement, &params)
                .await?;
            ctx.invocation().record_effect(EffectStatus::Completed);
            let mut metadata = BTreeMap::new();
            metadata.insert("driver".to_string(), json!(DRIVER_POSTGRESQL));
            metadata.insert("session".to_string(), json!(handle.session()));
            Ok(ConnectorOutput::with_metadata(data, metadata))
        })
    }

    fn literal_input_keys(&self, _operation: &str) -> &'static [&'static str] {
        SQL_STATEMENT_KEYS
    }

    fn dispose<'a>(
        &'a self,
        identity: &'a EffectiveConnectionIdentity,
    ) -> BoxProviderFuture<'a, ()> {
        Box::pin(async move {
            self.sessions.invalidate(identity, None);
        })
    }

    fn as_source(&self) -> Option<&dyn SourceConnectorFacet> {
        Some(self)
    }
}

impl SourceConnectorFacet for PostgresProviderBackend {
    fn open<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        checkpoint: Option<Value>,
    ) -> BoxProviderFuture<'a, Result<Value, ConnectorError>> {
        Box::pin(async move {
            ctx.run_context()
                .check_live(ctx.invocation().effect_status())?;
            Ok(json!({
                "driver": DRIVER_POSTGRESQL,
                "connection_id": ctx.invocation().connection().connection_id,
                "checkpoint": checkpoint.unwrap_or_else(|| json!({"lsn": 0})),
            }))
        })
    }

    fn poll<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        cursor: &'a Value,
        _max_items: u32,
    ) -> BoxProviderFuture<'a, Result<SourcePollBatch, ConnectorError>> {
        Box::pin(async move {
            ctx.run_context()
                .check_live(ctx.invocation().effect_status())?;
            Ok(SourcePollBatch {
                items: Vec::new(),
                next_checkpoint: cursor.get("checkpoint").cloned(),
            })
        })
    }

    fn settle<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        _delivery_id: &'a str,
        _settlement: SourceSettlement,
    ) -> BoxProviderFuture<'a, Result<(), ConnectorError>> {
        Box::pin(async move {
            ctx.run_context()
                .check_live(ctx.invocation().effect_status())?;
            Ok(())
        })
    }

    fn close<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        _cursor: Value,
    ) -> BoxProviderFuture<'a, Result<(), ConnectorError>> {
        Box::pin(async move {
            ctx.run_context()
                .check_live(ctx.invocation().effect_status())?;
            Ok(())
        })
    }
}

// ============================================================================
// 3. org.orion.sqlite (V2-14.3)
// ============================================================================

/// Reserved filenames that a connector is never allowed to open, preventing
/// any workflow from querying or mutating Orion's own configuration database (`V2-14.3`).
const RESERVED_ORION_DB_NAMES: &[&str] = &["orion.db", "orion.sqlite", "orion-state.db"];

/// Community foundation scoped SQLite connector provider (`org.orion.sqlite`, `V2-14.3`).
///
/// Enforces strict root-directory sandboxing:
/// - `sqlite::memory:` is always permitted (isolated per connection identity).
/// - File-backed SQLite URLs (`sqlite://...` or `sqlite:...`) must be relative paths
///   resolved beneath `approved_root`, containing no `..` traversal components,
///   and must not target Orion's own state database filenames.
pub struct SqliteProviderBackend {
    approved_root: PathBuf,
    sessions: Arc<VersionedSessionPool<String>>,
    live_pools: crate::connector::lru_cache::LruCache<sqlx::SqlitePool>,
    deployments: Option<super::deployment_store::ImmutableDeploymentStore>,
    limits: SqlLimits,
}

impl SqliteProviderBackend {
    pub fn new(approved_root: impl Into<PathBuf>, max_capacity_per_connection: usize) -> Self {
        Self {
            approved_root: approved_root.into(),
            sessions: Arc::new(VersionedSessionPool::new(
                DRIVER_SQLITE,
                max_capacity_per_connection,
            )),
            live_pools: crate::connector::lru_cache::LruCache::with_evict_handler(
                max_capacity_per_connection.max(4),
                "v2_sqlite",
                |pool: sqlx::SqlitePool| {
                    tokio::spawn(async move {
                        pool.close().await;
                    });
                },
            ),
            deployments: None,
            limits: SqlLimits::default(),
        }
    }

    /// The same provider, holding every query to `limits`.
    pub fn with_limits(mut self, limits: SqlLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_deployment_store(
        approved_root: impl Into<PathBuf>,
        max_capacity_per_connection: usize,
        deployments: super::deployment_store::ImmutableDeploymentStore,
    ) -> Self {
        let mut backend = Self::new(approved_root, max_capacity_per_connection);
        backend.deployments = Some(deployments);
        backend
    }

    /// The governed revision's connection string, or an in-memory database
    /// private to the connection when no revision names one.
    async fn resolve_connection_string(
        &self,
        identity: &EffectiveConnectionIdentity,
    ) -> Result<String, ConnectorError> {
        Ok(
            governed_connection_string(self.deployments.as_ref(), identity, DRIVER_SQLITE)
                .await?
                .unwrap_or_else(|| "sqlite::memory:".to_string()),
        )
    }

    pub fn session_pool(&self) -> Arc<VersionedSessionPool<String>> {
        self.sessions.clone()
    }

    /// Validate and resolve a SQLite `connection_string` within `approved_root` (`V2-14.3`).
    pub fn resolve_sandboxed_target(&self, conn_str: &str) -> Result<String, ConnectorError> {
        let trimmed = conn_str.trim();
        if trimmed.eq_ignore_ascii_case("sqlite::memory:")
            || trimmed.eq_ignore_ascii_case("sqlite://:memory:")
        {
            return Ok("sqlite::memory:".to_string());
        }

        let Some(raw_path) = trimmed
            .strip_prefix("sqlite://")
            .or_else(|| trimmed.strip_prefix("sqlite:"))
        else {
            return Err(ConnectorError::invalid_input(
                "org.orion.sqlite requires a sqlite: or sqlite:// connection_string",
                Some(json!({"pointer": "/connection_string"})),
            ));
        };

        // Strip query parameters (e.g. ?mode=rwc) before checking path safety.
        let path_part = raw_path.split('?').next().unwrap_or(raw_path).trim();
        if path_part.is_empty() {
            return Err(ConnectorError::invalid_input(
                "SQLite file path must not be empty",
                Some(json!({"pointer": "/connection_string"})),
            ));
        }

        let candidate = Path::new(path_part);
        if candidate.is_absolute() {
            return Err(ConnectorError::forbidden(
                codes::CAPABILITY_INVALID,
                "validation",
                "SQLite connector path must be relative to the approved data root, not absolute",
                EffectStatus::NotStarted,
            ));
        }

        for comp in candidate.components() {
            match comp {
                Component::Normal(_) | Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(ConnectorError::forbidden(
                        codes::CAPABILITY_INVALID,
                        "validation",
                        "SQLite connector path must not contain '..' traversal segments",
                        EffectStatus::NotStarted,
                    ));
                }
            }
        }

        if let Some(file_name) = candidate.file_name().and_then(|f| f.to_str())
            && RESERVED_ORION_DB_NAMES
                .iter()
                .any(|reserved| reserved.eq_ignore_ascii_case(file_name))
        {
            return Err(ConnectorError::forbidden(
                codes::CAPABILITY_INVALID,
                "validation",
                format!(
                    "SQLite filename '{file_name}' is reserved for Orion's configuration database"
                ),
                EffectStatus::NotStarted,
            ));
        }

        let resolved = self.approved_root.join(candidate);
        Ok(format!("sqlite://{}", resolved.display()))
    }

    /// Execute a real SQL statement against a sandboxed SQLite target (`V2-14.3`).
    pub async fn execute_live_sqlite(
        &self,
        conn_str: &str,
        operation: &str,
        statement: &str,
    ) -> Result<Value, ConnectorError> {
        self.execute_live_sqlite_for_connection(None, conn_str, operation, statement, &[])
            .await
    }

    /// Execute a real SQL statement against a sandboxed SQLite target, with
    /// `params` bound to its placeholders, optionally partitioning
    /// `sqlite::memory:` pools by `connection_key` so tables persist across
    /// statements on the same connection (`TD-24`, `V2-14.3`).
    pub async fn execute_live_sqlite_for_connection(
        &self,
        connection_key: Option<&str>,
        conn_str: &str,
        operation: &str,
        statement: &str,
        params: &[Value],
    ) -> Result<Value, ConnectorError> {
        let Some(sql_op) = SqlOperation::parse(operation) else {
            return Err(ConnectorError::forbidden(
                codes::OPERATION_FORBIDDEN,
                "dispatch",
                format!("Unsupported SQLite operation '{operation}'"),
                EffectStatus::NotStarted,
            ));
        };
        let target = self.resolve_sandboxed_target(conn_str)?;
        let cache_key = match connection_key {
            Some(key) if target == "sqlite::memory:" => format!("{key}::{target}"),
            _ => target.clone(),
        };
        let pool = self
            .live_pools
            .get_or_create(&cache_key, || {
                let target_url = target.clone();
                async move {
                    let builder = if target_url == "sqlite::memory:" {
                        sqlx::sqlite::SqlitePoolOptions::new().max_connections(1)
                    } else {
                        sqlx::sqlite::SqlitePoolOptions::new().max_connections(4)
                    };
                    builder.connect(&target_url).await.map_err(|e| {
                        ConnectorError::invalid_input(
                            format!(
                                "Failed to connect to sandboxed SQLite target '{target_url}': {e}"
                            ),
                            None,
                        )
                    })
                }
            })
            .await?;
        run_sql(
            &crate::connector::pool_cache::SqlPool::Sqlite(pool),
            DRIVER_SQLITE,
            sql_op,
            statement,
            params,
            self.limits,
        )
        .await
    }
}

impl Default for SqliteProviderBackend {
    fn default() -> Self {
        Self::new("./data/connectors/sqlite", 8)
    }
}

impl ProviderBackend for SqliteProviderBackend {
    fn kind(&self) -> ImplementationKind {
        ImplementationKind::Rust
    }

    fn driver_id(&self) -> &str {
        DRIVER_SQLITE
    }

    fn validate_config(&self, config: &Value) -> Result<(), ConnectorError> {
        let Some(conn_str) = config.get("connection_string").and_then(Value::as_str) else {
            return Err(ConnectorError::invalid_input(
                "SQLite connector requires 'connection_string'",
                Some(json!({"pointer": "/connection_string"})),
            ));
        };
        let _ = self.resolve_sandboxed_target(conn_str)?;
        Ok(())
    }

    fn invoke<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        operation: &'a str,
        input: Value,
    ) -> BoxProviderFuture<'a, Result<ConnectorOutput, ConnectorError>> {
        Box::pin(async move {
            ctx.run_context()
                .check_live(ctx.invocation().effect_status())?;

            let identity = ctx.invocation().effective_identity();
            let handle = self
                .sessions
                .acquire(identity, || async move {
                    Ok(format!(
                        "sqlite-session:{}:r{}",
                        identity.connection_id, identity.revision
                    ))
                })
                .await?;

            let conn_key = format!("{}:r{}", identity.connection_id, identity.revision);
            let Some(sql_op) = SqlOperation::parse(operation) else {
                return Err(ConnectorError::forbidden(
                    codes::OPERATION_FORBIDDEN,
                    "dispatch",
                    format!("Unsupported SQLite operation '{operation}'"),
                    ctx.invocation().effect_status(),
                ));
            };
            refuse_input_target(&input, &["connection_string"])?;
            let statement = sql_statement(&input, sql_op.statement_keys(), DRIVER_SQLITE)?;
            let params = sql_params(&input)?;
            let target_conn = self.resolve_connection_string(identity).await?;
            ctx.invocation().record_effect(EffectStatus::Unknown);
            let data = self
                .execute_live_sqlite_for_connection(
                    Some(&conn_key),
                    &target_conn,
                    operation,
                    statement,
                    &params,
                )
                .await?;
            ctx.invocation().record_effect(EffectStatus::Completed);
            let mut metadata = BTreeMap::new();
            metadata.insert("driver".to_string(), json!(DRIVER_SQLITE));
            metadata.insert("session".to_string(), json!(handle.session()));
            Ok(ConnectorOutput::with_metadata(data, metadata))
        })
    }

    fn literal_input_keys(&self, _operation: &str) -> &'static [&'static str] {
        SQL_STATEMENT_KEYS
    }

    fn dispose<'a>(
        &'a self,
        identity: &'a EffectiveConnectionIdentity,
    ) -> BoxProviderFuture<'a, ()> {
        Box::pin(async move {
            self.sessions.invalidate(identity, None);
        })
    }

    fn as_source(&self) -> Option<&dyn SourceConnectorFacet> {
        Some(self)
    }
}

impl SourceConnectorFacet for SqliteProviderBackend {
    fn open<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        checkpoint: Option<Value>,
    ) -> BoxProviderFuture<'a, Result<Value, ConnectorError>> {
        Box::pin(async move {
            ctx.run_context()
                .check_live(ctx.invocation().effect_status())?;
            Ok(json!({
                "driver": DRIVER_SQLITE,
                "connection_id": ctx.invocation().connection().connection_id,
                "checkpoint": checkpoint.unwrap_or_else(|| json!({"rowid": 0})),
            }))
        })
    }

    fn poll<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        cursor: &'a Value,
        _max_items: u32,
    ) -> BoxProviderFuture<'a, Result<SourcePollBatch, ConnectorError>> {
        Box::pin(async move {
            ctx.run_context()
                .check_live(ctx.invocation().effect_status())?;
            Ok(SourcePollBatch {
                items: Vec::<SourceBatchItem>::new(),
                next_checkpoint: cursor.get("checkpoint").cloned(),
            })
        })
    }

    fn settle<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        _delivery_id: &'a str,
        _settlement: SourceSettlement,
    ) -> BoxProviderFuture<'a, Result<(), ConnectorError>> {
        Box::pin(async move {
            ctx.run_context()
                .check_live(ctx.invocation().effect_status())?;
            Ok(())
        })
    }

    fn close<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        _cursor: Value,
    ) -> BoxProviderFuture<'a, Result<(), ConnectorError>> {
        Box::pin(async move {
            ctx.run_context()
                .check_live(ctx.invocation().effect_status())?;
            Ok(())
        })
    }
}

// ============================================================================
// 4. org.orion.memory-cache (V2-14.4)
// ============================================================================

#[derive(Debug, Clone)]
struct CacheEntry {
    value: Value,
    expires_at: Option<Instant>,
}

/// Bounded, connection-partitioned in-memory cache store (`V2-14.4`, `TD-29`).
///
/// Keyed by [`EffectiveConnectionIdentity`] so:
/// - Two different connections never share keys.
/// - Old and new revisions of a connection have isolated keyspaces while
///   participating in [`ConnectionPoolCache`] eviction.
/// - Total partitions, entries per partition, and per-value byte sizes are bounded.
pub struct PartitionedMemoryCache {
    max_entries_per_partition: usize,
    max_partitions: usize,
    max_value_bytes: usize,
    partitions: Mutex<BTreeMap<EffectiveConnectionIdentity, BTreeMap<String, CacheEntry>>>,
}

impl PartitionedMemoryCache {
    pub fn new(max_entries_per_partition: usize) -> Self {
        Self::with_limits(max_entries_per_partition, 256, 1024 * 1024)
    }

    pub fn with_limits(
        max_entries_per_partition: usize,
        max_partitions: usize,
        max_value_bytes: usize,
    ) -> Self {
        Self {
            max_entries_per_partition: max_entries_per_partition.max(1),
            max_partitions: max_partitions.max(1),
            max_value_bytes: max_value_bytes.max(64),
            partitions: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn max_value_bytes(&self) -> usize {
        self.max_value_bytes
    }

    pub fn get(&self, identity: &EffectiveConnectionIdentity, key: &str) -> Option<Value> {
        let mut guard = self.partitions.lock().unwrap_or_else(|e| e.into_inner());
        let partition = guard.get_mut(identity)?;
        let Some(entry) = partition.get(key) else {
            if partition.is_empty() {
                guard.remove(identity);
            }
            return None;
        };
        if let Some(exp) = entry.expires_at
            && Instant::now() >= exp
        {
            partition.remove(key);
            if partition.is_empty() {
                guard.remove(identity);
            }
            return None;
        }
        Some(entry.value.clone())
    }

    pub fn set(
        &self,
        identity: &EffectiveConnectionIdentity,
        key: String,
        value: Value,
        ttl: Option<Duration>,
    ) {
        let mut guard = self.partitions.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();

        if !guard.contains_key(identity) && guard.len() >= self.max_partitions {
            // Sweep expired entries across all partitions and drop empty partitions first (`TD-29`).
            for part in guard.values_mut() {
                part.retain(|_, v| v.expires_at.is_none_or(|exp| now < exp));
            }
            guard.retain(|_, part| !part.is_empty());

            // If still at capacity, evict the first partition so total partitions stay bounded.
            if guard.len() >= self.max_partitions
                && let Some(first_partition_key) = guard.keys().next().cloned()
            {
                guard.remove(&first_partition_key);
            }
        }

        let partition = guard.entry(identity.clone()).or_default();

        // Purge expired entries first if we are at capacity.
        if partition.len() >= self.max_entries_per_partition && !partition.contains_key(&key) {
            partition.retain(|_, v| v.expires_at.is_none_or(|exp| now < exp));
        }
        // If still at capacity, evict the lexicographically first key so size stays bounded.
        if partition.len() >= self.max_entries_per_partition
            && !partition.contains_key(&key)
            && let Some(first_key) = partition.keys().next().cloned()
        {
            partition.remove(&first_key);
        }

        let expires_at = ttl.and_then(|d| now.checked_add(d));
        partition.insert(key, CacheEntry { value, expires_at });
    }

    pub fn delete(&self, identity: &EffectiveConnectionIdentity, key: &str) -> bool {
        let mut guard = self.partitions.lock().unwrap_or_else(|e| e.into_inner());
        let Some(partition) = guard.get_mut(identity) else {
            return false;
        };
        let removed = partition.remove(key).is_some();
        if partition.is_empty() {
            guard.remove(identity);
        }
        removed
    }

    pub fn partition_count(&self) -> usize {
        self.partitions.lock().map(|g| g.len()).unwrap_or(0)
    }
}

impl ConnectionPoolCache for PartitionedMemoryCache {
    fn cache_id(&self) -> &str {
        DRIVER_MEMORY_CACHE
    }

    fn evict<'a>(&'a self, connector_name: &'a str) -> BoxPoolFuture<'a, ()> {
        Box::pin(async move {
            let mut guard = self.partitions.lock().unwrap_or_else(|e| e.into_inner());
            guard.retain(|k, _| k.connection_id != connector_name);
        })
    }

    fn evict_all<'a>(&'a self) -> BoxPoolFuture<'a, ()> {
        Box::pin(async move {
            let mut guard = self.partitions.lock().unwrap_or_else(|e| e.into_inner());
            guard.clear();
        })
    }
}

/// Community foundation in-memory cache connector provider (`org.orion.memory-cache`, `V2-14.4`).
pub struct MemoryCacheProviderBackend {
    store: Arc<PartitionedMemoryCache>,
}

impl MemoryCacheProviderBackend {
    pub fn new(max_entries_per_partition: usize) -> Self {
        Self {
            store: Arc::new(PartitionedMemoryCache::new(max_entries_per_partition)),
        }
    }

    pub fn store(&self) -> Arc<PartitionedMemoryCache> {
        self.store.clone()
    }
}

impl Default for MemoryCacheProviderBackend {
    fn default() -> Self {
        Self::new(1024)
    }
}

impl ProviderBackend for MemoryCacheProviderBackend {
    fn kind(&self) -> ImplementationKind {
        ImplementationKind::Rust
    }

    fn driver_id(&self) -> &str {
        DRIVER_MEMORY_CACHE
    }

    fn validate_config(&self, config: &Value) -> Result<(), ConnectorError> {
        if let Some(backend) = config.get("backend").and_then(Value::as_str)
            && !backend.eq_ignore_ascii_case("memory")
        {
            return Err(ConnectorError::invalid_input(
                format!(
                    "org.orion.memory-cache only supports backend 'memory' (got '{backend}'; \
                     use Enterprise provider 'com.orion.enterprise.redis' for Redis)"
                ),
                Some(json!({"pointer": "/backend"})),
            ));
        }
        Ok(())
    }

    fn invoke<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        operation: &'a str,
        input: Value,
    ) -> BoxProviderFuture<'a, Result<ConnectorOutput, ConnectorError>> {
        Box::pin(async move {
            ctx.run_context()
                .check_live(ctx.invocation().effect_status())?;
            let identity = ctx.invocation().effective_identity();

            let Some(key) = input.get("key").and_then(Value::as_str) else {
                return Err(ConnectorError::invalid_input(
                    "Memory cache operation requires a string 'key'",
                    Some(json!({"pointer": "/key"})),
                ));
            };

            match operation {
                "get" | "cache_read" | "cache.get" => {
                    let val = self.store.get(identity, key);
                    Ok(ConnectorOutput::new(json!({
                        "hit": val.is_some(),
                        "value": val.unwrap_or(Value::Null),
                    })))
                }
                "set" | "cache_write" | "cache.set" => {
                    let value = input.get("value").cloned().unwrap_or(Value::Null);
                    let approx_bytes =
                        crate::connector::sql_decode::approx_json_value_bytes(&value);
                    if approx_bytes > self.store.max_value_bytes() {
                        return Err(ConnectorError::invalid_input(
                            format!(
                                "Memory cache value size ({approx_bytes} bytes) exceeds maximum allowed ({} bytes)",
                                self.store.max_value_bytes()
                            ),
                            Some(json!({"pointer": "/value"})),
                        ));
                    }
                    let ttl = if let Some(raw_ms) = input.get("ttl_ms") {
                        if raw_ms.is_null() {
                            None
                        } else {
                            let ms = raw_ms.as_u64().filter(|&v| v > 0).ok_or_else(|| {
                                ConnectorError::invalid_input(
                                    "Memory cache 'ttl_ms' must be a positive integer",
                                    Some(json!({"pointer": "/ttl_ms"})),
                                )
                            })?;
                            Some(Duration::from_millis(ms))
                        }
                    } else if let Some(raw_secs) = input.get("ttl_secs") {
                        if raw_secs.is_null() {
                            None
                        } else {
                            let secs = raw_secs.as_u64().filter(|&v| v > 0).ok_or_else(|| {
                                ConnectorError::invalid_input(
                                    "Memory cache 'ttl_secs' must be a positive integer",
                                    Some(json!({"pointer": "/ttl_secs"})),
                                )
                            })?;
                            Some(Duration::from_secs(secs))
                        }
                    } else {
                        None
                    };
                    self.store.set(identity, key.to_string(), value, ttl);
                    Ok(ConnectorOutput::new(json!({
                        "stored": true,
                        "key": key,
                    })))
                }
                "delete" | "cache.delete" => {
                    let deleted = self.store.delete(identity, key);
                    Ok(ConnectorOutput::new(json!({
                        "deleted": deleted,
                        "key": key,
                    })))
                }
                other => Err(ConnectorError::forbidden(
                    codes::OPERATION_FORBIDDEN,
                    "dispatch",
                    format!("Unsupported memory-cache operation '{other}'"),
                    ctx.invocation().effect_status(),
                )),
            }
        })
    }

    fn dispose<'a>(
        &'a self,
        identity: &'a EffectiveConnectionIdentity,
    ) -> BoxProviderFuture<'a, ()> {
        Box::pin(async move {
            let mut guard = self
                .store
                .partitions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.remove(identity);
        })
    }
}

/// Register all four Community foundation Direct Rust providers and their session/cache
/// pools into `providers` and `pools` (`V2-11.1`, `V2-14.2`–`V2-14.4`).
pub fn register_community_foundation_providers(
    providers: &ProviderRegistry,
    pools: &ResourcePoolRegistry,
) {
    register_community_foundation_providers_with_store(providers, pools, None);
}

/// Register all four Community foundation Direct Rust providers bound to `deployments` (`V2-14.2`–`V2-14.4`).
pub fn register_community_foundation_providers_with_store(
    providers: &ProviderRegistry,
    pools: &ResourcePoolRegistry,
    deployments: Option<super::deployment_store::ImmutableDeploymentStore>,
) {
    register_community_foundation_providers_with(
        providers,
        pools,
        deployments,
        &FoundationOptions::default(),
    );
}

/// [`register_community_foundation_providers_with_store`], bounded by `options`.
pub fn register_community_foundation_providers_with(
    providers: &ProviderRegistry,
    pools: &ResourcePoolRegistry,
    deployments: Option<super::deployment_store::ImmutableDeploymentStore>,
    options: &FoundationOptions,
) {
    let http = match deployments.clone() {
        Some(store) => HttpProviderBackend::with_deployment_store(store),
        None => HttpProviderBackend::new(),
    };
    let http = Arc::new(http.with_options(options));
    let pg = match deployments.clone() {
        Some(store) => PostgresProviderBackend::with_deployment_store(16, store),
        None => PostgresProviderBackend::default(),
    };
    let pg = Arc::new(pg.with_limits(options.sql));
    let sqlite = match deployments {
        Some(store) => {
            SqliteProviderBackend::with_deployment_store(options.sqlite_root.clone(), 8, store)
        }
        None => SqliteProviderBackend::new(options.sqlite_root.clone(), 8),
    };
    let sqlite = Arc::new(sqlite.with_limits(options.sql));
    let mem_cache = Arc::new(MemoryCacheProviderBackend::default());

    pools.register(pg.session_pool());
    pools.register(sqlite.session_pool());
    pools.register(mem_cache.store());

    providers.register(http);
    providers.register(pg);
    providers.register(sqlite);
    providers.register(mem_cache);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
#[path = "foundation_tests.rs"]
mod tests;
