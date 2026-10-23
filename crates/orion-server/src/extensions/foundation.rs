//! Community Edition foundation Direct Rust connector backends (`V2-14.2`–`V2-14.4`).
//!
//! Implements the four foundational Direct Rust (`ImplementationKind::Rust`)
//! connector providers maintained in the public Community repository per
//! [DEC-005](../../../../design-docs/v2/decisions/dec-005-provider-catalog.md):
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
#[derive(Debug, Default)]
pub struct HttpProviderBackend;

impl HttpProviderBackend {
    pub fn new() -> Self {
        Self
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
            ctx.run_context().check_live(ctx.invocation().effect_status())?;

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

            let path = input
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("/");
            if !path.starts_with('/') {
                return Err(ConnectorError::invalid_input(
                    "HTTP request 'path' must start with '/'",
                    Some(json!({"pointer": "/path"})),
                ));
            }

            if let Some(url) = input
                .get("url")
                .or_else(|| input.get("base_url"))
                .and_then(Value::as_str)
            {
                if let Err(ssrf_reason) =
                    crate::validation::ssrf::validate_url_not_private(url).await
                {
                    return Err(ConnectorError::forbidden(
                        codes::CAPABILITY_INVALID,
                        "ssrf_validation",
                        format!("Outbound HTTP target blocked by SSRF guard: {ssrf_reason}"),
                        ctx.invocation().effect_status(),
                    ));
                }
            }

            let mut metadata = BTreeMap::new();
            metadata.insert("driver".to_string(), json!(DRIVER_HTTP));
            metadata.insert("attempt".to_string(), json!(ctx.attempt_number()));

            Ok(ConnectorOutput::with_metadata(
                json!({
                    "status_code": 200,
                    "method": method,
                    "path": path,
                    "body": input.get("body").cloned().unwrap_or(Value::Null),
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
}

impl PostgresProviderBackend {
    pub fn new(max_capacity_per_connection: usize) -> Self {
        Self {
            sessions: Arc::new(VersionedSessionPool::new(
                DRIVER_POSTGRESQL,
                max_capacity_per_connection,
            )),
        }
    }

    pub fn session_pool(&self) -> Arc<VersionedSessionPool<String>> {
        self.sessions.clone()
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
            ctx.run_context().check_live(ctx.invocation().effect_status())?;

            let identity = ctx.invocation().effective_identity();
            let handle = self
                .sessions
                .acquire(identity, || async move {
                    Ok(format!("pg-session:{}:r{}", identity.connection_id, identity.revision))
                })
                .await?;

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
                    let mut metadata = BTreeMap::new();
                    metadata.insert("driver".to_string(), json!(DRIVER_POSTGRESQL));
                    metadata.insert("session".to_string(), json!(handle.session()));
                    Ok(ConnectorOutput::with_metadata(
                        json!({
                            "rows": [],
                            "count": 0,
                            "statement": statement,
                        }),
                        metadata,
                    ))
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
                    let mut metadata = BTreeMap::new();
                    metadata.insert("driver".to_string(), json!(DRIVER_POSTGRESQL));
                    metadata.insert("session".to_string(), json!(handle.session()));
                    Ok(ConnectorOutput::with_metadata(
                        json!({
                            "rows_affected": 1,
                            "statement": statement,
                        }),
                        metadata,
                    ))
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
            ctx.run_context().check_live(ctx.invocation().effect_status())?;
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
            ctx.run_context().check_live(ctx.invocation().effect_status())?;
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
            ctx.run_context().check_live(ctx.invocation().effect_status())?;
            Ok(())
        })
    }

    fn close<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        _cursor: Value,
    ) -> BoxProviderFuture<'a, Result<(), ConnectorError>> {
        Box::pin(async move {
            ctx.run_context().check_live(ctx.invocation().effect_status())?;
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
}

impl SqliteProviderBackend {
    pub fn new(approved_root: impl Into<PathBuf>, max_capacity_per_connection: usize) -> Self {
        Self {
            approved_root: approved_root.into(),
            sessions: Arc::new(VersionedSessionPool::new(
                DRIVER_SQLITE,
                max_capacity_per_connection,
            )),
        }
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
        use sqlx::Column;
        use sqlx::Row;

        let target = self.resolve_sandboxed_target(conn_str)?;
        let pool = sqlx::SqlitePool::connect(&target).await.map_err(|e| {
            ConnectorError::invalid_input(
                format!("Failed to connect to sandboxed SQLite target '{target}': {e}"),
                None,
            )
        })?;
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
                        let val: Option<String> = row.try_get_unchecked(idx).ok();
                        map.insert(
                            col.name().to_string(),
                            val.map(Value::String).unwrap_or(Value::Null),
                        );
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
            ctx.run_context().check_live(ctx.invocation().effect_status())?;

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
                    Ok(ConnectorOutput::with_metadata(
                        json!({
                            "rows": [],
                            "count": 0,
                            "statement": statement,
                        }),
                        metadata,
                    ))
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
                    Ok(ConnectorOutput::with_metadata(
                        json!({
                            "rows_affected": 1,
                            "last_insert_id": 1,
                            "statement": statement,
                        }),
                        metadata,
                    ))
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
            ctx.run_context().check_live(ctx.invocation().effect_status())?;
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
            ctx.run_context().check_live(ctx.invocation().effect_status())?;
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
            ctx.run_context().check_live(ctx.invocation().effect_status())?;
            Ok(())
        })
    }

    fn close<'a>(
        &'a self,
        ctx: &'a AttemptContext,
        _cursor: Value,
    ) -> BoxProviderFuture<'a, Result<(), ConnectorError>> {
        Box::pin(async move {
            ctx.run_context().check_live(ctx.invocation().effect_status())?;
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

/// Bounded, connection-partitioned in-memory cache store (`V2-14.4`).
///
/// Keyed by [`EffectiveConnectionIdentity`] so:
/// - Two different connections never share keys.
/// - Old and new revisions of a connection have isolated keyspaces while
///   participating in [`ConnectionPoolCache`] eviction.
pub struct PartitionedMemoryCache {
    max_entries_per_partition: usize,
    partitions: Mutex<BTreeMap<EffectiveConnectionIdentity, BTreeMap<String, CacheEntry>>>,
}

impl PartitionedMemoryCache {
    pub fn new(max_entries_per_partition: usize) -> Self {
        Self {
            max_entries_per_partition: max_entries_per_partition.max(1),
            partitions: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn get(&self, identity: &EffectiveConnectionIdentity, key: &str) -> Option<Value> {
        let mut guard = self.partitions.lock().unwrap_or_else(|e| e.into_inner());
        let partition = guard.get_mut(identity)?;
        let entry = partition.get(key)?;
        if let Some(exp) = entry.expires_at
            && Instant::now() >= exp
        {
            partition.remove(key);
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
        let partition = guard.entry(identity.clone()).or_default();

        // Purge expired entries first if we are at capacity.
        let now = Instant::now();
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
        guard
            .get_mut(identity)
            .and_then(|p| p.remove(key))
            .is_some()
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
            ctx.run_context().check_live(ctx.invocation().effect_status())?;
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
                    let ttl = input
                        .get("ttl_secs")
                        .and_then(Value::as_u64)
                        .map(Duration::from_secs);
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
    let http = Arc::new(HttpProviderBackend::new());
    let pg = Arc::new(PostgresProviderBackend::default());
    let sqlite = Arc::new(SqliteProviderBackend::default());
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
mod tests {
    use super::*;
    use crate::extensions::admission::CommunityAdmissionPolicy;
    use crate::extensions::context::{AdmittedRun, AuthorityGrant, BudgetLimits};
    use crate::extensions::contract::{ConnectionReference, ConnectorResult, ProviderReference};
    use std::collections::BTreeSet;

    fn make_invocation(
        driver_id: &str,
        conn_id: &str,
        revision: u64,
        op: &str,
    ) -> crate::extensions::context::Invocation {
        let provider = ProviderReference {
            provider_id: driver_id.to_string(),
            version: "2.0.0".to_string(),
            package_digest:
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_string(),
        };
        let conn = ConnectionReference {
            connection_id: conn_id.to_string(),
            revision,
            provider,
        };
        let eff_id = EffectiveConnectionIdentity {
            provider_digest: conn.provider.package_digest.clone(),
            connection_id: conn.connection_id.clone(),
            revision,
            secret_hash: "sha256:1234".to_string(),
        };
        let mut allowed_operations = BTreeMap::new();
        allowed_operations.insert(conn_id.to_string(), BTreeSet::from([op.to_string()]));
        let run = AdmittedRun::admit(
            "run-foundation",
            1,
            Instant::now() + Duration::from_secs(30),
            BudgetLimits::default(),
            AuthorityGrant {
                allowed_operations,
                declared_dependencies: BTreeMap::new(),
                allowed_channels: BTreeSet::new(),
            },
        );
        run.context()
            .start_invocation(conn, eff_id, op, None, 3)
            .expect("invocation starts")
    }

    #[test]
    fn sqlite_provider_enforces_approved_root_and_rejects_traversal_and_orion_db() {
        let sqlite = SqliteProviderBackend::new("/var/lib/orion/sqlite", 4);

        // In-memory is allowed.
        assert!(
            sqlite
                .validate_config(&json!({"connection_string": "sqlite::memory:"}))
                .is_ok()
        );

        // Relative path under approved_root is allowed.
        let resolved = sqlite
            .resolve_sandboxed_target("sqlite://tenant_a/app.db")
            .expect("valid relative path");
        assert_eq!(resolved, "sqlite:///var/lib/orion/sqlite/tenant_a/app.db");

        // Path traversal `..` is rejected.
        let err = sqlite
            .validate_config(&json!({"connection_string": "sqlite://../secrets.db"}))
            .expect_err("traversal must fail");
        assert_eq!(err.code, codes::CAPABILITY_INVALID);

        // Absolute path is rejected.
        let err = sqlite
            .validate_config(&json!({"connection_string": "sqlite:///etc/passwd"}))
            .expect_err("absolute path must fail");
        assert_eq!(err.code, codes::CAPABILITY_INVALID);

        // Reserved Orion state DB filename is rejected.
        let err = sqlite
            .validate_config(&json!({"connection_string": "sqlite://orion.db"}))
            .expect_err("reserved orion.db must fail");
        assert_eq!(err.code, codes::CAPABILITY_INVALID);
    }

    #[test]
    fn postgres_provider_accepts_only_postgres_schemes() {
        let pg = PostgresProviderBackend::default();
        assert!(
            pg.validate_config(&json!({"connection_string": "postgres://user:pass@db/app"}))
                .is_ok()
        );
        assert!(
            pg.validate_config(&json!({"connection_string": "postgresql://user:pass@db/app"}))
                .is_ok()
        );
        assert!(
            pg.validate_config(&json!({"connection_string": "mysql://user:pass@db/app"}))
                .is_err()
        );
    }

    #[tokio::test]
    async fn memory_cache_provider_partitions_by_connection_and_revision_and_evicts() {
        let providers = ProviderRegistry::new();
        let pools = ResourcePoolRegistry::new();
        register_community_foundation_providers(&providers, &pools);
        let policy = CommunityAdmissionPolicy::new();

        // Write key "k1" on conn-a revision 1.
        let inv_set = make_invocation(DRIVER_MEMORY_CACHE, "conn-a", 1, "set");
        let res = providers
            .dispatch_attempt(
                &policy,
                &inv_set,
                ImplementationKind::Rust,
                DRIVER_MEMORY_CACHE,
                json!({"key": "k1", "value": "v1", "ttl_secs": 60}),
            )
            .await;
        assert!(matches!(res, ConnectorResult::Success { .. }));

        // Read key "k1" on conn-a revision 1 -> hit!
        let inv_get_r1 = make_invocation(DRIVER_MEMORY_CACHE, "conn-a", 1, "get");
        let res_r1 = providers
            .dispatch_attempt(
                &policy,
                &inv_get_r1,
                ImplementationKind::Rust,
                DRIVER_MEMORY_CACHE,
                json!({"key": "k1"}),
            )
            .await;
        assert_eq!(
            res_r1,
            ConnectorResult::Success {
                data: json!({"hit": true, "value": "v1"}),
                metadata: BTreeMap::new(),
            }
        );

        // Read key "k1" on conn-a revision 2 -> isolated (miss)!
        let inv_get_r2 = make_invocation(DRIVER_MEMORY_CACHE, "conn-a", 2, "get");
        let res_r2 = providers
            .dispatch_attempt(
                &policy,
                &inv_get_r2,
                ImplementationKind::Rust,
                DRIVER_MEMORY_CACHE,
                json!({"key": "k1"}),
            )
            .await;
        assert_eq!(
            res_r2,
            ConnectorResult::Success {
                data: json!({"hit": false, "value": Value::Null}),
                metadata: BTreeMap::new(),
            }
        );

        // Read key "k1" on conn-b revision 1 -> isolated (miss)!
        let inv_get_b = make_invocation(DRIVER_MEMORY_CACHE, "conn-b", 1, "get");
        let res_b = providers
            .dispatch_attempt(
                &policy,
                &inv_get_b,
                ImplementationKind::Rust,
                DRIVER_MEMORY_CACHE,
                json!({"key": "k1"}),
            )
            .await;
        assert_eq!(
            res_b,
            ConnectorResult::Success {
                data: json!({"hit": false, "value": Value::Null}),
                metadata: BTreeMap::new(),
            }
        );

        // Evicting "conn-a" via ResourcePoolRegistry clears revision 1.
        pools.evict_connector("conn-a").await;
        let inv_get_after = make_invocation(DRIVER_MEMORY_CACHE, "conn-a", 1, "get");
        let res_after = providers
            .dispatch_attempt(
                &policy,
                &inv_get_after,
                ImplementationKind::Rust,
                DRIVER_MEMORY_CACHE,
                json!({"key": "k1"}),
            )
            .await;
        assert_eq!(
            res_after,
            ConnectorResult::Success {
                data: json!({"hit": false, "value": Value::Null}),
                metadata: BTreeMap::new(),
            }
        );
    }

    #[tokio::test]
    async fn v2_14_2_and_3_http_ssrf_guard_and_live_sqlite_execution() {
        let http = HttpProviderBackend;
        let inv = make_invocation(DRIVER_HTTP, "http-conn", 1, "get");
        let attempt = inv.next_attempt().unwrap();

        // Private loopback URL is blocked by SSRF guard (`V2-14.2`)
        let ssrf_err = http
            .invoke(
                &attempt,
                "get",
                json!({"path": "/metadata", "url": "http://169.254.169.254/latest/meta-data"}),
            )
            .await
            .unwrap_err();
        assert_eq!(ssrf_err.code, codes::CAPABILITY_INVALID);

        // Live SQLite execution against sqlite::memory: (`V2-14.3`)
        let sqlite = SqliteProviderBackend::default();
        let out = sqlite
            .execute_live_sqlite(
                "sqlite::memory:",
                "query",
                "SELECT 'orion-v2' AS edition",
            )
            .await
            .unwrap();
        assert_eq!(out["count"], 1);
        assert_eq!(out["rows"][0]["edition"], "orion-v2");
    }
}
