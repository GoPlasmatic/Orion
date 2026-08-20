//! Managed OAuth2 for `http` connectors (#268): token acquisition, in-memory
//! caching, single-flight refresh, and rotation persistence.
//!
//! The lifecycle machinery here is **grant-agnostic** — cache, refresh margin,
//! single-flight, negative cache, persistence, cluster adoption are written
//! once; each grant contributes only its token-request shape. A future grant
//! (`jwt-bearer`, token exchange, device code) is a new
//! [`OAuth2Grant`] value plus a request builder, not new machinery.
//!
//! ## Lifecycle
//!
//! Tokens are acquired lazily on first use, cached per connector, and
//! refreshed `refresh_margin_secs` before expiry. A comfortably-fresh token
//! under an unchanged config is served lock-free (`ArcSwap`); everything else
//! falls to the per-connector `Mutex`, which makes refreshes
//! **single-flight**: concurrent requests wait on the in-flight result instead
//! of racing it — the race that, under refresh-token rotation, makes the
//! losers invalidate the winner's new token.
//!
//! ## Rotation persistence
//!
//! A `refresh_token` response carrying a new refresh token is persisted to the
//! `connector_oauth_state` table (encrypted at rest like `config_json`)
//! **together with the access token and its expiry** — that is what lets other
//! cluster nodes *adopt* a fresh token instead of re-racing the rotation. The
//! state row carries a fingerprint of the connector's oauth2 block: editing
//! the connector invalidates stale state, which is also the burned-token
//! recovery story (re-seed the config → new fingerprint → seed wins). If the
//! persist write fails, the live token stays in memory and the next refresh
//! retries persistence — loudly logged, since only a restart during a
//! persistent DB failure can lose the rotated value.
//!
//! ## Failure taxonomy (F6/F42)
//!
//! A token endpoint that cannot be reached is a **retryable** dependency
//! failure. A token endpoint that *rejects* the request (`invalid_grant`,
//! `invalid_client`) is a **non-retryable** configuration failure with a 30 s
//! negative cache, so a burned refresh token is never retry-looped against the
//! IdP — and it deliberately does not trip the API's circuit breaker, because
//! a credential failure is not evidence about the API's health.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::config::{HttpConnectorConfig, OAuth2ClientAuth, OAuth2Config, OAuth2Grant};
use crate::storage::repositories::connectors::ConnectorRepository;

/// How long a token-endpoint rejection (`invalid_grant`, …) is answered from
/// memory before the IdP is asked again.
const NEGATIVE_CACHE: Duration = Duration::from_secs(30);
/// Token-endpoint request timeout.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(10);
/// A token response larger than this is not a token, it is a problem.
const MAX_TOKEN_RESPONSE_BYTES: usize = 65_536;
/// `expires_in` absent from a token response → this conservative lifetime.
const DEFAULT_TOKEN_TTL_SECS: u64 = 300;
/// Floor on the effective lifetime, so a tiny `expires_in` cannot turn every
/// request into a refresh stampede against the IdP.
const MIN_TOKEN_TTL_SECS: u64 = 30;
/// Cluster refresh lease TTL. Comfortably above [`TOKEN_TIMEOUT`], so the
/// holder finishes (or dies) before the lease can change hands.
const LEASE_TTL_SECS: u64 = 30;
/// How a lease loser waits for the winner: poll the state row this many times…
const ADOPT_POLL_ATTEMPTS: u32 = 10;
/// …this far apart.
const ADOPT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// What the token manager needs at runtime, injected once from bootstrap.
pub struct OAuthRuntimeDeps {
    /// The shared engine client (SSRF-pinned, no auto-redirects).
    pub http_client: reqwest::Client,
    /// Where rotation state persists.
    pub repo: Arc<dyn ConnectorRepository>,
    /// Cross-node single-flight for refresh-token rotation. `None` on a
    /// single node, where the per-connector mutex already serialises.
    pub lease: Option<Arc<crate::cluster::JobLeaseGate>>,
}

/// A token-acquisition failure, classified for the estate's retry taxonomy.
#[derive(Debug, Clone)]
pub enum OAuthError {
    /// The auth block cannot be acted on (unknown grant, missing seed, the
    /// runtime not initialised). Non-retryable.
    Config(String),
    /// The IdP rejected the request (`invalid_grant`, `invalid_client`, …).
    /// Non-retryable; negative-cached.
    Rejected(String),
    /// The IdP could not be reached, answered a server error, or answered
    /// garbage. Retryable.
    Transport(String),
    /// Another node holds the refresh lease and its result did not appear in
    /// time. Retryable.
    NotReady(String),
}

impl OAuthError {
    /// Whether retrying can help — the dependency-health half of the estate's
    /// F42 split. Rejections and config problems do not fix themselves.
    pub fn retryable(&self) -> bool {
        matches!(self, OAuthError::Transport(_) | OAuthError::NotReady(_))
    }
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OAuthError::Config(m)
            | OAuthError::Rejected(m)
            | OAuthError::Transport(m)
            | OAuthError::NotReady(m) => f.write_str(m),
        }
    }
}

/// The persisted half of the lifecycle — one `connector_oauth_state` row's
/// `state_json`, decrypted. Epoch milliseconds rather than an `Instant` so it
/// means the same thing on every node.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PersistedState {
    access_token: String,
    expires_at_epoch_ms: u64,
    refresh_token: String,
}

/// One connector's cached token.
#[derive(Clone)]
struct CachedToken {
    access_token: String,
    expires_at: Instant,
    expires_at_epoch_ms: u64,
}

/// One connector's in-memory lifecycle state, all mutated under the entry's
/// mutex — which is exactly what makes the refresh single-flight.
struct EntryState {
    /// Fingerprint of the oauth2 block this state was minted under. A
    /// mismatch (the connector was edited) resets everything below.
    fingerprint: String,
    token: Option<CachedToken>,
    /// The freshest known refresh token (seed, persisted, or rotated).
    refresh_token: Option<String>,
    /// A recent IdP rejection: answered from memory for [`NEGATIVE_CACHE`].
    rejected: Option<(Instant, String)>,
}

impl EntryState {
    fn fresh(fingerprint: String) -> Self {
        Self {
            fingerprint,
            token: None,
            refresh_token: None,
            rejected: None,
        }
    }
}

/// The lock-free fast path: the last published token and the config it was
/// minted under. Written only by the slow path (holding `TokenEntry::state`),
/// read without any lock on every request — the common case is a cached token
/// comfortably inside its lifetime under an unchanged config, and it must not
/// queue behind an in-flight refresh or pay a serialize+hash fingerprint.
struct FastToken {
    config: Option<OAuth2Config>,
    token: Option<CachedToken>,
}

struct TokenEntry {
    fast: arc_swap::ArcSwap<FastToken>,
    state: tokio::sync::Mutex<EntryState>,
}

/// The process-wide token manager, held on the
/// [`ConnectorRegistry`](super::ConnectorRegistry) beside the circuit
/// breakers — per-connector runtime state keyed off the same identity.
pub struct OAuthTokenManager {
    entries: tokio::sync::RwLock<HashMap<String, Arc<TokenEntry>>>,
    deps: OnceLock<OAuthRuntimeDeps>,
}

impl Default for OAuthTokenManager {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthTokenManager {
    pub fn new() -> Self {
        Self {
            entries: tokio::sync::RwLock::new(HashMap::new()),
            deps: OnceLock::new(),
        }
    }

    /// Inject the runtime dependencies, once, from bootstrap. A second call
    /// (a test harness building over the same registry) is a no-op.
    pub fn init(&self, deps: OAuthRuntimeDeps) {
        let _ = self.deps.set(deps);
    }

    /// Drop the cached access token for `connector`, so the next call
    /// refetches. The refresh token is kept — it is still valid; only the
    /// access token was rejected. Called when the *API* answers 401 on an
    /// oauth2 connector: self-healing after IdP-side revocation.
    pub async fn invalidate(&self, connector: &str) {
        if let Some(entry) = self.entries.read().await.get(connector).cloned()
            && let Ok(mut st) = entry.state.try_lock()
        {
            st.token = None;
            let config = entry.fast.load().config.clone();
            entry.fast.store(Arc::new(FastToken {
                config,
                token: None,
            }));
        }
    }

    /// The current access token for `connector`, acquiring or refreshing as
    /// needed. `allow_private_urls` is the connector's own SSRF opt-out and
    /// covers the token endpoint too.
    pub async fn access_token(
        &self,
        connector: &str,
        cfg: &OAuth2Config,
        allow_private_urls: bool,
    ) -> Result<String, OAuthError> {
        let deps = self.deps.get().ok_or_else(|| {
            OAuthError::Config(
                "OAuth2 runtime is not initialised (managed OAuth2 is unavailable in \
                 this context)"
                    .to_string(),
            )
        })?;
        let grant = OAuth2Grant::parse(&cfg.grant).ok_or_else(|| {
            OAuthError::Config(format!(
                "connector '{connector}': unknown OAuth2 grant '{}' (expected {})",
                cfg.grant,
                OAuth2Grant::VALUES
            ))
        })?;
        let margin = Duration::from_secs(cfg.refresh_margin_secs.min(3600));
        let entry = self.entry(connector).await;

        // Fast path: exactly the condition the slow path answers from cache
        // (unchanged config, token comfortably fresh), without the mutex —
        // so requests never serialize behind an in-flight refresh while the
        // current token is still valid past the margin.
        {
            let fast = entry.fast.load();
            if fast.config.as_ref() == Some(cfg)
                && let Some(token) = fresh_token(&fast.token, margin)
            {
                return Ok(token);
            }
        }

        let fingerprint = fingerprint(cfg);
        let mut st = entry.state.lock().await;

        // The connector was edited since this state was minted: everything —
        // token, refresh token, negative cache — belongs to the old config.
        if st.fingerprint != fingerprint {
            *st = EntryState::fresh(fingerprint.clone());
        }

        // Re-check under the lock: a queued caller usually finds the winner's
        // freshly published token here.
        if let Some(token) = fresh_token(&st.token, margin) {
            return Ok(token);
        }
        if let Some((at, message)) = &st.rejected {
            if at.elapsed() < NEGATIVE_CACHE {
                return Err(OAuthError::Rejected(message.clone()));
            }
            st.rejected = None;
        }

        let result = match grant {
            OAuth2Grant::ClientCredentials => {
                self.acquire_client_credentials(deps, connector, cfg, allow_private_urls, &mut st)
                    .await
            }
            OAuth2Grant::AccountCredentials => {
                self.acquire_account_credentials(deps, connector, cfg, allow_private_urls, &mut st)
                    .await
            }
            OAuth2Grant::RefreshToken => {
                self.refresh_flow(
                    deps,
                    connector,
                    cfg,
                    &fingerprint,
                    margin,
                    allow_private_urls,
                    &mut st,
                )
                .await
            }
        };
        if let Err(e) = &result
            && !e.retryable()
        {
            st.rejected = Some((Instant::now(), e.to_string()));
        }
        // Publish for the fast path, whatever happened, while the lock is
        // still held (its writers are ordered by the mutex).
        entry.fast.store(Arc::new(FastToken {
            config: Some(cfg.clone()),
            token: st.token.clone(),
        }));
        result
    }

    async fn entry(&self, connector: &str) -> Arc<TokenEntry> {
        if let Some(entry) = self.entries.read().await.get(connector) {
            return Arc::clone(entry);
        }
        let mut entries = self.entries.write().await;
        Arc::clone(entries.entry(connector.to_string()).or_insert_with(|| {
            Arc::new(TokenEntry {
                fast: arc_swap::ArcSwap::new(Arc::new(FastToken {
                    config: None,
                    token: None,
                })),
                state: tokio::sync::Mutex::new(EntryState::fresh(String::new())),
            })
        }))
    }

    async fn acquire_client_credentials(
        &self,
        deps: &OAuthRuntimeDeps,
        connector: &str,
        cfg: &OAuth2Config,
        allow_private_urls: bool,
        st: &mut EntryState,
    ) -> Result<String, OAuthError> {
        let mut params: Vec<(&str, String)> =
            vec![("grant_type", "client_credentials".to_string())];
        if !cfg.scopes.is_empty() {
            params.push(("scope", cfg.scopes.join(" ")));
        }
        if let Some(a) = &cfg.audience {
            params.push(("audience", a.clone()));
        }
        if let Some(r) = &cfg.resource {
            params.push(("resource", r.clone()));
        }
        for (k, v) in &cfg.extra_params {
            params.push((k.as_str(), v.clone()));
        }
        let response = request_token(deps, connector, cfg, allow_private_urls, params).await?;
        Ok(cache_token(st, &response).access_token)
    }

    /// Zoom Server-to-Server OAuth: `grant_type=account_credentials` plus the
    /// account id, with Basic client auth (Zoom's expectation, and Orion's
    /// default).
    ///
    /// Everything below `acquire_*` is grant-agnostic, so this inherits with
    /// no new code: lazy acquisition, the per-connector `ArcSwap` fast path,
    /// early refresh, single-flight, the retryable/non-retryable failure split
    /// and its negative cache, 401-from-API self-healing, the token-request
    /// metric, and the admin connector probe.
    ///
    /// It deliberately does **not** inherit rotation persistence or the
    /// cluster job lease — those live only in `refresh_flow`. Zoom re-acquires
    /// from static credentials, so there is no `connector_oauth_state` row and
    /// no lease to take: correct by construction, not by omission.
    async fn acquire_account_credentials(
        &self,
        deps: &OAuthRuntimeDeps,
        connector: &str,
        cfg: &OAuth2Config,
        allow_private_urls: bool,
        st: &mut EntryState,
    ) -> Result<String, OAuthError> {
        let mut params: Vec<(&str, String)> =
            vec![("grant_type", "account_credentials".to_string())];
        // Validation requires it for this grant, so an absent value here means
        // a hand-edited row: fail as a non-retryable config error rather than
        // sending Zoom a request it will reject on every attempt.
        let Some(account_id) = &cfg.account_id else {
            return Err(OAuthError::Config(
                "oauth2 account_credentials requires 'account_id'".to_string(),
            ));
        };
        params.push(("account_id", account_id.clone()));
        if !cfg.scopes.is_empty() {
            params.push(("scope", cfg.scopes.join(" ")));
        }
        for (k, v) in &cfg.extra_params {
            params.push((k.as_str(), v.clone()));
        }
        let response = request_token(deps, connector, cfg, allow_private_urls, params).await?;
        Ok(cache_token(st, &response).access_token)
    }

    #[allow(clippy::too_many_arguments)]
    async fn refresh_flow(
        &self,
        deps: &OAuthRuntimeDeps,
        connector: &str,
        cfg: &OAuth2Config,
        fingerprint: &str,
        margin: Duration,
        allow_private_urls: bool,
        st: &mut EntryState,
    ) -> Result<String, OAuthError> {
        // First use in this process: the freshest refresh token may be a
        // rotation another node (or a previous run) persisted — and its
        // access token may still be perfectly good.
        if st.refresh_token.is_none() {
            if let Some(persisted) = load_state(deps, connector, fingerprint).await {
                st.refresh_token = Some(persisted.refresh_token.clone());
                if let Some(token) = adopt(st, &persisted, margin) {
                    return Ok(token);
                }
            } else {
                let seed = cfg.refresh_token.as_deref().unwrap_or("").trim();
                if seed.is_empty() {
                    return Err(OAuthError::Config(format!(
                        "connector '{connector}': the refresh_token grant needs a \
                         'refresh_token' seed in the auth block"
                    )));
                }
                st.refresh_token = Some(seed.to_string());
            }
        }

        // Cross-node single-flight: rotation must happen on one node. A lease
        // loser adopts the winner's persisted token instead of re-racing.
        if let Some(lease) = &deps.lease {
            let job = format!("oauth-refresh:{connector}");
            if !lease.try_acquire(&job, LEASE_TTL_SECS).await {
                for _ in 0..ADOPT_POLL_ATTEMPTS {
                    tokio::time::sleep(ADOPT_POLL_INTERVAL).await;
                    if let Some(persisted) = load_state(deps, connector, fingerprint).await {
                        st.refresh_token = Some(persisted.refresh_token.clone());
                        if let Some(token) = adopt(st, &persisted, margin) {
                            return Ok(token);
                        }
                    }
                }
                return Err(OAuthError::NotReady(format!(
                    "connector '{connector}': another node is refreshing the OAuth2 \
                     token and its result has not appeared yet"
                )));
            }
        }

        let current_rt = st
            .refresh_token
            .clone()
            .expect("ensured above: persisted, adopted, or seeded");
        let mut params: Vec<(&str, String)> = vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", current_rt.clone()),
        ];
        for (k, v) in &cfg.extra_params {
            params.push((k.as_str(), v.clone()));
        }
        let response = request_token(deps, connector, cfg, allow_private_urls, params).await?;
        let cached = cache_token(st, &response);

        // Rotation: the response's refresh token (when present) replaces the
        // one just spent. Persist token + rotation before anyone else asks.
        let next_rt = response.refresh_token.clone().unwrap_or(current_rt);
        st.refresh_token = Some(next_rt.clone());
        let persisted = PersistedState {
            access_token: cached.access_token.clone(),
            expires_at_epoch_ms: cached.expires_at_epoch_ms,
            refresh_token: next_rt,
        };
        match serde_json::to_string(&persisted) {
            Ok(json) => {
                if let Err(e) = deps
                    .repo
                    .put_oauth_state(connector, fingerprint, &json)
                    .await
                {
                    // The live values are still in memory, so the next
                    // refresh retries persistence — but a restart during a
                    // persistent failure here loses the rotation.
                    crate::metrics::record_error("oauth_state_persist");
                    tracing::error!(
                        connector,
                        error = %e,
                        "failed to persist rotated OAuth2 refresh token; \
                         it survives only in memory until the next refresh"
                    );
                }
            }
            Err(e) => {
                crate::metrics::record_error("oauth_state_persist");
                tracing::error!(connector, error = %e, "OAuth2 state did not serialize");
            }
        }
        Ok(cached.access_token)
    }
}

/// The cached token when it is still comfortably inside its lifetime.
fn fresh_token(token: &Option<CachedToken>, margin: Duration) -> Option<String> {
    let token = token.as_ref()?;
    (Instant::now() + margin < token.expires_at).then(|| token.access_token.clone())
}

/// Store a token response in the entry and hand back the cached form — the
/// caller reads the expiry from it rather than re-deriving it from `st`.
fn cache_token(st: &mut EntryState, response: &TokenResponse) -> CachedToken {
    let ttl = response
        .expires_in
        .unwrap_or(DEFAULT_TOKEN_TTL_SECS)
        .max(MIN_TOKEN_TTL_SECS);
    let token = CachedToken {
        access_token: response.access_token.clone(),
        expires_at: Instant::now() + Duration::from_secs(ttl),
        expires_at_epoch_ms: epoch_ms_now().saturating_add(ttl * 1000),
    };
    st.token = Some(token.clone());
    token
}

/// Adopt a persisted access token when it is still fresh under `margin`.
fn adopt(st: &mut EntryState, persisted: &PersistedState, margin: Duration) -> Option<String> {
    let now_ms = epoch_ms_now();
    let margin_ms = margin.as_millis() as u64;
    let remaining_ms = persisted.expires_at_epoch_ms.checked_sub(now_ms)?;
    if remaining_ms <= margin_ms {
        return None;
    }
    st.token = Some(CachedToken {
        access_token: persisted.access_token.clone(),
        expires_at: Instant::now() + Duration::from_millis(remaining_ms),
        expires_at_epoch_ms: persisted.expires_at_epoch_ms,
    });
    Some(persisted.access_token.clone())
}

/// The persisted state for `connector`, if any exists **for this exact
/// config** — a fingerprint mismatch means the connector was edited and the
/// stored state belongs to the old credentials.
async fn load_state(
    deps: &OAuthRuntimeDeps,
    connector: &str,
    fingerprint: &str,
) -> Option<PersistedState> {
    match deps.repo.get_oauth_state(connector).await {
        Ok(Some(row)) if row.fingerprint == fingerprint => {
            serde_json::from_str(&row.state_json).ok()
        }
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(connector, error = %e, "could not read OAuth2 state");
            None
        }
    }
}

/// A fingerprint of the oauth2 block: what ties cached and persisted state to
/// the exact credentials/endpoint they were minted under.
fn fingerprint(cfg: &OAuth2Config) -> String {
    let serialized = serde_json::to_string(cfg).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(serialized.as_bytes());
    hex::encode(hasher.finalize())
}

fn epoch_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// The RFC 6749 §5.1 fields this build reads.
#[derive(Debug, Clone)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
}

/// One form-encoded token request (RFC 6749 §4.4.2 / §6), classified per the
/// failure taxonomy and counted in `orion_oauth_token_requests_total`.
async fn request_token(
    deps: &OAuthRuntimeDeps,
    connector: &str,
    cfg: &OAuth2Config,
    allow_private_urls: bool,
    params: Vec<(&str, String)>,
) -> Result<TokenResponse, OAuthError> {
    let result = request_token_inner(deps, cfg, allow_private_urls, params).await;
    let outcome = match &result {
        Ok(_) => "ok",
        Err(OAuthError::Rejected(_)) => "rejected",
        Err(_) => "transport_error",
    };
    crate::metrics::record_oauth_token_request(connector, outcome);
    result.map_err(|e| prefix_connector(connector, e))
}

/// Put the connector name on the message once, at the boundary.
fn prefix_connector(connector: &str, e: OAuthError) -> OAuthError {
    let wrap = |m: String| format!("connector '{connector}': {m}");
    match e {
        OAuthError::Config(m) => OAuthError::Config(wrap(m)),
        OAuthError::Rejected(m) => OAuthError::Rejected(wrap(m)),
        OAuthError::Transport(m) => OAuthError::Transport(wrap(m)),
        OAuthError::NotReady(m) => OAuthError::NotReady(wrap(m)),
    }
}

async fn request_token_inner(
    deps: &OAuthRuntimeDeps,
    cfg: &OAuth2Config,
    allow_private_urls: bool,
    mut params: Vec<(&str, String)>,
) -> Result<TokenResponse, OAuthError> {
    // The token endpoint gets the same SSRF treatment as the connector's own
    // endpoint — it is admin-authored config, but the check is cheap and the
    // opt-out is the connector's existing one.
    if !allow_private_urls
        && let Err(msg) = crate::validation::validate_url_not_private(&cfg.token_url).await
    {
        return Err(OAuthError::Config(format!("SSRF protection: {msg}")));
    }

    let client_auth = OAuth2ClientAuth::parse(&cfg.client_auth).ok_or_else(|| {
        OAuthError::Config(format!(
            "unknown client_auth '{}' (expected {})",
            cfg.client_auth,
            OAuth2ClientAuth::VALUES
        ))
    })?;
    let mut req = deps.http_client.post(&cfg.token_url).timeout(TOKEN_TIMEOUT);
    match client_auth {
        OAuth2ClientAuth::Basic => {
            req = req.basic_auth(&cfg.client_id, Some(&cfg.client_secret));
        }
        OAuth2ClientAuth::Body => {
            params.push(("client_id", cfg.client_id.clone()));
            params.push(("client_secret", cfg.client_secret.clone()));
        }
    }

    // Form-encoded per RFC 6749 §4 — the same encoder `http_call`'s
    // `body_format: "form"` uses (#261), built manually because the shared
    // client is compiled without reqwest's form feature. Scoped: the
    // serializer is not `Send` and must not live across the await.
    let form_body = {
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in &params {
            ser.append_pair(k, v);
        }
        ser.finish()
    };
    let response = req
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body)
        .send()
        .await
        .map_err(|e| OAuthError::Transport(format!("token request failed: {e}")))?;
    let status = response.status();
    if let Some(len) = response.content_length()
        && len as usize > MAX_TOKEN_RESPONSE_BYTES
    {
        return Err(OAuthError::Transport(format!(
            "token response declared Content-Length {len} (cap {MAX_TOKEN_RESPONSE_BYTES})"
        )));
    }
    let body = response
        .bytes()
        .await
        .map_err(|e| OAuthError::Transport(format!("token response read failed: {e}")))?;
    if body.len() > MAX_TOKEN_RESPONSE_BYTES {
        return Err(OAuthError::Transport(format!(
            "token response is {} bytes (cap {MAX_TOKEN_RESPONSE_BYTES})",
            body.len()
        )));
    }
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();

    if !status.is_success() {
        // RFC 6749 §5.2: the error code is bounded vocabulary and safe to
        // surface; the free-text description is logged, not returned.
        let code = json
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("no error code");
        if let Some(desc) = json.get("error_description").and_then(|d| d.as_str()) {
            tracing::warn!(
                status = status.as_u16(),
                code,
                desc,
                "OAuth2 token request rejected"
            );
        }
        if status.is_client_error() {
            return Err(OAuthError::Rejected(format!(
                "token endpoint rejected the request ({status}, {code}) — check the \
                 credentials, grant, and (for refresh_token) whether the seed is \
                 still valid; re-seed the connector to recover a burned token"
            )));
        }
        return Err(OAuthError::Transport(format!(
            "token endpoint answered {status} ({code})"
        )));
    }

    let access_token = json
        .get("access_token")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            OAuthError::Transport("token response carried no access_token".to_string())
        })?;
    if let Some(token_type) = json.get("token_type").and_then(|t| t.as_str())
        && !token_type.eq_ignore_ascii_case("bearer")
    {
        return Err(OAuthError::Config(format!(
            "token endpoint issued a '{token_type}' token; only Bearer is supported"
        )));
    }
    Ok(TokenResponse {
        access_token: access_token.to_string(),
        expires_in: json.get("expires_in").and_then(|e| e.as_u64()),
        refresh_token: json
            .get("refresh_token")
            .and_then(|t| t.as_str())
            .map(str::to_string),
    })
}

/// The auth to actually apply for one request to `connector`: static variants
/// pass through untouched; `oauth2` resolves to a Bearer token through the
/// manager. The typed [`OAuthError`] is surfaced so the caller can map it
/// into its own error vocabulary (the engine maps retryable → `Io`).
pub async fn effective_auth<'a>(
    manager: &OAuthTokenManager,
    connector: &str,
    http: &'a HttpConnectorConfig,
) -> Result<Option<std::borrow::Cow<'a, super::config::AuthConfig>>, OAuthError> {
    use super::config::AuthConfig;
    match &http.auth {
        None => Ok(None),
        Some(AuthConfig::OAuth2(cfg)) => {
            let token = manager
                .access_token(connector, cfg, http.allow_private_urls)
                .await?;
            Ok(Some(std::borrow::Cow::Owned(AuthConfig::Bearer { token })))
        }
        Some(other) => Ok(Some(std::borrow::Cow::Borrowed(other))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::config::AuthConfig;
    use crate::connector::test_support::StubConnectorRepo;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// What the fake IdP records and how it answers — the observable half of
    /// every test below.
    struct IdpState {
        hits: AtomicU64,
        /// The `refresh_token` presented on the most recent refresh.
        last_refresh_token: Mutex<Option<String>>,
        /// Whether the last request carried HTTP Basic client auth.
        last_had_basic: Mutex<bool>,
        last_form: Mutex<Vec<(String, String)>>,
        /// The canned answer: `(status, body)`.
        respond: Mutex<(u16, Value)>,
    }

    impl IdpState {
        fn ok(body: Value) -> Arc<Self> {
            Arc::new(Self {
                hits: AtomicU64::new(0),
                last_refresh_token: Mutex::new(None),
                last_had_basic: Mutex::new(false),
                last_form: Mutex::new(Vec::new()),
                respond: Mutex::new((200, body)),
            })
        }
        fn hits(&self) -> u64 {
            self.hits.load(Ordering::SeqCst)
        }
        fn set_response(&self, status: u16, body: Value) {
            *self.respond.lock().expect("test") = (status, body);
        }
    }

    /// An in-process token endpoint speaking just enough RFC 6749.
    async fn fake_idp(state: Arc<IdpState>) -> String {
        async fn token(
            State(st): State<Arc<IdpState>>,
            headers: HeaderMap,
            body: String,
        ) -> (StatusCode, axum::Json<Value>) {
            let form: Vec<(String, String)> = url::form_urlencoded::parse(body.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            st.hits.fetch_add(1, Ordering::SeqCst);
            *st.last_had_basic.lock().expect("test") = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("Basic "));
            *st.last_refresh_token.lock().expect("test") = form
                .iter()
                .find(|(k, _)| k == "refresh_token")
                .map(|(_, v)| v.clone());
            *st.last_form.lock().expect("test") = form;
            let (status, body) = st.respond.lock().expect("test").clone();
            (
                StatusCode::from_u16(status).expect("test status"),
                axum::Json(body),
            )
        }
        let app = axum::Router::new()
            .route("/token", axum::routing::post(token))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        format!("http://{addr}/token")
    }

    fn manager_with(repo: Arc<StubConnectorRepo>) -> OAuthTokenManager {
        let manager = OAuthTokenManager::new();
        manager.init(OAuthRuntimeDeps {
            http_client: reqwest::Client::new(),
            repo,
            lease: None,
        });
        manager
    }

    fn cc_config(token_url: &str) -> OAuth2Config {
        OAuth2Config {
            grant: "client_credentials".to_string(),
            token_url: token_url.to_string(),
            client_id: "cid".to_string(),
            client_secret: "csecret".to_string(),
            client_auth: "basic".to_string(),
            refresh_token: None,
            scopes: vec!["api.read".to_string(), "api.write".to_string()],
            audience: Some("https://api.example.com".to_string()),
            resource: None,
            account_id: None,
            extra_params: HashMap::new(),
            refresh_margin_secs: 1,
        }
    }

    fn rt_config(token_url: &str, seed: &str) -> OAuth2Config {
        OAuth2Config {
            grant: "refresh_token".to_string(),
            refresh_token: Some(seed.to_string()),
            scopes: Vec::new(),
            audience: None,
            ..cc_config(token_url)
        }
    }

    /// Zoom Server-to-Server: the account id replaces the audience, and there
    /// is no seed to carry.
    fn ac_config(token_url: &str) -> OAuth2Config {
        OAuth2Config {
            grant: "account_credentials".to_string(),
            account_id: Some("zoom-acct-1".to_string()),
            audience: None,
            scopes: Vec::new(),
            ..cc_config(token_url)
        }
    }

    // ---- client_credentials: acquisition, caching, request shape ----

    #[tokio::test]
    async fn a_cached_token_serves_every_call_until_the_margin() {
        let idp = IdpState::ok(json!({
            "access_token": "tok-1", "token_type": "Bearer", "expires_in": 3600
        }));
        let url = fake_idp(Arc::clone(&idp)).await;
        let manager = manager_with(Arc::new(StubConnectorRepo::with(vec![])));
        let cfg = cc_config(&url);

        for _ in 0..5 {
            let token = manager
                .access_token("crm", &cfg, true)
                .await
                .expect("token");
            assert_eq!(token, "tok-1");
        }
        assert_eq!(idp.hits(), 1, "one acquisition serves the cache window");

        // The request carried the RFC 6749 shape: Basic client auth, the
        // grant, the space-joined scopes, and the audience.
        assert!(*idp.last_had_basic.lock().expect("test"));
        let form = idp.last_form.lock().expect("test").clone();
        let get = |k: &str| {
            form.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("grant_type").as_deref(), Some("client_credentials"));
        assert_eq!(get("scope").as_deref(), Some("api.read api.write"));
        assert_eq!(get("audience").as_deref(), Some("https://api.example.com"));
        assert_eq!(
            get("client_id"),
            None,
            "basic auth puts nothing in the body"
        );
    }

    // ---- account_credentials (Zoom Server-to-Server) ----

    /// #273: the grant's whole request shape —
    /// `grant_type=account_credentials`, the account id, and Basic client
    /// auth, which is what Zoom expects and Orion's default.
    #[tokio::test]
    async fn account_credentials_sends_the_zoom_request_shape() {
        let idp = IdpState::ok(json!({
            "access_token": "zoom-tok", "token_type": "Bearer", "expires_in": 3600
        }));
        let url = fake_idp(Arc::clone(&idp)).await;
        let manager = manager_with(Arc::new(StubConnectorRepo::with(vec![])));
        let cfg = ac_config(&url);

        let token = manager
            .access_token("zoom", &cfg, true)
            .await
            .expect("token");
        assert_eq!(token, "zoom-tok");

        assert!(
            *idp.last_had_basic.lock().expect("test"),
            "Zoom authenticates the client with HTTP Basic"
        );
        let form = idp.last_form.lock().expect("test").clone();
        let get = |k: &str| {
            form.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("grant_type").as_deref(), Some("account_credentials"));
        assert_eq!(get("account_id").as_deref(), Some("zoom-acct-1"));
        assert_eq!(
            get("client_id"),
            None,
            "basic auth puts nothing in the body"
        );
    }

    /// The grant inherits the cache for free — it goes through the same
    /// `cache_token` path as `client_credentials`, with no rotation state.
    #[tokio::test]
    async fn an_account_credentials_token_is_cached_like_any_other() {
        let idp = IdpState::ok(json!({
            "access_token": "zoom-tok", "expires_in": 3600
        }));
        let url = fake_idp(Arc::clone(&idp)).await;
        let repo = Arc::new(StubConnectorRepo::with(vec![]));
        let manager = manager_with(Arc::clone(&repo) as Arc<StubConnectorRepo>);
        let cfg = ac_config(&url);

        for _ in 0..5 {
            assert_eq!(
                manager
                    .access_token("zoom", &cfg, true)
                    .await
                    .expect("token"),
                "zoom-tok"
            );
        }
        assert_eq!(idp.hits(), 1, "one acquisition serves the cache window");
        assert!(
            repo.get_oauth_state("zoom").await.expect("repo").is_none(),
            "re-acquired from static credentials, so there is no rotation state \
             to persist and no cluster lease to take"
        );
    }

    /// ⚠️ The upgrade hazard, pinned. `fingerprint` hashes the whole
    /// serialized `OAuth2Config`, and a persisted state row is adopted only on
    /// a fingerprint match. If `account_id` serialized as `"account_id": null`
    /// when unset, **every existing refresh_token connector** would change
    /// fingerprint on upgrade, discard its persisted state and fall back to
    /// the config seed — which, after any prior rotation, is a spent refresh
    /// token. `skip_serializing_if` is what prevents that, and this test is
    /// what stops someone "tidying" it away.
    #[test]
    fn an_unset_account_id_does_not_move_an_existing_fingerprint() {
        let cfg = rt_config("https://idp.example/token", "rt-seed");
        let serialized = serde_json::to_string(&cfg).expect("json");
        assert!(
            !serialized.contains("account_id"),
            "an unset account_id must not serialize at all, or it changes the \
             fingerprint of every stored connector: {serialized}"
        );
    }

    #[tokio::test]
    async fn client_auth_body_moves_the_credentials_into_the_form() {
        let idp = IdpState::ok(json!({ "access_token": "t", "expires_in": 3600 }));
        let url = fake_idp(Arc::clone(&idp)).await;
        let manager = manager_with(Arc::new(StubConnectorRepo::with(vec![])));
        let cfg = OAuth2Config {
            client_auth: "body".to_string(),
            ..cc_config(&url)
        };
        manager
            .access_token("crm", &cfg, true)
            .await
            .expect("token");
        assert!(!*idp.last_had_basic.lock().expect("test"));
        let form = idp.last_form.lock().expect("test").clone();
        assert!(form.iter().any(|(k, v)| k == "client_id" && v == "cid"));
        assert!(
            form.iter()
                .any(|(k, v)| k == "client_secret" && v == "csecret")
        );
    }

    /// The single-flight guarantee: concurrent first requests produce one
    /// token acquisition, not a race.
    #[tokio::test]
    async fn concurrent_requests_share_one_acquisition() {
        let idp = IdpState::ok(json!({ "access_token": "t", "expires_in": 3600 }));
        let url = fake_idp(Arc::clone(&idp)).await;
        let manager = Arc::new(manager_with(Arc::new(StubConnectorRepo::with(vec![]))));
        let cfg = Arc::new(cc_config(&url));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let manager = Arc::clone(&manager);
            let cfg = Arc::clone(&cfg);
            handles.push(tokio::spawn(async move {
                manager.access_token("crm", &cfg, true).await
            }));
        }
        for h in handles {
            h.await.expect("join").expect("token");
        }
        assert_eq!(idp.hits(), 1, "eight callers, one token request");
    }

    // ---- refresh_token: seed, rotation, persistence, adoption ----

    #[tokio::test]
    async fn rotation_persists_and_the_next_refresh_uses_the_new_token() {
        let idp = IdpState::ok(json!({
            "access_token": "at-1", "expires_in": 3600, "refresh_token": "rt-2"
        }));
        let url = fake_idp(Arc::clone(&idp)).await;
        let repo = Arc::new(StubConnectorRepo::with(vec![]));
        let manager = manager_with(Arc::clone(&repo) as Arc<StubConnectorRepo>);
        let cfg = rt_config(&url, "rt-1");

        manager
            .access_token("crm", &cfg, true)
            .await
            .expect("token");
        assert_eq!(
            idp.last_refresh_token.lock().expect("test").as_deref(),
            Some("rt-1"),
            "the first refresh presents the seed"
        );

        // The rotation was persisted with the fingerprint.
        let row = repo
            .get_oauth_state("crm")
            .await
            .expect("read")
            .expect("state row");
        assert_eq!(row.fingerprint, fingerprint(&cfg));
        let persisted: PersistedState = serde_json::from_str(&row.state_json).expect("state json");
        assert_eq!(persisted.refresh_token, "rt-2");
        assert_eq!(persisted.access_token, "at-1");

        // Invalidate the access token: the next refresh presents the
        // *rotated* token, not the burned seed.
        manager.invalidate("crm").await;
        idp.set_response(
            200,
            json!({ "access_token": "at-2", "expires_in": 3600, "refresh_token": "rt-3" }),
        );
        manager
            .access_token("crm", &cfg, true)
            .await
            .expect("token");
        assert_eq!(
            idp.last_refresh_token.lock().expect("test").as_deref(),
            Some("rt-2")
        );
    }

    /// A fresh process (or another node) adopts persisted state instead of
    /// spending a refresh — zero IdP traffic.
    #[tokio::test]
    async fn persisted_state_is_adopted_without_touching_the_idp() {
        let idp = IdpState::ok(json!({ "access_token": "never", "expires_in": 3600 }));
        let url = fake_idp(Arc::clone(&idp)).await;
        let repo = Arc::new(StubConnectorRepo::with(vec![]));
        let cfg = rt_config(&url, "rt-seed");
        let state = PersistedState {
            access_token: "adopted".to_string(),
            expires_at_epoch_ms: epoch_ms_now() + 3_600_000,
            refresh_token: "rt-live".to_string(),
        };
        repo.put_oauth_state(
            "crm",
            &fingerprint(&cfg),
            &serde_json::to_string(&state).expect("json"),
        )
        .await
        .expect("seed state");

        let manager = manager_with(Arc::clone(&repo) as Arc<StubConnectorRepo>);
        let token = manager
            .access_token("crm", &cfg, true)
            .await
            .expect("token");
        assert_eq!(token, "adopted");
        assert_eq!(idp.hits(), 0, "adoption costs no token request");
    }

    /// Editing the connector discards state minted under the old config —
    /// which is exactly the burned-token recovery story: re-seed, refresh.
    #[tokio::test]
    async fn a_fingerprint_mismatch_falls_back_to_the_seed() {
        let idp = IdpState::ok(json!({
            "access_token": "at", "expires_in": 3600, "refresh_token": "rt-next"
        }));
        let url = fake_idp(Arc::clone(&idp)).await;
        let repo = Arc::new(StubConnectorRepo::with(vec![]));
        let stale = PersistedState {
            access_token: "stale".to_string(),
            expires_at_epoch_ms: epoch_ms_now() + 3_600_000,
            refresh_token: "rt-stale".to_string(),
        };
        repo.put_oauth_state(
            "crm",
            "an-old-fingerprint",
            &serde_json::to_string(&stale).expect("json"),
        )
        .await
        .expect("seed state");

        let manager = manager_with(Arc::clone(&repo) as Arc<StubConnectorRepo>);
        let cfg = rt_config(&url, "rt-reseeded");
        manager
            .access_token("crm", &cfg, true)
            .await
            .expect("token");
        assert_eq!(
            idp.last_refresh_token.lock().expect("test").as_deref(),
            Some("rt-reseeded"),
            "stale state must lose to the freshly-seeded config"
        );
    }

    // ---- failure taxonomy ----

    /// `invalid_grant` is non-retryable and negative-cached: a burned token
    /// is never retry-looped against the IdP.
    #[tokio::test]
    async fn a_rejection_is_negative_cached_and_non_retryable() {
        let idp = IdpState::ok(json!({})); // replaced below
        idp.set_response(
            400,
            json!({ "error": "invalid_grant", "error_description": "revoked" }),
        );
        let url = fake_idp(Arc::clone(&idp)).await;
        let manager = manager_with(Arc::new(StubConnectorRepo::with(vec![])));
        let cfg = rt_config(&url, "rt-burned");

        let err = manager
            .access_token("crm", &cfg, true)
            .await
            .expect_err("burned token");
        assert!(!err.retryable(), "{err}");
        assert!(err.to_string().contains("invalid_grant"), "{err}");
        assert!(err.to_string().contains("re-seed"), "{err}");
        assert!(
            !err.to_string().contains("revoked"),
            "the free-text description is logged, never surfaced: {err}"
        );

        for _ in 0..3 {
            let err = manager
                .access_token("crm", &cfg, true)
                .await
                .expect_err("still cached");
            assert!(!err.retryable(), "{err}");
        }
        assert_eq!(idp.hits(), 1, "the rejection is answered from memory");
    }

    #[tokio::test]
    async fn a_server_error_is_retryable_and_not_cached() {
        let idp = IdpState::ok(json!({}));
        idp.set_response(503, json!({ "error": "temporarily_unavailable" }));
        let url = fake_idp(Arc::clone(&idp)).await;
        let manager = manager_with(Arc::new(StubConnectorRepo::with(vec![])));
        let cfg = cc_config(&url);

        for _ in 0..2 {
            let err = manager
                .access_token("crm", &cfg, true)
                .await
                .expect_err("5xx");
            assert!(err.retryable(), "{err}");
        }
        assert_eq!(idp.hits(), 2, "transport failures are not negative-cached");
    }

    #[tokio::test]
    async fn config_mistakes_are_named_without_a_request() {
        let manager = manager_with(Arc::new(StubConnectorRepo::with(vec![])));
        let mut cfg = cc_config("http://127.0.0.1:9/token");
        cfg.grant = "password".to_string();
        let err = manager
            .access_token("crm", &cfg, true)
            .await
            .expect_err("ROPC is not a thing here");
        assert!(err.to_string().contains("password"), "{err}");
        assert!(err.to_string().contains(OAuth2Grant::VALUES), "{err}");

        let cfg = OAuth2Config {
            refresh_token: None,
            ..rt_config("http://127.0.0.1:9/token", "x")
        };
        let err = manager
            .access_token("crm", &cfg, true)
            .await
            .expect_err("no seed");
        assert!(err.to_string().contains("refresh_token"), "{err}");
    }

    /// An uninitialised manager (a bare registry in a unit test) refuses
    /// clearly instead of panicking.
    #[tokio::test]
    async fn an_uninitialised_manager_refuses_cleanly() {
        let manager = OAuthTokenManager::new();
        let err = manager
            .access_token("crm", &cc_config("http://x/token"), true)
            .await
            .expect_err("no deps");
        assert!(err.to_string().contains("not initialised"), "{err}");
        assert!(!err.retryable());
    }

    // ---- effective_auth: the seam the request paths use ----

    #[tokio::test]
    async fn effective_auth_passes_static_variants_through() {
        let manager = OAuthTokenManager::new();
        let http = crate::connector::HttpConnectorConfig {
            auth: Some(AuthConfig::Bearer {
                token: "static".to_string(),
            }),
            ..http_config_base()
        };
        let auth = effective_auth(&manager, "crm", &http)
            .await
            .expect("static auth needs no runtime");
        assert!(matches!(
            auth.as_deref(),
            Some(AuthConfig::Bearer { token }) if token == "static"
        ));

        let no_auth = crate::connector::HttpConnectorConfig {
            auth: None,
            ..http_config_base()
        };
        assert!(
            effective_auth(&manager, "crm", &no_auth)
                .await
                .expect("no auth")
                .is_none()
        );
    }

    #[tokio::test]
    async fn effective_auth_resolves_oauth2_to_a_bearer() {
        let idp = IdpState::ok(json!({ "access_token": "resolved", "expires_in": 3600 }));
        let url = fake_idp(Arc::clone(&idp)).await;
        let manager = manager_with(Arc::new(StubConnectorRepo::with(vec![])));
        let http = crate::connector::HttpConnectorConfig {
            auth: Some(AuthConfig::OAuth2(Box::new(cc_config(&url)))),
            allow_private_urls: true,
            ..http_config_base()
        };
        let auth = effective_auth(&manager, "crm", &http)
            .await
            .expect("resolves");
        assert!(matches!(
            auth.as_deref(),
            Some(AuthConfig::Bearer { token }) if token == "resolved"
        ));
    }

    fn http_config_base() -> crate::connector::HttpConnectorConfig {
        serde_json::from_value(json!({
            "url": "https://api.example.com",
            "method": "GET"
        }))
        .expect("base http config")
    }
}
