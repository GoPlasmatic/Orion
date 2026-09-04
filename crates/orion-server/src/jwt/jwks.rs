//! JWKS cache (#267): one component serving both verify surfaces (the channel
//! mode and `jwt_verify`).
//!
//! Lifecycle: HTTPS-only fetches through the process's shared, SSRF-pinned
//! HTTP client; cached per URL with a TTL from `Cache-Control: max-age`
//! clamped to [60 s, 24 h] (300 s when absent); **single-flight** refresh so a
//! thundering herd on expiry costs one fetch; **stale-serve** on refresh
//! failure, because serving stale *public* keys never weakens verification —
//! refusing valid traffic because an issuer had a blip would. A `kid` miss
//! forces one refetch, rate-limited to one per 30 s per URL, which is what
//! makes issuer-side key rotation invisible.
//!
//! **Why it is owned rather than global.** This was a pair of `OnceLock`s: a
//! process-wide entry map and a `reqwest::Client` built here. That client was
//! the one HTTP client in the process without `PinnedDnsResolver`, and
//! `jwks_url` is authored input (`jwt_verify` takes it as a task field), so a
//! definition could reach an internal HTTPS host through the one egress path
//! that neither pinned its lookups nor consulted
//! [`crate::validation::validate_url_not_private`]. The cache now hangs off
//! `AppState`, is constructed with the serving client, and address-checks
//! every fetch.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jsonwebtoken::{Algorithm, DecodingKey};

use super::RejectReason;

const DEFAULT_TTL: Duration = Duration::from_secs(300);
const MIN_TTL: Duration = Duration::from_secs(60);
const MAX_TTL: Duration = Duration::from_secs(86_400);
/// Floor between forced (kid-miss) refetches per URL.
const REFETCH_FLOOR: Duration = Duration::from_secs(30);
/// A JWKS document larger than this is not a key set, it is a problem.
const MAX_JWKS_BYTES: usize = 262_144;
/// Per-request deadline, applied on top of whatever the shared client's own
/// timeout is: a key fetch sits in a request's critical path and must not
/// inherit a connector-shaped budget.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// One cached key set: pre-parsed decoding keys with their routing facts.
struct Entry {
    keys: Vec<(Option<String>, Option<Algorithm>, Arc<DecodingKey>)>,
    fetched_at: Instant,
    ttl: Duration,
    last_forced: Option<Instant>,
}

/// The per-instance JWKS cache. One is built at startup and shared by the
/// channel `jwt` auth mode and the `jwt_verify` task.
pub struct JwksCache {
    entries: tokio::sync::RwLock<HashMap<String, Arc<Entry>>>,
    /// Single-flight: fetches for all URLs serialize here. JWKS fetches are
    /// rare (cache TTL, refetch floor), so one lock is simpler than a
    /// per-URL map and costs nothing observable.
    fetch_lock: tokio::sync::Mutex<()>,
    /// The serving HTTP client — the one built with `PinnedDnsResolver`.
    client: reqwest::Client,
    /// `jwt.allow_private_jwks_urls`: skip the private-address check. Off by
    /// default; operators running an in-cluster issuer turn it on.
    allow_private: bool,
}

impl JwksCache {
    pub fn new(client: reqwest::Client, allow_private: bool) -> Self {
        Self {
            entries: tokio::sync::RwLock::new(HashMap::new()),
            fetch_lock: tokio::sync::Mutex::new(()),
            client,
            allow_private,
        }
    }

    /// The decoding keys to try for (`kid`, `alg`): kid-exact matches when the
    /// token names one, else every cached key of the right algorithm (or with
    /// no declared algorithm). A kid miss triggers the rate-limited forced
    /// refetch.
    pub async fn decoding_keys(
        &self,
        url: &str,
        kid: Option<&str>,
        alg: Algorithm,
    ) -> Result<Vec<Arc<DecodingKey>>, RejectReason> {
        let entry = match self.fresh_entry(url, false).await {
            Some(entry) => entry,
            None => return Err(RejectReason::KeysUnavailable),
        };
        let matched = select(&entry, kid, alg);
        if !matched.is_empty() {
            return Ok(matched);
        }
        // Unknown kid: the issuer may have rotated since we cached. One forced
        // refetch, floored, then the answer stands.
        if kid.is_some()
            && let Some(entry) = self.fresh_entry(url, true).await
        {
            let matched = select(&entry, kid, alg);
            if !matched.is_empty() {
                return Ok(matched);
            }
        }
        Err(RejectReason::UnknownKid)
    }

    /// The cache entry for `url`, refreshed when expired (or when `force`d and
    /// the floor allows). Stale-serves on refresh failure.
    async fn fresh_entry(&self, url: &str, force: bool) -> Option<Arc<Entry>> {
        let existing = self.entries.read().await.get(url).cloned();
        if !needs_fetch(existing.as_ref(), force) {
            return existing;
        }

        let _flight = self.fetch_lock.lock().await;
        // Someone else may have fetched while we queued.
        let current = self.entries.read().await.get(url).cloned();
        if !needs_fetch(current.as_ref(), force) {
            return current;
        }

        match self.fetch(url).await {
            Ok((keys, ttl)) => {
                let entry = Arc::new(Entry {
                    keys,
                    fetched_at: Instant::now(),
                    ttl,
                    last_forced: force.then(Instant::now),
                });
                self.entries
                    .write()
                    .await
                    .insert(url.to_string(), Arc::clone(&entry));
                Some(entry)
            }
            Err(e) => {
                tracing::warn!(url = %url, error = %e, "JWKS refresh failed; serving cached keys");
                // Stale-serve; stamp the forced attempt so a flapping issuer is
                // not hammered by every unknown-kid token.
                if force && let Some(old) = current.clone() {
                    let entry = Arc::new(Entry {
                        keys: old.keys.clone(),
                        fetched_at: old.fetched_at,
                        ttl: old.ttl,
                        last_forced: Some(Instant::now()),
                    });
                    self.entries
                        .write()
                        .await
                        .insert(url.to_string(), Arc::clone(&entry));
                    return Some(entry);
                }
                current
            }
        }
    }

    /// One fetch, address-checked.
    ///
    /// The HTTPS-only rule is applied where the URL is authored
    /// ([`super::validate_jwks_url`]); the private-address rule is applied
    /// here, at the moment of egress. That split is the same one
    /// `validation/endpoints.rs` makes and for the same reason: an admin API
    /// that resolves DNS to accept a channel is an admin API that hangs when
    /// the issuer is down, and a host that was public when the channel was
    /// stored can be private by the time it is dialled.
    async fn fetch(&self, url: &str) -> Result<(FetchedKeys, Duration), String> {
        if !self.allow_private {
            crate::validation::validate_url_not_private(url).await?;
        }
        let response = self
            .client
            .get(url)
            .timeout(FETCH_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("fetch failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        let ttl = ttl_from_cache_control(
            response
                .headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
        );
        // Bounded *while streaming* (`http_body`): an issuer — or anything
        // answering on its behalf — must not be able to hand this cache a
        // body larger than the cap by omitting `Content-Length`. Reading it
        // whole and measuring afterwards enforced the cap on the result and
        // not on the memory, which is what the cap is for.
        let body = crate::http_body::read_bounded(response, MAX_JWKS_BYTES)
            .await
            .map_err(|e| format!("JWKS {e}"))?;
        let set: jsonwebtoken::jwk::JwkSet =
            serde_json::from_slice(&body).map_err(|e| format!("not a JWK set: {e}"))?;

        let mut keys: FetchedKeys = Vec::with_capacity(set.keys.len());
        for jwk in &set.keys {
            // A key we cannot parse (unsupported kty/crv) is skipped, not fatal:
            // issuers publish mixed sets and the usable keys still verify.
            let Ok(decoded) = DecodingKey::from_jwk(jwk) else {
                continue;
            };
            let alg = jwk
                .common
                .key_algorithm
                .and_then(|a| super::parse_algorithm(a.to_string().as_str()).ok());
            keys.push((jwk.common.key_id.clone(), alg, Arc::new(decoded)));
        }
        Ok((keys, ttl))
    }
}

fn select(entry: &Entry, kid: Option<&str>, alg: Algorithm) -> Vec<Arc<DecodingKey>> {
    entry
        .keys
        .iter()
        .filter(|(entry_kid, entry_alg, _)| {
            entry_alg.is_none_or(|a| a == alg)
                && match kid {
                    Some(kid) => entry_kid.as_deref() == Some(kid),
                    None => true,
                }
        })
        .map(|(_, _, key)| Arc::clone(key))
        .collect()
}

/// Whether an entry needs a (re)fetch: absent, expired, or a forced refetch
/// the floor allows. One predicate for the lock-free pre-check and the
/// re-check under the single-flight lock — the triple condition is subtle
/// enough that two hand-negated copies would drift.
fn needs_fetch(entry: Option<&Arc<Entry>>, force: bool) -> bool {
    match entry {
        None => true,
        Some(entry) => {
            entry.fetched_at.elapsed() > entry.ttl
                || (force
                    && entry
                        .last_forced
                        .is_none_or(|at| at.elapsed() > REFETCH_FLOOR))
        }
    }
}

type FetchedKeys = Vec<(Option<String>, Option<Algorithm>, Arc<DecodingKey>)>;

/// `Cache-Control: max-age` clamped to [`MIN_TTL`, `MAX_TTL`]; absent or
/// unparseable → [`DEFAULT_TTL`].
fn ttl_from_cache_control(header: Option<&str>) -> Duration {
    let max_age = header.and_then(|value| {
        value.split(',').find_map(|directive| {
            directive
                .trim()
                .strip_prefix("max-age=")
                .and_then(|secs| secs.trim().parse::<u64>().ok())
        })
    });
    match max_age {
        Some(secs) => Duration::from_secs(secs).clamp(MIN_TTL, MAX_TTL),
        None => DEFAULT_TTL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A JWKS mock on loopback, plus the count of requests it has served.
    async fn mock_jwks() -> (String, Arc<AtomicUsize>) {
        use base64::Engine as _;
        let k = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("a-symmetric-test-secret");
        let hits = Arc::new(AtomicUsize::new(0));
        let served = hits.clone();
        let app = axum::Router::new().route(
            "/jwks.json",
            axum::routing::get(move || {
                let hits = served.clone();
                let k = k.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    axum::Json(serde_json::json!({
                        "keys": [{"kty": "oct", "k": k, "kid": "one", "alg": "HS256"}]
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener");
        let addr = listener.local_addr().expect("test addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("test serve") });
        (format!("http://{addr}/jwks.json"), hits)
    }

    /// `jwks_url` is authored input, so the fetch is address-checked like any
    /// other egress. The mock is on loopback, so the request must never leave
    /// — asserted on the mock's own hit count, not just on the error, because
    /// a refusal after the connection is no refusal at all.
    #[tokio::test]
    async fn a_private_jwks_url_is_refused_before_the_request_is_made() {
        let (url, hits) = mock_jwks().await;
        let cache = JwksCache::new(reqwest::Client::new(), false);

        let result = cache
            .decoding_keys(&url, Some("one"), Algorithm::HS256)
            .await;

        assert_eq!(result.err(), Some(RejectReason::KeysUnavailable));
        assert_eq!(hits.load(Ordering::SeqCst), 0, "the mock was contacted");
    }

    /// The `jwt.allow_private_jwks_urls` escape hatch: an operator running an
    /// in-cluster issuer turns the address check off and the same URL works.
    #[tokio::test]
    async fn allow_private_lets_an_in_cluster_issuer_through() {
        let (url, hits) = mock_jwks().await;
        let cache = JwksCache::new(reqwest::Client::new(), true);

        let keys = cache
            .decoding_keys(&url, Some("one"), Algorithm::HS256)
            .await
            .expect("the key set is served");

        assert_eq!(keys.len(), 1);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// An issuer that streams without declaring a length cannot make this
    /// cache buffer more than `MAX_JWKS_BYTES`.
    ///
    /// The cap used to be checked after `bytes()` had already read the body to
    /// its end, so it bounded the *document this accepted* and not the memory
    /// it took to refuse one — and a JWKS URL is reachable from a channel's
    /// `auth` config, fetched on a cache miss, per issuer.
    #[tokio::test]
    async fn a_flooding_issuer_is_cut_off_at_the_cap() {
        const CHUNK: usize = 64 * 1024;
        const CHUNKS: usize = 128; // 8 MiB against a 256 KiB cap
        let (url, server) = crate::http_body::flood_server(CHUNK, CHUNKS).await;
        // `allow_private` — the flood server is on loopback, and the address
        // check would otherwise refuse it before any body was read.
        let cache = JwksCache::new(reqwest::Client::new(), true);

        let result = cache
            .decoding_keys(&url, Some("one"), Algorithm::HS256)
            .await;

        assert_eq!(result.err(), Some(RejectReason::KeysUnavailable));
        crate::http_body::assert_stopped_early(
            server.await.expect("test server"),
            CHUNK * CHUNKS,
            "the JWKS fetch",
        );
    }
}
