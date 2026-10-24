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

// ============================================================================
// 1. org.orion.http (V2-14.2)
// ============================================================================

/// Community foundation HTTP/REST connector provider (`org.orion.http`, `V2-14.2`).
#[derive(Default)]
pub struct HttpProviderBackend {
    deployments: Option<super::deployment_store::ImmutableDeploymentStore>,
}

impl std::fmt::Debug for HttpProviderBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpProviderBackend").finish()
    }
}

impl HttpProviderBackend {
    pub fn new() -> Self {
        Self { deployments: None }
    }

    pub fn with_deployment_store(
        deployments: super::deployment_store::ImmutableDeploymentStore,
    ) -> Self {
        Self {
            deployments: Some(deployments),
        }
    }

    fn resolve_connection_url(
        &self,
        identity: &EffectiveConnectionIdentity,
        input: &Value,
    ) -> Option<String> {
        if let Some(url) = input
            .get("url")
            .or_else(|| input.get("base_url"))
            .and_then(Value::as_str)
        {
            return Some(url.to_string());
        }
        let store = self.deployments.as_ref()?;
        let conn = store
            .get_connection(&identity.connection_id, identity.revision)
            .or_else(|| store.get_active_connection(&identity.connection_id))?;
        conn.public_config
            .get("url")
            .or_else(|| conn.public_config.get("base_url"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }
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

            let path = input.get("path").and_then(Value::as_str).unwrap_or("/");
            if !path.starts_with('/') {
                return Err(ConnectorError::invalid_input(
                    "HTTP request 'path' must start with '/'",
                    Some(json!({"pointer": "/path"})),
                ));
            }

            let identity = ctx.invocation().effective_identity();
            let Some(url) = self.resolve_connection_url(identity, &input) else {
                return Err(ConnectorError::invalid_input(
                    format!(
                        "HTTP connector invocation on '{}' requires a configured connection 'url' or input 'url'",
                        identity.connection_id
                    ),
                    Some(json!({"pointer": "/url"})),
                ));
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
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()
                .map_err(|e| {
                    ConnectorError::unavailable(
                        format!("Failed to build HTTP client: {e}"),
                        EffectStatus::NotStarted,
                    )
                })?;
            let mut req_builder = client.request(req_method, &full_url);
            if let Some(body_val) = input.get("body")
                && !body_val.is_null()
            {
                req_builder = req_builder.json(body_val);
            }
            ctx.invocation().record_effect(EffectStatus::Unknown);
            let resp = req_builder.send().await.map_err(|e| {
                ConnectorError::unavailable(
                    format!("HTTP request failed: {e}"),
                    EffectStatus::Unknown,
                )
            })?;
            let status_code = resp.status().as_u16();
            let bytes = crate::http_body::read_bounded(resp, 4 * 1024 * 1024)
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

    pub fn session_pool(&self) -> Arc<VersionedSessionPool<String>> {
        self.sessions.clone()
    }

    fn resolve_connection_string(
        &self,
        identity: &EffectiveConnectionIdentity,
        input: &Value,
    ) -> Result<String, ConnectorError> {
        let conn_str = if let Some(s) = input.get("connection_string").and_then(Value::as_str) {
            Some(s.to_string())
        } else if let Some(ref store) = self.deployments {
            store
                .get_connection(&identity.connection_id, identity.revision)
                .or_else(|| store.get_active_connection(&identity.connection_id))
                .and_then(|c| {
                    c.public_config
                        .get("connection_string")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
        } else {
            None
        };
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
        ctx: &AttemptContext,
        conn_key: &str,
        conn_str: &str,
        operation: &str,
        statement: &str,
    ) -> Result<Value, ConnectorError> {
        use sqlx::{Column, Row, TypeInfo, ValueRef};

        let cache_key = format!("{conn_key}::{conn_str}");
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
                        .map_err(|e| {
                            ConnectorError::unavailable(
                                format!("Failed to connect to PostgreSQL target: {e}"),
                                EffectStatus::NotStarted,
                            )
                        })
                }
            })
            .await?;

        let statement_owned = statement.to_string();
        match operation {
            "query" | "data_query" => {
                ctx.invocation().record_effect(EffectStatus::Unknown);
                let rows = sqlx::query(sqlx::AssertSqlSafe(statement_owned.clone()))
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| {
                        ConnectorError::invalid_input(format!("PostgreSQL query error: {e}"), None)
                    })?;
                ctx.invocation().record_effect(EffectStatus::Completed);
                let mut json_rows = Vec::with_capacity(rows.len());
                for row in &rows {
                    let mut map = serde_json::Map::new();
                    for (idx, col) in row.columns().iter().enumerate() {
                        let cell_val = if let Ok(raw) = row.try_get_raw(idx) {
                            if raw.is_null() {
                                Value::Null
                            } else {
                                match raw.type_info().name() {
                                    "INT2" | "INT4" | "INT8" => row
                                        .try_get_unchecked::<i64, _>(idx)
                                        .map(Value::from)
                                        .unwrap_or(Value::Null),
                                    "FLOAT4" | "FLOAT8" => row
                                        .try_get_unchecked::<f64, _>(idx)
                                        .ok()
                                        .and_then(serde_json::Number::from_f64)
                                        .map(Value::Number)
                                        .unwrap_or(Value::Null),
                                    "BOOL" => row
                                        .try_get_unchecked::<bool, _>(idx)
                                        .map(Value::Bool)
                                        .unwrap_or(Value::Null),
                                    "JSON" | "JSONB" => row
                                        .try_get_unchecked::<Value, _>(idx)
                                        .unwrap_or(Value::Null),
                                    _ => row
                                        .try_get_unchecked::<Option<String>, _>(idx)
                                        .ok()
                                        .flatten()
                                        .map(Value::String)
                                        .unwrap_or(Value::Null),
                                }
                            }
                        } else {
                            Value::Null
                        };
                        map.insert(col.name().to_string(), cell_val);
                    }
                    json_rows.push(Value::Object(map));
                }
                Ok(json!({
                    "rows": json_rows,
                    "count": rows.len(),
                    "statement": statement_owned,
                }))
            }
            "execute" | "data_write" => {
                ctx.invocation().record_effect(EffectStatus::Unknown);
                let res = sqlx::query(sqlx::AssertSqlSafe(statement_owned.clone()))
                    .execute(&pool)
                    .await
                    .map_err(|e| {
                        ConnectorError::invalid_input(format!("PostgreSQL write error: {e}"), None)
                    })?;
                ctx.invocation().record_effect(EffectStatus::Completed);
                Ok(json!({
                    "rows_affected": res.rows_affected(),
                    "statement": statement_owned,
                }))
            }
            other => Err(ConnectorError::forbidden(
                codes::OPERATION_FORBIDDEN,
                "dispatch",
                format!("Unsupported PostgreSQL operation '{other}'"),
                ctx.invocation().effect_status(),
            )),
        }
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
            match operation {
                "query" | "data_query" => {
                    let statement = input
                        .get("sql")
                        .or_else(|| input.get("source"))
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            ConnectorError::invalid_input(
                                "PostgreSQL query requires 'sql' or 'source'",
                                Some(json!({"pointer": "/sql"})),
                            )
                        })?;
                    let conn_str = self.resolve_connection_string(identity, &input)?;
                    let data = self
                        .execute_live_postgres(ctx, &conn_key, &conn_str, operation, statement)
                        .await?;
                    let mut metadata = BTreeMap::new();
                    metadata.insert("driver".to_string(), json!(DRIVER_POSTGRESQL));
                    metadata.insert("session".to_string(), json!(handle.session()));
                    Ok(ConnectorOutput::with_metadata(data, metadata))
                }
                "execute" | "data_write" => {
                    let statement = input
                        .get("sql")
                        .or_else(|| input.get("target"))
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            ConnectorError::invalid_input(
                                "PostgreSQL write requires 'sql' or 'target'",
                                Some(json!({"pointer": "/sql"})),
                            )
                        })?;
                    let conn_str = self.resolve_connection_string(identity, &input)?;
                    let data = self
                        .execute_live_postgres(ctx, &conn_key, &conn_str, operation, statement)
                        .await?;
                    let mut metadata = BTreeMap::new();
                    metadata.insert("driver".to_string(), json!(DRIVER_POSTGRESQL));
                    metadata.insert("session".to_string(), json!(handle.session()));
                    Ok(ConnectorOutput::with_metadata(data, metadata))
                }
                other => Err(ConnectorError::forbidden(
                    codes::OPERATION_FORBIDDEN,
                    "dispatch",
                    format!("Unsupported PostgreSQL operation '{other}'"),
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
        }
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

    fn resolve_connection_string(
        &self,
        identity: &EffectiveConnectionIdentity,
        input: &Value,
    ) -> String {
        if let Some(s) = input.get("connection_string").and_then(Value::as_str) {
            return s.to_string();
        }
        if let Some(ref store) = self.deployments
            && let Some(conn) = store
                .get_connection(&identity.connection_id, identity.revision)
                .or_else(|| store.get_active_connection(&identity.connection_id))
            && let Some(s) = conn
                .public_config
                .get("connection_string")
                .and_then(Value::as_str)
        {
            return s.to_string();
        }
        "sqlite::memory:".to_string()
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
        self.execute_live_sqlite_for_connection(None, conn_str, operation, statement)
            .await
    }

    /// Execute a real SQL statement against a sandboxed SQLite target, optionally
    /// partitioning `sqlite::memory:` pools by `connection_key` so tables persist
    /// across statements on the same connection (`TD-24`, `V2-14.3`).
    pub async fn execute_live_sqlite_for_connection(
        &self,
        connection_key: Option<&str>,
        conn_str: &str,
        operation: &str,
        statement: &str,
    ) -> Result<Value, ConnectorError> {
        use sqlx::{Column, Row, TypeInfo, ValueRef};

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
        let statement_owned = statement.to_string();

        match operation {
            "query" | "data_query" => {
                let rows = sqlx::query(sqlx::AssertSqlSafe(statement_owned.clone()))
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| {
                        ConnectorError::invalid_input(format!("SQLite query error: {e}"), None)
                    })?;
                let mut json_rows = Vec::with_capacity(rows.len());
                for row in &rows {
                    let mut map = serde_json::Map::new();
                    for (idx, col) in row.columns().iter().enumerate() {
                        let cell_val = if let Ok(raw) = row.try_get_raw(idx) {
                            if raw.is_null() {
                                Value::Null
                            } else {
                                match raw.type_info().name() {
                                    "INTEGER" | "INT" | "BIGINT" | "INT8" => row
                                        .try_get_unchecked::<i64, _>(idx)
                                        .map(Value::from)
                                        .unwrap_or(Value::Null),
                                    "REAL" | "FLOAT" | "DOUBLE" => row
                                        .try_get_unchecked::<f64, _>(idx)
                                        .ok()
                                        .and_then(serde_json::Number::from_f64)
                                        .map(Value::Number)
                                        .unwrap_or(Value::Null),
                                    "BOOLEAN" | "BOOL" => row
                                        .try_get_unchecked::<bool, _>(idx)
                                        .map(Value::Bool)
                                        .unwrap_or(Value::Null),
                                    _ => row
                                        .try_get_unchecked::<Option<String>, _>(idx)
                                        .ok()
                                        .flatten()
                                        .map(Value::String)
                                        .unwrap_or(Value::Null),
                                }
                            }
                        } else {
                            Value::Null
                        };
                        map.insert(col.name().to_string(), cell_val);
                    }
                    json_rows.push(Value::Object(map));
                }
                Ok(json!({
                    "rows": json_rows,
                    "count": rows.len(),
                    "statement": statement_owned,
                }))
            }
            "execute" | "data_write" => {
                let res = sqlx::query(sqlx::AssertSqlSafe(statement_owned.clone()))
                    .execute(&pool)
                    .await
                    .map_err(|e| {
                        ConnectorError::invalid_input(format!("SQLite write error: {e}"), None)
                    })?;
                Ok(json!({
                    "rows_affected": res.rows_affected(),
                    "last_insert_id": res.last_insert_rowid(),
                    "statement": statement_owned,
                }))
            }
            other => Err(ConnectorError::forbidden(
                codes::OPERATION_FORBIDDEN,
                "dispatch",
                format!("Unsupported SQLite operation '{other}'"),
                EffectStatus::NotStarted,
            )),
        }
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
            let target_conn = self.resolve_connection_string(identity, &input);

            match operation {
                "query" | "data_query" => {
                    let statement = input
                        .get("sql")
                        .or_else(|| input.get("source"))
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            ConnectorError::invalid_input(
                                "SQLite query requires 'sql' or 'source'",
                                Some(json!({"pointer": "/sql"})),
                            )
                        })?;
                    let mut metadata = BTreeMap::new();
                    metadata.insert("driver".to_string(), json!(DRIVER_SQLITE));
                    metadata.insert("session".to_string(), json!(handle.session()));
                    let data = self
                        .execute_live_sqlite_for_connection(
                            Some(&conn_key),
                            &target_conn,
                            operation,
                            statement,
                        )
                        .await?;
                    Ok(ConnectorOutput::with_metadata(data, metadata))
                }
                "execute" | "data_write" => {
                    let statement = input
                        .get("sql")
                        .or_else(|| input.get("target"))
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            ConnectorError::invalid_input(
                                "SQLite write requires 'sql' or 'target'",
                                Some(json!({"pointer": "/sql"})),
                            )
                        })?;
                    let mut metadata = BTreeMap::new();
                    metadata.insert("driver".to_string(), json!(DRIVER_SQLITE));
                    metadata.insert("session".to_string(), json!(handle.session()));
                    let data = self
                        .execute_live_sqlite_for_connection(
                            Some(&conn_key),
                            &target_conn,
                            operation,
                            statement,
                        )
                        .await?;
                    Ok(ConnectorOutput::with_metadata(data, metadata))
                }
                other => Err(ConnectorError::forbidden(
                    codes::OPERATION_FORBIDDEN,
                    "dispatch",
                    format!("Unsupported SQLite operation '{other}'"),
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
    let http = match deployments.clone() {
        Some(store) => Arc::new(HttpProviderBackend::with_deployment_store(store)),
        None => Arc::new(HttpProviderBackend::new()),
    };
    let pg = match deployments.clone() {
        Some(store) => Arc::new(PostgresProviderBackend::with_deployment_store(16, store)),
        None => Arc::new(PostgresProviderBackend::default()),
    };
    let sqlite = match deployments {
        Some(store) => Arc::new(SqliteProviderBackend::with_deployment_store(
            "./data/connectors/sqlite",
            8,
            store,
        )),
        None => Arc::new(SqliteProviderBackend::default()),
    };
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
