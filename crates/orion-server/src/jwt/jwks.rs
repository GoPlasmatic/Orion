//! Process-wide JWKS cache (#267): one component serving both verify
//! surfaces (the channel mode and `jwt_verify`).
//!
//! Lifecycle: HTTPS-only fetches through a dedicated pinned client (the
//! `vault_http_client` precedent — an operator-configured issuer URL, not
//! user-supplied egress); cached per URL with a TTL from `Cache-Control:
//! max-age` clamped to [60 s, 24 h] (300 s when absent); **single-flight**
//! refresh so a thundering herd on expiry costs one fetch; **stale-serve**
//! on refresh failure, because serving stale *public* keys never weakens
//! verification — refusing valid traffic because an issuer had a blip
//! would. A `kid` miss forces one refetch, rate-limited to one per 30 s per
//! URL, which is what makes issuer-side key rotation invisible.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
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
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// One cached key set: pre-parsed decoding keys with their routing facts.
struct Entry {
    keys: Vec<(Option<String>, Option<Algorithm>, Arc<DecodingKey>)>,
    fetched_at: Instant,
    ttl: Duration,
    last_forced: Option<Instant>,
}

struct Cache {
    entries: tokio::sync::RwLock<HashMap<String, Arc<Entry>>>,
    /// Single-flight: fetches for all URLs serialize here. JWKS fetches are
    /// rare (cache TTL, refetch floor), so one lock is simpler than a
    /// per-URL map and costs nothing observable.
    fetch: tokio::sync::Mutex<()>,
}

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(|| Cache {
        entries: tokio::sync::RwLock::new(HashMap::new()),
        fetch: tokio::sync::Mutex::new(()),
    })
}

/// One HTTP client for every JWKS fetch — pinned config, shared pool.
fn client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(FETCH_TIMEOUT)
                .build()
                .expect("reqwest client with static config")
        })
        .clone()
}

/// The decoding keys to try for (`kid`, `alg`): kid-exact matches when the
/// token names one, else every cached key of the right algorithm (or with no
/// declared algorithm). A kid miss triggers the rate-limited forced refetch.
pub async fn decoding_keys(
    url: &str,
    kid: Option<&str>,
    alg: Algorithm,
) -> Result<Vec<Arc<DecodingKey>>, RejectReason> {
    let entry = match fresh_entry(url, false).await {
        Some(entry) => entry,
        None => return Err(RejectReason::KeysUnavailable),
    };
    let matched = select(&entry, kid, alg);
    if !matched.is_empty() {
        return Ok(matched);
    }
    // Unknown kid: the issuer may have rotated since we cached. One forced
    // refetch, floored, then the answer stands.
    if kid.is_some() {
        if let Some(entry) = fresh_entry(url, true).await {
            let matched = select(&entry, kid, alg);
            if !matched.is_empty() {
                return Ok(matched);
            }
        }
        return Err(RejectReason::UnknownKid);
    }
    Err(RejectReason::UnknownKid)
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

/// The cache entry for `url`, refreshed when expired (or when `force`d and
/// the floor allows). Stale-serves on refresh failure.
async fn fresh_entry(url: &str, force: bool) -> Option<Arc<Entry>> {
    let existing = cache().entries.read().await.get(url).cloned();
    let needs_fetch = match &existing {
        None => true,
        Some(entry) => {
            let expired = entry.fetched_at.elapsed() > entry.ttl;
            let force_allowed = force
                && entry
                    .last_forced
                    .is_none_or(|at| at.elapsed() > REFETCH_FLOOR);
            expired || force_allowed
        }
    };
    if !needs_fetch {
        return existing;
    }

    let _flight = cache().fetch.lock().await;
    // Someone else may have fetched while we queued.
    let current = cache().entries.read().await.get(url).cloned();
    if let Some(entry) = &current
        && entry.fetched_at.elapsed() <= entry.ttl
        && !(force
            && entry
                .last_forced
                .is_none_or(|at| at.elapsed() > REFETCH_FLOOR))
    {
        return current;
    }

    match fetch(url).await {
        Ok((keys, ttl)) => {
            let entry = Arc::new(Entry {
                keys,
                fetched_at: Instant::now(),
                ttl,
                last_forced: force.then(Instant::now),
            });
            cache()
                .entries
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
                cache()
                    .entries
                    .write()
                    .await
                    .insert(url.to_string(), Arc::clone(&entry));
                return Some(entry);
            }
            current
        }
    }
}

type FetchedKeys = Vec<(Option<String>, Option<Algorithm>, Arc<DecodingKey>)>;

async fn fetch(url: &str) -> Result<(FetchedKeys, Duration), String> {
    let response = client()
        .get(url)
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
    let body = response
        .bytes()
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    if body.len() > MAX_JWKS_BYTES {
        return Err(format!(
            "document is {} bytes (cap {MAX_JWKS_BYTES})",
            body.len()
        ));
    }
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
