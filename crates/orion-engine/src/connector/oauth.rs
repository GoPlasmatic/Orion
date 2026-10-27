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
use crate::storage::repositories::oauth_state::OAuthStateRepository;

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
    pub repo: Arc<dyn OAuthStateRepository>,
    /// Cross-node single-flight for refresh-token rotation. `None` on a
    /// single node, where the per-connector mutex already serialises.
    pub lease: Option<Arc<crate::storage::repositories::job_lease::JobLeaseGate>>,
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
    /// The invalidation generation `token` was acquired under — the same
    /// stamp the published [`FastToken`] carries, so both paths refuse a
    /// token a 401 has already rejected.
    generation: u64,
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
            generation: 0,
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
    /// The invalidation generation this token was acquired under. Stale
    /// against [`TokenEntry::invalidated`] means a 401 arrived since, and the
    /// fast path must not serve it. See [`OAuthTokenManager::invalidate`].
    generation: u64,
}

struct TokenEntry {
    fast: arc_swap::ArcSwap<FastToken>,
    state: tokio::sync::Mutex<EntryState>,
    /// Bumped by [`OAuthTokenManager::invalidate`] on every API 401, without
    /// taking `state`.
    ///
    /// Not under `state`: a `try_lock` that skipped when another task held it
    /// would skip exactly when it matters, because the task holding it is
    /// usually the one about to publish a token. A skipped invalidation would
    /// leave the rejected token fast-path eligible until its refresh margin,
    /// so the connector would answer 401 after 401 while a perfectly good
    /// refresh sat one call away.
    ///
    /// A counter rather than a flag, and read *before* a fetch rather than
    /// after: that is what makes an invalidation arriving mid-flight
    /// survive. See `access_token`.
    invalidated: std::sync::atomic::AtomicU64,
}

/// The process-wide token manager, held on the
/// [`ConnectorRegistry`](super::ConnectorRegistry) beside the circuit
/// breakers — per-connector runtime state keyed off the same identity.
/// The context every grant acquires against: what the connector is, how it is
/// configured, and the SSRF opt-out that config carries.
///
/// One shape for all three grants, so the dispatch reads as three ways of
/// doing the same thing rather than three unrelated argument lists — and so
/// `refresh_flow`, which needs two fields the others do not, does not become a
/// seven-parameter signature where transposing `connector` and `fingerprint`
/// compiles.
#[derive(Clone, Copy)]
struct GrantCtx<'a> {
    deps: &'a OAuthRuntimeDeps,
    connector: &'a str,
    cfg: &'a OAuth2Config,
    /// Ties cached and persisted state to the config that produced it; only
    /// the refresh-token grant persists a row, so only it reads this.
    fingerprint: &'a str,
    /// How early a token counts as stale. Read by the refresh grant, which can
    /// adopt another node's persisted access token if it is still fresh.
    margin: Duration,
    /// The connector's own SSRF opt-out.
    allow_private_urls: bool,
}

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
    ///
    /// Cannot fail and cannot wait. It takes no lock at all: bumping the
    /// generation is what invalidates, and both the fast path and the slow
    /// path compare against it. Clearing the published token as well is a
    /// courtesy — it saves the next caller a trip through the mutex — but it
    /// is not what makes this correct, because an acquisition already in
    /// flight will publish over it. The generation is what that publish
    /// cannot outrun.
    pub async fn invalidate(&self, connector: &str) {
        let Some(entry) = self.entries.read().await.get(connector).cloned() else {
            return;
        };
        // Release: a reader that sees this bump must also see everything
        // written before the 401 that prompted it.
        entry
            .invalidated
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        let fast = entry.fast.load();
        entry.fast.store(Arc::new(FastToken {
            config: fast.config.clone(),
            token: None,
            generation: fast.generation,
        }));
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

        // The invalidation generation, read once and used for both paths.
        // Read *before* anything else so that an invalidation racing this call
        // is either already visible here (and honoured) or lands after the
        // token this call publishes is stamped — in which case the stamp is
        // stale and the *next* caller refetches. Either way it is not lost,
        // which is the whole difference from the `try_lock` this replaces.
        let generation = entry.invalidated.load(std::sync::atomic::Ordering::Acquire);

        // Fast path: exactly the condition the slow path answers from cache
        // (unchanged config, token comfortably fresh, no 401 since it was
        // acquired), without the mutex — so requests never serialize behind an
        // in-flight refresh while the current token is still valid past the
        // margin.
        {
            let fast = entry.fast.load();
            if fast.generation == generation
                && fast.config.as_ref() == Some(cfg)
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

        // A 401 since this token was cached means it is not a token any more,
        // however fresh its expiry looks. Dropped here rather than by
        // `invalidate` itself, because `invalidate` deliberately takes no
        // lock — the generation is the message, this is where it is read.
        if st.generation < generation {
            st.token = None;
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

        let ctx = GrantCtx {
            deps,
            connector,
            cfg,
            fingerprint: &fingerprint,
            margin,
            allow_private_urls,
        };
        let result = match grant {
            OAuth2Grant::ClientCredentials => self.acquire_client_credentials(&ctx, &mut st).await,
            OAuth2Grant::AccountCredentials => {
                self.acquire_account_credentials(&ctx, &mut st).await
            }
            OAuth2Grant::RefreshToken => self.refresh_flow(&ctx, &mut st).await,
        };
        if let Err(e) = &result
            && !e.retryable()
        {
            st.rejected = Some((Instant::now(), e.to_string()));
        }
        // Publish for the fast path, whatever happened, while the lock is
        // still held (its writers are ordered by the mutex).
        //
        // Stamped with the generation read at the *top* of this call, not a
        // fresh load: an invalidation that arrived while the token request was
        // in flight rejected the token this is about to publish, and stamping
        // it with the newer generation would hide that. Stale-stamping is the
        // safe direction — the fast path skips it and the next caller
        // refetches.
        st.generation = generation;
        entry.fast.store(Arc::new(FastToken {
            config: Some(cfg.clone()),
            token: st.token.clone(),
            generation,
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
                    generation: 0,
                })),
                state: tokio::sync::Mutex::new(EntryState::fresh(String::new())),
                invalidated: std::sync::atomic::AtomicU64::new(0),
            })
        }))
    }

    async fn acquire_client_credentials(
        &self,
        ctx: &GrantCtx<'_>,
        st: &mut EntryState,
    ) -> Result<String, OAuthError> {
        let GrantCtx {
            deps,
            connector,
            cfg,
            allow_private_urls,
            ..
        } = *ctx;
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
        ctx: &GrantCtx<'_>,
        st: &mut EntryState,
    ) -> Result<String, OAuthError> {
        let GrantCtx {
            deps,
            connector,
            cfg,
            allow_private_urls,
            ..
        } = *ctx;
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

    async fn refresh_flow(
        &self,
        ctx: &GrantCtx<'_>,
        st: &mut EntryState,
    ) -> Result<String, OAuthError> {
        let GrantCtx {
            deps,
            connector,
            cfg,
            fingerprint,
            margin,
            allow_private_urls,
        } = *ctx;
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
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: Option<String>,
    pub expires_in: Option<u64>,
    pub refresh_token: Option<String>,
    /// OIDC (#307). The connector grants never see one; the inbound
    /// authorization-code grant is the whole reason it is read.
    pub id_token: Option<String>,
    /// The scopes actually granted, which an IdP may narrow from what was
    /// asked for.
    pub scope: Option<String>,
}

/// The four things a token request needs, independent of which grant asked for
/// it and of whether the caller is a connector at all.
///
/// `request_token_inner` takes this rather than `&OAuth2Config` whole, so the
/// inbound sign-in flow (#307) reaches the exchange without
/// authorization-code fields on that struct. Adding them would not be free:
/// `fingerprint` hashes the serialized `OAuth2Config`, and a persisted
/// `connector_oauth_state` row is adopted only on a fingerprint match, so a new
/// field would orphan every stored rotation state on upgrade.
#[derive(Clone, Copy)]
pub struct TokenEndpoint<'a> {
    pub token_url: &'a str,
    pub client_id: &'a str,
    pub client_secret: &'a str,
    /// `basic` or `body`; anything else is a config error, named.
    pub client_auth: &'a str,
}

impl<'a> TokenEndpoint<'a> {
    fn from_config(cfg: &'a OAuth2Config) -> Self {
        Self {
            token_url: &cfg.token_url,
            client_id: &cfg.client_id,
            client_secret: &cfg.client_secret,
            client_auth: &cfg.client_auth,
        }
    }
}

/// Exchange an authorization code for tokens (RFC 6749 §4.1.3) — the inbound
/// half of OAuth2, where Orion is the relying party rather than the client.
///
/// Shares every safety property of the connector grants because it is the same
/// function underneath: the SSRF check on the token URL, the response size
/// caps, the RFC 6749 §5.2 error classification and the Bearer-only check.
/// `connector` is the metric label — the channel name, here — so one dashboard
/// covers outbound and inbound token traffic.
pub async fn exchange_code(
    http_client: &reqwest::Client,
    label: &str,
    endpoint: TokenEndpoint<'_>,
    allow_private_urls: bool,
    params: Vec<(&str, String)>,
) -> Result<TokenResponse, OAuthError> {
    let result = request_token_inner(http_client, endpoint, allow_private_urls, params).await;
    let outcome = match &result {
        Ok(_) => "ok",
        Err(OAuthError::Rejected(_)) => "rejected",
        Err(_) => "transport_error",
    };
    crate::metrics::record_oauth_token_request(label, outcome);
    result
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
    let result = request_token_inner(
        &deps.http_client,
        TokenEndpoint::from_config(cfg),
        allow_private_urls,
        params,
    )
    .await;
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
    http_client: &reqwest::Client,
    endpoint: TokenEndpoint<'_>,
    allow_private_urls: bool,
    mut params: Vec<(&str, String)>,
) -> Result<TokenResponse, OAuthError> {
    // The token endpoint gets the same SSRF treatment as the connector's own
    // endpoint — it is admin-authored config, but the check is cheap and the
    // opt-out is the connector's existing one.
    if !allow_private_urls
        && let Err(msg) = orion_common::ssrf::validate_url_not_private(endpoint.token_url).await
    {
        return Err(OAuthError::Config(format!("SSRF protection: {msg}")));
    }

    let client_auth = OAuth2ClientAuth::parse(endpoint.client_auth).ok_or_else(|| {
        OAuthError::Config(format!(
            "unknown client_auth '{}' (expected {})",
            endpoint.client_auth,
            OAuth2ClientAuth::VALUES
        ))
    })?;
    let mut req = http_client.post(endpoint.token_url).timeout(TOKEN_TIMEOUT);
    match client_auth {
        OAuth2ClientAuth::Basic => {
            req = req.basic_auth(endpoint.client_id, Some(endpoint.client_secret));
        }
        OAuth2ClientAuth::Body => {
            params.push(("client_id", endpoint.client_id.to_string()));
            params.push(("client_secret", endpoint.client_secret.to_string()));
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
        // RFC 6749 §5.1 says the response is JSON, and every IdP that follows
        // it ignores this header. GitHub does not: without an explicit
        // `Accept` its token endpoint answers `application/x-www-form-urlencoded`,
        // which parses to `null` below and surfaces as the wrong error
        // entirely ("carried no access_token", classified retryable).
        .header("accept", "application/json")
        .body(form_body)
        .send()
        .await
        .map_err(|e| OAuthError::Transport(format!("token request failed: {e}")))?;
    let status = response.status();
    // Bounded *while streaming* (`http_body`), and read on every status: the
    // body is the authority on whether the request was rejected (see below),
    // so this cannot skip it for a non-2xx. The declared-length check alone
    // was no cap at all against a token endpoint that omits `Content-Length`.
    let body = crate::http_body::read_bounded(response, MAX_TOKEN_RESPONSE_BYTES)
        .await
        .map_err(|e| OAuthError::Transport(format!("token response {e}")))?;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();

    // RFC 6749 §5.2 pairs the error body with a 400, and most IdPs comply.
    // GitHub answers `200` with `{"error": "bad_verification_code"}` for a
    // spent or forged authorization code. Classified by status alone that
    // falls through to "carried no access_token" — an `OAuthError::Transport`,
    // which `retryable()` reports as **true**, so a permanently dead code
    // would be retried. The body is the authority on whether the request was
    // rejected; the status only says how politely.
    let rejected_with_200 = status.is_success() && json.get("error").is_some();

    if !status.is_success() || rejected_with_200 {
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
        if status.is_client_error() || rejected_with_200 {
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
    let token_type = json.get("token_type").and_then(|t| t.as_str());
    if let Some(token_type) = token_type
        && !token_type.eq_ignore_ascii_case("bearer")
    {
        return Err(OAuthError::Config(format!(
            "token endpoint issued a '{token_type}' token; only Bearer is supported"
        )));
    }
    let string_field = |name: &str| {
        json.get(name)
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    Ok(TokenResponse {
        access_token: access_token.to_string(),
        token_type: token_type.map(str::to_string),
        expires_in: json.get("expires_in").and_then(|e| e.as_u64()),
        refresh_token: string_field("refresh_token"),
        id_token: string_field("id_token"),
        scope: string_field("scope"),
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
#[path = "oauth_tests.rs"]
mod tests;
