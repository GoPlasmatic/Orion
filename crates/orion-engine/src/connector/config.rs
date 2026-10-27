use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// A parsed connector configuration.
///
/// `PartialEq` runs the whole tree and is load-bearing, not a convenience:
/// [`crate::connector::ConnectorRegistry::load`] compares the map it
/// just built against the one it holds and only advances
/// `config_generation` when they differ, which is what keeps a deployment
/// that touched no connector from invalidating every channel's cached runtime.
/// Every field of every variant therefore has to take part — a field
/// left out of the comparison would be a config change the channel loader
/// never notices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ConnectorConfig {
    Http(HttpConnectorConfig),
    Db(DbConnectorConfig),
    Cache(CacheConnectorConfig),
}

/// One parse of a connector variant's struct — the hook
/// [`ConnectorConfig::parse_variant`] types each variant through.
pub trait VariantParse {
    type Error;
    fn parse<T: serde::de::DeserializeOwned>(
        &self,
        value: &serde_json::Value,
    ) -> Result<T, Self::Error>;
}

impl ConnectorConfig {
    /// Type `value` as `connector_type`'s variant, through `parser`.
    ///
    /// The same result as deserializing the enum with `type` injected, except
    /// that the variant's struct is deserialized directly. That matters to a
    /// parse that needs error *paths* — the authoring parse that stands a
    /// placeholder in for a reference, the load parse that coerces a resolved
    /// boolean: serde buffers an internally tagged document before handing it
    /// to the variant, and the path is lost there. A `type` key in `value` is
    /// ignored, as the tag is.
    pub fn parse_variant<P: VariantParse>(
        connector_type: ConnectorType,
        value: &serde_json::Value,
        parser: &P,
    ) -> Result<Self, P::Error> {
        let mut untagged = value.clone();
        if let Some(obj) = untagged.as_object_mut() {
            obj.remove("type");
        }
        let value = &untagged;
        Ok(match connector_type {
            ConnectorType::Http => Self::Http(parser.parse(value)?),
            ConnectorType::Db => Self::Db(parser.parse(value)?),
            ConnectorType::Cache => Self::Cache(parser.parse(value)?),
        })
    }

    /// The wire `type` this config is. The stored `connectors.connector_type`
    /// column says the same thing, but the registry holds parsed configs — this
    /// is how a caller with only the parsed form (F52's activation-time type
    /// check) asks what it got.
    pub fn connector_type(&self) -> ConnectorType {
        match self {
            ConnectorConfig::Http(_) => ConnectorType::Http,
            ConnectorConfig::Db(_) => ConnectorType::Db,
            ConnectorConfig::Cache(_) => ConnectorType::Cache,
        }
    }

    /// The db operation gates. The other connector types carry gates of their
    /// own shape — [`CacheOperationGates`] and [`HttpOperationGates`] —
    /// reached through their typed configs,
    /// because a `methods` list on a database or a `raw_write` flag on a cache
    /// would be a field nothing reads (the defect F22 closed).
    pub fn operation_gates(&self) -> Option<&OperationGates> {
        match self {
            ConnectorConfig::Db(c) => Some(&c.operations),
            _ => None,
        }
    }

    /// The portable-dialect guards for connectors that carry them (db).
    pub fn dialect_guards(&self) -> Option<&DialectGuards> {
        match self {
            ConnectorConfig::Db(c) => Some(&c.dialect),
            _ => None,
        }
    }
}

/// Operator-owned bounds on what the portable dialect (`data_query` /
/// `data_write`) may reach through this connector.
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
/// - `read` gates `data_query` and `db_read`.
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

/// Cache-connector operation gates. Everything defaults to allowed;
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

/// HTTP-connector method allow-list.
///
/// The db gates are booleans because their operation set is fixed; an HTTP
/// connector's is its method, so the gate is a list. Empty — the default —
/// means every method `http_call` can issue is allowed. Naming even one
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
    /// Query parameters appended to every request through this connector —
    /// the home for APIs that authenticate with credentials **in the query
    /// string** (legacy SMS/telecom gateways, older payment and lookup APIs).
    ///
    /// Values are secret-resolvable (`env://`, `vault://`, `${VAR}`) and
    /// masked on admin reads, and they are applied via `RequestBuilder::query`
    /// inside `execute_request` rather than merged into the URL — so the
    /// SSRF-validated URL, every error message and the `url` OTel field stay
    /// credential-free, and the parameters cannot ride a cross-host redirect.
    ///
    /// **`BTreeMap`, not `HashMap`** — a deliberate departure from `headers`.
    /// Query-parameter order is observable on the wire and matters to
    /// signature-based gateways; `HashMap` iteration order varies per call,
    /// and `serde_json` is pinned without `preserve_order` so author order is
    /// unrecoverable anyway. Sorted is the only deterministic option.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub query_params: BTreeMap<String, String>,
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub retry: RetryConfig,
    /// Maximum response body size in bytes (default 10 MB). Prevents OOM from large responses.
    /// Retry non-idempotent methods (POST, PATCH) too. Default false:
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
    /// `"client_credentials"`, `"refresh_token"` or `"account_credentials"` —
    /// parsed by [`OAuth2Grant::parse`], validated at authoring time.
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
    /// `account_id` token-request parameter, required by the
    /// `account_credentials` grant (Zoom Server-to-Server OAuth).
    ///
    /// A dedicated field rather than an `extra_params` entry for three
    /// reasons: `extra_params` values mask unconditionally, and an account id
    /// is a **tenant identifier, not a credential**, so masking it makes admin
    /// reads and package diffs useless for it; a dedicated field gets the
    /// grant-conditional required/refused matrix that `refresh_token` has, so
    /// a typo is an authoring 400 rather than a runtime provider rejection;
    /// and secret references cost nothing either way, since resolution is a
    /// generic tree walk over every string.
    ///
    /// `skip_serializing_if` is load-bearing, not style — see the note on
    /// [`OAuth2Grant::AccountCredentials`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
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
    /// Zoom's Server-to-Server OAuth: `grant_type=account_credentials` plus an
    /// `account_id`, with Basic client auth. The grant Zoom moved every
    /// server-side integration to when it retired JWT apps in 2023.
    ///
    /// Like `client_credentials` it re-acquires from static credentials, so it
    /// deliberately gets no rotation persistence and no cluster job lease —
    /// there is no `connector_oauth_state` row to keep.
    ///
    /// ⚠️ **Upgrade hazard, for whoever adds the next grant field.**
    /// `fingerprint` is a hash of the whole serialized [`OAuth2Config`], and
    /// a persisted state row is adopted only when the fingerprint matches. A
    /// new field that serializes as `null` when unset changes the fingerprint
    /// of **every existing `refresh_token` connector** on upgrade, discarding
    /// its persisted state and falling back to the config seed — which, after
    /// any prior rotation, is a spent refresh token. Hence
    /// `skip_serializing_if = "Option::is_none"` on `account_id`, and on
    /// anything added beside it.
    AccountCredentials,
}

impl OAuth2Grant {
    pub const VALUES: &'static str = "client_credentials/refresh_token/account_credentials";

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "client_credentials" => OAuth2Grant::ClientCredentials,
            "refresh_token" => OAuth2Grant::RefreshToken,
            "account_credentials" => OAuth2Grant::AccountCredentials,
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

/// Retry policy for `http_call`. Lives on [`HttpConnectorConfig`] alone: no
/// other connector type retries (F7).
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

/// SQL connector. The backend is selected by the `connection_string` scheme
/// (`postgres://`, `mysql://`, `sqlite:`). Credentials belong in the
/// connection string, which is why there is no `auth`.
///
/// `driver` names the backend for a reader that cannot see the scheme — a
/// connection string that is a secret reference or a `${VAR}` placeholder —
/// which is what `compile --format deployment` needs to pick the provider.
/// It is not what selects the backend at run time; the scheme still is. So
/// it is held to the scheme instead: where the scheme is visible, a `driver`
/// that disagrees is refused at create, update and lint
/// ([`DbDriver::for_connection_string`]). Until 1.0 a free-form `driver`
/// existed that nothing read and nothing checked, which made `driver:
/// "mysql"` with a `postgres://` URL look meaningful (proposal F22); the
/// agreement check is what makes the field honest this time. A definition
/// set must name it on every `db` connector (`lint`'s `connector.db_driver`);
/// the single-entity admin API does not require it.
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
    /// The backend this connector reaches, stated. `None` when absent — and
    /// when a stored row carries a pre-1.0 free-form value (`"postgres"`,
    /// `"mongodb"`), which loads as it always did, ignored; the admin door
    /// refuses anything but the three names.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "DbDriver::deserialize_lenient"
    )]
    pub driver: Option<DbDriver>,
    #[serde(default)]
    pub max_connections: Option<u32>,
    #[serde(default)]
    pub connect_timeout_ms: Option<u64>,
    #[serde(default)]
    pub query_timeout_ms: Option<u64>,
    /// Allow connecting to private/internal IP addresses. Default false
    /// (SSRF protection) — the same opt-out the HTTP connector carries. A database on a private network is the normal case, so this is
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
}

/// The backend a `db` connector names in its `driver` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DbDriver {
    Postgresql,
    Mysql,
    Sqlite,
}

impl DbDriver {
    /// Every value `driver` may take, as written.
    pub const NAMES: &'static [&'static str] = &["postgresql", "mysql", "sqlite"];

    /// The driver `name` spells, if it is one of [`Self::NAMES`].
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "postgresql" => Some(Self::Postgresql),
            "mysql" => Some(Self::Mysql),
            "sqlite" => Some(Self::Sqlite),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgresql => "postgresql",
            Self::Mysql => "mysql",
            Self::Sqlite => "sqlite",
        }
    }

    /// The driver a connection-string scheme selects: `postgres` and
    /// `postgresql`, `mysql` and `mariadb`, `sqlite`.
    pub fn from_scheme(scheme: &str) -> Option<Self> {
        match scheme.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" => Some(Self::Postgresql),
            "mysql" | "mariadb" => Some(Self::Mysql),
            "sqlite" => Some(Self::Sqlite),
            _ => None,
        }
    }

    /// What `connection_string`'s scheme says about the driver: `None` when
    /// the scheme is not visible — the string is a secret reference
    /// (`env://…`) or a `${VAR}` placeholder, which only the serving node
    /// resolves — else `Some` of the driver the scheme selects, itself
    /// `None` for a scheme no SQL backend serves (which the endpoint check
    /// refuses on its own).
    pub fn for_connection_string(connection_string: &str) -> Option<Option<Self>> {
        let trimmed = connection_string.trim_start();
        if trimmed.starts_with("${")
            || crate::connector::secrets::is_secret_reference(connection_string)
        {
            return None;
        }
        let scheme = trimmed.split_once(':').map(|(scheme, _)| scheme)?;
        Some(Self::from_scheme(scheme))
    }

    /// A stored row's `driver`, read as tolerantly as a removed field is:
    /// one of the three names, or `None` for anything else. Refusing an
    /// unknown value here would drop a pre-1.0 row that carried one from
    /// the registry; refusing it is the admin door's job.
    fn deserialize_lenient<'de, D>(deserializer: D) -> Result<Option<Self>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Option::<serde_json::Value>::deserialize(deserializer)?;
        Ok(value
            .as_ref()
            .and_then(|v| v.as_str())
            .and_then(Self::from_name))
    }
}

/// Cache connector: a named, per-node in-memory store. `default_ttl_secs`,
/// `max_connections`, `auth` and `retry` were accepted here until 1.0 and
/// never read (proposal F22): TTL comes from each `cache_write` task.
///
/// `url` and `allow_private_urls` described the Redis backend, which is no
/// longer in Community. Stored rows carrying them still parse (connector
/// configs do not deny unknown fields) and the keys are ignored; validation
/// refuses a non-null `url`, and a `backend` other than `memory`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheConnectorConfig {
    /// Cache backend. `"memory"` is the only one. Required — no default, so
    /// a config written for another backend is refused rather than quietly
    /// becoming a per-node store.
    pub backend: String,
    /// Which operations workflows may run through this connector.
    #[serde(default)]
    pub operations: CacheOperationGates,
}

impl CacheConnectorConfig {
    /// Why this build cannot serve `backend`, naming the backends it can, or
    /// `None` when it can. One sentence for the authoring refusal, the
    /// registry load and the pool, so a stored `redis` row reads the same
    /// wherever it is met.
    pub fn unsupported_backend(&self) -> Option<String> {
        (!VALID_CACHE_BACKENDS.contains(&self.backend.as_str())).then(|| {
            format!(
                "Invalid cache backend '{}'. Must be one of: {}",
                self.backend,
                VALID_CACHE_BACKENDS.join(", ")
            )
        })
    }
}

/// Allowed connector type values.
pub const VALID_CONNECTOR_TYPES: &[&str] = &["http", "db", "cache"];

/// Allowed cache backend values.
pub const VALID_CACHE_BACKENDS: &[&str] = &["memory"];

/// Typed enum for the connector `type` field on create/update requests.
/// Wire format is lowercase ("http", "db", "cache");
/// deserialization is case-insensitive so "HTTP" or "Cache" also parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ConnectorType {
    Http,
    Db,
    Cache,
}

impl ConnectorType {
    /// Every connector type, for the callers that must answer a question for
    /// all of them. [`PoolSlot::ALL`](super::kind::PoolSlot::ALL) exists next
    /// door for the same reason: the per-kind vocabulary was written out by
    /// hand in four places, and a gate struct had already fallen out of one
    /// of them — the gate-key drift guard, which enumerated five types and so
    /// could not see the sixth.
    ///
    /// A `match` over this with no wildcard arm is the pattern to reach for:
    /// it turns a new variant into a compile error rather than a check that
    /// silently stops covering it.
    pub const ALL: &'static [ConnectorType] =
        &[ConnectorType::Http, ConnectorType::Db, ConnectorType::Cache];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Db => "db",
            Self::Cache => "cache",
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
            Self::Db => &["read", "insert", "update", "delete", "upsert", "raw_write"],
            Self::Cache => &["read", "write"],
            Self::Http => &["methods"],
        }
    }
}

impl std::fmt::Display for ConnectorType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ConnectorType {
    /// The type a stored `connector_type` column names — exactly as the
    /// config enum's tag matches it, so the two cannot disagree on a row.
    pub fn from_stored(s: &str) -> Option<Self> {
        Self::ALL.iter().find(|t| t.as_str() == s).copied()
    }
}

impl<'de> serde::Deserialize<'de> for ConnectorType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?.to_ascii_lowercase();
        // Matched against `ALL` rather than a second hand-written arm list:
        // the spelling each variant answers to is `as_str`'s to decide, and a
        // new variant should not need a line here to become parseable.
        Self::ALL
            .iter()
            .find(|t| t.as_str() == s)
            .copied()
            .ok_or_else(|| serde::de::Error::unknown_variant(&s, VALID_CONNECTOR_TYPES))
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

        // The `match` is the point, and it has no wildcard arm: a new
        // connector type does not compile until someone says which gate
        // struct backs it. The hand-written list this replaces enumerated
        // five types and stopped, so a later type's gate struct was never
        // checked at all.
        for &connector_type in ConnectorType::ALL {
            let struct_fields = match connector_type {
                ConnectorType::Db => fields(&OperationGates::default()),
                ConnectorType::Cache => fields(&CacheOperationGates::default()),
                ConnectorType::Http => fields(&HttpOperationGates::default()),
            };
            assert_eq!(
                struct_fields,
                declared(connector_type),
                "{connector_type}: the gate keys the admin door validates \
                 against must be exactly the fields the gate struct carries"
            );
        }
    }

    /// `VALID_CONNECTOR_TYPES` is what a rejected `type` value is reported
    /// against, and it is spelled out by hand because `unknown_variant` needs
    /// a `&'static` list. This is what keeps that hand-written copy honest.
    #[test]
    fn the_valid_type_list_is_the_enum() {
        let from_enum: Vec<&str> = ConnectorType::ALL.iter().map(|t| t.as_str()).collect();
        assert_eq!(from_enum, VALID_CONNECTOR_TYPES);
    }

    /// Every type parses back from the spelling it serializes to, including
    /// the case-insensitivity the wire contract promises.
    #[test]
    fn every_type_round_trips_through_its_wire_spelling() {
        for &connector_type in ConnectorType::ALL {
            let wire = connector_type.as_str();
            for spelling in [wire.to_string(), wire.to_ascii_uppercase()] {
                let parsed = serde_json::from_value::<ConnectorType>(serde_json::Value::String(
                    spelling.clone(),
                ));
                assert_eq!(
                    parsed.ok(),
                    Some(connector_type),
                    "'{spelling}' must parse back to {connector_type}"
                );
            }
        }
    }

    #[test]
    fn test_valid_connector_types() {
        assert!(VALID_CONNECTOR_TYPES.contains(&"http"));
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
    fn test_connector_config_deserialization_cache_memory() {
        let json = r#"{"type":"cache","backend":"memory","default_ttl_secs":60}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        match config {
            ConnectorConfig::Cache(cache) => {
                assert_eq!(cache.backend, "memory");
            }
            _ => unreachable!("Expected Cache config"),
        }
    }

    /// F22/F7: connector configs deliberately do **not** use
    /// `deny_unknown_fields`, so a config carrying a key no connector reads
    /// (a db `retry`, an `auth` block) loads and the key is ignored, rather
    /// than the connector failing to deserialize and dropping out of the
    /// registry.
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

        let cache = r#"{"type":"cache","backend":"memory","default_ttl_secs":300,
                        "max_connections":10,"retry":{"max_retries":3}}"#;
        match serde_json::from_str::<ConnectorConfig>(cache)
            .expect("0.3.x cache config must still load")
        {
            ConnectorConfig::Cache(c) => assert_eq!(c.backend, "memory"),
            _ => unreachable!("Expected Cache config"),
        }
    }

    /// `retry` on a db connector was declared, validated and documented
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
        let json = r#"{"type":"cache","operations":{"read":true}}"#;
        let result = serde_json::from_str::<ConnectorConfig>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_valid_connector_types_expanded() {
        assert!(VALID_CONNECTOR_TYPES.contains(&"http"));
        assert!(VALID_CONNECTOR_TYPES.contains(&"db"));
        assert!(VALID_CONNECTOR_TYPES.contains(&"cache"));
        assert!(!VALID_CONNECTOR_TYPES.contains(&"es"));
        assert!(!VALID_CONNECTOR_TYPES.contains(&"storage"));
        assert!(!VALID_CONNECTOR_TYPES.contains(&"kafka"));
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

    // -----------------------------------------------------------------
    // F24: connector-level dialect guards
    // -----------------------------------------------------------------

    /// Both guards are opt-in: the 1.0 `unmapped: reject` default is what makes
    /// the safe mode the default one, and these only add restrictions on top.
    #[test]
    fn dialect_guards_default_to_off_on_db() {
        let json = r#"{"type":"db","connection_string":"sqlite::memory:"}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        let guards = config.dialect_guards().expect("db carries dialect guards");
        assert!(!guards.require_schema, "{json}");
        assert!(guards.allowed_entities.is_empty(), "{json}");
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
            r#"{"type":"db","connection_string":"sqlite::memory:",
                "dialect":{"require_schemas":true}}"#,
        ] {
            let err = serde_json::from_str::<ConnectorConfig>(bad)
                .expect_err("a misspelled dialect key must not parse as no guard");
            assert!(err.to_string().contains("unknown field"), "{err}");
        }
    }

    /// The db gate struct stays db-only: the other types carry
    /// gates of their own shape, so `operation_gates()` must not start
    /// answering for them (a `raw_write` flag on a cache reads as meaningful
    /// and is not — the F22 defect).
    #[test]
    fn test_operation_gates_absent_on_http() {
        let json = r#"{"type":"http","url":"https://example.com"}"#;
        let config: ConnectorConfig = serde_json::from_str(json).expect("test");
        assert!(config.operation_gates().is_none());
    }

    // ---- F22e: gates on the other connector types ----

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
