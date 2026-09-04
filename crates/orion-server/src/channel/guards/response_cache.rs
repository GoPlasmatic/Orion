//! The per-channel response cache: the lookup, and the key it is stored under.
//!
//! Split out of `guards` as one concept. The key derivation is the larger half
//! and the more delicate one — what goes into the hash decides which requests
//! are allowed to share an answer.

use dataflow_rs::datalogic_rs;
use std::sync::Arc;

use serde_json::Value;

use super::ChannelRuntimeConfig;
use crate::connector::cache_backend::CacheBackend;
use crate::metrics;
use sha2::{Digest, Sha256};

/// Resolve one `cache_key_fields` entry against the request payload.
///
/// Three spellings resolve, tried in this order:
///
/// 1. The **literal key** — `data.get(f)`. This was the only spelling the
///    original implementation supported, so trying it first keeps every stored
///    channel keying exactly as it did, including a payload whose top-level key
///    genuinely contains a dot.
/// 2. A **dotted path** from the payload root — `user.id` walks
///    `{"user": {"id": …}}`.
/// 3. The same path with a leading `data.` stripped — the spelling the docs
///    have always shown (`data.user_id`), which resolved to nothing under (1)
///    because the payload *is* `data` and has no member of that name.
///
/// (3) is what made this worth fixing: a channel configured from the
/// documented example matched no field at all, and a field that matches
/// nothing contributed nothing to the hash, so every request on the channel
/// collapsed onto one cache entry and the first caller's body was served to
/// everyone for the TTL. [`compute_cache_key`] now refuses to build a key at
/// all in that case rather than building a meaningless one.
pub(super) fn resolve_key_field<'a>(data: &'a Value, field: &str) -> Option<&'a Value> {
    fn walk<'a>(mut cur: &'a Value, path: &str) -> Option<&'a Value> {
        for segment in path.split('.') {
            if segment.is_empty() {
                return None;
            }
            cur = cur.get(segment)?;
        }
        Some(cur)
    }

    if let Some(v) = data.get(field) {
        return Some(v);
    }
    if !field.contains('.') {
        return None;
    }
    walk(data, field).or_else(|| field.strip_prefix("data.").and_then(|p| walk(data, p)))
}

/// Compute a deterministic cache key from channel name and request data.
///
/// `None` means **this request has no meaningful cache key** and must neither
/// be served from the cache nor stored in it. That happens when the channel
/// declares `cache_key_fields` and not one of them resolves against the
/// payload: the request is then indistinguishable from every other request on
/// the channel, which is precisely when a cache entry is dangerous rather than
/// merely useless. Bypassing the cache costs one workflow run; keying on
/// nothing costs correctness.
///
/// # Why SHA-256 and not a fast hash
///
/// The key must be **stable across processes** — replicas sharing a Redis
/// cache have to agree on the key for the same request — which rules out
/// `DefaultHasher` (SipHash under a per-process random seed) and `ahash`
/// (randomises its seed on construction). FNV-1a satisfied that and was used
/// here first.
///
/// It is not sufficient on its own. Two requests that hash alike are served
/// each other's response bodies, and FNV-1a is a multiply-xor over a 64-bit
/// state with no collision resistance whatsoever: it inverts in closed form,
/// so a colliding payload is *constructed*, not searched for. The data plane
/// is unauthenticated by design, which makes the request body attacker-shaped
/// input on most deployments.
///
/// SHA-256 truncated to 128 bits keeps the determinism the cache actually
/// requires and puts a collision beyond construction. The cost lands next to
/// the `serde_json::to_vec` of the same bytes, which this function already
/// pays, and only on channels that enable caching.
pub(super) fn compute_cache_key(
    channel: &str,
    data: &Value,
    metadata: &Value,
    cache_cfg: &crate::channel::ChannelCacheConfig,
    key_logic: Option<&datalogic_rs::Logic>,
    datalogic: &datalogic_rs::Engine,
) -> Option<String> {
    // `key_logic` replaces the whole payload-derived half of the key rather
    // than adding to it: an expression that says what varies the response is a
    // complete answer, and mixing it with a payload hash would put back the
    // very fields it was written to exclude. The channel, method, params and
    // query below still frame it.
    let mut h = Sha256::new();

    // Every chunk is length-prefixed, so no arrangement of field names and
    // values can be re-read as a different arrangement. A separator byte would
    // have to argue that the byte never occurs inside a chunk; framing does not
    // need the argument.
    fn feed(h: &mut Sha256, bytes: &[u8]) {
        h.update((bytes.len() as u64).to_be_bytes());
        h.update(bytes);
    }

    // The request's route identity must always distinguish keys: for a REST
    // channel like `GET /orders/{id}` the body is empty, so hashing only the
    // body would serve the first caller's response to every id.
    feed(
        &mut h,
        metadata
            .get("http_method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .as_bytes(),
    );
    feed_object_sorted(&mut h, metadata.get("params"));
    feed_object_sorted(&mut h, metadata.get("query"));

    if let Some(ref fields) = cache_cfg.cache_key_fields {
        // Hash selected fields directly — no intermediate Map or clones. An
        // absent field feeds its *name* and a marker byte rather than nothing,
        // so `{"a": 1}` and `{"b": 1}` under fields `["a", "b"]` cannot land on
        // the same key by each contributing one term and skipping the other.
        let mut resolved = 0usize;
        for f in fields {
            feed(&mut h, f.as_bytes());
            match resolve_key_field(data, f) {
                Some(v) => {
                    resolved += 1;
                    h.update([1u8]);
                    feed(&mut h, &serde_json::to_vec(v).unwrap_or_default());
                }
                None => h.update([0u8]),
            }
        }
        if resolved == 0 {
            return None;
        }
    } else if let Some(compiled) = key_logic {
        let context = serde_json::json!({ "data": data, "metadata": metadata });
        // No usable key means bypass, exactly as an unresolvable
        // `cache_key_fields` does: a key that cannot be computed must not
        // collapse onto one shared entry and serve one caller's body to the
        // next.
        let key = datalogic
            .session()
            .eval_into::<Value, _>(compiled, &context)
            .ok()?;
        if key.is_null() {
            return None;
        }
        feed(&mut h, &serde_json::to_vec(&key).unwrap_or_default());
    } else {
        feed(&mut h, &serde_json::to_vec(data).unwrap_or_default());
    };

    // 128 bits of a 256-bit digest: the birthday bound is 2^64 distinct
    // requests per channel, and the full digest would only make the Redis key
    // longer.
    let digest = h.finalize();
    Some(format!("cache:{channel}:{}", hex::encode(&digest[..16])))
}

/// Feed an optional JSON object into the digest in sorted-key order, so the
/// key is independent of map iteration and query-string order.
pub(super) fn feed_object_sorted(h: &mut Sha256, v: Option<&Value>) {
    let Some(Value::Object(map)) = v else {
        h.update([0u8]);
        return;
    };
    h.update([1u8]);
    h.update((map.len() as u64).to_be_bytes());
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort_unstable();
    for k in keys {
        h.update((k.len() as u64).to_be_bytes());
        h.update(k.as_bytes());
        let bytes = serde_json::to_vec(&map[k.as_str()]).unwrap_or_default();
        h.update((bytes.len() as u64).to_be_bytes());
        h.update(&bytes);
    }
}

/// Context carried from cache pre-check to post-success cache store:
/// (cache key, backend, TTL seconds).
pub type CacheStoreCtx = (String, Arc<dyn CacheBackend>, u64);

/// Outcome of the response-cache pre-check.
pub(super) enum CacheLookup {
    /// Cache hit — carries the cached pre-serialized JSON body.
    Hit(String),
    /// No cache hit. Carries the (key, backend, ttl) needed to store the
    /// computed response on success, or `None` if caching is disabled.
    Miss(Option<CacheStoreCtx>),
}

/// Check the response cache; return a hit or the context needed to store
/// the eventual response on success.
pub(super) async fn check_response_cache(
    channel: &str,
    data: &Value,
    metadata: &Value,
    channel_config: &Option<Arc<ChannelRuntimeConfig>>,
    datalogic: &datalogic_rs::Engine,
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
    let Some(key) = compute_cache_key(
        channel,
        data,
        metadata,
        cache_cfg,
        cfg.cache_key_logic.as_ref(),
        datalogic,
    ) else {
        // The key could not be computed — every declared field absent from this
        // payload, or a `key_logic` that produced nothing. Any key built anyway
        // would be shared with every other request on the channel. Run the
        // workflow and store nothing.
        tracing::warn!(
            channel = %channel,
            fields = ?cache_cfg.cache_key_fields,
            has_key_logic = cfg.cache_key_logic.is_some(),
            "No cache key resolved against the request; bypassing the response cache. \
             Field names are literal payload keys or dotted paths (`user.id`, or \
             `data.user_id` for a top-level `user_id`)."
        );
        return CacheLookup::Miss(None);
    };
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
