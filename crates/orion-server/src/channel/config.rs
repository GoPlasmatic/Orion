use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Per-channel baseline configuration.
/// All fields are optional with sensible defaults.
///
/// `deny_unknown_fields` because every field here is a *guard*, and a key this
/// struct does not recognise is a guard that silently does not run: a stored
/// `"deduplicaton"` typo meant no idempotency, no error, forever. The channel's
/// stored `config_json` is the operator's original document — nothing
/// re-serialises it — so an unrecognised key survives every reload until
/// someone notices the behaviour is missing. Rejecting it turns that into a
/// create-time 400, or an F35 quarantine for a channel already stored: refused
/// at every ingress rather than served with a guard quietly absent. This is the
/// same posture the config file (`deny_unknown_fields` throughout), the
/// connector configs and both dialect envelopes (W5/W6) already take.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    /// Rate limiting configuration.
    #[serde(default)]
    pub rate_limit: Option<ChannelRateLimitConfig>,

    /// Maximum workflow execution time in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,

    /// Response caching configuration.
    /// When enabled, sync responses are cached using the configured (or default
    /// in-memory) cache backend. Cache key is derived from channel name +
    /// request data hash (optionally scoped to `cache_key_fields`).
    #[serde(default)]
    pub cache: Option<ChannelCacheConfig>,

    /// Server-side allow-list of `Origin` header values for this channel.
    /// A request whose `Origin` is present and unlisted is refused `403`;
    /// `"*"` in the list allows any origin, and an absent list checks
    /// nothing. See [`ChannelConfig::allowed_origins`] for why this is not
    /// spelled `cors` any more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_allow_list: Option<Vec<String>>,

    /// Backpressure / load-shedding configuration.
    #[serde(default)]
    pub backpressure: Option<BackpressureConfig>,

    /// Request deduplication configuration.
    /// Extracts an idempotency key from the configured header and rejects
    /// duplicate submissions within the time window with 409 Conflict.
    #[serde(default)]
    pub deduplication: Option<DeduplicationConfig>,

    /// JSONLogic expression for input validation at the channel boundary.
    /// Evaluated against the request data. Returns truthy = pass, falsy = 400 reject.
    /// Example: `{ "and": [{ "!!": { "var": "data.order_id" } }, { ">": [{ "var": "data.quantity" }, 0] }] }`
    #[serde(default)]
    pub validation_logic: Option<Value>,

    /// Per-channel override of `[trace_storage]`. Each field is independently
    /// optional; unset fields fall back to the global setting.
    #[serde(default)]
    pub tracing: Option<ChannelTracingConfig>,

    /// How the synchronous HTTP response is built. Absent (the default) is the
    /// fixed `{id, status, data, errors}` envelope with a `200`.
    #[serde(default)]
    pub response: Option<ChannelResponseConfig>,

    /// Who may call this channel over HTTP. Absent (the default) is
    /// unauthenticated, which is what every channel was before 1.0.
    #[serde(default)]
    pub auth: Option<ChannelAuthConfig>,
}

impl ChannelConfig {
    /// The channel's server-side origin allow-list.
    ///
    /// N24: the pre-1.0 key was `cors: { allowed_origins: [...] }`, which
    /// promised CORS and delivered a rejection check. It set no
    /// `Access-Control-Allow-Origin` and answered no preflight — the router's
    /// platform CORS layer short-circuits a genuine preflight (`OPTIONS`
    /// carrying `Access-Control-Request-Method`) before a channel is even
    /// resolved. The control is real and worth keeping: it is the only
    /// *server-side* origin check, and it runs on every request that reaches
    /// the handler, since `[cors]` leaves a non-preflighted cross-origin
    /// request to run and merely omits the response header, and does nothing
    /// at all for a non-browser caller. Only the name was a lie, so the name
    /// is what changed.
    ///
    /// The old spelling is not accepted. Silently ignoring it would drop the
    /// check on every stored channel that used it — a security regression
    /// dressed as a rename — so `deny_unknown_fields` on this struct refuses
    /// the whole config instead, and the channel is quarantined rather than
    /// served without its allow-list. `orion-server preflight` names every
    /// stored channel still carrying it.
    pub fn allowed_origins(&self) -> Option<&[String]> {
        self.origin_allow_list.as_deref()
    }
}

/// Who may call a channel over HTTP.
///
/// Before this existed the data plane had no authentication at all: `admin_auth`
/// covers `/api/v1/admin` and nothing else, and the two controls the docs
/// pointed at are not authentication. `origin_allow_list` reads a
/// client-supplied header, and a `validation_logic` header comparison means the
/// credential sits in the channel's stored config in plain text and is compared
/// byte-by-byte with an early exit.
///
/// Absent (the default) keeps a channel unauthenticated, so nothing that is
/// stored today changes behaviour.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelAuthConfig {
    /// Which scheme this channel enforces.
    pub mode: AuthMode,

    /// **`api_key`** — the accepted keys. Each entry may be a literal or an
    /// `env://VAR` reference resolved at channel load (the same resolver
    /// connector secrets use), so production credentials need not sit in the
    /// stored config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys: Option<Vec<String>>,

    /// Header carrying the credential. Defaults to `Authorization` for
    /// `api_key` and `X-Signature` for `hmac`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,

    /// **`api_key`** — expected prefix on the header value, e.g. `Bearer `.
    /// Defaults to `Bearer ` when the header is `Authorization`, and to none
    /// otherwise (an `X-API-Key` header carries a bare key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,

    /// **`hmac`** — the shared secret, literal or `env://VAR`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,

    /// **`hmac`** — additional accepted secrets, each tried in constant time —
    /// zero-downtime rotation, the list shape `keys` already has. Merged with
    /// `secret`; at least one of the two is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<Vec<String>>,

    /// **`hmac`** — prefix stripped from the signature header before decoding,
    /// e.g. `sha256=` for GitHub. Defaults to none. Mutually exclusive with
    /// `signature_key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_prefix: Option<String>,

    /// **`hmac`** — extract the signature from a comma-separated `k=v` packed
    /// header instead: the value(s) of this key (Stripe's `v1`). Mutually
    /// exclusive with `signature_prefix`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_key: Option<String>,

    /// **`hmac`** — MAC algorithm: `sha1` | `sha256` (default) | `sha512`.
    /// The provider chooses; refusing sha1 would only leave those webhooks
    /// unauthenticated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub algorithm: Option<String>,

    /// **`hmac`** — the signing-string template: literals plus `{body}`
    /// (required), `{header:<name>}`, and `{header:<name>:<key>}` for packed
    /// headers. Defaults to `{body}` — today's raw-body behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// **`hmac`** — pins the presented signature encoding: `hex` | `base64` |
    /// `base64url`. Absent keeps auto-detection (hex first, then base64).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,

    /// **`hmac`** — where the unix-seconds timestamp lives: `<header>` or
    /// `<header>:<key>` for packed headers. Paired with `tolerance_secs`;
    /// either alone is a config error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,

    /// **`hmac`** — replay window in seconds around `timestamp`; requests
    /// outside it are refused before the MAC is computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tolerance_secs: Option<u64>,

    /// **`hmac`** — provider preset (`zoom` | `slack` | `stripe` | `github` |
    /// `shopify` | `webex`) expanding to the explicit fields; an explicitly
    /// set field overrides its preset row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,

    /// **`jwt`** — static verification keys. At least one of `jwt_keys` /
    /// `jwks_url` is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwt_keys: Option<Vec<JwtKeyEntry>>,

    /// **`jwt`** — a JWKS document URL (HTTPS only); cached process-wide with
    /// single-flight refresh and stale-serve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwks_url: Option<String>,

    /// **`jwt`** — the mandatory, non-empty algorithm allowlist. Checked
    /// before anything else about a token; `alg: none` is unrepresentable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub algorithms: Option<Vec<String>>,

    /// **`jwt`** — accepted `iss` value(s); string or array. Absent skips the
    /// check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<StringOrVec>,

    /// **`jwt`** — accepted `aud` value(s); string or array. Absent skips the
    /// check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<StringOrVec>,

    /// **`jwt`** — clock-skew allowance for `exp`/`nbf`, seconds. Default 30,
    /// capped at 300.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leeway_secs: Option<u64>,

    /// **`jwt`** — whether a token must carry `exp` (RFC 8725 default true).
    /// Opting out is loud, deliberate config for non-expiring internal tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_exp: Option<bool>,

    /// **`jwt`** — whether a token is required at all. `false` admits
    /// token-less requests with no `metadata.auth` key; a present-but-invalid
    /// token is still rejected ("optional" never means "invalid passes").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,

    /// **`jwt`** — where the token is presented. Default: the
    /// `Authorization` header with the `Bearer` scheme. Query parameters are
    /// deliberately not offered (RFC 6750 §2.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<JwtSource>,

    /// **`jwt`** — token size cap. Default 8192.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_token_bytes: Option<usize>,

    /// **`jwt`** — which verified claims reach `metadata.auth.claims`.
    /// Absent → all of them (verified claims are not secrets from the
    /// workflow that admitted them); the list is the opt-in filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claims_to_metadata: Option<Vec<String>>,

    /// **`jwt`** — JSONLogic over `{"claims": …}`, evaluated after successful
    /// verification. Falsy → **403** `insufficient_scope` (RFC 6750): role
    /// and scope checks are authorization, not validation, and the wire
    /// should say so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_logic: Option<Value>,
}

/// A string, or an array of strings — the JWT `iss`/`aud` convenience shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StringOrVec {
    One(String),
    Many(Vec<String>),
}

impl StringOrVec {
    pub fn into_vec(&self) -> Vec<String> {
        match self {
            Self::One(s) => vec![s.clone()],
            Self::Many(v) => v.clone(),
        }
    }
}

/// One static JWT verification key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JwtKeyEntry {
    /// Algorithm this key verifies (`HS256` … `EdDSA`).
    pub algorithm: String,
    /// The material: an HS secret or a public-key PEM; literal or a secret
    /// reference (`env://`, `vault://`).
    pub key: String,
    /// Optional key id for `kid` routing; rotation = old + new entries under
    /// distinct kids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kid: Option<String>,
    /// How an HS secret becomes bytes: `utf8` (default) / `base64` / `hex` —
    /// the #259 precedent, for Supabase-class base64 secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_encoding: Option<String>,
}

/// Where a channel's JWT is presented.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, untagged)]
pub enum JwtSource {
    /// A header, minus an optional scheme prefix (`Bearer `).
    Header {
        header: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scheme: Option<String>,
    },
    /// A cookie by name — an extraction point only; sessions/CSRF stay out
    /// of scope.
    Cookie { cookie: String },
}

/// The authentication scheme a channel enforces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// A shared secret presented in a header, compared in constant time.
    ///
    /// Also the derived `Default` for `ChannelAuthConfig` — a test/
    /// struct-update convenience; production configs always deserialize,
    /// where `mode` is required.
    #[default]
    ApiKey,
    /// An HMAC over a configurable signing string (default: the **raw request
    /// body**, SHA-256) — the scheme webhook providers use. Zoom/Slack-style
    /// timestamped templates, Stripe's packed header, provider presets, and
    /// sha1/sha512 are all data on [`ChannelAuthConfig`].
    Hmac,
    /// A bearer JWT (#267): verified at ingress against static keys and/or a
    /// JWKS, with the verified claims — never the token — exposed at
    /// `metadata.auth.claims.*` and to `authorization_logic`.
    Jwt,
}

/// How a sync channel turns its workflow's output into an HTTP response.
///
/// The default (this key absent) is the envelope every channel has always
/// returned: `{id, status, data, errors}` with a `200`, whatever happened. That
/// is a fine contract for a workflow whose caller is another workflow, and a
/// poor one for a REST API — there is no `201` with a `Location`, no `404` for
/// a record that is not there, no `Content-Type` other than JSON. Every
/// consumer ends up special-casing "200 means maybe-error, look inside
/// `errors`", which is exactly the per-service glue channels exist to remove.
///
/// `mode = "shaped"` opts a channel into reading `data._orion.response` from
/// its workflow's output instead. It is opt-in per channel, so an existing
/// channel's bytes do not change, and so a workflow that happens to produce an
/// `_orion` key cannot affect a channel that never asked for it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ChannelResponseConfig {
    /// `envelope` (default) or `shaped`.
    pub mode: ResponseMode,
    /// Response headers the workflow is permitted to set, case-insensitive.
    ///
    /// Replaces [`DEFAULT_ALLOWED_RESPONSE_HEADERS`] rather than extending it,
    /// so a channel can narrow the set as well as widen it. Entries in
    /// [`FORBIDDEN_RESPONSE_HEADERS`] are refused even when listed here — the
    /// allowlist grants what the workflow may set, it does not override what
    /// the protocol layer owns.
    pub allowed_headers: Option<Vec<String>>,
}

/// Whether a channel returns the standard envelope or a workflow-shaped
/// response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ResponseMode {
    /// `{id, status, data, errors}` with a `200`. The pre-1.0 behaviour, and
    /// still the default.
    #[default]
    Envelope,
    /// Status, headers and body come from `data._orion.response`.
    Shaped,
}

/// Response headers a shaped workflow may set when the channel lists none of
/// its own: the ones a REST handler legitimately needs.
pub const DEFAULT_ALLOWED_RESPONSE_HEADERS: &[&str] = &[
    "content-type",
    "location",
    "cache-control",
    "etag",
    "last-modified",
    "retry-after",
    "content-language",
    "link",
];

/// Headers a workflow may never set, whatever the channel's allowlist says.
///
/// The hop-by-hop set (RFC 9110 §7.6.1) plus `content-length`, because the
/// framing of the response belongs to the server and not to its body; and
/// `x-request-id`, which the platform assigns and the trace is correlated by —
/// a workflow overwriting it would break the one thread tying a response to
/// its stored trace.
pub const FORBIDDEN_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
    "x-request-id",
];

impl ChannelResponseConfig {
    /// Whether this channel reads `data._orion.response`.
    pub fn is_shaped(&self) -> bool {
        self.mode == ResponseMode::Shaped
    }

    /// Whether the workflow may set `name` (already lowercased by the caller).
    pub fn allows_header(&self, name: &str) -> bool {
        if FORBIDDEN_RESPONSE_HEADERS.contains(&name) {
            return false;
        }
        match self.allowed_headers {
            Some(ref list) => list.iter().any(|h| h.eq_ignore_ascii_case(name)),
            None => DEFAULT_ALLOWED_RESPONSE_HEADERS.contains(&name),
        }
    }
}

/// Per-channel override for the trace-storage policy. Each field overrides
/// the corresponding global value when set; otherwise the global default
/// applies. The resolved `EffectiveTraceConfig` lives on `ChannelRuntimeConfig`
/// so the request path doesn't merge per request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelTracingConfig {
    #[serde(default)]
    pub mode: Option<crate::config::TraceStorageMode>,
    #[serde(default)]
    pub sample_rate: Option<f64>,
    #[serde(default)]
    pub errors_only: Option<bool>,
    /// When `true`, the engine captures a per-task execution trace
    /// (intermediate input/output snapshots from `dataflow_rs::ExecutionTrace`)
    /// and persists it to the `task_trace_json` column. Off by default
    /// because each persisted trace grows proportional to message size
    /// times task count — only enable for debugging.
    #[serde(default)]
    pub task_details: Option<bool>,
}

/// What a guard does when its backing store cannot answer (N7).
///
/// Applies to the shared-Redis rate-limit window and to Redis-backed dedup
/// stores: a backend outage forces a choice between availability and
/// enforcement. `allow` (the default) keeps serving without the guard;
/// `deny` refuses the request with `503` — the right trade for
/// payment/idempotency workloads where a duplicate is worse than an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackendErrorPolicy {
    /// Fail open: the request proceeds as if the guard had passed.
    #[default]
    Allow,
    /// Fail closed: the request is refused with `503 Service Unavailable`.
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelRateLimitConfig {
    /// Maximum requests per second.
    pub requests_per_second: u32,
    /// Burst allowance above the steady rate.
    #[serde(default)]
    pub burst: Option<u32>,
    /// JSONLogic expression to compute the rate limit key from request context.
    /// Context: `{ "client_ip": "...", "channel": "...", "headers": { ... } }`
    /// Default (absent): uses `client_ip` as the key.
    /// Example: `{ "var": "headers.x-api-key" }` for per-API-key limiting.
    /// Example: `{ "cat": [{ "var": "client_ip" }, ":", { "var": "headers.x-tenant-id" }] }`
    #[serde(default)]
    pub key_logic: Option<Value>,
    /// Extra request headers `key_logic` may read, beyond the built-in set
    /// (`authorization`, `x-api-key`, `x-forwarded-for`, `x-real-ip`,
    /// `user-agent`, `content-type`, `origin`, `x-tenant-id`).
    ///
    /// **Merged with the built-ins, never replacing them** — declaring
    /// `["deviceid"]` cannot take `x-tenant-id` away from an expression that
    /// already reads it, so no stored `key_logic` changes meaning. Names are
    /// lowercased at load (HTTP header names are case-insensitive), and a
    /// redundant declaration of a built-in is a no-op.
    ///
    /// Note a header is caller-supplied and therefore spoofable: a key derived
    /// from one bounds an honest client, which is appropriate for a burst
    /// control and not for a quota.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_headers: Option<Vec<String>>,
    /// Policy when the rate-limit backend (the shared cluster Redis) cannot
    /// answer. Irrelevant to the in-process limiter, which cannot fail.
    #[serde(default)]
    pub on_backend_error: BackendErrorPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelCacheConfig {
    /// Whether caching is enabled.
    pub enabled: bool,
    /// Cache TTL in seconds.
    #[serde(default)]
    pub ttl_secs: Option<u64>,
    /// Fields used to compute the cache key.
    #[serde(default)]
    pub cache_key_fields: Option<Vec<String>>,
    /// Optional cache connector name for the response cache backend.
    #[serde(default)]
    pub connector: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackpressureConfig {
    /// Maximum concurrent requests for this channel **on this node**.
    /// Excess requests are rejected immediately with 503 (no queueing).
    ///
    /// N9: named for what it bounds — the semaphore is per process, so N
    /// replicas admit up to N× this value in total. The pre-1.0 name
    /// `max_concurrent` read as an absolute cluster-wide cap while sitting
    /// next to dedup/rate-limit controls that *are* shared in cluster mode.
    /// It is not accepted: the field has no `serde(default)`, so a stored
    /// config using the old spelling fails with `missing field
    /// max_concurrent_per_node` and the channel is quarantined — a channel
    /// admitted N× its intended concurrency is not a quiet outcome worth
    /// having.
    pub max_concurrent_per_node: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeduplicationConfig {
    /// Header name containing the idempotency key.
    pub header: String,
    /// Time window in seconds for deduplication.
    #[serde(default)]
    pub window_secs: Option<u64>,
    /// Optional cache connector name for the dedup backend.
    /// When set, uses the connector's backend (redis or memory).
    /// When absent, uses the built-in in-memory store.
    #[serde(default)]
    pub connector: Option<String>,
    /// Policy when the dedup backend cannot answer: without it, an outage
    /// silently disables idempotency (every request treated as new).
    #[serde(default)]
    pub on_backend_error: BackendErrorPolicy,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_config_default() {
        let config = ChannelConfig::default();
        assert!(config.rate_limit.is_none());
        assert!(config.timeout_ms.is_none());
        assert!(config.cache.is_none());
        assert!(config.backpressure.is_none());
        assert!(config.deduplication.is_none());
        assert!(config.validation_logic.is_none());
    }

    #[test]
    fn test_channel_config_deserialization() {
        let json = r#"{
            "rate_limit": { "requests_per_second": 100, "burst": 20, "key_logic": { "var": "client_ip" } },
            "timeout_ms": 5000,
            "backpressure": { "max_concurrent_per_node": 200 },
            "deduplication": { "header": "Idempotency-Key", "window_secs": 300 }
        }"#;
        let config: ChannelConfig = serde_json::from_str(json).expect("test");
        let rl = config.rate_limit.expect("test");
        assert_eq!(rl.requests_per_second, 100);
        assert_eq!(rl.burst, Some(20));
        assert!(rl.key_logic.is_some());
        assert_eq!(rl.on_backend_error, BackendErrorPolicy::Allow);
        assert_eq!(config.timeout_ms, Some(5000));
        let bp = config.backpressure.expect("test");
        assert_eq!(bp.max_concurrent_per_node, 200);
        let dedup = config.deduplication.expect("test");
        assert_eq!(dedup.header, "Idempotency-Key");
        assert_eq!(dedup.window_secs, Some(300));
        assert_eq!(dedup.on_backend_error, BackendErrorPolicy::Allow);
    }

    /// N9: the pre-1.0 spelling is refused, not silently accepted. Failing to
    /// parse quarantines the channel; accepting it under a name that means
    /// something else would admit N× the intended concurrency.
    #[test]
    fn test_backpressure_old_name_is_refused() {
        let err =
            serde_json::from_str::<ChannelConfig>(r#"{"backpressure": {"max_concurrent": 7}}"#)
                .expect_err("the pre-1.0 `max_concurrent` spelling must not parse");
        let message = err.to_string();
        assert!(
            message.contains("max_concurrent"),
            "the error must name the offending key: {message}"
        );
    }

    /// N7: `on_backend_error` parses on both guard blocks; unknown values fail.
    #[test]
    fn test_on_backend_error_deserialization() {
        let json = r#"{
            "rate_limit": { "requests_per_second": 5, "on_backend_error": "deny" },
            "deduplication": { "header": "idem", "on_backend_error": "deny" }
        }"#;
        let config: ChannelConfig = serde_json::from_str(json).expect("test");
        assert_eq!(
            config.rate_limit.expect("test").on_backend_error,
            BackendErrorPolicy::Deny
        );
        assert_eq!(
            config.deduplication.expect("test").on_backend_error,
            BackendErrorPolicy::Deny
        );
        assert!(
            serde_json::from_str::<ChannelConfig>(
                r#"{"deduplication": {"header": "idem", "on_backend_error": "explode"}}"#
            )
            .is_err(),
            "unknown policy values must be rejected, not defaulted"
        );
    }

    /// N24: `origin_allow_list` is the only spelling.
    #[test]
    fn test_origin_allow_list() {
        let new_key: ChannelConfig =
            serde_json::from_str(r#"{"origin_allow_list": ["https://app.example.com"]}"#)
                .expect("test");
        assert_eq!(
            new_key.allowed_origins(),
            Some(["https://app.example.com".to_string()].as_slice())
        );

        // No list at all means the channel checks nothing.
        assert!(ChannelConfig::default().allowed_origins().is_none());
    }

    /// N24: the pre-1.0 `cors` spelling is *refused*, not ignored. Parsing it
    /// and dropping the key would leave every channel that used it serving
    /// with no origin check — the security regression the rename was written
    /// to avoid. `deny_unknown_fields` makes the whole config fail instead,
    /// which quarantines the channel.
    #[test]
    fn test_pre_1_0_cors_spelling_is_refused_not_ignored() {
        for stored in [
            r#"{"cors": {"allowed_origins": ["https://old.example.com"]}}"#,
            r#"{"cors": {}}"#,
        ] {
            let err = serde_json::from_str::<ChannelConfig>(stored)
                .expect_err("the pre-1.0 `cors` spelling must not parse");
            let message = err.to_string();
            assert!(
                message.contains("cors"),
                "the error must name the offending key: {message}"
            );
        }
    }

    /// The general case the `cors` removal relies on: an unrecognised key is a
    /// guard that would silently not run, so it fails the whole config.
    #[test]
    fn test_unknown_channel_config_key_is_refused() {
        let err = serde_json::from_str::<ChannelConfig>(
            r#"{"deduplicaton": {"header": "Idempotency-Key"}}"#,
        )
        .expect_err("a misspelled guard key must not be silently ignored");
        assert!(
            err.to_string().contains("deduplicaton"),
            "the error must name the typo: {err}"
        );
    }

    /// N25: the same posture one level down. A typo *inside* a guard's own
    /// body previously fell back to a default silently — a misspelled
    /// `key_logic` meant per-IP rate keying, a misspelled `window_secs` meant
    /// the default dedup window — which is the quiet-outcome failure the
    /// top-level refusal exists to prevent.
    #[test]
    fn test_unknown_key_inside_a_guard_is_refused() {
        for (config, typo) in [
            (
                r#"{"rate_limit": {"requests_per_second": 10, "key_logic_": {"var": "client_ip"}}}"#,
                "key_logic_",
            ),
            (
                r#"{"deduplication": {"header": "Idempotency-Key", "window_seconds": 60}}"#,
                "window_seconds",
            ),
            (
                r#"{"cache": {"enabled": true, "ttl_seconds": 30}}"#,
                "ttl_seconds",
            ),
            (r#"{"tracing": {"sampling_rate": 0.5}}"#, "sampling_rate"),
        ] {
            let err = serde_json::from_str::<ChannelConfig>(config)
                .expect_err("a misspelled key inside a guard must not be silently ignored");
            assert!(
                err.to_string().contains(typo),
                "the error must name the typo `{typo}`: {err}"
            );
        }
    }

    #[test]
    fn test_channel_config_empty_json() {
        let config: ChannelConfig = serde_json::from_str("{}").expect("test");
        assert!(config.rate_limit.is_none());
        assert!(config.timeout_ms.is_none());
    }
}
