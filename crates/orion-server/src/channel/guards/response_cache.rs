//! The per-channel response cache: the lookup, and the key it is stored under.
//!
//! Split out of `guards` as one concept. The key derivation is the larger half
//! and the more delicate one — what goes into the hash decides which requests
//! are allowed to share an answer.

use dataflow_rs::datalogic_rs;
use std::sync::Arc;

use serde_json::Value;

use super::ChannelRuntimeConfig;
use crate::channel::cache_namespace;
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

    // `key_logic` first: it is the general form and the documented one to win
    // when both are declared. The two branches used to be the other way round,
    // so a channel declaring both keyed on the fields and never evaluated the
    // expression its author wrote to say what varies the response (#354).
    if let Some(compiled) = key_logic {
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
    } else if let Some(ref fields) = cache_cfg.cache_key_fields {
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

/// Context carried from cache pre-check to post-success cache store.
pub struct CacheStoreCtx {
    key: String,
    backend: Arc<dyn CacheBackend>,
    ttl_secs: u64,
    /// The namespace versions read at lookup, for a channel declaring
    /// `cache.namespaces`; `None` otherwise. Captured before the workflow ran,
    /// on purpose — see `channel::cache_namespace`.
    versions: Option<Vec<i64>>,
    /// Held by the one request running the workflow for this key while
    /// others wait (`cache.coalesce_misses`). Dropped with this context —
    /// after the store, or without one when the run failed — which is what
    /// releases the waiters. Boxed: this context rides inside every
    /// `Admission`, and most channels never coalesce.
    _flight: Option<Box<FlightGuard>>,
}

impl CacheStoreCtx {
    /// Store the response body this request produced.
    pub async fn store(&self, body: &str) -> Result<(), crate::errors::OrionError> {
        match &self.versions {
            None => self.backend.set_ex(&self.key, body, self.ttl_secs).await,
            Some(versions) => {
                let stored = cache_namespace::encode_entry(versions, body);
                self.backend.set_ex(&self.key, &stored, self.ttl_secs).await
            }
        }
    }

    fn miss(
        key: String,
        backend: &Arc<dyn CacheBackend>,
        ttl_secs: u64,
        versions: Option<Vec<i64>>,
    ) -> CacheLookup {
        CacheLookup::Miss(Some(Self {
            key,
            backend: backend.clone(),
            ttl_secs,
            versions,
            _flight: None,
        }))
    }
}

/// Outcome of the response-cache pre-check.
pub(super) enum CacheLookup {
    /// Cache hit — carries the cached pre-serialized JSON body.
    Hit(String),
    /// No cache hit. Carries what is needed to store the computed response on
    /// success, or `None` if nothing may be stored.
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
    let ttl_secs = cache_cfg.ttl_secs.unwrap_or(300);
    let namespaces = cache_cfg.namespaces.as_deref().unwrap_or(&[]);
    // The storage key, decided once: a namespaced entry lives under a prefix
    // of its own.
    let key = if namespaces.is_empty() {
        key
    } else {
        cache_namespace::entry_key(&key)
    };

    let first = lookup_once(channel, cache, key, ttl_secs, namespaces).await;
    let Some(flights) = cfg.cache_flights.as_ref() else {
        return first;
    };
    let CacheLookup::Miss(Some(mut ctx)) = first else {
        return first;
    };
    match flights.join(&ctx.key) {
        Flight::Leader(guard) => {
            ctx._flight = Some(Box::new(guard));
            CacheLookup::Miss(Some(ctx))
        }
        Flight::Follower(mut done) => {
            // Wait for the leader's context to drop — after its store, or
            // without one. `changed` errors as soon as the sender is gone,
            // including when it went before this call.
            let wait = cfg
                .parsed_config
                .timeout_ms
                .map_or(MAX_COALESCE_WAIT, |ms| {
                    std::time::Duration::from_millis(ms).min(MAX_COALESCE_WAIT)
                });
            let _ = tokio::time::timeout(wait, done.changed()).await;
            // The same lookup again, uncoalesced: a hit is the leader's entry;
            // anything else and this request runs the workflow itself.
            let retry = lookup_once(channel, cache, ctx.key, ttl_secs, namespaces).await;
            if matches!(retry, CacheLookup::Hit(_)) {
                metrics::record_cache_coalesced(channel);
            }
            retry
        }
    }
}

/// Longest a coalesced miss waits for its leader before running the workflow
/// itself.
const MAX_COALESCE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// One lookup of `key`, the storage key: a hit, or the context to store
/// under. A namespaced channel reads its counters and its entry in one `MGET`.
async fn lookup_once(
    channel: &str,
    cache: &Arc<dyn CacheBackend>,
    key: String,
    ttl_secs: u64,
    namespaces: &[String],
) -> CacheLookup {
    if namespaces.is_empty() {
        return match cache.get(&key).await {
            Ok(Some(cached)) => {
                metrics::record_cache_hit(channel);
                CacheLookup::Hit(cached)
            }
            _ => {
                metrics::record_cache_miss(channel);
                CacheStoreCtx::miss(key, cache, ttl_secs, None)
            }
        };
    }

    // One round trip: every namespace's version, then the entry.
    let mut keys: Vec<String> = namespaces
        .iter()
        .map(|ns| cache_namespace::version_key(ns))
        .collect();
    keys.push(key);
    let result = cache.get_many(&keys).await;
    let key = keys.pop().unwrap_or_default();
    let mut values = match result {
        Ok(values) if values.len() == keys.len() + 1 => values,
        other => {
            // Without the versions nothing can be judged fresh, and nothing
            // stored could be tagged honestly: run the workflow, store nothing.
            if let Err(e) = other {
                tracing::debug!(channel = %channel, error = %e, "Response-cache lookup failed");
            }
            metrics::record_cache_miss(channel);
            return CacheLookup::Miss(None);
        }
    };
    let entry = values.pop().flatten();
    let versions: Option<Vec<i64>> = values
        .iter()
        .map(|raw| cache_namespace::parse_version(raw.as_deref()))
        .collect();
    let Some(versions) = versions else {
        tracing::warn!(
            channel = %channel,
            "A response-cache namespace counter holds something other than an integer; \
             bypassing the response cache"
        );
        metrics::record_cache_miss(channel);
        return CacheLookup::Miss(None);
    };
    if let Some(body) = entry.and_then(|stored| cache_namespace::decode_entry(stored, &versions)) {
        metrics::record_cache_hit(channel);
        return CacheLookup::Hit(body);
    }
    metrics::record_cache_miss(channel);
    CacheStoreCtx::miss(key, cache, ttl_secs, Some(versions))
}

/// The misses in flight on one channel, by storage key (`coalesce_misses`).
///
/// An entry exists while one request — the leader — is running the workflow
/// for that key. It maps to a receiver whose sender the leader holds, so every
/// follower learns the leader is done the moment its `FlightGuard` drops,
/// whatever the reason: the entry stored, the workflow failed, the request was
/// cancelled. Nothing is ever sent; the drop is the signal.
#[derive(Default)]
pub struct CacheFlights(dashmap::DashMap<String, tokio::sync::watch::Receiver<()>>);

/// Held by a flight's leader. Dropping it ends the flight.
pub struct FlightGuard {
    flights: Arc<CacheFlights>,
    key: String,
    _done: tokio::sync::watch::Sender<()>,
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        // Out of the table first, then `_done` drops with the struct and wakes
        // the followers — so a request arriving after the wake cannot join a
        // flight that has already ended.
        self.flights.0.remove(&self.key);
    }
}

enum Flight {
    Leader(FlightGuard),
    Follower(tokio::sync::watch::Receiver<()>),
}

impl CacheFlights {
    fn join(self: &Arc<Self>, key: &str) -> Flight {
        use dashmap::mapref::entry::Entry;
        // Most joins under load are followers: answer those without
        // allocating a key.
        if let Some(flight) = self.0.get(key) {
            return Flight::Follower(flight.clone());
        }
        match self.0.entry(key.to_string()) {
            Entry::Occupied(flight) => Flight::Follower(flight.get().clone()),
            Entry::Vacant(slot) => {
                let (done, waiting) = tokio::sync::watch::channel(());
                slot.insert(waiting);
                Flight::Leader(FlightGuard {
                    flights: self.clone(),
                    key: key.to_string(),
                    _done: done,
                })
            }
        }
    }

    /// Misses currently being run by a leader, for tests.
    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> usize {
        self.0.len()
    }
}
