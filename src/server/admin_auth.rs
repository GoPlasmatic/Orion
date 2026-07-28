//! Admin API authentication middleware.
//!
//! When enabled, requires a valid API key for all `/api/v1/admin/*` endpoints,
//! the `/metrics` endpoint, and the trace read endpoints under
//! `/api/v1/data/traces` (traces expose full request/response payloads).
//! Supports `Authorization: Bearer <token>` or custom header (e.g. `X-API-Key: <token>`).

use std::time::Duration;

use axum::extract::{MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use dashmap::DashMap;
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::config::AdminAuthConfig;
use crate::errors::OrionError;
use crate::metrics;
use crate::server::state::AppState;

/// Consecutive failures tolerated from one client before a lockout starts.
/// A human fat-fingering a key gets a few tries; a script does not.
const FAILURES_BEFORE_LOCKOUT: u32 = 5;
/// First lockout, doubling per subsequent failure.
const LOCKOUT_BASE: Duration = Duration::from_millis(500);
/// Ceiling on the doubling, so a sustained attack cannot lock a shared NAT
/// egress address out for an unbounded time.
const LOCKOUT_MAX: Duration = Duration::from_secs(30);
/// Idle period after which a client's failure record is forgotten.
const FAILURE_TTL: Duration = Duration::from_secs(300);
/// Map size that triggers a stale sweep before the next insert.
const EVICT_THRESHOLD: usize = 10_000;

#[derive(Debug, Clone, Copy)]
struct FailureRecord {
    consecutive: u32,
    /// Monotonic — a wall-clock step must not shorten or extend a lockout.
    locked_until: Option<Instant>,
    last_seen: Instant,
}

/// Per-client failed-admin-auth tracker with exponential backoff.
///
/// Without this, `admin_auth.api_keys` faced unlimited guessing attempts: the
/// middleware returns 401 without calling `next.run`, so before the S16 layer
/// reorder the rate limiter never even saw the request — and the limiter is
/// off by default regardless (proposal S12).
#[derive(Debug, Default)]
pub struct FailedAuthTracker {
    clients: DashMap<String, FailureRecord>,
}

impl FailedAuthTracker {
    /// Remaining lockout for `client`, if any.
    pub fn locked_for(&self, client: &str) -> Option<Duration> {
        let rec = self.clients.get(client)?;
        let until = rec.locked_until?;
        until.checked_duration_since(Instant::now())
    }

    /// Record a failed attempt and return the lockout it triggered, if any.
    pub fn record_failure(&self, client: &str) -> Option<Duration> {
        let now = Instant::now();
        // An attacker cycling source addresses would otherwise grow the map
        // without bound; sweeping on the way in keeps it proportional to the
        // number of *recently* failing clients.
        if self.clients.len() >= EVICT_THRESHOLD {
            self.evict_stale();
        }
        let mut entry = self
            .clients
            .entry(client.to_string())
            .or_insert(FailureRecord {
                consecutive: 0,
                locked_until: None,
                last_seen: now,
            });
        // A long-idle record is a fresh start, not a continuation.
        if now.duration_since(entry.last_seen) > FAILURE_TTL {
            entry.consecutive = 0;
            entry.locked_until = None;
        }
        entry.consecutive = entry.consecutive.saturating_add(1);
        entry.last_seen = now;

        if entry.consecutive < FAILURES_BEFORE_LOCKOUT {
            return None;
        }
        let steps = entry.consecutive - FAILURES_BEFORE_LOCKOUT;
        let backoff = LOCKOUT_BASE
            .checked_mul(1u32.checked_shl(steps.min(16)).unwrap_or(u32::MAX))
            .unwrap_or(LOCKOUT_MAX)
            .min(LOCKOUT_MAX);
        entry.locked_until = Some(now + backoff);
        Some(backoff)
    }

    /// Clear a client's history after a successful authentication.
    pub fn record_success(&self, client: &str) {
        self.clients.remove(client);
    }

    /// Drop records that have been idle past their TTL. Called opportunistically
    /// so an attacker cycling source addresses cannot grow the map without bound.
    fn evict_stale(&self) {
        let now = Instant::now();
        self.clients
            .retain(|_, rec| now.duration_since(rec.last_seen) <= FAILURE_TTL);
    }
}

/// Identity of the authenticated admin principal, stored in request extensions.
#[derive(Debug, Clone)]
pub struct AdminPrincipal {
    /// Truncated key prefix for audit logging (never the full key).
    pub key_prefix: String,
}

impl AdminPrincipal {
    fn from_token(token: &str) -> Self {
        let prefix_len = token.len().min(8);
        Self {
            key_prefix: format!("{}...", &token[..prefix_len]),
        }
    }

    /// For keys configured in the `sha256:` hash-at-rest form: identify by a
    /// prefix of the (already public-at-rest) hash rather than leak plaintext
    /// characters of the presented token into audit logs.
    fn from_digest(digest: &[u8; 32]) -> Self {
        Self {
            key_prefix: format!("sha256:{}...", hex::encode(&digest[..4])),
        }
    }
}

/// True when `path` — an Axum `MatchedPath` template such as
/// `/api/v1/admin/channels/{id}` — is behind admin authentication.
///
/// The trace *list* returns rows for every caller, so it is guarded like
/// admin routes. The single-trace GET is **not** in this set (R12): its
/// handler enforces its own two-lane rule — a valid admin credential, or the
/// per-submission capability token returned with the async 202 — so a
/// data-plane caller can poll its own result without holding an admin key.
/// Channel traffic cannot collide with the traces prefix: its `MatchedPath`
/// is always the `/api/v1/data/{*path}` catch-all template.
///
/// The OpenAPI `SecurityAddon` (`server::routes::openapi`) applies the spec's
/// `security` requirement through this same predicate, so the documented
/// surface cannot drift from what the middleware enforces. The templates it
/// feeds in are OpenAPI path keys, which are byte-identical to Axum's.
pub(crate) fn is_guarded_path(path: &str) -> bool {
    path.starts_with("/api/v1/admin") || path == "/metrics" || path == "/api/v1/data/traces"
}

/// Middleware that authenticates admin API requests.
///
/// Skips authentication for non-admin routes and when auth is disabled.
pub async fn admin_auth_middleware(
    State(state): State<AppState>,
    matched_path: Option<MatchedPath>,
    mut req: Request,
    next: Next,
) -> Result<Response, OrionError> {
    if !state.config.admin_auth.enabled {
        return Ok(next.run(req).await);
    }

    let path = matched_path
        .as_ref()
        .map(|m| m.as_str())
        .unwrap_or(req.uri().path());

    if !is_guarded_path(path) {
        return Ok(next.run(req).await);
    }

    // Identify the caller with the same policy the rate limiter uses, so a
    // spoofed `X-Forwarded-For` cannot mint a fresh lockout budget per request.
    let client =
        crate::server::rate_limit::extract_client_ip(&req, state.rate_limit_trusted_proxies());

    if let Some(remaining) = state.admin_auth_failures.locked_for(&client) {
        metrics::record_admin_auth_failure("locked_out");
        tracing::warn!(
            client = %client,
            path = %req.uri().path(),
            remaining_ms = remaining.as_millis() as u64,
            "Admin API authentication refused: client is in failed-auth backoff"
        );
        return Err(OrionError::Unauthorized("Invalid API key".into()));
    }

    let token = match extract_api_key(req.headers(), &state.config.admin_auth) {
        Ok(t) => t,
        Err(e) => {
            metrics::record_admin_auth_failure("missing_or_malformed");
            state.admin_auth_failures.record_failure(&client);
            return Err(e);
        }
    };

    // Compare SHA-256 digests instead of raw keys: fixed width, so timing
    // reveals neither key length nor content (S11).
    let presented: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    let matched_key = state
        .config
        .admin_auth
        .admin_keys()
        .into_iter()
        .find(|key| constant_time_eq(&presented, &key.digest));

    let Some(matched_key) = matched_key else {
        metrics::record_admin_auth_failure("invalid_key");
        let lockout = state.admin_auth_failures.record_failure(&client);
        tracing::warn!(
            client = %client,
            path = %req.uri().path(),
            lockout_ms = lockout.map(|d| d.as_millis() as u64),
            "Admin API authentication failed: invalid API key"
        );
        return Err(OrionError::Unauthorized("Invalid API key".into()));
    };

    state.admin_auth_failures.record_success(&client);

    // Store principal identity in request extensions for audit logging
    let principal = if matched_key.hashed {
        AdminPrincipal::from_digest(&matched_key.digest)
    } else {
        AdminPrincipal::from_token(&token)
    };
    req.extensions_mut().insert(principal);

    Ok(next.run(req).await)
}

/// True when the request headers present a valid admin credential. For
/// surfaces that stay reachable without auth but serve a reduced body to
/// anonymous callers (O9: `/health`'s topology detail). Failures are not an
/// error here — they just mean "anonymous" — so nothing is logged or counted.
pub(crate) fn headers_present_valid_key(
    headers: &axum::http::HeaderMap,
    config: &AdminAuthConfig,
) -> bool {
    let Ok(token) = extract_api_key(headers, config) else {
        return false;
    };
    let presented: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    config
        .admin_keys()
        .into_iter()
        .any(|key| constant_time_eq(&presented, &key.digest))
}

/// SHA-256 hex of an async-trace capability token (R12). The trace row
/// stores this instead of the token itself.
pub(crate) fn hash_trace_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Constant-time check of a presented trace token against the stored hash.
pub(crate) fn trace_token_matches(presented: &str, stored_hash: &str) -> bool {
    let presented: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
    let Ok(decoded) = hex::decode(stored_hash) else {
        return false;
    };
    let Ok(stored) = <[u8; 32]>::try_from(decoded) else {
        return false;
    };
    constant_time_eq(&presented, &stored)
}

/// Extract the API key from the request headers based on the configured header.
fn extract_api_key(
    headers: &axum::http::HeaderMap,
    config: &AdminAuthConfig,
) -> Result<String, OrionError> {
    let header_value = headers
        .get(&config.header)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| OrionError::Unauthorized(format!("Missing {} header", config.header)))?;

    if config.header.eq_ignore_ascii_case("authorization") {
        // Expect "Bearer <token>" format
        header_value
            .strip_prefix("Bearer ")
            .or_else(|| header_value.strip_prefix("bearer "))
            .map(|t| t.to_string())
            .ok_or_else(|| {
                OrionError::Unauthorized(
                    "Authorization header must use 'Bearer <token>' format".into(),
                )
            })
    } else {
        // Custom header — use raw value
        Ok(header_value.to_string())
    }
}

/// Constant-time comparison of two SHA-256 digests. Digests are fixed width,
/// so there is no length branch for a timing side channel to observe (S11).
fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(s: &str) -> [u8; 32] {
        Sha256::digest(s.as_bytes()).into()
    }

    #[test]
    fn test_digest_compare_equal() {
        assert!(constant_time_eq(&digest("secret"), &digest("secret")));
    }

    #[test]
    fn test_digest_compare_unequal() {
        assert!(!constant_time_eq(&digest("secret"), &digest("wrong!")));
    }

    #[test]
    fn test_digest_compare_length_differs() {
        // Tokens of different lengths still produce fixed-width digests, so
        // the comparison runs to completion instead of early-returning.
        assert!(!constant_time_eq(
            &digest("short"),
            &digest("a-much-longer-candidate-key")
        ));
    }

    #[test]
    fn test_digest_compare_empty_token() {
        assert!(constant_time_eq(&digest(""), &digest("")));
        assert!(!constant_time_eq(&digest(""), &digest("secret")));
    }

    #[test]
    fn test_sha256_config_form_matches_presented_plaintext() {
        // Operator stores sha256:<hex>; client presents the plaintext key.
        let config = AdminAuthConfig {
            enabled: true,
            api_keys: vec![format!("sha256:{}", hex::encode(digest("the-real-key")))],
            header: "Authorization".to_string(),
        };
        let presented = digest("the-real-key");
        let keys = config.admin_keys();
        assert!(keys.iter().any(|k| constant_time_eq(&presented, &k.digest)));
        let wrong = digest("not-the-key");
        assert!(!keys.iter().any(|k| constant_time_eq(&wrong, &k.digest)));
    }

    #[test]
    fn test_principal_prefix_for_hashed_key_uses_hash() {
        let d = digest("the-real-key");
        let principal = AdminPrincipal::from_digest(&d);
        assert!(principal.key_prefix.starts_with("sha256:"));
        assert!(!principal.key_prefix.contains("the-real"));
    }

    // -- failed-auth backoff (S12) --------------------------------------

    #[tokio::test(start_paused = true)]
    async fn backoff_starts_only_after_a_grace_period() {
        let t = FailedAuthTracker::default();
        for _ in 1..FAILURES_BEFORE_LOCKOUT {
            assert!(
                t.record_failure("1.2.3.4").is_none(),
                "a few typos must not lock anyone out"
            );
            assert!(t.locked_for("1.2.3.4").is_none());
        }
        let first = t.record_failure("1.2.3.4").expect("lockout starts");
        assert_eq!(first, LOCKOUT_BASE);
        assert!(t.locked_for("1.2.3.4").is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_doubles_and_is_capped() {
        let t = FailedAuthTracker::default();
        let mut last = Duration::ZERO;
        for _ in 0..40 {
            if let Some(d) = t.record_failure("1.2.3.4") {
                assert!(d >= last, "backoff must not shrink");
                last = d;
            }
        }
        assert_eq!(last, LOCKOUT_MAX, "backoff must saturate, not overflow");
    }

    #[tokio::test(start_paused = true)]
    async fn lockout_expires_on_the_monotonic_clock() {
        let t = FailedAuthTracker::default();
        for _ in 0..FAILURES_BEFORE_LOCKOUT {
            t.record_failure("1.2.3.4");
        }
        assert!(t.locked_for("1.2.3.4").is_some());
        tokio::time::advance(LOCKOUT_BASE + Duration::from_millis(1)).await;
        assert!(
            t.locked_for("1.2.3.4").is_none(),
            "the lockout must lift once it elapses"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn success_clears_the_record_and_clients_are_independent() {
        let t = FailedAuthTracker::default();
        for _ in 0..FAILURES_BEFORE_LOCKOUT {
            t.record_failure("1.2.3.4");
        }
        assert!(t.locked_for("1.2.3.4").is_some());
        // A different client is unaffected by the first one's lockout.
        assert!(t.locked_for("5.6.7.8").is_none());

        t.record_success("1.2.3.4");
        assert!(t.locked_for("1.2.3.4").is_none());
        assert!(
            t.record_failure("1.2.3.4").is_none(),
            "the counter must restart after a success"
        );
    }
}
