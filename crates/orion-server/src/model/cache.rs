//! The loaded-model cache: what is resident in a runtime right now, across
//! every generation, bounded by `models.max_loaded_bytes`.
//!
//! Process-wide rather than per generation, because a load is the expensive
//! step — a parse plus a runtime allocation, seconds and hundreds of
//! megabytes for a large model — and a reload that changed one channel must
//! not pay it again for every model. A generation carries the *compiled
//! adapters* (cheap, bound to its engine); this holds the *runtime
//! sessions* (expensive, bound to nothing but the bytes and the binding),
//! keyed by the digest, the binding, the runtime and the device, so one
//! artifact loaded on two runtimes is two entries and a new version — a new
//! digest — never serves from the old one.
//!
//! The binding is in the key because a session is built from one: a runtime
//! pins the declared input facts and bakes the graph's output permutation
//! into the plan ([`super::LoadBinding`]). Two models naming one artifact
//! with different bindings are therefore two sessions, counted twice
//! against the ceiling, because they are two plans — and keying them on the
//! digest alone served one model the other's, silently reordering its
//! outputs. Two models whose bindings agree still share one session, which
//! is what makes registering one artifact under two manifests cheap.
//!
//! Two properties a load path needs and a plain map does not give:
//!
//! - **Single flight.** The first inference after a publish, and every
//!   concurrent inference behind it, share one load: a per-key mutex
//!   serialises the loaders, and a caller that waited re-checks the map
//!   before loading again. Without it, ten requests arriving at a cold
//!   model would parse it ten times and keep one.
//! - **Bounded residency.** An insertion accounts the model's
//!   [`resident_bytes`](super::runtimes::LoadedModel::resident_bytes) and
//!   evicts least-recently-used entries until the total fits under the
//!   ceiling. An entry larger than the whole ceiling is loaded for the call
//!   that asked and **not retained**: it serves that inference and is
//!   dropped, and the model reads as `evicted` afterwards, which is the
//!   honest description of a ceiling the operator set below one model.
//!
//! What was resident and is no longer is remembered by digest, so a health
//! read can tell "never loaded here" from "loaded and evicted".

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use super::runtimes::{LoadError, LoadedModel};

/// What a loaded session is keyed by — the same four things it is a
/// function of: the bytes, the binding, the runtime and the device.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub digest: String,
    /// One of [`super::runtimes::NAMES`], as the runtime reports it.
    pub runtime: &'static str,
    pub device: String,
    /// [`super::LoadBinding::fingerprint`] of what the load was given.
    pub binding: String,
}

struct Entry {
    model: Arc<dyn LoadedModel>,
    bytes: u64,
    /// The tick of the last hit; the smallest is the least recently used.
    last_used: u64,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<CacheKey, Entry>,
    /// The `(digest, binding)` pairs that were resident at some point and
    /// are no longer, on any runtime — what tells `evicted` from `never
    /// loaded`.
    gone: HashSet<(String, String)>,
    tick: u64,
}

/// The cache. Cheap to share; every method takes `&self`.
pub struct LoadedCache {
    max_bytes: u64,
    inner: Mutex<Inner>,
    /// One mutex per key, held for the length of a load so concurrent first
    /// callers share it. Kept for the life of the process: the set of keys
    /// is the set of bindings × runtimes this node has ever loaded, which is
    /// small.
    flights: Mutex<HashMap<CacheKey, Arc<tokio::sync::Mutex<()>>>>,
}

impl LoadedCache {
    /// A cache holding at most `max_bytes` of resident models.
    pub fn new(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            inner: Mutex::new(Inner::default()),
            flights: Mutex::new(HashMap::new()),
        }
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// The model under `key`, loading it with `load` when it is not
    /// resident. The second value says whether *this call* had to wait for
    /// a load — its own, or one another caller was already running — which
    /// is what an inference reports as `cold_load`.
    ///
    /// `load` is called at most once per concurrent group of callers and
    /// only when the key is absent; the caller runs its parse on the
    /// blocking pool inside the future it hands over. A failed load retains
    /// nothing, and the next caller tries again.
    pub async fn get_or_load<F, Fut>(
        &self,
        key: CacheKey,
        load: F,
    ) -> Result<(Arc<dyn LoadedModel>, bool), LoadError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Arc<dyn LoadedModel>, LoadError>>,
    {
        if let Some(model) = self.hit(&key) {
            return Ok((model, false));
        }
        let flight = self
            .flights
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.clone())
            .or_default()
            .clone();
        let _in_flight = flight.lock().await;
        // A caller that waited behind a load finds it resident now — cold
        // for this call, because it paid the wait, but loaded once.
        if let Some(model) = self.hit(&key) {
            return Ok((model, true));
        }
        let model = load().await?;
        self.insert(key, model.clone());
        Ok((model, true))
    }

    /// The resident model under `key`, touched as most recently used.
    fn hit(&self, key: &CacheKey) -> Option<Arc<dyn LoadedModel>> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.tick += 1;
        let tick = inner.tick;
        let entry = inner.entries.get_mut(key)?;
        entry.last_used = tick;
        Some(entry.model.clone())
    }

    /// Account `model` under `key` and evict least-recently-used entries
    /// until the total fits. A model larger than the ceiling on its own is
    /// not retained — see the module docs.
    fn insert(&self, key: CacheKey, model: Arc<dyn LoadedModel>) {
        let bytes = model.resident_bytes() as u64;
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.tick += 1;
        let tick = inner.tick;
        if bytes > self.max_bytes {
            tracing::warn!(
                digest = %key.digest,
                runtime = key.runtime,
                resident_bytes = bytes,
                max_loaded_bytes = self.max_bytes,
                "Model is larger than models.max_loaded_bytes: served for this call and not \
                 kept resident"
            );
            inner.gone.insert((key.digest, key.binding));
            crate::metrics::set_model_loaded_bytes(total(&inner));
            return;
        }
        inner.entries.insert(
            key,
            Entry {
                model,
                bytes,
                last_used: tick,
            },
        );
        while total(&inner) > self.max_bytes {
            let Some(victim) = inner
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(evicted) = inner.entries.remove(&victim) {
                tracing::info!(
                    digest = %victim.digest,
                    runtime = victim.runtime,
                    resident_bytes = evicted.bytes,
                    "Model evicted: models.max_loaded_bytes reached"
                );
            }
            inner.gone.insert((victim.digest, victim.binding));
        }
        crate::metrics::set_model_loaded_bytes(total(&inner));
    }

    /// Every resident entry and its bytes.
    pub fn states(&self) -> Vec<(CacheKey, u64)> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<(CacheKey, u64)> = inner
            .entries
            .iter()
            .map(|(k, e)| (k.clone(), e.bytes))
            .collect();
        out.sort_by(|a, b| {
            a.0.digest
                .cmp(&b.0.digest)
                .then(a.0.runtime.cmp(b.0.runtime))
                .then(a.0.binding.cmp(&b.0.binding))
        });
        out
    }

    /// Bytes resident right now, across every runtime.
    pub fn loaded_bytes(&self) -> u64 {
        total(&self.inner.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn contains(&self, key: &CacheKey) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .contains_key(key)
    }

    /// The first resident entry under `digest` with `binding`, on any
    /// runtime, and its bytes. The binding is asked for because another
    /// model's session over the same artifact is not this one's: it answers
    /// for the model that asked, not for the bytes.
    pub fn loaded_for(&self, digest: &str, binding: &str) -> Option<(CacheKey, u64)> {
        self.states()
            .into_iter()
            .find(|(key, _)| key.digest == digest && key.binding == binding)
    }

    /// Whether this pair was resident on some runtime and is on none now.
    pub fn was_evicted(&self, digest: &str, binding: &str) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .gone
            .contains(&(digest.to_string(), binding.to_string()))
            && !inner
                .entries
                .keys()
                .any(|k| k.digest == digest && k.binding == binding)
    }
}

fn total(inner: &Inner) -> u64 {
    inner.entries.values().map(|e| e.bytes).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dataflow_rs::datavalue::OwnedDataTensor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A resident model of a stated size that runs nothing.
    struct Fake {
        digest: String,
        bytes: usize,
    }

    impl LoadedModel for Fake {
        fn digest(&self) -> &str {
            &self.digest
        }
        fn resident_bytes(&self) -> usize {
            self.bytes
        }
        fn run(
            &self,
            _inputs: Vec<OwnedDataTensor>,
        ) -> Result<Vec<OwnedDataTensor>, super::super::runtimes::RunError> {
            Ok(Vec::new())
        }
    }

    fn key(digest: &str) -> CacheKey {
        key_with(digest, "binding")
    }

    fn key_with(digest: &str, binding: &str) -> CacheKey {
        CacheKey {
            digest: digest.to_string(),
            runtime: "tract",
            device: "cpu".to_string(),
            binding: binding.to_string(),
        }
    }

    fn fake(digest: &str, bytes: usize) -> Arc<dyn LoadedModel> {
        Arc::new(Fake {
            digest: digest.to_string(),
            bytes,
        })
    }

    /// The first call loads, the second hits, a failed load keeps nothing.
    #[tokio::test]
    async fn a_hit_does_not_reload_and_a_failure_retains_nothing() {
        let cache = LoadedCache::new(100);
        let loads = AtomicUsize::new(0);
        let (_, cold) = cache
            .get_or_load(key("a"), || {
                loads.fetch_add(1, Ordering::SeqCst);
                async { Ok(fake("a", 10)) }
            })
            .await
            .expect("loads");
        assert!(cold);
        let (model, cold) = cache
            .get_or_load(key("a"), || {
                loads.fetch_add(1, Ordering::SeqCst);
                async { Ok(fake("a", 10)) }
            })
            .await
            .expect("hits");
        assert!(!cold);
        assert_eq!(model.digest(), "a");
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert!(cache.contains(&key("a")));
        assert_eq!(cache.loaded_bytes(), 10);
        assert_eq!(cache.loaded_for("a", "binding").map(|(_, b)| b), Some(10));

        let Err(err) = cache
            .get_or_load(key("b"), || async {
                Err(LoadError::new("parse", "bad bytes"))
            })
            .await
        else {
            unreachable!("a failed load must not produce a model")
        };
        assert_eq!(err.stage, "parse");
        assert!(!cache.contains(&key("b")));
        assert!(!cache.was_evicted("b", "binding"));
        assert_eq!(cache.loaded_bytes(), 10);
    }

    /// Eviction is by bytes, least recently used first, and remembers what
    /// it dropped.
    #[tokio::test]
    async fn eviction_is_lru_by_bytes() {
        let cache = LoadedCache::new(100);
        for (digest, bytes) in [("a", 40), ("b", 40)] {
            cache
                .get_or_load(key(digest), || async move { Ok(fake(digest, bytes)) })
                .await
                .expect("loads");
        }
        // Touch `a`, so `b` is the least recently used.
        cache
            .get_or_load(key("a"), || async { unreachable!("a is resident") })
            .await
            .expect("hit");
        cache
            .get_or_load(key("c"), || async { Ok(fake("c", 40)) })
            .await
            .expect("loads");
        assert!(cache.contains(&key("a")));
        assert!(!cache.contains(&key("b")));
        assert!(cache.contains(&key("c")));
        assert_eq!(cache.loaded_bytes(), 80);
        assert!(cache.was_evicted("b", "binding"));
        assert!(!cache.was_evicted("a", "binding"));
        assert_eq!(
            cache
                .states()
                .iter()
                .map(|(k, b)| (k.digest.as_str(), *b))
                .collect::<Vec<_>>(),
            [("a", 40), ("c", 40)]
        );
        // Reloaded, it is resident again and no longer counts as evicted.
        cache
            .get_or_load(key("b"), || async { Ok(fake("b", 10)) })
            .await
            .expect("loads");
        assert!(!cache.was_evicted("b", "binding"));
    }

    /// A model over the whole ceiling serves its caller and is not kept.
    #[tokio::test]
    async fn an_oversized_model_is_served_but_not_retained() {
        let cache = LoadedCache::new(100);
        cache
            .get_or_load(key("small"), || async { Ok(fake("small", 30)) })
            .await
            .expect("loads");
        let (model, cold) = cache
            .get_or_load(key("huge"), || async { Ok(fake("huge", 500)) })
            .await
            .expect("loads");
        assert!(cold);
        assert_eq!(model.resident_bytes(), 500);
        assert!(!cache.contains(&key("huge")));
        assert!(cache.was_evicted("huge", "binding"));
        // The small one was not sacrificed for a model that could never fit.
        assert!(cache.contains(&key("small")));
        assert_eq!(cache.loaded_bytes(), 30);
    }

    /// Two models over one artifact are two entries, not one.
    ///
    /// A session is built from a binding, so two manifests over one file are
    /// two plans. Keyed on the digest alone they aliased, and the second
    /// model was served the first's session — silently, when the two named
    /// the same outputs in a different order. Residency is per pair too: the
    /// health view must answer for the model that asked, not for the bytes.
    #[tokio::test]
    async fn one_digest_with_two_bindings_is_two_entries() {
        let cache = LoadedCache::new(100);
        for binding in ["order-a", "order-b"] {
            cache
                .get_or_load(key_with("one", binding), || async { Ok(fake("one", 40)) })
                .await
                .expect("loads");
        }
        assert!(cache.contains(&key_with("one", "order-a")));
        assert!(cache.contains(&key_with("one", "order-b")));
        assert_eq!(cache.loaded_bytes(), 80, "two plans, counted twice");
        assert_eq!(
            cache
                .loaded_for("one", "order-b")
                .map(|(key, _)| key.binding),
            Some("order-b".to_string())
        );
        assert!(
            cache.loaded_for("one", "order-c").is_none(),
            "a third binding over the same bytes is not resident"
        );

        // Evicting one of the pair leaves the other's answer alone: the
        // digest is still resident, and it is still not this binding's.
        cache
            .get_or_load(key_with("two", "order-a"), || async { Ok(fake("two", 40)) })
            .await
            .expect("loads");
        assert!(!cache.contains(&key_with("one", "order-a")));
        assert!(cache.was_evicted("one", "order-a"));
        assert!(!cache.was_evicted("one", "order-b"));
        assert!(cache.contains(&key_with("one", "order-b")));
    }

    /// Concurrent first callers share one load, and every one of them
    /// reports a cold call.
    #[tokio::test]
    async fn concurrent_first_callers_share_one_load() {
        let cache = Arc::new(LoadedCache::new(100));
        let loads = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Notify::new());
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let cache = cache.clone();
            let loads = loads.clone();
            let gate = gate.clone();
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_load(key("shared"), move || async move {
                        loads.fetch_add(1, Ordering::SeqCst);
                        // Hold the load open until every caller is queued.
                        gate.notified().await;
                        Ok(fake("shared", 10))
                    })
                    .await
                    .expect("loads")
            }));
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        gate.notify_one();
        for task in tasks {
            let (model, cold) = task.await.expect("joins");
            assert_eq!(model.digest(), "shared");
            assert!(cold, "every caller in the first group waited for the load");
        }
        assert_eq!(
            loads.load(Ordering::SeqCst),
            1,
            "one load for eight callers"
        );
        assert_eq!(cache.max_bytes(), 100);
    }
}
