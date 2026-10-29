use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use super::HttpOperationGates;

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
