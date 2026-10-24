use std::collections::HashMap;
use std::sync::Arc;

use dataflow_rs::Expr;
use dataflow_rs::expr::ExprEngine;
use tokio::sync::Semaphore;

use super::config::ChannelConfig;
use super::config::ChannelRateLimitConfig;
use super::rate_limit_backend::{LocalRateLimitBackend, RateLimitBackend, RedisRateLimitBackend};
use super::routing::{RouteMatch, RouteTable};

use crate::config::{TraceStorageConfig, TraceStorageMode};
use crate::connector::ConnectorConfig;
use crate::connector::ConnectorRegistry;
use crate::connector::cache_backend::{CacheBackend, CachePool, CachePurpose};
use crate::storage::models::Channel;

/// Trace-storage policy resolved for a single channel.
///
/// Produced by merging the global `[trace_storage]` config with the
/// channel-level `tracing` override (`ChannelTracingConfig`). Lives on
/// `ChannelRuntimeConfig` so the request hot path looks up exactly one
/// `Arc<ChannelRuntimeConfig>` and reads pre-resolved values.
#[derive(Debug, Clone, Copy)]
pub struct EffectiveTraceConfig {
    pub mode: TraceStorageMode,
    pub sample_rate: f64,
    pub errors_only: bool,
    /// When `true`, the engine should capture per-task execution traces
    /// and persist them. No global default — only enabled per-channel
    /// because the storage cost scales with task count and payload size.
    pub task_details: bool,
}

impl EffectiveTraceConfig {
    /// Draw this trace's sampling coin. **Call exactly once per trace**, at
    /// the single point the trace's persistence is decided, and feed the
    /// outcome to [`Self::should_drop`] — that is what makes sampling
    /// per-*trace* rather than per-*write* (N22). Deterministic at the
    /// extremes: `sample_rate >= 1.0` never draws, `0.0` never samples in.
    pub fn draw_sample(&self) -> bool {
        self.sample_rate >= 1.0 || rand::random::<f64>() < self.sample_rate
    }

    /// Returns `Some(reason)` if a trace with the given error state should be
    /// dropped per this config (off / errors_only / sampled out), or `None`
    /// to persist. Used by both the sync request path and the async-queue
    /// post-processing path so filter semantics stay consistent.
    ///
    /// Pure and deterministic: `sampled_in` is the once-per-trace outcome of
    /// [`Self::draw_sample`], injected so this method never rolls its own
    /// dice — an `&self` method that read as pure used to call
    /// `rand::random` internally, which made the filter untestable between
    /// the extremes and let one trace get independently sampled per call
    /// site (N22).
    pub fn should_drop(&self, has_errors: bool, sampled_in: bool) -> Option<&'static str> {
        if matches!(self.mode, TraceStorageMode::Off) {
            return Some("off");
        }
        if self.errors_only && !has_errors {
            return Some("errors_only");
        }
        if !sampled_in {
            return Some("sampled_out");
        }
        None
    }

    /// The same config as seen by an **async submission**, where trace
    /// persistence is not optional.
    ///
    /// R11: `mode = "off"` on the async path used to mint a throwaway UUID,
    /// answer 202 with `{"trace_id": null, "trace_token": null}` plus a
    /// `Warning: 299` header, and enqueue the work anyway — a 202 whose
    /// documented follow-up (`GET /admin/traces/{id}`) was structurally
    /// impossible, and a nullable `trace_id` baked into the schema forever.
    ///
    /// Appending `/async` *is* the request for a result to be fetched later,
    /// and the trace row is the mechanism that makes that possible, not an
    /// optional extra. So `Off` is upgraded to `Sync` here and nowhere else:
    /// the sync path, where the caller already has the answer in hand, still
    /// honours `off` exactly.
    ///
    /// N22: `sample_rate` is pinned to `1.0` for the same reason. It used to
    /// be "deliberately left alone", which dropped the *result* while the
    /// status rows still landed — the caller polled a `completed` trace with
    /// nothing in it, the storage was spent anyway, and a sampled-out trace
    /// was half-written instead of absent. A 202 is a receipt for a
    /// fetchable result, so async traces are never sampled out; sampling
    /// applies in full on the sync path, where a sampled-out trace produces
    /// no rows at all.
    ///
    /// `errors_only` keeps its documented behaviour: it drops the result of
    /// clean runs, but `create_pending` still writes the row, so the id the
    /// caller was handed continues to resolve.
    pub fn for_async_submission(self) -> Self {
        Self {
            mode: match self.mode {
                TraceStorageMode::Off => TraceStorageMode::Sync,
                other => other,
            },
            sample_rate: 1.0,
            ..self
        }
    }

    /// Compute the effective config by overlaying a channel-level
    /// override on top of the global storage config.
    pub fn resolve(
        global: &TraceStorageConfig,
        channel: Option<&super::config::ChannelTracingConfig>,
    ) -> Self {
        let (mode, sample_rate, errors_only, task_details) = match channel {
            Some(c) => (
                c.mode.unwrap_or(global.mode),
                c.sample_rate.unwrap_or(global.sample_rate),
                c.errors_only.unwrap_or(global.errors_only),
                c.task_details.unwrap_or(false),
            ),
            None => (global.mode, global.sample_rate, global.errors_only, false),
        };
        Self {
            mode,
            sample_rate,
            errors_only,
            task_details,
        }
    }
}

/// Runtime state for a single active channel.
pub struct ChannelRuntimeConfig {
    /// The channel DB model.
    pub channel: Channel,
    /// Parsed per-channel configuration.
    pub parsed_config: ChannelConfig,
    /// Per-channel rate limiter, built from `parsed_config.rate_limit` if
    /// configured. Governor-local on a single node; shared Redis fixed
    /// window in cluster mode.
    pub rate_limiter: Option<Arc<dyn RateLimitBackend>>,
    /// Pre-compiled JSONLogic expression for computing the rate limit key.
    pub rate_limit_key_logic: Option<Arc<Expr>>,
    /// The post-authentication limiter, built from
    /// `parsed_config.principal_rate_limit`. A separate backend from
    /// [`Self::rate_limiter`] so the two counts never share a bucket.
    pub principal_rate_limiter: Option<Arc<dyn RateLimitBackend>>,
    /// Compiled `principal_rate_limit.key_logic`. Always `Some` when
    /// [`Self::principal_rate_limiter`] is — the key is required there.
    pub principal_rate_limit_key_logic: Option<Arc<Expr>>,
    /// Compiled `cache.key_logic`, when the channel sets one.
    pub cache_key_logic: Option<Arc<Expr>>,
    /// Extra header names `rate_limit.key_logic` may read, lowercased once at
    /// load so the request path never allocates to normalize them. `None` means
    /// the built-in set only.
    pub rate_limit_key_headers: Option<Arc<[String]>>,
    /// Pre-compiled JSONLogic expression for input validation.
    /// Evaluated against request data — truthy = pass, falsy = 400 reject.
    pub validation_logic: Option<Arc<Expr>>,
    /// Per-channel concurrency limiter for backpressure.
    /// Limits max in-flight requests — returns 503 when exhausted.
    pub backpressure_semaphore: Option<Arc<Semaphore>>,
    /// Per-channel deduplication backend for idempotent request handling.
    /// Can be backed by in-memory DashMap or Redis, depending on channel config.
    pub dedup_store: Option<Arc<dyn CacheBackend>>,
    /// Per-channel response cache backend.
    /// When set, sync responses are cached with a configurable TTL.
    pub response_cache: Option<Arc<dyn CacheBackend>>,
    /// In-flight response-cache misses, by key, when the channel sets
    /// `cache.coalesce_misses`. Built per load: a reload that changes the
    /// channel starts an empty table, and a request already waiting on the
    /// old one still holds what it waits on.
    pub cache_flights: Option<Arc<crate::channel::guards::CacheFlights>>,
    /// Trace storage policy after merging the global and per-channel config.
    pub trace_storage: EffectiveTraceConfig,
    /// Compiled `auth` policy: secrets resolved and keys pre-hashed once at
    /// load, so the request path never resolves an `env://` reference or
    /// hashes a stored key. `None` means the channel is unauthenticated.
    pub auth: Option<crate::channel::auth::CompiledAuth>,
    /// The compiled inbound sign-in flow (#307), secrets resolved and both key
    /// sets built. `None` for every channel that declares no `oauth2_login`.
    pub oauth2_login: Option<Arc<crate::channel::CompiledOAuth2Login>>,
    /// The compiled schedule, for `protocol: "cron"` channels only.
    ///
    /// Compiled here, at load, for the same reason every other guard is: a
    /// schedule that no longer parses must quarantine its channel rather than
    /// surface at the first fire, in a background task with no caller to answer.
    /// `None` for every other protocol.
    pub cron: Option<Arc<crate::channel::CronDescriptor>>,
}

/// A channel the registry refused to load, and why.
///
/// Two classes produce one:
/// - **Config invariants (any mode).** A `config_json` that no longer parses
///   (N3) or a `validation_logic` that no longer compiles (N4) would load the
///   channel with its guards silently absent — serving unvalidated,
///   unthrottled traffic on the strength of a log line. Both are checked at
///   create/update time, so a failure here means the stored row is corrupt.
/// - **Backend degradation (cluster mode only).** A dedup/response-cache
///   backend that would fall back to per-node memory is a correctness loss in
///   a cluster but an acceptable degradation on one node.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChannelLoadIssue {
    pub channel: String,
    pub reason: String,
}

/// Why a channel is quarantined, and which rows it is: the reason, plus the
/// identities a caller matches on (`package apply` checks its own members by
/// `channel_id`, and finds a channel of another package bound to one of its
/// workflows by `workflow_id`) rather than by parsing the reason's prose.
#[derive(Debug, Clone)]
struct QuarantineEntry {
    reason: String,
    /// Empty when no stored row carries the name (never, in practice: every
    /// issue comes from a row being loaded).
    channel_id: String,
    workflow_id: Option<String>,
}

/// One quarantined channel, as [`ChannelSnapshot::quarantined`] reports it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct QuarantinedChannel {
    pub channel: String,
    pub channel_id: String,
    pub workflow_id: Option<String>,
    pub reason: String,
}

/// The shared backends cluster mode provides to channel loading. `Some` on
/// the registry means cluster mode: strict backend resolution (no silent
/// in-memory fallbacks) with these as the defaults. Deliberately narrow —
/// the registry needs these two handles, not the whole cluster runtime.
pub struct ClusterBackends {
    /// Default dedup/response-cache backend (the shared cluster Redis).
    pub default_cache: Option<Arc<dyn CacheBackend>>,
    /// Connection for shared rate-limit windows.
    pub redis: Option<redis::aio::ConnectionManager>,
}

/// The identity of one stored channel row.
///
/// `channel_id` + `version` names the row and `updated_at` moves whenever it
/// is edited, so two equal keys mean the same bytes — which is what makes
/// reusing a whole [`ChannelRuntimeConfig`] across a reload sound (N17).
/// Active rows are immutable by DB trigger, so in practice a serving channel's
/// key only ever changes by gaining a new version.
#[derive(Clone, PartialEq, Eq)]
struct ChannelRow {
    channel_id: String,
    version: i64,
    updated_at: chrono::NaiveDateTime,
}

impl ChannelRow {
    /// The owned form, for the `route_key` the snapshot keeps. Owned because
    /// it outlives the `&[Channel]` the reload was handed.
    fn of(channel: &Channel) -> Self {
        Self {
            channel_id: channel.channel_id.clone(),
            version: channel.version,
            updated_at: channel.updated_at,
        }
    }

    /// Whether two rows are the same row, without building either identity.
    /// The reuse check runs once per channel per reload and is the hot path
    /// N17 exists for; `ChannelRow::of(a) == ChannelRow::of(b)` allocated two
    /// `String`s per channel just to throw them away.
    fn same_row(a: &Channel, b: &Channel) -> bool {
        a.channel_id == b.channel_id && a.version == b.version && a.updated_at == b.updated_at
    }
}

/// The inputs to [`ChannelLoader::build`] that are *not* channel rows.
///
/// A `ChannelRuntimeConfig` is derived from its row **and** from these, so the
/// per-channel cache is only valid while they hold. Both move rarely — the
/// connector token only when a load actually changed the connector set (see
/// [`ConnectorRegistry::config_generation`]), the trace-storage config never
/// after boot — so in steady state every unchanged channel is reused, on every
/// node, including through an epoch resync.
///
/// # Why two fields and not five
///
/// [`RuntimeDeps`] has five, and a reused `Arc<ChannelRuntimeConfig>` embeds
/// products of three of them. The three that are absent are absent
/// deliberately:
///
/// - **`exprs`** — one `ExprEngine` is built at boot and lives on
///   `AppState` for the life of the process. Nothing rebuilds or replaces it,
///   so a compiled `Expr` carried over a reload was compiled by the same
///   engine that would compile its replacement.
/// - **`cache_pool`** — likewise a process singleton, and its mutations are
///   evictions (`evict_pool` / `evict_all_pools`), not replacements. An
///   eviction drops the pool's *cached* handle; a channel holding an
///   `Arc<dyn CacheBackend>` keeps a live one. That is safe precisely while
///   the connector behind it is unchanged: a memory backend is keyed
///   `(purpose, connector)` and is never evicted at all, and a Redis backend
///   holds a self-healing `ConnectionManager` against the same URL the
///   re-resolved one would use. Every eviction path — the connector admin
///   handlers, and `resync_from_db` on a scope that touches connectors —
///   reloads the connector registry in the same breath, so a connector that
///   actually changed moves the token and forces the rebuild. **An eviction
///   that is not paired with a connector load would not.** A
///   definitions-scoped resync satisfies the rule by doing neither.
/// - **`cluster_redis`** — held by the registry itself, fixed at construction.
///
/// So the obligation this key places on the rest of the process is: keep the
/// expression engine and the cache pool process-singletons, and never evict a
/// pool without loading connectors.
#[derive(Clone, PartialEq)]
struct DepsFingerprint {
    connectors: u64,
    trace_storage: TraceStorageConfig,
    /// `[cron] enabled`. Part of the fingerprint because it decides whether a
    /// cron channel compiles at all: without it, a node restarted with the
    /// scheduler turned off would carry over the descriptors it built while it
    /// was on, and keep serving schedules it can no longer run.
    cron_enabled: bool,
}

/// One immutable generation of the channel estate: the serving map, the route
/// table and the quarantine set, self-consistent with each other.
///
/// N17: `by_name`, the route table and the quarantine map used to live behind
/// three separate locks written one after another, so a reader could see a new
/// serving map against an old route table or an old quarantine map — long
/// enough to answer a request from a mismatched pair (a channel quarantined by
/// this very reload resolved to `Ok(None)`, i.e. 404 "unknown channel" instead
/// of 503 "quarantined"). They are one value now.
///
/// It is a *value*, not a self-swapping registry: it carries no lock and no
/// `ArcSwap`, and it is published — together with the engine built from the
/// same rows — by [`crate::runtime::RuntimeHandle`]. That is what makes the
/// pair atomic. While this type owned its own `ArcSwap`, each half of a reload
/// was individually atomic and the *pair* was not, so a request could be
/// admitted against a channel this generation had just added and then executed
/// by the engine of the one before it — matching zero workflows and answering
/// `200` with the caller's own input.
pub struct ChannelSnapshot {
    by_name: HashMap<String, Arc<ChannelRuntimeConfig>>,
    /// `Arc` so an unchanged route set survives a reload without being
    /// rebuilt — `RouteTable::build` parses every pattern and its conflict
    /// scan is quadratic in route-bearing channels.
    route_table: Arc<RouteTable>,
    /// Channels that failed to load, by name, with the reason (F35).
    ///
    /// A quarantined channel is refused at every ingress rather than served
    /// with none of its guards — that refusal is the whole point of N3/N4.
    /// Keeping them here instead of aborting the reload confines the blast
    /// radius to the broken channel: the other channels still load, and the
    /// admin mutations that trigger a reload still work.
    quarantined: HashMap<String, QuarantineEntry>,
    /// The serviceable rows `route_table` was built from, in supplied order —
    /// the key that decides whether it can be carried over.
    route_key: Vec<ChannelRow>,
    deps: DepsFingerprint,
}

impl ChannelSnapshot {
    /// The empty generation a node boots on. Its fingerprint is deliberately
    /// unmatchable (`connectors: u64::MAX` is never drawn by
    /// `ConnectorRegistry::config_generation`), so the first build cannot
    /// "reuse" anything.
    pub fn empty() -> Self {
        Self {
            by_name: HashMap::new(),
            route_table: Arc::new(RouteTable::default()),
            quarantined: HashMap::new(),
            route_key: Vec::new(),
            deps: DepsFingerprint {
                connectors: u64::MAX,
                trace_storage: TraceStorageConfig::default(),
                cron_enabled: false,
            },
        }
    }

    /// Why a channel is quarantined, or `None` when it is serviceable (F35).
    pub fn quarantine_reason(&self, name: &str) -> Option<String> {
        self.quarantined.get(name).map(|entry| entry.reason.clone())
    }

    /// Every quarantined channel, for `/health` and the admin surface.
    ///
    /// N21: this — not a return value from [`ChannelLoader::build`] — is the
    /// single place the quarantine set is read from. The build used to hand
    /// the same list back as a `Vec`, which meant two representations of one
    /// fact: callers that wanted the set afterwards could read either, and the
    /// `Vec` could carry two entries for a channel broken in both the engine
    /// build and its own config while the map (last-write-wins) carried the
    /// more specific one.
    pub fn quarantined(&self) -> Vec<QuarantinedChannel> {
        let mut out: Vec<QuarantinedChannel> = self
            .quarantined
            .iter()
            .map(|(channel, entry)| QuarantinedChannel {
                channel: channel.clone(),
                channel_id: entry.channel_id.clone(),
                workflow_id: entry.workflow_id.clone(),
                reason: entry.reason.clone(),
            })
            .collect();
        // A stable order, so two reads of one generation — `/health`, the
        // reload answer — list it the same way.
        out.sort_by(|a, b| a.channel.cmp(&b.channel));
        out
    }

    /// Every serviceable cron channel's compiled schedule, in a stable order.
    ///
    /// The reconciler's whole view of what is scheduled. Read from the
    /// generation it already loaded, so the schedules it plans against and the
    /// engine that will run them are one build — the same rule every ingress
    /// follows, and the reason the descriptor lives on the snapshot rather than
    /// in a table of its own.
    ///
    /// Quarantined channels are absent by construction: `by_name` has already
    /// had them removed, so a channel whose schedule stopped compiling stops
    /// being materialised rather than materialising occurrences nothing can run.
    pub fn cron_descriptors(&self) -> Vec<Arc<crate::channel::CronDescriptor>> {
        let mut out: Vec<Arc<crate::channel::CronDescriptor>> = self
            .by_name
            .values()
            .filter_map(|runtime| runtime.cron.clone())
            .collect();
        // By stable identity, not by hash order: two nodes planning the same
        // pass should walk the same schedules in the same order, and a log line
        // naming "the first channel" should mean the same one twice.
        out.sort_by(|a, b| a.channel_id.cmp(&b.channel_id));
        out
    }

    /// One serviceable cron channel by its **stable id**, for a worker settling
    /// an occurrence.
    ///
    /// By `channel_id` rather than by name because an occurrence outlives the
    /// name it was materialised under: a channel renamed between materialisation
    /// and execution is the same channel, and a name lookup would fail as though
    /// it had been deleted.
    pub fn cron_by_channel_id(&self, channel_id: &str) -> Option<Arc<ChannelRuntimeConfig>> {
        self.by_name
            .values()
            .find(|runtime| runtime.cron.is_some() && runtime.channel.channel_id == channel_id)
            .cloned()
    }

    /// Whether this generation has any cron channel at all — what `/health` and
    /// the reconciler's idle path ask before doing anything more expensive.
    pub fn has_cron_channels(&self) -> bool {
        self.by_name.values().any(|runtime| runtime.cron.is_some())
    }

    /// Look up a channel's runtime config, refusing quarantined channels.
    ///
    /// Every ingress path goes through this rather than [`Self::get_by_name`]:
    /// a quarantined channel returns `Ok(None)` from a plain lookup, which is
    /// indistinguishable from "no config" and would serve it with none of its
    /// guards — the exact failure N3/N4 exist to prevent.
    ///
    /// Both halves read one generation (N17), and so does everything else the
    /// request touches: the caller holds an `Arc<RuntimeGeneration>` for the
    /// whole request, so the quarantine map, the serving map, the route table
    /// and the engine it will run on all come from the same build.
    pub fn require_serviceable(
        &self,
        name: &str,
    ) -> Result<Option<Arc<ChannelRuntimeConfig>>, crate::errors::OrionError> {
        if let Some(entry) = self.quarantined.get(name) {
            return Err(crate::errors::OrionError::unavailable(
                crate::errors::Unavailable::ChannelQuarantined,
                format!(
                    "Channel '{name}' failed to load and is not being served: {}",
                    entry.reason
                ),
            ));
        }
        Ok(self.by_name.get(name).cloned())
    }

    /// Look up an active channel by name.
    pub fn get_by_name(&self, name: &str) -> Option<Arc<ChannelRuntimeConfig>> {
        self.by_name.get(name).cloned()
    }

    /// Match a request (method, path) against REST channel route patterns.
    /// Path should NOT include the `/api/v2/data/` prefix.
    ///
    /// `Err(BadRequest)` when the path carries an invalid percent-sequence
    /// (N10) — the request is malformed however it would have resolved.
    pub fn match_route(
        &self,
        method: &str,
        path: &str,
    ) -> Result<Option<RouteMatch>, crate::errors::OrionError> {
        self.route_table.match_route(method, path)
    }

    /// How many channels this generation serves.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Whether this generation serves no channel at all.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Prune every per-channel rate limiter's in-process keyed store.
    ///
    /// A limiter keyed by client identity keeps one entry per address (or
    /// `key_logic` header value) ever seen, and governor's store only shrinks
    /// when asked — and per-channel limiters are carried across reloads by
    /// `Arc`, so nothing else ever prompts it. The supervised limiter-prune
    /// task calls this; a Redis-backed limiter is a no-op ([`RateLimitBackend::prune`]).
    pub fn prune_rate_limiters(&self) {
        for cfg in self.by_name.values() {
            if let Some(limiter) = &cfg.rate_limiter {
                limiter.prune();
            }
            if let Some(limiter) = &cfg.principal_rate_limiter {
                limiter.prune();
            }
        }
    }
}

mod loader;
pub use loader::*;

#[cfg(test)]
mod tests;
