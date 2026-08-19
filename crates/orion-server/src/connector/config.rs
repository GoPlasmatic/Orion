use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A parsed connector configuration.
///
/// `PartialEq` runs the whole tree and is load-bearing, not a convenience:
/// [`crate::connector::ConnectorRegistry::load_from_repo`] compares the map it
/// just built against the one it holds and only advances
/// `config_generation` when they differ, which is what keeps an epoch resync
/// that touched no connector from invalidating every channel's cached runtime
/// (N17). Every field of every variant therefore has to take part — a field
/// left out of the comparison would be a config change the channel registry
/// never notices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ConnectorConfig {
    Http(HttpConnectorConfig),
    Kafka(KafkaConnectorConfig),
    Db(DbConnectorConfig),
    Cache(CacheConnectorConfig),
    Es(EsConnectorConfig),
    Smtp(SmtpConnectorConfig),
    Storage(StorageConnectorConfig),
}

impl ConnectorConfig {
    /// The wire `type` this config is. The stored `connectors.connector_type`
    /// column says the same thing, but the registry holds parsed configs — this
    /// is how a caller with only the parsed form (F52's activation-time type
    /// check) asks what it got.
    pub fn connector_type(&self) -> ConnectorType {
        match self {
            ConnectorConfig::Http(_) => ConnectorType::Http,
            ConnectorConfig::Kafka(_) => ConnectorType::Kafka,
            ConnectorConfig::Db(_) => ConnectorType::Db,
            ConnectorConfig::Cache(_) => ConnectorType::Cache,
            ConnectorConfig::Es(_) => ConnectorType::Es,
            ConnectorConfig::Smtp(_) => ConnectorType::Smtp,
            ConnectorConfig::Storage(_) => ConnectorType::Storage,
        }
    }

    /// Whether this is a MongoDB connector, i.e. a `db` connector whose
    /// connection string names the Mongo scheme. Both backends share the `Db`
    /// variant, so the type alone does not tell them apart.
    pub fn is_mongo(&self) -> bool {
        matches!(self, ConnectorConfig::Db(c) if is_mongo_url(&c.connection_string))
    }

    /// The db/es operation gates. The other three connector types carry gates
    /// of their own shape — [`CacheOperationGates`], [`KafkaOperationGates`]
    /// and [`HttpOperationGates`] — reached through their typed configs,
    /// because a `publish` flag on a database or a `raw_write` flag on a cache
    /// would be a field nothing reads (the defect F22 closed).
    pub fn operation_gates(&self) -> Option<&OperationGates> {
        match self {
            ConnectorConfig::Db(c) => Some(&c.operations),
            ConnectorConfig::Es(c) => Some(&c.operations),
            _ => None,
        }
    }

    /// The portable-dialect guards for connectors that carry them (db, es).
    pub fn dialect_guards(&self) -> Option<&DialectGuards> {
        match self {
            ConnectorConfig::Db(c) => Some(&c.dialect),
            ConnectorConfig::Es(c) => Some(&c.dialect),
            _ => None,
        }
    }
}

/// Operator-owned bounds on what the portable dialect (`data_query` /
/// `data_write`) may reach through this connector (F24).
///
/// [`OperationGates`] answers *which verbs*; this answers *which tables*. The
/// two are separate because a read-only connector still needs bounding: `read`
/// alone does not stop a workflow author from selecting the whole database.
///
/// Both default to off, because the 1.0 flip of `unmapped` to `reject` already
/// makes the safe mode the default one. These exist for the case that flip
/// cannot cover: a task may still write `"unmapped": "identity"` itself, and
/// only the connector's owner can say that is not allowed here.
/// Unknown keys are rejected (unlike the connector config around it, which
/// must keep loading 0.3.x rows carrying since-removed fields). Nothing
/// pre-1.0 can carry a `dialect` block, and a misspelled key here — the
/// `requireSchema` / `allowed_entites` class of typo — is privileged security
/// configuration silently not applying, the same rationale
/// `query::schema` denies unknown fields for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DialectGuards {
    /// Refuse any dialect call that did not declare a real schema — no
    /// entities, or an explicit `"unmapped": "identity"`. Closes the per-task
    /// opt-out so one forgotten `schema` key cannot reopen the connector.
    pub require_schema: bool,
    /// Physical table/collection/index names this connector may touch. Empty
    /// (the default) means unrestricted. Matched against the name *after*
    /// schema renames apply, so a rename cannot step around it, and it covers
    /// relation targets and many-to-many junction tables as well as the
    /// envelope's own `source`/`target`.
    pub allowed_entities: Vec<String>,
}

impl DialectGuards {
    /// Whether `require_schema` is satisfied by the registry a task supplied.
    ///
    /// "A real schema" means both halves: at least one declared entity, and
    /// allowlist mode. Either alone is empty — an entity list under
    /// `"unmapped": "identity"` bounds nothing, and allowlist mode with no
    /// entities can reach nothing anyway.
    pub fn schema_is_sufficient(&self, declared_entities: bool, identity_mode: bool) -> bool {
        !self.require_schema || (declared_entities && !identity_mode)
    }
}

/// Per-connector operation gates. Everything defaults to allowed; disabling an
/// operation turns the corresponding handler call into a located validation
/// error, so a connector can be made read-only (or insert-only, …) in its
/// config without touching workflows.
///
/// - `read` gates `data_query`, `db_read`, and `mongo_read`.
/// - `insert` / `update` / `delete` / `upsert` gate the matching `data_write` op.
/// - `raw_write` gates the raw-SQL `db_write` escape hatch, which cannot be
///   classified per-op (hand-written SQL may contain any statement).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OperationGates {
    pub read: bool,
    pub insert: bool,
    pub update: bool,
    pub delete: bool,
    pub upsert: bool,
    pub raw_write: bool,
}

impl Default for OperationGates {
    fn default() -> Self {
        Self {
            read: true,
            insert: true,
            update: true,
            delete: true,
            upsert: true,
            raw_write: true,
        }
    }
}

impl OperationGates {
    /// Whether the named operation is enabled. Unknown names are denied
    /// (defensive — callers pass the fixed set above).
    pub fn allows(&self, op: &str) -> bool {
        match op {
            "read" => self.read,
            "insert" => self.insert,
            "update" => self.update,
            "delete" => self.delete,
            "upsert" => self.upsert,
            "raw_write" => self.raw_write,
            _ => false,
        }
    }
}

/// Cache-connector operation gates (F22e). Everything defaults to allowed;
/// disabling an operation turns the corresponding handler call into a located
/// validation error, so a cache connector can be made read-only in its config
/// without touching workflows.
///
/// - `read` gates `cache_read`.
/// - `write` gates `cache_write`.
///
/// There is deliberately no `delete`: the `CacheBackend` trait exposes
/// get / set / set_ex only, no workflow function deletes a key, and a gate
/// over an operation that cannot be performed would be exactly the kind of
/// accepted-but-never-read field F22 removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheOperationGates {
    pub read: bool,
    pub write: bool,
}

impl Default for CacheOperationGates {
    fn default() -> Self {
        Self {
            read: true,
            write: true,
        }
    }
}

/// Kafka-connector operation gates (F22e). The connector is producer-only, so
/// `publish` — gating `publish_kafka` — is the whole surface. Ingest consumers
/// are configured under `[kafka]` in the server config, not here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KafkaOperationGates {
    pub publish: bool,
}

impl Default for KafkaOperationGates {
    fn default() -> Self {
        Self { publish: true }
    }
}

/// HTTP-connector method allow-list (F22e).
///
/// The db/es gates are booleans because their operation set is fixed; an HTTP
/// connector's is its method, so the gate is a list. Empty — the default —
/// means every method `http_call` can issue is allowed, which is what every
/// connector authored before this existed keeps meaning. Naming even one
/// method makes the list exhaustive: a connector pointed at a read-only
/// upstream can be `["GET"]` and no workflow can POST through it.
///
/// Matching is case-insensitive; entries are validated against the methods
/// `http_call` supports when the connector is created or updated, so a typo is
/// a 400 rather than a connector that refuses every call at request time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpOperationGates {
    pub methods: Vec<String>,
}

impl HttpOperationGates {
    /// Whether `method` is allowed. An empty list allows everything.
    pub fn allows_method(&self, method: &str) -> bool {
        self.methods.is_empty()
            || self
                .methods
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(method))
    }
}

/// The methods `http_call` can issue — the vocabulary an
/// [`HttpOperationGates`] allow-list is validated against. Anything else in
/// the list would silently never match, because `HttpCallConfig` cannot
/// produce it.
pub const VALID_HTTP_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpConnectorConfig {
    pub url: String,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub retry: RetryConfig,
    /// Maximum response body size in bytes (default 10 MB). Prevents OOM from large responses.
    /// Retry non-idempotent methods (POST, PATCH) too (F8). Default false:
    /// a timed-out POST may already have been applied, so re-sending it can
    /// duplicate the side effect. Enable only when the endpoint is
    /// idempotent in practice — e.g. it honours an idempotency key the
    /// workflow sets in `headers`.
    #[serde(default)]
    pub retry_non_idempotent: bool,
    #[serde(default = "default_max_response_size")]
    pub max_response_size: usize,
    /// Allow requests to private/internal IP addresses. Default false (SSRF protection).
    #[serde(default)]
    pub allow_private_urls: bool,
    /// Which HTTP methods workflows may issue through this connector.
    #[serde(default)]
    pub operations: HttpOperationGates,
}

fn default_max_response_size() -> usize {
    10 * 1024 * 1024 // 10 MB
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AuthConfig {
    Bearer {
        token: String,
    },
    Basic {
        username: String,
        password: String,
    },
    ApiKey {
        header: String,
        key: String,
    },
    /// Managed OAuth2 (#268): Orion acquires, caches and refreshes the access
    /// token itself — see [`crate::connector::oauth`]. `http` connectors only;
    /// authoring validation refuses it elsewhere. Boxed: the block is an
    /// order of magnitude wider than the static variants, and every stored
    /// `Option<AuthConfig>` would carry that width inline.
    OAuth2(Box<OAuth2Config>),
}

/// The `auth.type = "oauth2"` block (#268). The lifecycle machinery consuming
/// it (cache, margin, single-flight, rotation persistence) is grant-agnostic;
/// `grant` is an **open value set** so a future grant (`jwt-bearer`, token
/// exchange, device code) is a new value with its own request shape, not a new
/// auth type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuth2Config {
    /// `"client_credentials"` or `"refresh_token"` — parsed by
    /// [`OAuth2Grant::parse`], validated at authoring time.
    pub grant: String,
    /// The IdP's token endpoint. Gets the same SSRF validation as the
    /// connector's own endpoint (`allow_private_urls` opts out).
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
    /// How the client authenticates to the token endpoint (RFC 6749 §2.3.1):
    /// `"basic"` (HTTP Basic, the RFC-recommended default) or `"body"`
    /// (`client_id`/`client_secret` as form parameters).
    #[serde(default = "default_client_auth")]
    pub client_auth: String,
    /// The bootstrap **seed** for the `refresh_token` grant. Rotated values
    /// are persisted to `connector_oauth_state`, never written back here; a
    /// state row for the current config fingerprint overrides this seed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Requested scopes, space-joined per RFC 6749 §3.3.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// `audience` token-request parameter (Auth0-style IdPs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    /// `resource` token-request parameter (RFC 8707 / Azure-style IdPs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    /// Escape hatch for provider quirks: extra form parameters on the token
    /// request. Reserved parameter names are refused at authoring time.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extra_params: HashMap<String, String>,
    /// Refresh this many seconds before the token expires. Default 60.
    #[serde(default = "default_refresh_margin_secs")]
    pub refresh_margin_secs: u64,
}

fn default_client_auth() -> String {
    "basic".to_string()
}

fn default_refresh_margin_secs() -> u64 {
    60
}

/// The grants this build implements. An open set: a new grant is a new
/// variant + validation row + request shape in `connector/oauth.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuth2Grant {
    ClientCredentials,
    RefreshToken,
}

impl OAuth2Grant {
    pub const VALUES: &'static str = "client_credentials/refresh_token";

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "client_credentials" => OAuth2Grant::ClientCredentials,
            "refresh_token" => OAuth2Grant::RefreshToken,
            _ => return None,
        })
    }
}

/// The client-authentication spellings of RFC 6749 §2.3.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuth2ClientAuth {
    Basic,
    Body,
}

impl OAuth2ClientAuth {
    pub const VALUES: &'static str = "basic/body";

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "basic" => OAuth2ClientAuth::Basic,
            "body" => OAuth2ClientAuth::Body,
            _ => return None,
        })
    }
}

/// Retry policy for `http_call`. Lives on [`HttpConnectorConfig`] alone —
/// F7 removed the copies on the db and es connectors, which nothing read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryConfig {
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_retry_delay_ms")]
    pub retry_delay_ms: u64,
}

fn default_max_retries() -> u32 {
    3
}

fn default_retry_delay_ms() -> u64 {
    1000
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            retry_delay_ms: default_retry_delay_ms(),
        }
    }
}

/// Kafka connector. Producer-only: `publish_kafka` writes to `topic`.
/// Ingest consumers are configured under `[kafka]` in the server config, not
/// here — which is why there is no `group_id` (proposal F22).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KafkaConnectorConfig {
    pub brokers: Vec<String>,
    pub topic: String,
    /// Allow brokers on private/internal IP addresses. Default false (SSRF
    /// protection, S6). Brokers are bare `host:port` rather than URLs, so
    /// they are checked as host/port pairs rather than parsed as a URL.
    #[serde(default)]
    pub allow_private_urls: bool,
    /// Whether workflows may publish through this connector.
    #[serde(default)]
    pub operations: KafkaOperationGates,
}

/// SQL or MongoDB connector. The backend is selected by the
/// `connection_string` scheme (`postgres://`, `mysql://`, `sqlite:`,
/// `mongodb://`) — there is deliberately no `driver` field, because one
/// existed until 1.0, was never read, and made `driver: "mysql"` with a
/// `postgres://` URL look meaningful when it silently connected to Postgres
/// (proposal F22). Credentials belong in the connection string, which is why
/// there is no `auth` either.
///
/// `retry` was accepted here until 1.0 and never read (proposal F7): the only
/// reader of a [`RetryConfig`] is `http_call`. Re-driving a database call is
/// not the same problem as re-driving an HTTP one — a statement that timed out
/// may already have been applied, exactly the hazard F8 closed for POST — and
/// the pool's own `connect_timeout_ms` already bounds connection setup, so
/// there was nothing safe left for the field to mean. Timeouts and pool sizing
/// are the knobs that exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbConnectorConfig {
    pub connection_string: String,
    #[serde(default)]
    pub max_connections: Option<u32>,
    #[serde(default)]
    pub connect_timeout_ms: Option<u64>,
    #[serde(default)]
    pub query_timeout_ms: Option<u64>,
    /// Allow connecting to private/internal IP addresses. Default false
    /// (SSRF protection, S6) — the same opt-out the HTTP and ES connectors
    /// carry. A database on a private network is the normal case, so this is
    /// the field most deployments will set; it exists so that reaching
    /// `169.254.169.254` or a neighbouring service is a deliberate act rather
    /// than the default. Ignored for `sqlite:`, which opens a file.
    #[serde(default)]
    pub allow_private_urls: bool,
    /// Which operations workflows may run through this connector.
    #[serde(default)]
    pub operations: OperationGates,
    /// Which tables the portable dialect may reach through this connector.
    #[serde(default)]
    pub dialect: DialectGuards,
    /// Permit the aggregation write stages (`$out`/`$merge`) in
    /// `mongo_aggregate` pipelines (#263). Default **false** — the one
    /// deliberate default-deny among the gates, because "aggregation" reads as
    /// a read operation and must not silently write. MongoDB connectors only;
    /// a SQL connector never reaches the aggregate handler.
    #[serde(default)]
    pub aggregate_write_stages: bool,
}

/// Cache connector. `default_ttl_secs`, `max_connections`, `auth` and `retry`
/// were accepted here until 1.0 and never read (proposal F22): TTL comes from
/// each `cache_write` task, Redis credentials go in the `url`, and the
/// connection manager has neither a pool size nor a retry policy to configure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheConnectorConfig {
    /// Cache backend: `"redis"` or `"memory"`. Required — no default.
    pub backend: String,
    /// Connection URL. Required when `backend = "redis"`, ignored for `"memory"`.
    /// Carries credentials when Redis needs them: `redis://user:pass@host:6379`.
    #[serde(default)]
    pub url: Option<String>,
    /// Allow connecting to private/internal IP addresses. Default false
    /// (SSRF protection, S6). Redis is usually private, so this is commonly
    /// set; the point is that it is stated rather than assumed. Ignored for
    /// `backend = "memory"`, which opens no socket.
    #[serde(default)]
    pub allow_private_urls: bool,
    /// Which operations workflows may run through this connector.
    #[serde(default)]
    pub operations: CacheOperationGates,
}

/// Elasticsearch connector: a REST endpoint queried by the `data_query` handler
/// (executed via the shared HTTP client — no dedicated ES driver).
///
/// `retry` was accepted here until 1.0 and never read (proposal F7). The
/// dialect drives `_bulk` / `_update_by_query` / `_delete_by_query` through
/// this connector as well as `_search`, and none of those are safe to re-send
/// blind on a timeout — see the note on [`DbConnectorConfig`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EsConnectorConfig {
    /// Base URL, e.g. `http://localhost:9200`.
    pub url: String,
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub request_timeout_ms: Option<u64>,
    /// Allow requests to private/internal IP addresses. Default false (SSRF protection).
    #[serde(default)]
    pub allow_private_urls: bool,
    /// Maximum response body size in bytes (F12), same default as the HTTP
    /// connector — an unbounded ES response was the one egress read left
    /// without a cap.
    #[serde(default = "default_max_response_size")]
    pub max_response_size: usize,
    /// Which operations workflows may run through this connector.
    #[serde(default)]
    pub operations: OperationGates,
    /// Which indices the portable dialect may reach through this connector.
    #[serde(default)]
    pub dialect: DialectGuards,
}

/// SMTP connector (#262): the transport and credentials behind `send_email`.
/// Message logic (recipients, subject, bodies) lives on the task, matching
/// the transport/logic split every other connector type uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmtpConnectorConfig {
    /// SMTP server hostname or IP — a host, not a URL.
    pub host: String,
    /// 587 (submission/STARTTLS) by default; 465 pairs with `implicit`, 25
    /// with internal relays.
    #[serde(default = "default_smtp_port")]
    pub port: u16,
    /// Connection security. There is deliberately no skip-verification knob:
    /// certificates validate against the platform trust store, so private-CA
    /// relays work by installing the CA at the OS level.
    #[serde(default)]
    pub tls: SmtpTls,
    /// `none` (internal smarthosts) or `basic`; future mechanisms (xoauth2)
    /// are new tagged variants here, invisible to workflows.
    #[serde(default)]
    pub auth: SmtpAuth,
    /// Default sender. Accepts `addr@example.com` or `Name <addr@example.com>`.
    pub from: String,
    /// Whether a task-level `from` may override the default. Off by default:
    /// most submission servers enforce the envelope sender anyway, and an
    /// override should be a connector decision, not a workflow's.
    #[serde(default)]
    pub allow_from_override: bool,
    /// Allow connecting to private/internal addresses. Default false, the
    /// same S6 posture as every other connector endpoint — internal relays
    /// are common and opt in explicitly.
    #[serde(default)]
    pub allow_private_urls: bool,
    /// Per-send timeout in milliseconds (connect + protocol exchange).
    #[serde(default = "default_smtp_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_smtp_port() -> u16 {
    587
}

fn default_smtp_timeout_ms() -> u64 {
    10_000
}

/// How the SMTP connection is secured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SmtpTls {
    /// Plaintext connect, upgrade via STARTTLS (port 587 convention).
    #[default]
    Starttls,
    /// TLS from the first byte (port 465 convention).
    Implicit,
    /// No TLS at all — dev/localhost relays only; flagged by validation.
    None,
}

/// SMTP authentication. Tagged like the HTTP connector's `AuthConfig`, so new
/// mechanisms are additive variants.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SmtpAuth {
    /// Unauthenticated — internal smarthosts that trust by network position.
    #[default]
    None,
    /// LOGIN/PLAIN with a username and password (secret references accepted).
    Basic { username: String, password: String },
}

/// Object-storage connector (#265): a scoped, zero-data-path surface —
/// `storage_presign` computes URLs locally over these credentials, and
/// `storage_head` makes one bounded metadata request. Object bytes never move
/// through the runtime; that invariant is the type's scoping rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageConnectorConfig {
    /// Which signing scheme the store speaks. `s3` covers every S3-compatible
    /// store (AWS, Linode, Garage, SeaweedFS, RustFS, R2, Wasabi); GCS signed
    /// URLs or Azure SAS later are new tagged values here — the functions
    /// never change.
    #[serde(default)]
    pub provider: StorageProvider,
    /// Base endpoint URL, e.g. `https://ap-south-1.linodeobjects.com`.
    pub endpoint: String,
    /// Signing region (SigV4 credential scope).
    pub region: String,
    /// The bucket this connector reaches. Deliberately connector-owned, not a
    /// task field: the connector is the unit of scoping and gating, and a
    /// task-level override would silently widen what a workflow can reach.
    pub bucket: String,
    /// Access key id — a credential identifier; masked on reads like its
    /// secret half, so use `env://` references, which survive export/import.
    pub access_key: String,
    /// Secret access key; literal or `env://VAR`.
    pub secret_key: String,
    /// STS temporary-credential session token, signed as
    /// `X-Amz-Security-Token` when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
    /// Path-style addressing (`endpoint/bucket/key`) instead of
    /// virtual-hosted (`bucket.endpoint/key`). Most self-hosted stores
    /// (Garage, SeaweedFS, RustFS) want `true`.
    #[serde(default)]
    pub force_path_style: bool,
    /// Allow a private/internal endpoint for `storage_head`'s network call —
    /// the S6 posture of every connector endpoint; a LAN-hosted store opts in.
    #[serde(default)]
    pub allow_private_urls: bool,
    /// `storage_head` request timeout in milliseconds. Presigning makes no
    /// network call and ignores this.
    #[serde(default = "default_storage_timeout_ms")]
    pub timeout_ms: u64,
    /// Which operations workflows may run through this connector.
    #[serde(default)]
    pub operations: StorageOperationGates,
}

fn default_storage_timeout_ms() -> u64 {
    10_000
}

impl StorageConnectorConfig {
    /// The `(scheme, host, canonical path)` for one object key — or the
    /// bucket root when `key` is `None` (the connectivity probe). Addressing
    /// style is decided here and nowhere else: virtual-hosted
    /// (`bucket.endpoint`) by default, path-style under `force_path_style`.
    pub fn address(&self, key: Option<&str>) -> Result<(String, String, String), String> {
        let url = url::Url::parse(&self.endpoint)
            .map_err(|e| format!("storage endpoint '{}' is not a URL: {e}", self.endpoint))?;
        let scheme = url.scheme().to_string();
        let host = url
            .host_str()
            .ok_or_else(|| format!("storage endpoint '{}' has no host", self.endpoint))?;
        let host_port = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        let key_path = match key {
            Some(k) => crate::connector::sigv4::encode_path(k),
            None => "/".to_string(),
        };
        Ok(if self.force_path_style {
            let bucket = crate::connector::sigv4::uri_encode(&self.bucket, false);
            let path = if key_path == "/" {
                format!("/{bucket}")
            } else {
                format!("/{bucket}{key_path}")
            };
            (scheme, host_port, path)
        } else {
            (scheme, format!("{}.{host_port}", self.bucket), key_path)
        })
    }
}

/// The signing scheme a storage connector speaks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageProvider {
    #[default]
    S3,
}

/// Storage-connector operation gates (F22e). Everything defaults to allowed;
/// `presign_put: false` makes a media connector read-only in its config
/// without touching workflows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageOperationGates {
    pub presign_get: bool,
    pub presign_put: bool,
    pub head: bool,
}

impl Default for StorageOperationGates {
    fn default() -> Self {
        Self {
            presign_get: true,
            presign_put: true,
            head: true,
        }
    }
}

/// A connection string targets MongoDB when it uses a `mongodb` scheme;
/// otherwise it is a SQL connector (dialect from the URL scheme). The `db`
/// connector type covers both, so this is the only thing that separates them.
pub fn is_mongo_url(connection_string: &str) -> bool {
    connection_string.starts_with("mongodb://") || connection_string.starts_with("mongodb+srv://")
}

/// Allowed connector type values.
pub const VALID_CONNECTOR_TYPES: &[&str] =
    &["http", "kafka", "db", "cache", "es", "smtp", "storage"];

/// Allowed cache backend values.
pub const VALID_CACHE_BACKENDS: &[&str] = &["redis", "memory"];

/// Typed enum for the connector `type` field on create/update requests.
/// Wire format is lowercase ("http", "kafka", "db", "cache", "es");
/// deserialization is case-insensitive so "HTTP" or "Kafka" also parse.
///
/// `"storage"` was accepted through the whole 0.x line with no handler behind
/// it and was removed in 1.0 (proposal F15); `load_from_repo` reports stored
/// rows as a load issue naming the removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ConnectorType {
    Http,
    Kafka,
    Db,
    Cache,
    Es,
    Smtp,
    Storage,
}

impl ConnectorType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Kafka => "kafka",
            Self::Db => "db",
            Self::Cache => "cache",
            Self::Es => "es",
            Self::Smtp => "smtp",
            Self::Storage => "storage",
        }
    }

    /// The keys this type's `operations` object may carry — the gate
    /// vocabulary its handlers actually read.
    ///
    /// Connector configs deliberately do not use `deny_unknown_fields` (a
    /// 0.3.x row carrying a since-removed field has to keep loading — see
    /// `legacy_configs_with_removed_fields_still_load`), and every gate struct
    /// is `#[serde(default)]`. So `{"operations": {"writes": false}}` on a
    /// cache would deserialize into a *fully open* gate: accepted, stored, and
    /// silently doing nothing. For a control whose promise is "regardless of
    /// what any workflow asks for", that is the worst possible failure mode,
    /// so `validation::connectors` checks the keys against this list at the
    /// admin door. Stored configs are never re-validated, so nothing that
    /// already loads stops loading.
    pub fn operation_gate_keys(self) -> &'static [&'static str] {
        match self {
            Self::Db | Self::Es => &["read", "insert", "update", "delete", "upsert", "raw_write"],
            Self::Cache => &["read", "write"],
            Self::Kafka => &["publish"],
            Self::Http => &["methods"],
            // Send-only: there is exactly one operation, so a gate would be
            // an accepted-but-never-read field. Disable the connector instead.
            Self::Smtp => &[],
            Self::Storage => &["presign_get", "presign_put", "head"],
        }
    }
}

impl std::fmt::Display for ConnectorType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for ConnectorType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.to_ascii_lowercase().as_str() {
            "http" => Ok(Self::Http),
            "kafka" => Ok(Self::Kafka),
            "db" => Ok(Self::Db),
            "cache" => Ok(Self::Cache),
            "es" => Ok(Self::Es),
            "smtp" => Ok(Self::Smtp),
            "storage" => Ok(Self::Storage),
            other => Err(serde::de::Error::unknown_variant(
                other,
                VALID_CONNECTOR_TYPES,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vocabulary the admin door validates against must be exactly the
    /// fields the gate structs carry. A gate added to a struct but forgotten
    /// here would be refused as an unknown key; one left here after being
    /// removed would be accepted and ignored — the silent no-op the key check
    /// exists to prevent.
    #[test]
    fn operation_gate_keys_match_the_gate_structs() {
        fn fields<T: Serialize>(value: &T) -> std::collections::BTreeSet<String> {
            serde_json::to_value(value)
                .expect("gates serialize")
                .as_object()
                .expect("gates are a JSON object")
                .keys()
                .cloned()
                .collect()
        }
        fn declared(t: ConnectorType) -> std::collections::BTreeSet<String> {
            t.operation_gate_keys()
                .iter()
                .map(|k| (*k).to_string())
                .collect()
        }

        for connector_type in [ConnectorType::Db, ConnectorType::Es] {
            assert_eq!(
                fields(&OperationGates::default()),
                declared(connector_type),
                "{connector_type}"
            );
        }
        assert_eq!(
            fields(&CacheOperationGates::default()),
            declared(ConnectorType::Cache)
        );
        assert_eq!(
            fields(&KafkaOperationGates::default()),
            declared(ConnectorType::Kafka)
        );
        assert_eq!(
            fields(&HttpOperationGates::default()),
            declared(ConnectorType::Http)
        );
    }

    #[test]
    fn test_valid_connector_types() {
        assert!(VALID_CONNECTOR_TYPES.contains(&"http"));
        assert!(VALID_CONNECTOR_TYPES.contains(&"kafka"));
        assert!(!VALID_CONNECTOR_TYPES.contains(&"grpc"));
    }

    #[test]
    fn test_retry_config_default() {
        let config = RetryConfig::default();
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.retry_delay_ms, 1000);
    }

    #[test]
    fn test_connector_config_deserialization_http() {
        let json = r#"{"type":"http","url":"https://api.example.com","headers":{},"retry":{"max_retries":2,"retry_delay_ms":500}}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Http(http) => {
                assert_eq!(http.url, "https://api.example.com");
                assert_eq!(http.retry.max_retries, 2);
                assert_eq!(http.retry.retry_delay_ms, 500);
                assert_eq!(http.max_response_size, 10 * 1024 * 1024);
            }
            _ => unreachable!("Expected Http config"),
        }
    }

    #[test]
    fn test_connector_config_deserialization_kafka() {
        let json = r#"{"type":"kafka","brokers":["localhost:9092"],"topic":"test-topic","group_id":"test-group"}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Kafka(kafka) => {
                assert_eq!(kafka.brokers, vec!["localhost:9092"]);
                assert_eq!(kafka.topic, "test-topic");
            }
            _ => unreachable!("Expected Kafka config"),
        }
    }

    #[test]
    fn test_connector_config_deserialization_db() {
        let json =
            r#"{"type":"db","connection_string":"postgres://localhost/mydb","max_connections":5}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Db(db) => {
                assert_eq!(db.connection_string, "postgres://localhost/mydb");
                assert_eq!(db.max_connections, Some(5));
            }
            _ => unreachable!("Expected Db config"),
        }
    }

    #[test]
    fn test_connector_config_deserialization_cache_redis() {
        let json = r#"{"type":"cache","backend":"redis","url":"redis://localhost:6379","default_ttl_secs":300}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Cache(cache) => {
                assert_eq!(cache.backend, "redis");
                assert_eq!(cache.url, Some("redis://localhost:6379".to_string()));
            }
            _ => unreachable!("Expected Cache config"),
        }
    }

    #[test]
    fn test_connector_config_deserialization_cache_memory() {
        let json = r#"{"type":"cache","backend":"memory","default_ttl_secs":60}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Cache(cache) => {
                assert_eq!(cache.backend, "memory");
                assert!(cache.url.is_none());
            }
            _ => unreachable!("Expected Cache config"),
        }
    }

    /// F22 removed seven connector fields that were accepted, persisted, and
    /// never read; F7 removed the last two (`retry` on db and es). Connector
    /// configs deliberately do **not** use `deny_unknown_fields`, so a 0.3.x
    /// row carrying them still loads and the keys are ignored — which is what
    /// they always effectively were. Without this, every stored connector using
    /// one would fail to deserialize on upgrade and drop out of the registry.
    #[test]
    fn legacy_configs_with_removed_fields_still_load() {
        let db = r#"{"type":"db","connection_string":"postgres://localhost/mydb",
                     "driver":"mysql","retry":{"max_retries":5,"retry_delay_ms":250},
                     "auth":{"type":"basic","username":"u","password":"p"}}"#;
        match serde_json::from_str::<ConnectorConfig>(db).expect("0.3.x db config must still load")
        {
            ConnectorConfig::Db(db) => {
                assert_eq!(db.connection_string, "postgres://localhost/mydb");
            }
            _ => unreachable!("Expected Db config"),
        }

        let cache = r#"{"type":"cache","backend":"redis","url":"redis://localhost:6379",
                        "default_ttl_secs":300,"max_connections":10,"retry":{"max_retries":3}}"#;
        match serde_json::from_str::<ConnectorConfig>(cache)
            .expect("0.3.x cache config must still load")
        {
            ConnectorConfig::Cache(c) => assert_eq!(c.backend, "redis"),
            _ => unreachable!("Expected Cache config"),
        }

        let kafka = r#"{"type":"kafka","brokers":["b:9092"],"topic":"t","group_id":"g"}"#;
        match serde_json::from_str::<ConnectorConfig>(kafka)
            .expect("0.3.x kafka config must still load")
        {
            ConnectorConfig::Kafka(k) => assert_eq!(k.topic, "t"),
            _ => unreachable!("Expected Kafka config"),
        }

        let es = r#"{"type":"es","url":"http://localhost:9200",
                     "retry":{"max_retries":9,"retry_delay_ms":100}}"#;
        match serde_json::from_str::<ConnectorConfig>(es).expect("0.3.x es config must still load")
        {
            ConnectorConfig::Es(es) => assert_eq!(es.url, "http://localhost:9200"),
            _ => unreachable!("Expected Es config"),
        }
    }

    /// F7: `retry` on a db/es connector was declared, validated and documented
    /// while the only reader of a `RetryConfig` is `http_call`. An operator who
    /// set `max_retries: 5` on their Postgres connector got exactly nothing.
    /// It now belongs to the HTTP connector alone, and the round-trip is where
    /// that shows: `GET /admin/connectors` renders these structs, so a field
    /// re-added without a consumer would start advertising itself again.
    #[test]
    fn only_the_http_connector_advertises_a_retry_policy() {
        let rendered = |json: &str| {
            let config: ConnectorConfig = serde_json::from_str(json).expect("config parses");
            serde_json::to_value(&config).expect("config serializes")
        };

        assert!(
            !rendered(r#"{"type":"db","connection_string":"sqlite::memory:"}"#)["retry"]
                .is_object(),
            "a db connector must not advertise a retry policy nothing applies"
        );
        assert!(
            !rendered(r#"{"type":"es","url":"http://localhost:9200"}"#)["retry"].is_object(),
            "an es connector must not advertise a retry policy nothing applies"
        );
        assert!(
            !rendered(r#"{"type":"cache","backend":"memory"}"#)["retry"].is_object(),
            "a cache connector must not advertise a retry policy nothing applies"
        );
        // http_call is the one reader, so this one stays.
        assert_eq!(
            rendered(r#"{"type":"http","url":"https://example.com"}"#)["retry"]["max_retries"],
            3
        );
    }

    #[test]
    fn test_connector_config_deserialization_cache_missing_backend() {
        // backend is required — deserialization should fail
        let json = r#"{"type":"cache","url":"redis://localhost:6379"}"#;
        let result = serde_json::from_str::<ConnectorConfig>(json);
        assert!(result.is_err());
    }

    /// `storage` spent 0.x as an accepted type with no handler (F15) and was
    /// removed in 1.0. #265 reinstates the name *with* a handler — which is
    /// what the removal message said was missing — behind a real config
    /// shape, so a bare 0.x-style row (no endpoint/region/credentials) still
    /// fails to parse rather than loading as a half-configured connector.
    #[test]
    fn storage_connector_type_parses_with_the_265_shape() {
        let json = r#"{"type":"storage","provider":"s3","bucket":"my-bucket"}"#;
        serde_json::from_str::<ConnectorConfig>(json)
            .expect_err("a 0.x-style storage row lacks the required fields");

        let json = r#"{"type":"storage","endpoint":"https://ap-south-1.linodeobjects.com",
            "region":"ap-south-1","bucket":"media","access_key":"AK","secret_key":"SK"}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Storage(storage) => {
                assert_eq!(storage.provider, StorageProvider::S3);
                assert!(!storage.force_path_style);
                assert!(storage.operations.presign_get);
            }
            _ => unreachable!("expected storage config"),
        }
        assert!(VALID_CONNECTOR_TYPES.contains(&"storage"));
    }

    #[test]
    fn test_connector_config_deserialization_es() {
        let json = r#"{"type":"es","url":"http://localhost:9200"}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Es(es) => {
                assert_eq!(es.url, "http://localhost:9200");
                assert!(es.auth.is_none());
                assert!(!es.allow_private_urls);
            }
            _ => unreachable!("Expected Es config"),
        }
    }

    #[test]
    fn test_valid_connector_types_expanded() {
        assert!(VALID_CONNECTOR_TYPES.contains(&"http"));
        assert!(VALID_CONNECTOR_TYPES.contains(&"kafka"));
        assert!(VALID_CONNECTOR_TYPES.contains(&"db"));
        assert!(VALID_CONNECTOR_TYPES.contains(&"cache"));
        assert!(VALID_CONNECTOR_TYPES.contains(&"es"));
        assert!(!VALID_CONNECTOR_TYPES.contains(&"grpc"));
    }

    #[test]
    fn test_operation_gates_default_all_allowed() {
        let json = r#"{"type":"db","connection_string":"sqlite::memory:"}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        let gates = config.operation_gates().expect("db has gates");
        for op in ["read", "insert", "update", "delete", "upsert", "raw_write"] {
            assert!(gates.allows(op), "{op} should default to allowed");
        }
        assert!(!gates.allows("unknown"), "unknown ops are denied");
    }

    #[test]
    fn test_operation_gates_partial_override() {
        let json = r#"{"type":"db","connection_string":"sqlite::memory:",
            "operations":{"delete":false,"raw_write":false}}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        let gates = config.operation_gates().expect("db has gates");
        assert!(!gates.allows("delete"));
        assert!(!gates.allows("raw_write"));
        assert!(gates.allows("read"));
        assert!(gates.allows("insert"));
        assert!(gates.allows("update"));
        assert!(gates.allows("upsert"));
    }

    #[test]
    fn test_operation_gates_on_es() {
        let json = r#"{"type":"es","url":"http://localhost:9200","operations":{"update":false}}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        let gates = config.operation_gates().expect("es has gates");
        assert!(!gates.allows("update"));
        assert!(gates.allows("insert"));
    }

    // -----------------------------------------------------------------
    // F24: connector-level dialect guards
    // -----------------------------------------------------------------

    /// Both guards are opt-in: the 1.0 `unmapped: reject` default is what makes
    /// the safe mode the default one, and these only add restrictions on top.
    #[test]
    fn dialect_guards_default_to_off_on_db_and_es() {
        for json in [
            r#"{"type":"db","connection_string":"sqlite::memory:"}"#,
            r#"{"type":"es","url":"http://localhost:9200"}"#,
        ] {
            let config: ConnectorConfig = serde_json::from_str(json).expect("test");
            let guards = config.dialect_guards().expect("db/es carry dialect guards");
            assert!(!guards.require_schema, "{json}");
            assert!(guards.allowed_entities.is_empty(), "{json}");
        }
        let http: ConnectorConfig =
            serde_json::from_str(r#"{"type":"http","url":"https://example.com"}"#).expect("test");
        assert!(http.dialect_guards().is_none(), "http has no dialect");
    }

    #[test]
    fn dialect_guards_parse_from_connector_config() {
        let json = r#"{"type":"db","connection_string":"sqlite::memory:",
            "dialect":{"require_schema":true,"allowed_entities":["users","orders"]}}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        let guards = config.dialect_guards().expect("db has guards");
        assert!(guards.require_schema);
        assert_eq!(guards.allowed_entities, vec!["users", "orders"]);
    }

    /// `require_schema` has to reject *both* ways of having no effective
    /// schema. An entity list under `"unmapped": "identity"` bounds nothing —
    /// every undeclared name still passes through — so it is not a schema.
    #[test]
    fn require_schema_refuses_identity_mode_and_empty_schemas() {
        let off = DialectGuards::default();
        assert!(
            off.schema_is_sufficient(false, true),
            "off permits anything"
        );

        let on = DialectGuards {
            require_schema: true,
            allowed_entities: vec![],
        };
        assert!(
            on.schema_is_sufficient(true, false),
            "entities + reject mode is a real schema"
        );
        assert!(
            !on.schema_is_sufficient(false, false),
            "no entities declared is not a schema"
        );
        assert!(
            !on.schema_is_sufficient(true, true),
            "entities under identity mode still let every other name through"
        );
        assert!(!on.schema_is_sufficient(false, true));
    }

    /// A misspelled guard key is the guard not applying at all, on a connector
    /// whose whole purpose is to bound what workflows reach — so it is a
    /// refused config, not an ignored key.
    #[test]
    fn a_misspelled_dialect_guard_is_rejected() {
        for bad in [
            r#"{"type":"db","connection_string":"sqlite::memory:",
                "dialect":{"requireSchema":true}}"#,
            r#"{"type":"db","connection_string":"sqlite::memory:",
                "dialect":{"allowed_entites":["users"]}}"#,
            r#"{"type":"es","url":"http://localhost:9200",
                "dialect":{"require_schemas":true}}"#,
        ] {
            let err = serde_json::from_str::<ConnectorConfig>(bad)
                .expect_err("a misspelled dialect key must not parse as no guard");
            assert!(err.to_string().contains("unknown field"), "{err}");
        }
    }

    /// The db/es gate struct stays db/es-only: the other three types carry
    /// gates of their own shape, so `operation_gates()` must not start
    /// answering for them (a `raw_write` flag on a cache reads as meaningful
    /// and is not — the F22 defect).
    #[test]
    fn test_operation_gates_absent_on_http() {
        let json = r#"{"type":"http","url":"https://example.com"}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        assert!(config.operation_gates().is_none());
    }

    // ---- F22e: gates on the other three connector types ----

    /// Additive with an all-allowed default: every connector authored before
    /// the gates existed keeps behaving exactly as it did.
    #[test]
    fn every_connector_type_defaults_to_fully_open() {
        let cache: ConnectorConfig =
            serde_json::from_str(r#"{"type":"cache","backend":"memory"}"#).expect("test");
        match cache {
            ConnectorConfig::Cache(c) => {
                assert!(c.operations.read);
                assert!(c.operations.write);
            }
            _ => unreachable!("Expected Cache config"),
        }

        let kafka: ConnectorConfig =
            serde_json::from_str(r#"{"type":"kafka","brokers":["b:9092"],"topic":"t"}"#)
                .expect("test");
        match kafka {
            ConnectorConfig::Kafka(k) => {
                assert!(k.operations.publish);
            }
            _ => unreachable!("Expected Kafka config"),
        }

        let http: ConnectorConfig =
            serde_json::from_str(r#"{"type":"http","url":"https://example.com"}"#).expect("test");
        match http {
            ConnectorConfig::Http(h) => {
                assert!(h.operations.methods.is_empty());
                for method in VALID_HTTP_METHODS {
                    assert!(h.operations.allows_method(method), "{method} must be open");
                }
            }
            _ => unreachable!("Expected Http config"),
        }
    }

    /// A read-only cache connector: `cache_write` is refused, `cache_read` is
    /// not. Partial overrides leave the other gate alone.
    #[test]
    fn a_cache_connector_can_be_made_read_only() {
        let json = r#"{"type":"cache","backend":"memory","operations":{"write":false}}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Cache(c) => {
                assert!(!c.operations.write);
                assert!(c.operations.read);
            }
            _ => unreachable!("Expected Cache config"),
        }
    }

    #[test]
    fn a_kafka_connector_can_be_made_publish_proof() {
        let json =
            r#"{"type":"kafka","brokers":["b:9092"],"topic":"t","operations":{"publish":false}}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Kafka(k) => assert!(!k.operations.publish),
            _ => unreachable!("Expected Kafka config"),
        }
    }

    /// Naming one method makes the list exhaustive, and matching ignores case
    /// so `["get"]` is not a connector that refuses every call.
    #[test]
    fn an_http_method_allow_list_is_exhaustive_and_case_insensitive() {
        let json = r#"{"type":"http","url":"https://example.com",
                       "operations":{"methods":["get","POST"]}}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Http(h) => {
                assert!(h.operations.allows_method("GET"));
                assert!(h.operations.allows_method("post"));
                assert!(!h.operations.allows_method("DELETE"));
                assert!(!h.operations.allows_method("PUT"));
            }
            _ => unreachable!("Expected Http config"),
        }
    }

    #[test]
    fn test_http_connector_config_defaults() {
        let json = r#"{"type":"http","url":"https://example.com"}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Http(http) => {
                assert!(http.headers.is_empty());
                assert!(http.auth.is_none());
                assert_eq!(http.retry.max_retries, 3);
                assert_eq!(http.retry.retry_delay_ms, 1000);
                assert_eq!(http.max_response_size, 10 * 1024 * 1024);
            }
            _ => unreachable!("Expected Http config"),
        }
    }
}
