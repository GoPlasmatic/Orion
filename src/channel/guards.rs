//! Transport-neutral per-channel ingress guards.
//!
//! Every entry point that dispatches a message to a channel — sync HTTP,
//! async HTTP submission, Kafka ingest, in-process `channel_call` — applies
//! the subset of these guards that makes sense for its transport, so a
//! channel's declared contract (CORS, input validation, deduplication,
//! backpressure, response cache) holds regardless of how the message
//! arrived. Header-derived inputs are lowered to plain strings / lookup
//! closures so non-HTTP callers don't need an `axum::http::HeaderMap`.

use std::sync::Arc;

use serde_json::{Value, json};

use super::ChannelRuntimeConfig;
use crate::connector::cache_backend::CacheBackend;
use crate::errors::OrionError;
use crate::metrics;

/// JSONLogic truthiness: false, null, 0, "", and [] are falsy; everything else is truthy.
fn is_truthy(val: &Value) -> bool {
    match val {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(_) => true,
    }
}

/// Check per-channel CORS: reject the request if an `Origin` header value is
/// present but not in the channel's allowed-origins list.
pub fn check_cors_origin(
    channel: &str,
    channel_config: &Option<Arc<ChannelRuntimeConfig>>,
    origin: Option<&str>,
) -> Result<(), OrionError> {
    if let Some(cfg) = channel_config
        && let Some(cors) = &cfg.parsed_config.cors
        && let Some(allowed_origins) = &cors.allowed_origins
        && let Some(origin) = origin
        && !allowed_origins.iter().any(|o| o == "*" || o == origin)
    {
        return Err(OrionError::Forbidden(format!(
            "Origin '{origin}' is not allowed for channel '{channel}'"
        )));
    }
    Ok(())
}

/// Evaluate per-channel input validation logic (JSONLogic). Returns `Ok(())` when
/// validation passes or no validation is configured.
pub fn validate_input(
    channel: &str,
    channel_config: &Option<Arc<ChannelRuntimeConfig>>,
    data: &Value,
    metadata: &Value,
    datalogic: &datalogic_rs::Engine,
) -> Result<(), OrionError> {
    if let Some(cfg) = channel_config
        && let Some(ref compiled) = cfg.validation_logic
    {
        let context = json!({ "data": data, "metadata": metadata });
        match datalogic
            .session()
            .eval_into::<serde_json::Value, _>(compiled, &context)
        {
            Ok(result) => {
                if !is_truthy(&result) {
                    return Err(OrionError::BadRequest(
                        "Input validation failed".to_string(),
                    ));
                }
            }
            Err(e) => {
                tracing::warn!(channel = %channel, error = %e, "validation_logic evaluation failed, rejecting");
                return Err(OrionError::BadRequest(format!(
                    "Input validation error: {e}"
                )));
            }
        }
    }
    Ok(())
}

/// Check per-channel request deduplication. `header` is a lookup view over
/// the transport's headers (the idempotency header name is per-channel
/// config). Returns `Err(Conflict)` when a duplicate idempotency key is
/// detected within the configured window.
pub async fn check_deduplication<F>(
    channel: &str,
    channel_config: &Option<Arc<ChannelRuntimeConfig>>,
    header: F,
) -> Result<(), OrionError>
where
    F: FnOnce(&str) -> Option<String>,
{
    if let Some(cfg) = channel_config
        && let Some(ref dedup) = cfg.parsed_config.deduplication
        && let Some(ref store) = cfg.dedup_store
        && let Some(key) = header(&dedup.header)
    {
        let window = dedup.window_secs.unwrap_or(300);
        // Scope the key per channel (same format family as the response cache
        // key at `compute_cache_key`) — raw tokens would collide across
        // channels sharing a backend.
        let scoped_key = format!("dedup:{channel}:{key}");
        // Fail open on backend errors: a dedup-store outage must not reject
        // every request with 409 — availability wins over strict idempotency.
        let is_new = match store.check_and_insert(&scoped_key, window).await {
            Ok(is_new) => is_new,
            Err(e) => {
                metrics::record_error("dedup_backend");
                tracing::warn!(
                    error = %e,
                    header = %dedup.header,
                    "Dedup backend error; failing open (request allowed without dedup check)"
                );
                true
            }
        };
        if !is_new {
            return Err(OrionError::Conflict(format!(
                "Duplicate request: idempotency key '{key}' already seen"
            )));
        }
    }
    Ok(())
}

/// Acquire a per-channel backpressure permit. Returns `Err(ServiceUnavailable)`
/// when the channel's concurrency limit has been reached. The caller must
/// hold the returned permit for the duration of processing.
pub fn acquire_backpressure(
    channel: &str,
    channel_config: &Option<Arc<ChannelRuntimeConfig>>,
) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, OrionError> {
    if let Some(cfg) = channel_config
        && let Some(ref semaphore) = cfg.backpressure_semaphore
    {
        match semaphore.clone().try_acquire_owned() {
            Ok(permit) => Ok(Some(permit)),
            Err(_) => {
                metrics::record_error("backpressure");
                Err(OrionError::ServiceUnavailable(format!(
                    "Channel '{channel}' is at capacity"
                )))
            }
        }
    } else {
        Ok(None)
    }
}

use crate::engine::utils::{FNV1A_SEED, fnv1a_feed};

/// Compute a deterministic cache key from channel name and request data.
///
/// Uses FNV-1a (64-bit) rather than `std::collections::hash_map::DefaultHasher`
/// because the cache key must be **stable across processes** (multiple
/// orion-server instances sharing a Redis cache must agree on the key for the
/// same request). `DefaultHasher` is `SipHash` keyed by a per-process random
/// seed and would produce different keys per process. `ahash` likewise
/// randomises its seed on construction. FNV-1a is unkeyed and deterministic.
fn compute_cache_key(
    channel: &str,
    data: &Value,
    metadata: &Value,
    cache_cfg: &crate::channel::ChannelCacheConfig,
) -> String {
    let mut h: u64 = FNV1A_SEED;

    // The request's route identity must always distinguish keys: for a REST
    // channel like `GET /orders/{id}` the body is empty, so hashing only the
    // body would serve the first caller's response to every id.
    if let Some(method) = metadata.get("http_method").and_then(Value::as_str) {
        fnv1a_feed(&mut h, method.as_bytes());
    }
    fnv1a_feed(&mut h, &[0]);
    fnv1a_feed_object_sorted(&mut h, metadata.get("params"));
    fnv1a_feed(&mut h, &[0]);
    fnv1a_feed_object_sorted(&mut h, metadata.get("query"));
    fnv1a_feed(&mut h, &[0]);

    if let Some(ref fields) = cache_cfg.cache_key_fields {
        // Hash selected fields directly — no intermediate Map or clones
        for f in fields {
            if let Some(v) = data.get(f) {
                fnv1a_feed(&mut h, f.as_bytes());
                let v_bytes = serde_json::to_vec(v).unwrap_or_default();
                fnv1a_feed(&mut h, &v_bytes);
            }
        }
    } else {
        let bytes = serde_json::to_vec(data).unwrap_or_default();
        fnv1a_feed(&mut h, &bytes);
    };

    format!("cache:{channel}:{h:016x}")
}

/// Feed an optional JSON object into the FNV-1a state in sorted-key order,
/// so the key is independent of map iteration and query-string order.
fn fnv1a_feed_object_sorted(h: &mut u64, v: Option<&Value>) {
    if let Some(Value::Object(map)) = v {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort_unstable();
        for k in keys {
            fnv1a_feed(h, k.as_bytes());
            let bytes = serde_json::to_vec(&map[k.as_str()]).unwrap_or_default();
            fnv1a_feed(h, &bytes);
        }
    }
}

/// Context carried from cache pre-check to post-success cache store:
/// (cache key, backend, TTL seconds).
pub type CacheStoreCtx = (String, Arc<dyn CacheBackend>, u64);

/// Outcome of the response-cache pre-check.
pub enum CacheLookup {
    /// Cache hit — carries the cached pre-serialized JSON body.
    Hit(String),
    /// No cache hit. Carries the (key, backend, ttl) needed to store the
    /// computed response on success, or `None` if caching is disabled.
    Miss(Option<CacheStoreCtx>),
}

/// Check the response cache; return a hit or the context needed to store
/// the eventual response on success.
pub async fn check_response_cache(
    channel: &str,
    data: &Value,
    metadata: &Value,
    channel_config: &Option<Arc<ChannelRuntimeConfig>>,
) -> CacheLookup {
    let Some(cfg) = channel_config else {
        return CacheLookup::Miss(None);
    };
    let Some(ref cache_cfg) = cfg.parsed_config.cache else {
        return CacheLookup::Miss(None);
    };
    if !cache_cfg.enabled {
        return CacheLookup::Miss(None);
    }
    let Some(ref cache) = cfg.response_cache else {
        return CacheLookup::Miss(None);
    };
    let key = compute_cache_key(channel, data, metadata, cache_cfg);
    match cache.get(&key).await {
        Ok(Some(cached)) => {
            metrics::record_cache_hit(channel);
            CacheLookup::Hit(cached)
        }
        _ => {
            metrics::record_cache_miss(channel);
            CacheLookup::Miss(Some((
                key,
                cache.clone(),
                cache_cfg.ttl_secs.unwrap_or(300),
            )))
        }
    }
}
