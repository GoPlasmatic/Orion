use dataflow_rs::datalogic_rs;
use std::collections::HashMap;
use std::sync::Arc;

use datalogic_rs::{Engine as DatalogicEngine, Logic};
use tokio::sync::Semaphore;

use super::config::ChannelConfig;
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
    pub rate_limit_key_logic: Option<Logic>,
    /// Compiled `cache.key_logic`, when the channel sets one.
    pub cache_key_logic: Option<Logic>,
    /// Extra header names `rate_limit.key_logic` may read, lowercased once at
    /// load so the request path never allocates to normalize them. `None` means
    /// the built-in set only.
    pub rate_limit_key_headers: Option<Arc<[String]>>,
    /// Pre-compiled JSONLogic expression for input validation.
    /// Evaluated against request data — truthy = pass, falsy = 400 reject.
    pub validation_logic: Option<Logic>,
    /// Per-channel concurrency limiter for backpressure.
    /// Limits max in-flight requests — returns 503 when exhausted.
    pub backpressure_semaphore: Option<Arc<Semaphore>>,
    /// Per-channel deduplication backend for idempotent request handling.
    /// Can be backed by in-memory DashMap or Redis, depending on channel config.
    pub dedup_store: Option<Arc<dyn CacheBackend>>,
    /// Per-channel response cache backend.
    /// When set, sync responses are cached with a configurable TTL.
    pub response_cache: Option<Arc<dyn CacheBackend>>,
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
/// - **`datalogic`** — one `DatalogicEngine` is built at boot and lives on
///   `AppState` for the life of the process. Nothing rebuilds or replaces it,
///   so a compiled `Logic` carried over a reload was compiled by the same
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
/// datalogic engine and the cache pool process-singletons, and never evict a
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
    quarantined: HashMap<String, String>,
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
        self.quarantined.get(name).cloned()
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
    pub fn quarantined(&self) -> Vec<ChannelLoadIssue> {
        self.quarantined
            .iter()
            .map(|(channel, reason)| ChannelLoadIssue {
                channel: channel.clone(),
                reason: reason.clone(),
            })
            .collect()
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
        if let Some(reason) = self.quarantined.get(name) {
            return Err(crate::errors::OrionError::unavailable(
                crate::errors::Unavailable::ChannelQuarantined,
                format!("Channel '{name}' failed to load and is not being served: {reason}"),
            ));
        }
        Ok(self.by_name.get(name).cloned())
    }

    /// Look up an active channel by name.
    pub fn get_by_name(&self, name: &str) -> Option<Arc<ChannelRuntimeConfig>> {
        self.by_name.get(name).cloned()
    }

    /// Match a request (method, path) against REST channel route patterns.
    /// Path should NOT include the `/api/v1/data/` prefix.
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
}

/// Everything [`ChannelLoader::build`] needs beyond the channel rows.
///
/// Named rather than positional for the reason [`crate::engine::HandlerDeps`]
/// is: five same-shaped borrows in a row is a silent-transposition hazard the
/// compiler cannot see, and the two production callers (boot and reload) build
/// the identical set from `ServingComponents` and `AppState` respectively.
pub struct ReloadDeps<'a> {
    /// The instance's `[vars]`, for `var://` references in a stored channel
    /// config. `None` declares nothing, so every reference fails and names the
    /// channel.
    pub vars: Option<&'a std::sync::Arc<serde_json::Value>>,
    pub connector_registry: &'a ConnectorRegistry,
    pub cache_pool: &'a CachePool,
    pub datalogic: &'a DatalogicEngine,
    /// The instance's JWKS cache, for `auth.mode = "jwt"` channels.
    pub jwks: &'a std::sync::Arc<crate::jwt::jwks::JwksCache>,
    /// The shared outbound client, for a channel's `oauth2_login` code
    /// exchange (#307).
    pub http_client: &'a reqwest::Client,
    /// `[oauth2_login] allow_private_token_urls`.
    pub allow_private_token_urls: bool,
    pub global_trace_storage: &'a TraceStorageConfig,
    /// `[cron] enabled`. A cron channel loaded on a node with the scheduler off
    /// is quarantined rather than served as a schedule that never fires.
    pub cron_enabled: bool,
}

/// Everything [`ChannelLoader::build_runtime`] needs beyond the channel row.
struct RuntimeDeps<'a> {
    vars: Option<&'a std::sync::Arc<serde_json::Value>>,
    connector_registry: &'a ConnectorRegistry,
    cache_pool: &'a CachePool,
    datalogic: &'a DatalogicEngine,
    /// The instance's JWKS cache, for `auth.mode = "jwt"` channels. Like
    /// `datalogic` and `cache_pool` it is a process singleton, so a carried-over
    /// `ChannelRuntimeConfig` holding a compiled verifier built from it stays
    /// valid — see [`DepsFingerprint`].
    jwks: &'a std::sync::Arc<crate::jwt::jwks::JwksCache>,
    /// The shared outbound client (#307). A `reqwest::Client` is an `Arc`
    /// internally, so a compiled sign-in block holding a clone holds a handle
    /// to the one pool, not a second one.
    http_client: &'a reqwest::Client,
    allow_private_token_urls: bool,
    global_trace_storage: &'a TraceStorageConfig,
    cluster_redis: Option<redis::aio::ConnectionManager>,
    cron_enabled: bool,
}

/// Refuse an object with more than one key anywhere inside a channel
/// expression.
///
/// N3/N4 refuse a channel whose guard config stopped compiling rather than
/// serve it with a weaker control than the one configured, and a multi-key
/// object used to be caught by that: the datalogic engine Orion built directly
/// rejected one outright.
///
/// The serving engine is now dataflow-rs's, so channel expressions speak the
/// same dialect workflow expressions do — which is what lets them read
/// `{"secret": …}`, and which also runs in **templating mode**, where a
/// multi-key object is a legal template emitting a structured object. A typo'd
/// `{"==": [1, 1], "!=": [1, 2]}` would compile to a truthy object and
/// `validate_input` would then admit everything, silently — precisely the
/// outcome N4 exists to prevent.
///
/// So the refusal is stated here rather than inherited from a compiler mode.
/// Nothing is lost by it: every channel expression is a predicate or a key, and
/// none has a reason to emit a structured object.
/// Whether a channel-config key holds a JSONLogic expression.
///
/// Deployment references resolve at *load*; an expression is evaluated per
/// *message*, against the message. Substituting into one would mix the two — a
/// literal `"var://x"` inside a predicate is a string the author wrote to
/// compare against, not a value to replace — so the walk stops here.
///
/// By suffix rather than by a list of four names: `_logic` is the convention
/// every channel expression already follows (`validation_logic`,
/// `authorization_logic`, and the rate-limit and cache `key_logic`), so one
/// added later is covered without anyone remembering to add it.
pub fn is_expression_field(key: &str) -> bool {
    key.ends_with("_logic")
}

fn refuse_multi_key_object(logic: &serde_json::Value, field: &str) -> Result<(), String> {
    match logic {
        serde_json::Value::Object(map) if map.len() > 1 => Err(format!(
            "{field} does not compile: an object with more than one key ({}) is not an \
             expression — an operator call has exactly one",
            map.keys().cloned().collect::<Vec<_>>().join(", ")
        )),
        serde_json::Value::Object(map) => map
            .values()
            .try_for_each(|v| refuse_multi_key_object(v, field)),
        serde_json::Value::Array(items) => items
            .iter()
            .try_for_each(|v| refuse_multi_key_object(v, field)),
        _ => Ok(()),
    }
}

/// Turns stored channel rows into a [`ChannelSnapshot`].
///
/// It holds no published state — the generation it builds is handed to
/// [`crate::runtime::RuntimeHandle::publish`] alongside the engine built from
/// the same rows, and those two become visible together or not at all. What
/// stays here is what building needs and a request does not: the cluster
/// backends, the per-channel construction, and the reuse rules that make a
/// reload cost what changed.
///
/// It used to own an `ArcSwap` of the generation and a `reload_lock` of its
/// own. Both are gone: publication belongs to the runtime handle, and
/// serialising the read-modify-write belongs to `AppStateInner::reload_lock`,
/// which already spans the wider sequence (read the rows, build both halves,
/// publish). The registry's own lock only ever covered its half — which is
/// exactly why the two halves could be published a moment apart.
pub struct ChannelLoader {
    /// `Some` = cluster mode (strict backend matrix in
    /// [`ChannelLoader::resolve_backend`]: shared-Redis defaults and load
    /// errors instead of silent in-memory fallbacks).
    cluster: Option<ClusterBackends>,
}

impl Default for ChannelLoader {
    fn default() -> Self {
        Self::new()
    }
}

impl ChannelLoader {
    pub fn new() -> Self {
        Self { cluster: None }
    }

    pub fn with_cluster(cluster: ClusterBackends) -> Self {
        Self {
            cluster: Some(cluster),
        }
    }

    /// Resolve a channel's dedup or response-cache backend — the full
    /// fallback matrix, shared by both stores:
    ///
    /// |                | connector named               | no connector           |
    /// |----------------|-------------------------------|------------------------|
    /// | single node    | resolve, warn + memory on any failure | process memory |
    /// | cluster mode   | resolve; failure/memory = load error  | shared Redis (else memory) |
    async fn resolve_backend(
        &self,
        connector_registry: &ConnectorRegistry,
        cache_pool: &CachePool,
        connector: Option<&str>,
        purpose: CachePurpose,
        channel_name: &str,
    ) -> Result<Arc<dyn CacheBackend>, ChannelLoadIssue> {
        let Some(connector_name) = connector else {
            return Ok(self
                .cluster
                .as_ref()
                .and_then(|c| c.default_cache.clone())
                .unwrap_or_else(|| cache_pool.default_memory(purpose)));
        };

        let strict = self.cluster.is_some();
        let resolved = match connector_registry.get(connector_name).await {
            Some(cfg) => match cfg.as_ref() {
                ConnectorConfig::Cache(cache_cfg) => {
                    if !cache_cfg.operations.write {
                        // A dedup store and a response cache both *write*
                        // through the connector, so a write-gated one cannot
                        // back either — the gate means "nothing in Orion
                        // writes here", not "no workflow function does"
                        // (F22e). `read` is deliberately not checked: both
                        // stores only ever read back a key Orion itself
                        // wrote, so a `read: false` connector — "no workflow
                        // pulls this system's data into a payload" — is not
                        // violated by one.
                        Err(format!(
                            "connector '{connector_name}' has operations.write = false, \
                             and {purpose} writes through it"
                        ))
                    } else if strict && cache_cfg.backend == "memory" {
                        Err(format!(
                            "connector '{connector_name}' uses the in-memory backend — \
                             per-node state in cluster mode is a silent correctness loss"
                        ))
                    } else {
                        cache_pool
                            .get_backend(purpose, connector_name, cache_cfg)
                            .await
                            .map_err(|e| {
                                format!(
                                    "failed to create backend from connector '{connector_name}': {e}"
                                )
                            })
                    }
                }
                _ => Err(format!(
                    "connector '{connector_name}' is not a cache connector"
                )),
            },
            None => Err(format!("connector '{connector_name}' not found")),
        };

        match resolved {
            Ok(backend) => Ok(backend),
            Err(reason) if strict => Err(ChannelLoadIssue {
                channel: channel_name.to_string(),
                reason: format!("{purpose}: {reason}"),
            }),
            Err(reason) => {
                tracing::warn!(
                    channel = %channel_name,
                    connector = %connector_name,
                    reason = %reason,
                    "{purpose} connector unavailable, falling back to in-memory"
                );
                Ok(cache_pool.default_memory(purpose))
            }
        }
    }

    /// Build one channel's runtime config, or the [`ChannelLoadIssue`] that
    /// keeps it out of the registry.
    ///
    /// N17: `reload` used to carry this inline, which is how it reached 218
    /// lines with six `continue`s threading a shared issue list through it.
    /// Pulled out, the per-channel decision is one `Result` and the reload is a
    /// fold over it.
    ///
    /// `prior` is the outgoing generation's entry for this channel, present
    /// only when the channel was serving before. It is *not* a shortcut past
    /// this function — the caller takes that shortcut itself when the row and
    /// the dependency fingerprint are both unchanged. Here it supplies the
    /// pieces of guard **state** that must survive a genuine rebuild (N6).
    async fn build_runtime(
        &self,
        channel: &Channel,
        prior: Option<&ChannelRuntimeConfig>,
        deps: &RuntimeDeps<'_>,
    ) -> Result<Arc<ChannelRuntimeConfig>, ChannelLoadIssue> {
        let issue = |reason: String| ChannelLoadIssue {
            channel: channel.name.clone(),
            reason,
        };

        // Deployment references resolve *before* the config is typed, because
        // what they stand in for is often not a string: a `var://` TTL has to
        // arrive as a number or `ttl_secs: Option<u64>` refuses it. Resolving
        // into the JSON and parsing afterwards means every field is
        // parameterisable, not just the string ones.
        let mut raw: serde_json::Value =
            serde_json::from_str(&channel.config_json).map_err(|e| {
                tracing::error!(
                    channel = %channel.name,
                    error = %e,
                    "Refusing to load channel: config_json does not parse"
                );
                issue(format!("config_json does not parse: {e}"))
            })?;
        crate::config::vars::resolve_var_references(
            &mut raw,
            deps.vars.map(|v| v.as_ref()),
            &is_expression_field,
        )
        .map_err(|e| {
            tracing::error!(
                channel = %channel.name,
                error = %e,
                "Refusing to load channel: unresolved var reference"
            );
            issue(e)
        })?;

        // N3: `unwrap_or_default()` here used to turn one typo in the
        // stored config into a channel with no rate limit, validation,
        // dedup, backpressure, timeout, or cache — and no log line.
        let parsed_config: ChannelConfig = serde_json::from_value(raw).map_err(|e| {
            tracing::error!(
                channel = %channel.name,
                error = %e,
                "Refusing to load channel: config_json does not parse"
            );
            issue(format!("config_json does not parse: {e}"))
        })?;

        let rate_limiter: Option<Arc<dyn RateLimitBackend>> =
            parsed_config.rate_limit.as_ref().map(|rl| {
                // N6: reuse the previous limiter when its identity —
                // (rps, burst, key_logic, key_headers) — is unchanged, so
                // consumed burst and per-key state carry across the reload.
                // `on_backend_error` is deliberately not part of the
                // identity: the policy is applied at the call site, not
                // baked into the limiter.
                //
                // `key_headers` is part of it because it changes which values
                // a key can be computed from: editing the list re-dimensions
                // the buckets, so carrying the old per-key state forward would
                // credit new keys with old consumption.
                if let Some(prev) = prior
                    && let Some(prev_rl) = prev.parsed_config.rate_limit.as_ref()
                    && let Some(prev_limiter) = prev.rate_limiter.as_ref()
                    && prev_rl.requests_per_second == rl.requests_per_second
                    && prev_rl.burst == rl.burst
                    && prev_rl.key_logic == rl.key_logic
                    && prev_rl.key_headers == rl.key_headers
                {
                    return prev_limiter.clone();
                }
                let burst = rl.burst.unwrap_or(rl.requests_per_second / 2 + 1);
                match deps.cluster_redis.clone() {
                    // Cluster: shared fixed window — the configured limit
                    // holds across all replicas combined, and survives
                    // engine reloads (state lives in Redis, not here).
                    Some(conn) => Arc::new(RedisRateLimitBackend::new(
                        conn,
                        channel.name.clone(),
                        rl.requests_per_second,
                        burst,
                    )) as Arc<dyn RateLimitBackend>,
                    None => Arc::new(LocalRateLimitBackend::new(rl.requests_per_second, burst)),
                }
            });

        // N5: an uncompilable `key_logic` used to fall back to
        // `client_ip`, silently re-dimensioning the limit — a per-API-key
        // or per-tenant limit became per-IP, so every tenant behind one
        // NAT shared a bucket and one tenant got N× its quota by rotating
        // IPs. Quarantine the channel instead, like N3/N4 do for the
        // other guard-bearing config.
        let rate_limit_key_logic = parsed_config
            .rate_limit
            .as_ref()
            .and_then(|rl| rl.key_logic.as_ref())
            .map(|logic| {
                refuse_multi_key_object(logic, "rate_limit.key_logic")
                    .map_err(|e| issue(e.clone()))?;
                deps.datalogic.compile(logic).map_err(|e| {
                    tracing::error!(
                        channel = %channel.name,
                        error = %e,
                        "Refusing to load channel: rate_limit.key_logic does not compile"
                    );
                    issue(format!("rate_limit.key_logic does not compile: {e}"))
                })
            })
            .transpose()?;

        // Same call as N5 above: a response-cache key that silently fell back
        // to the whole-payload hash would serve one caller's body to another
        // whenever the key was meant to narrow on something else.
        let cache_key_logic = parsed_config
            .cache
            .as_ref()
            .and_then(|c| c.key_logic.as_ref())
            .map(|logic| {
                refuse_multi_key_object(logic, "cache.key_logic").map_err(|e| issue(e.clone()))?;
                deps.datalogic.compile(logic).map_err(|e| {
                    tracing::error!(
                        channel = %channel.name,
                        error = %e,
                        "Refusing to load channel: cache.key_logic does not compile"
                    );
                    issue(format!("cache.key_logic does not compile: {e}"))
                })
            })
            .transpose()?;

        // Lowercase once, here, so the request path compares against
        // already-normalized names. HTTP header names are case-insensitive and
        // every transport's lookup is byte-lowercase.
        let rate_limit_key_headers: Option<Arc<[String]>> = parsed_config
            .rate_limit
            .as_ref()
            .and_then(|rl| rl.key_headers.as_ref())
            .map(|names| {
                names
                    .iter()
                    .map(|n| n.trim().to_ascii_lowercase())
                    .collect::<Vec<_>>()
                    .into()
            });

        // A `key_logic` reading a header no context will ever carry resolves to
        // `null` on every request. The guard now refuses such a request rather
        // than collapsing every caller into one bucket, but a 429 at traffic
        // time is a poor way to learn about a typo — so say it once, at load,
        // while the operator is still watching the deploy. A warning and not a
        // refusal: the path may be composed dynamically in ways this cannot
        // see, and quarantining a channel over a static guess would be worse
        // than the defect.
        if let Some(logic) = parsed_config
            .rate_limit
            .as_ref()
            .and_then(|rl| rl.key_logic.as_ref())
        {
            let declared = rate_limit_key_headers.as_deref().unwrap_or(&[]);
            let unreachable: Vec<String> = super::guards::key_logic_header_paths(logic)
                .into_iter()
                .filter(|name| {
                    !super::guards::COMMON_KEY_HEADERS.contains(&name.as_str())
                        && !declared.iter().any(|d| d == name)
                })
                .collect();
            if !unreachable.is_empty() {
                tracing::warn!(
                    channel = %channel.name,
                    headers = %unreachable.join(", "),
                    "rate_limit.key_logic reads headers that are not in the key context; \
                     every request will be refused 429 until they are added to \
                     rate_limit.key_headers"
                );
            }
        }

        // N4: dropping an uncompilable expression to `None` made
        // `validate_input` a no-op, so the channel's declared input
        // contract disappeared at reload time. Refuse instead — the
        // expression compiled at create/update time, so this is a
        // corrupt row or a datalogic upgrade that changed semantics.
        let validation_logic = parsed_config
            .validation_logic
            .as_ref()
            .map(|logic| {
                refuse_multi_key_object(logic, "validation_logic").map_err(|e| issue(e.clone()))?;
                deps.datalogic.compile(logic).map_err(|e| {
                    tracing::error!(
                        channel = %channel.name,
                        error = %e,
                        "Refusing to load channel: validation_logic does not compile"
                    );
                    issue(format!("validation_logic does not compile: {e}"))
                })
            })
            .transpose()?;

        // N6: reuse the semaphore while `max_concurrent_per_node` is
        // unchanged — a fresh one would forget every in-flight permit,
        // letting a reload admit up to 2× the configured concurrency.
        let backpressure_semaphore = parsed_config.backpressure.as_ref().map(|bp| {
            if let Some(prev) = prior
                && let Some(prev_bp) = prev.parsed_config.backpressure.as_ref()
                && let Some(prev_sem) = prev.backpressure_semaphore.as_ref()
                && prev_bp.max_concurrent_per_node == bp.max_concurrent_per_node
            {
                return prev_sem.clone();
            }
            Arc::new(Semaphore::new(bp.max_concurrent_per_node))
        });

        // Dedup / response-cache backends via the shared fallback matrix
        // (see resolve_backend). A strict-mode refusal skips the channel.
        let dedup_store: Option<Arc<dyn CacheBackend>> = match parsed_config.deduplication {
            Some(ref dedup) => Some(
                self.resolve_backend(
                    deps.connector_registry,
                    deps.cache_pool,
                    dedup.connector.as_deref(),
                    CachePurpose::Dedup,
                    &channel.name,
                )
                .await?,
            ),
            None => None,
        };

        let response_cache: Option<Arc<dyn CacheBackend>> = match parsed_config.cache {
            Some(ref cache_cfg) if cache_cfg.enabled => Some(
                self.resolve_backend(
                    deps.connector_registry,
                    deps.cache_pool,
                    cache_cfg.connector.as_deref(),
                    CachePurpose::ResponseCache,
                    &channel.name,
                )
                .await?,
            ),
            _ => None,
        };

        let trace_storage = EffectiveTraceConfig::resolve(
            deps.global_trace_storage,
            parsed_config.tracing.as_ref(),
        );

        // Same posture as N3/N4/N5: a guard that cannot be built quarantines
        // the channel rather than loading it without the guard. Serving an
        // `auth`-bearing channel with its authentication silently absent —
        // because an `env://` secret was not set on this host, say — is the
        // worst possible reading of the operator's intent.
        let auth = match parsed_config.auth.as_ref() {
            Some(cfg) => Some(
                crate::channel::auth::CompiledAuth::compile(
                    cfg,
                    Some(deps.datalogic),
                    Some(deps.jwks),
                )
                .await
                .map_err(|e| {
                    tracing::error!(
                        channel = %channel.name,
                        error = %e,
                        "Refusing to load channel: auth config cannot be compiled"
                    );
                    issue(format!("auth: {e}"))
                })?,
            ),
            None => None,
        };

        // Same posture as `auth` above, and for a sharper reason: a channel
        // whose sign-in flow did not compile and was served anyway would
        // complete callbacks with the CSRF binding absent, which is the exact
        // failure the block exists to prevent.
        let oauth2_login = match parsed_config.oauth2_login.as_ref() {
            Some(cfg) => Some(Arc::new(
                crate::channel::CompiledOAuth2Login::compile(
                    cfg,
                    &channel.name,
                    &crate::channel::oauth2_login::LoginDeps {
                        http_client: deps.http_client,
                        jwks: deps.jwks,
                        allow_private_token_urls: deps.allow_private_token_urls,
                    },
                )
                .await
                .map_err(|e| {
                    tracing::error!(
                        channel = %channel.name,
                        error = %e,
                        "Refusing to load channel: oauth2_login cannot be compiled"
                    );
                    issue(format!("oauth2_login: {e}"))
                })?,
            )),
            None => None,
        };

        // Same posture again, with one addition: a cron channel on a node whose
        // scheduler is off is quarantined too. `cron.enabled = false` with an
        // active schedule is the one configuration whose failure an operator
        // cannot see — the channel would sit in every listing looking healthy
        // and simply never run — so it is refused loudly here and named on
        // `/health`, exactly as an active plugin row is on a node with the
        // sandbox off.
        let cron = match channel.protocol.as_str() {
            p if p == crate::storage::models::ChannelProtocol::Cron.as_str() => {
                if !deps.cron_enabled {
                    tracing::error!(
                        channel = %channel.name,
                        "Refusing to load channel: cron.enabled is false on this node"
                    );
                    return Err(issue(
                        "cron scheduler disabled on this node (cron.enabled = false): \
                         the schedule would never fire"
                            .to_string(),
                    ));
                }
                Some(Arc::new(
                    crate::channel::cron::compile_from_json(
                        &channel.transport_config_json,
                        crate::channel::CronIdentity {
                            channel_id: channel.channel_id.clone(),
                            channel_name: channel.name.clone(),
                            version: channel.version,
                            workflow_id: channel.workflow_id.clone(),
                        },
                    )
                    .map_err(|errors| {
                        let reasons: Vec<String> = errors
                            .iter()
                            .map(|e| format!("{}: {}", e.path, e.message))
                            .collect();
                        tracing::error!(
                            channel = %channel.name,
                            errors = %reasons.join("; "),
                            "Refusing to load channel: cron transport_config does not compile"
                        );
                        issue(format!("cron schedule: {}", reasons.join("; ")))
                    })?,
                ))
            }
            _ => None,
        };

        Ok(Arc::new(ChannelRuntimeConfig {
            channel: channel.clone(),
            parsed_config,
            rate_limiter,
            rate_limit_key_logic,
            rate_limit_key_headers,
            cache_key_logic,
            validation_logic,
            backpressure_semaphore,
            dedup_store,
            response_cache,
            trace_storage,
            auth,
            oauth2_login,
            cron,
        }))
    }

    /// Build one generation of the channel estate from the active rows.
    ///
    /// Returns it rather than publishing it: the caller pairs it with the
    /// engine built from the same rows and publishes both in a single store
    /// (`RuntimeHandle::publish`). `previous` is the outgoing generation,
    /// which is both the reuse cache and the source of guard *state* — see
    /// the cost note below.
    ///
    /// Channels that fail to load (see [`ChannelLoadIssue`] for the two
    /// classes) are **quarantined**: absent from the serving map and from the
    /// route table, and refused at every ingress by
    /// [`ChannelSnapshot::require_serviceable`]. The rest of the build
    /// succeeds. Read the resulting set with
    /// [`ChannelSnapshot::quarantined`] — that map is the one
    /// representation of it (N21).
    ///
    /// This used to be all-or-nothing — a non-empty issue list left the
    /// estate untouched and callers hard-failed. That kept a broken channel
    /// from being served unguarded, which is right, but it also meant one
    /// channel with an unparseable `config_json` failed *every* operation that
    /// triggers a reload (activate, archive, delete, rollout) with a 500, and
    /// stopped the cluster epoch watcher resyncing every node (F35). Refusing
    /// the broken channel individually keeps the guarantee and drops the
    /// blast radius to the one row that is actually broken.
    ///
    /// # Cost (N17)
    ///
    /// This runs on every admin mutation and, in cluster mode, on every epoch
    /// tick on every node — so it is written to cost what *changed*, not what
    /// exists. A channel whose row and dependency fingerprint are unchanged
    /// keeps the exact `Arc<ChannelRuntimeConfig>` it had: no JSON parse, no
    /// datalogic compilation, no backend resolution, no `Channel` clone. An
    /// unchanged serviceable set likewise keeps the built [`RouteTable`].
    pub async fn build(
        &self,
        previous: &ChannelSnapshot,
        channels: &[Channel],
        deps: ReloadDeps<'_>,
        engine_issues: Vec<ChannelLoadIssue>,
    ) -> ChannelSnapshot {
        let ReloadDeps {
            vars,
            connector_registry,
            cache_pool,
            datalogic,
            jwks,
            http_client,
            allow_private_token_urls,
            global_trace_storage,
            cron_enabled,
        } = deps;
        let deps = RuntimeDeps {
            vars,
            connector_registry,
            cache_pool,
            datalogic,
            jwks,
            http_client,
            allow_private_token_urls,
            global_trace_storage,
            cluster_redis: self.cluster.as_ref().and_then(|c| c.redis.clone()),
            cron_enabled,
        };
        let fingerprint = DepsFingerprint {
            connectors: connector_registry.config_generation(),
            trace_storage: global_trace_storage.clone(),
            cron_enabled,
        };

        // N6/N17: the outgoing generation is both the cache (an unchanged
        // channel is carried over whole) and the source of guard *state* for
        // the channels that do get rebuilt. Every admin mutation — and, in
        // cluster mode, every epoch resync on every node — runs this;
        // constructing fresh limiters and semaphores each time silently
        // refilled every channel's burst and released every in-flight
        // concurrency count, so a caller could bypass a per-channel limit by
        // causing (or just waiting for) a reload.
        let cache_valid = previous.deps == fingerprint;

        // F33: seed with the engine-build failures (workflow missing or
        // unconvertible). Those channels are quarantined exactly like ones
        // whose own config fails to load — previously they stayed in the
        // route table with no workflow behind them and served opaque engine
        // errors. The registry still runs its own checks on them below, so a
        // channel broken in both stages reports its more specific
        // config-load reason (the quarantine map is last-write-wins), and
        // engine-quarantined channels are removed from the serving map after
        // the loop.
        let mut issues: Vec<ChannelLoadIssue> = engine_issues;

        let mut by_name: HashMap<String, Arc<ChannelRuntimeConfig>> =
            HashMap::with_capacity(channels.len());
        let mut reused = 0usize;
        for channel in channels {
            let prior = previous.by_name.get(&channel.name);
            if cache_valid
                && let Some(prev) = prior
                && ChannelRow::same_row(&prev.channel, channel)
            {
                by_name.insert(channel.name.clone(), prev.clone());
                reused += 1;
                continue;
            }
            match self
                .build_runtime(channel, prior.map(|p| p.as_ref()), &deps)
                .await
            {
                Ok(runtime) => {
                    by_name.insert(channel.name.clone(), runtime);
                }
                Err(issue) => issues.push(issue),
            }
        }

        let quarantined: HashMap<String, String> = issues
            .iter()
            .map(|i| (i.channel.clone(), i.reason.clone()))
            .collect();
        // F33: a channel whose workflows failed to build must not serve even
        // when its own config loaded fine. The quarantine map is the one
        // representation of the refused set (N21), so the serving map is
        // filtered against it rather than against a second copy of the names.
        by_name.retain(|name, _| !quarantined.contains_key(name));

        // Quarantined channels are excluded from the route table too, so
        // their REST routes 404 rather than resolving to a channel that will
        // then be refused — and so a broken channel cannot shadow the route
        // of a working one.
        let serviceable: Vec<&Channel> = channels
            .iter()
            .filter(|c| !quarantined.contains_key(&c.name))
            .collect();
        let route_key: Vec<ChannelRow> = serviceable.iter().copied().map(ChannelRow::of).collect();
        let route_table = if route_key == previous.route_key {
            previous.route_table.clone()
        } else {
            Arc::new(RouteTable::build(serviceable.iter().copied()))
        };

        // Counted off the published maps, not off `issues`: a channel broken
        // in both the engine build and its own config contributes two entries
        // to that `Vec` and one to the map, and `channels.len() - issues.len()`
        // could therefore underflow.
        let loaded = by_name.len();
        let refused = quarantined.len();

        let snapshot = ChannelSnapshot {
            by_name,
            route_table,
            quarantined,
            route_key,
            deps: fingerprint,
        };

        if refused > 0 {
            tracing::error!(
                quarantined = refused,
                loaded,
                "Some channels failed to load and are being refused at every \
                 ingress. See /health for the list; the rest of the instance \
                 is unaffected."
            );
        }
        tracing::debug!(
            channels = channels.len(),
            reused,
            "Channel generation built"
        );
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::test_support::StubConnectorRepo;
    use arc_swap::ArcSwap;

    /// A [`ChannelLoader`] plus the generation it last built.
    ///
    /// In production those two live apart: the loader is a component of the
    /// node and the generation is published — with the engine — through
    /// `RuntimeHandle`. These tests are about the loader alone, and several of
    /// them (N6, N17) are specifically about state carried *across*
    /// generations, so the outgoing one has to be kept somewhere between
    /// calls. `ArcSwap` rather than a field because
    /// `test_reader_never_sees_a_half_applied_quarantine` reads it from
    /// another task while a build publishes.
    struct TestRegistry {
        loader: ChannelLoader,
        current: ArcSwap<ChannelSnapshot>,
    }

    impl TestRegistry {
        fn new() -> Self {
            Self::wrap(ChannelLoader::new())
        }

        fn with_cluster(cluster: ClusterBackends) -> Self {
            Self::wrap(ChannelLoader::with_cluster(cluster))
        }

        fn wrap(loader: ChannelLoader) -> Self {
            Self {
                loader,
                current: ArcSwap::from_pointee(ChannelSnapshot::empty()),
            }
        }

        fn snapshot(&self) -> Arc<ChannelSnapshot> {
            self.current.load_full()
        }

        /// Build the next generation from the current one and publish it —
        /// the pair of steps `runtime::reload` performs around the engine.
        async fn build(
            &self,
            channels: &[Channel],
            deps: ReloadDeps<'_>,
            engine_issues: Vec<ChannelLoadIssue>,
        ) {
            let next = self
                .loader
                .build(&self.snapshot(), channels, deps, engine_issues)
                .await;
            self.current.store(Arc::new(next));
        }

        fn get_by_name(&self, name: &str) -> Option<Arc<ChannelRuntimeConfig>> {
            self.snapshot().get_by_name(name)
        }

        fn quarantined(&self) -> Vec<ChannelLoadIssue> {
            self.snapshot().quarantined()
        }

        fn quarantine_reason(&self, name: &str) -> Option<String> {
            self.snapshot().quarantine_reason(name)
        }

        fn require_serviceable(
            &self,
            name: &str,
        ) -> Result<Option<Arc<ChannelRuntimeConfig>>, crate::errors::OrionError> {
            self.snapshot().require_serviceable(name)
        }

        fn match_route(
            &self,
            method: &str,
            path: &str,
        ) -> Result<Option<RouteMatch>, crate::errors::OrionError> {
            self.snapshot().match_route(method, path)
        }
    }

    /// A cache connector a channel can name for its dedup store without any
    /// external service behind it.
    const MEMORY_CACHE: &str = r#"{"backend":"memory"}"#;

    #[tokio::test]
    async fn test_channel_registry_empty() {
        let registry = TestRegistry::new();
        assert!(registry.get_by_name("nonexistent").is_none());
        assert!(registry.snapshot().by_name.is_empty());
    }

    fn test_channel(name: &str, config_json: &str) -> Channel {
        let now = chrono::Utc::now().naive_utc();
        Channel {
            tags_json: "[]".to_string(),
            channel_id: format!("ch_{name}"),
            version: 1,
            name: name.to_string(),
            description: None,
            channel_type: "sync".to_string(),
            protocol: "http".to_string(),
            methods_json: None,
            route_pattern: None,
            topic: None,
            consumer_group: None,
            transport_config_json: "{}".to_string(),
            workflow_id: None,
            config_json: config_json.to_string(),
            status: "active".to_string(),
            priority: 0,
            created_at: now,
            updated_at: now,
        }
    }

    /// A cron channel carrying `transport_config`, so it compiles a descriptor
    /// and registers nowhere else.
    fn cron_channel(name: &str, transport_config_json: &str) -> Channel {
        Channel {
            protocol: "cron".to_string(),
            channel_type: "async".to_string(),
            transport_config_json: transport_config_json.to_string(),
            workflow_id: Some("wf".to_string()),
            ..test_channel(name, "{}")
        }
    }

    #[tokio::test]
    async fn a_cron_channel_compiles_a_descriptor_and_claims_no_route() {
        let registry = TestRegistry::new();
        let channel = cron_channel(
            "nightly",
            r#"{"schedule": "0 15 2 * * *", "timezone": "Asia/Kolkata"}"#,
        );
        TestDeps::new().reload(&registry, &[channel]).await;
        assert!(registry.quarantined().is_empty());

        let runtime = registry.get_by_name("nightly").expect("loaded");
        let cron = runtime.cron.as_ref().expect("a compiled descriptor");
        assert_eq!(cron.timezone, chrono_tz::Tz::Asia__Kolkata);
        assert_eq!(cron.channel_id, "ch_nightly");
        // The singleton key defaults to the stable id, not the name.
        assert_eq!(cron.singleton_key, "ch_nightly");

        let snapshot = registry.snapshot();
        assert!(snapshot.has_cron_channels());
        assert_eq!(snapshot.cron_descriptors().len(), 1);
        assert!(snapshot.cron_by_channel_id("ch_nightly").is_some());
        // Registers no route: a cron channel is started by its schedule, and a
        // route table entry would make it reachable by request.
        assert!(
            matches!(snapshot.match_route("POST", "/nightly"), Ok(None)),
            "a cron channel must not enter the route table"
        );
    }

    /// N3's posture applied to the schedule: a `transport_config` that no longer
    /// compiles refuses the channel rather than loading it with the schedule
    /// silently absent — which, for this protocol, would be a channel that
    /// simply never runs.
    #[tokio::test]
    async fn a_schedule_that_does_not_compile_quarantines_its_channel() {
        let registry = TestRegistry::new();
        let channel = cron_channel("broken", r#"{"schedule": "not a cron expression"}"#);
        TestDeps::new().reload(&registry, &[channel]).await;

        let issues = registry.quarantined();
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(
            issues[0].reason.contains("cron schedule"),
            "the reason must name the schedule: {}",
            issues[0].reason
        );
        assert!(registry.get_by_name("broken").is_none());
        assert!(registry.snapshot().cron_descriptors().is_empty());
    }

    /// D3: the one configuration whose failure an operator cannot otherwise
    /// see. The channel is refused loudly rather than sitting in every listing
    /// looking active and never firing.
    #[tokio::test]
    async fn a_cron_channel_is_quarantined_when_the_scheduler_is_off() {
        let registry = TestRegistry::new();
        let channel = cron_channel("nightly", r#"{"schedule": "0 15 2 * * *"}"#);
        let mut deps = TestDeps::new();
        deps.cron_enabled = false;
        deps.reload(&registry, &[channel]).await;

        let issues = registry.quarantined();
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(
            issues[0].reason.contains("cron.enabled = false"),
            "the reason must name the setting: {}",
            issues[0].reason
        );
    }

    /// The flag is part of the dependency fingerprint, so flipping it rebuilds
    /// rather than carrying over descriptors compiled under the old answer.
    #[tokio::test]
    async fn turning_the_scheduler_off_does_not_reuse_a_compiled_descriptor() {
        let registry = TestRegistry::new();
        let channel = cron_channel("nightly", r#"{"schedule": "0 15 2 * * *"}"#);

        TestDeps::new()
            .reload(&registry, std::slice::from_ref(&channel))
            .await;
        assert!(registry.get_by_name("nightly").is_some());

        let mut off = TestDeps::new();
        off.cron_enabled = false;
        off.reload(&registry, &[channel]).await;
        assert!(
            registry.get_by_name("nightly").is_none(),
            "a carried-over descriptor would keep a schedule alive that this node \
             can no longer run"
        );
    }

    /// A REST channel that claims `route_pattern`, so it lands in the route
    /// table as well as the serving map.
    fn rest_channel(name: &str, route_pattern: &str) -> Channel {
        Channel {
            protocol: "rest".to_string(),
            route_pattern: Some(route_pattern.to_string()),
            ..test_channel(name, "{}")
        }
    }

    /// The dependencies a reload resolves against, held for a whole test.
    ///
    /// N17 keys the per-channel runtime cache on the connector token and the
    /// global trace-storage config, so reuse only happens while these are the
    /// same handles — which is what a real process looks like: both are built
    /// once at boot and outlive every reload.
    struct TestDeps {
        connectors: ConnectorRegistry,
        cache_pool: CachePool,
        datalogic: DatalogicEngine,
        jwks: std::sync::Arc<crate::jwt::jwks::JwksCache>,
        http_client: reqwest::Client,
        trace_storage: TraceStorageConfig,
        /// `[cron] enabled` as this harness's node sees it. Default `true`;
        /// the D3 quarantine test flips it.
        cron_enabled: bool,
    }

    impl TestDeps {
        fn new() -> Self {
            Self {
                connectors: ConnectorRegistry::new(
                    crate::config::EngineConfig::default().circuit_breaker,
                ),
                cache_pool: CachePool::new(4, 60, 1000),
                datalogic: DatalogicEngine::new(),
                jwks: test_jwks(),
                http_client: reqwest::Client::new(),
                trace_storage: TraceStorageConfig::default(),
                cron_enabled: true,
            }
        }

        async fn reload(&self, registry: &TestRegistry, channels: &[Channel]) {
            self.reload_with_issues(registry, channels, Vec::new())
                .await;
        }

        /// A reload that also carries engine-build failures — what
        /// `reload_engine` passes when `build_engine_workflows` could not
        /// give a channel its workflow (F33).
        async fn reload_with_issues(
            &self,
            registry: &TestRegistry,
            channels: &[Channel],
            engine_issues: Vec<ChannelLoadIssue>,
        ) {
            registry
                .build(
                    channels,
                    ReloadDeps {
                        vars: None,
                        connector_registry: &self.connectors,
                        cache_pool: &self.cache_pool,
                        datalogic: &self.datalogic,
                        jwks: &self.jwks,
                        http_client: &self.http_client,
                        allow_private_token_urls: false,
                        global_trace_storage: &self.trace_storage,
                        cron_enabled: self.cron_enabled,
                    },
                    engine_issues,
                )
                .await;
        }
    }

    /// A JWKS cache for tests: no channel config here sets `auth.jwks_url`,
    /// so nothing is ever fetched through it.
    fn test_jwks() -> std::sync::Arc<crate::jwt::jwks::JwksCache> {
        std::sync::Arc::new(crate::jwt::jwks::JwksCache::new(
            reqwest::Client::new(),
            false,
        ))
    }

    fn cluster_backends() -> ClusterBackends {
        ClusterBackends {
            // Stands in for the shared cluster Redis, which serves both
            // internal purposes (their keys are structurally prefixed).
            default_cache: Some(CachePool::new(4, 60, 1000).default_memory(CachePurpose::Dedup)),
            redis: None,
        }
    }

    #[tokio::test]
    async fn test_cluster_strict_mode_refuses_broken_dedup_connector() {
        let registry = TestRegistry::with_cluster(cluster_backends());
        let channel = test_channel(
            "strict-ch",
            r#"{"deduplication": {"header": "idem", "connector": "missing-connector"}}"#,
        );
        TestDeps::new().reload(&registry, &[channel]).await;
        let issues = registry.quarantined();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].reason.contains("missing-connector"));
        // The channel must NOT be served with a silent per-node fallback.
        assert!(registry.get_by_name("strict-ch").is_none());
    }

    #[tokio::test]
    async fn test_cluster_mode_defaults_dedup_to_shared_cache() {
        let registry = TestRegistry::with_cluster(cluster_backends());
        let channel = test_channel("shared-ch", r#"{"deduplication": {"header": "idem"}}"#);
        TestDeps::new().reload(&registry, &[channel]).await;
        assert!(registry.quarantined().is_empty());
        let runtime = registry.get_by_name("shared-ch").expect("loaded");
        assert!(runtime.dedup_store.is_some());
    }

    /// A connector registry holding one memory-backed cache connector with
    /// its `write` gate set either way (F22e).
    async fn registry_with_cache(name: &str, write: bool) -> ConnectorRegistry {
        let registry =
            ConnectorRegistry::new(crate::config::EngineConfig::default().circuit_breaker);
        let config: crate::connector::ConnectorConfig = serde_json::from_value(serde_json::json!({
            "type": "cache",
            "backend": "memory",
            "operations": { "write": write }
        }))
        .expect("a cache connector config");
        registry.insert_for_test(name, config).await;
        registry
    }

    /// F22e: a dedup store *writes* through its connector, so a cache
    /// connector gated `write: false` cannot back one. Without this the gate
    /// would cover `cache_write` only, and a channel pointing its dedup store
    /// at a shared Redis would keep writing to it — which is exactly what the
    /// gate exists to prevent.
    #[tokio::test]
    async fn test_write_gated_connector_cannot_back_a_dedup_store() {
        let registry = TestRegistry::with_cluster(cluster_backends());
        let channel = test_channel(
            "gated-dedup-ch",
            r#"{"deduplication": {"header": "idem", "connector": "ro-cache"}}"#,
        );
        registry
            .build(
                &[channel],
                ReloadDeps {
                    vars: None,
                    connector_registry: &registry_with_cache("ro-cache", false).await,
                    cache_pool: &CachePool::new(4, 60, 1000),
                    datalogic: &DatalogicEngine::new(),
                    jwks: &test_jwks(),
                    http_client: &reqwest::Client::new(),
                    allow_private_token_urls: false,
                    global_trace_storage: &TraceStorageConfig::default(),
                    cron_enabled: true,
                },
                Vec::new(),
            )
            .await;
        // N21: the build no longer returns the issues it also records.
        let issues = registry.quarantined();
        assert_eq!(issues.len(), 1, "issues = {issues:?}");
        assert!(
            issues[0].reason.contains("operations.write"),
            "the reason must name the gate, got: {}",
            issues[0].reason
        );
        assert!(registry.get_by_name("gated-dedup-ch").is_none());
    }

    /// The control: the same connector with the gate open is accepted, so the
    /// refusal above is the gate and not the connector.
    #[tokio::test]
    async fn test_ungated_connector_still_backs_a_dedup_store() {
        let registry = TestRegistry::new();
        let channel = test_channel(
            "open-dedup-ch",
            r#"{"deduplication": {"header": "idem", "connector": "rw-cache"}}"#,
        );
        registry
            .build(
                &[channel],
                ReloadDeps {
                    vars: None,
                    connector_registry: &registry_with_cache("rw-cache", true).await,
                    cache_pool: &CachePool::new(4, 60, 1000),
                    datalogic: &DatalogicEngine::new(),
                    jwks: &test_jwks(),
                    http_client: &reqwest::Client::new(),
                    allow_private_token_urls: false,
                    global_trace_storage: &TraceStorageConfig::default(),
                    cron_enabled: true,
                },
                Vec::new(),
            )
            .await;
        // N21: the build no longer returns the issues it also records.
        let issues = registry.quarantined();
        assert!(issues.is_empty(), "issues = {issues:?}");
        let runtime = registry.get_by_name("open-dedup-ch").expect("loaded");
        assert!(runtime.dedup_store.is_some());
    }

    /// On a single node a write-gated connector degrades the way an
    /// unreachable one does — a warning and the in-memory store — rather than
    /// taking the channel out of service. What it must not do is keep writing
    /// through the gated connector.
    #[tokio::test]
    async fn test_single_node_write_gated_connector_falls_back() {
        let registry = TestRegistry::new();
        let channel = test_channel(
            "gated-fallback-ch",
            r#"{"deduplication": {"header": "idem", "connector": "ro-cache"}}"#,
        );
        registry
            .build(
                &[channel],
                ReloadDeps {
                    vars: None,
                    connector_registry: &registry_with_cache("ro-cache", false).await,
                    cache_pool: &CachePool::new(4, 60, 1000),
                    datalogic: &DatalogicEngine::new(),
                    jwks: &test_jwks(),
                    http_client: &reqwest::Client::new(),
                    allow_private_token_urls: false,
                    global_trace_storage: &TraceStorageConfig::default(),
                    cron_enabled: true,
                },
                Vec::new(),
            )
            .await;
        // N21: the build no longer returns the issues it also records.
        let issues = registry.quarantined();
        assert!(issues.is_empty(), "issues = {issues:?}");
        assert!(registry.get_by_name("gated-fallback-ch").is_some());
    }

    #[tokio::test]
    async fn test_single_node_broken_connector_still_falls_back() {
        let registry = TestRegistry::new();
        let channel = test_channel(
            "fallback-ch",
            r#"{"deduplication": {"header": "idem", "connector": "missing-connector"}}"#,
        );
        TestDeps::new().reload(&registry, &[channel]).await;
        // Backend *degradation* stays cluster-only: on one node an in-memory
        // dedup store is the documented fallback, not a correctness loss.
        assert!(registry.quarantined().is_empty());
        assert!(registry.get_by_name("fallback-ch").is_some());
    }

    async fn reload_single_node(channel: Channel) -> (TestRegistry, Vec<ChannelLoadIssue>) {
        let registry = TestRegistry::new();
        let issues = reload_into(&registry, channel).await;
        (registry, issues)
    }

    /// Reload `channel` into an existing registry and hand back the quarantine
    /// set — for the N6 tests, which exercise state carried *across* reloads of
    /// one registry.
    ///
    /// Deliberately builds fresh dependency handles per call, which stands for
    /// a reload where the connectors changed too. That invalidates N17's
    /// whole-config cache, so these tests land on the rebuild path — the one
    /// where N6's field-level reuse is what preserves guard state.
    async fn reload_into(registry: &TestRegistry, channel: Channel) -> Vec<ChannelLoadIssue> {
        TestDeps::new().reload(registry, &[channel]).await;
        registry.quarantined()
    }

    /// N3: a stored config that does not parse must not load as "no guards".
    #[tokio::test]
    async fn test_malformed_config_json_refuses_to_load_single_node() {
        // `requests_per_second` typed as a string — passes create-time
        // validation nowhere, but this is what a hand-edited row looks like.
        let channel = test_channel(
            "broken-cfg-ch",
            r#"{"rate_limit": {"requests_per_second": "100"}}"#,
        );
        let (registry, issues) = reload_single_node(channel).await;
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].channel, "broken-cfg-ch");
        assert!(
            issues[0].reason.contains("config_json does not parse"),
            "unexpected reason: {}",
            issues[0].reason
        );
        assert!(registry.get_by_name("broken-cfg-ch").is_none());
    }

    #[tokio::test]
    async fn test_malformed_config_json_is_not_valid_json_refuses() {
        let channel = test_channel("not-json-ch", "{ this is not json ");
        let (registry, issues) = reload_single_node(channel).await;
        assert_eq!(issues.len(), 1);
        assert!(registry.get_by_name("not-json-ch").is_none());
    }

    /// N4: an uncompilable `validation_logic` must not silently disable
    /// input validation.
    #[tokio::test]
    async fn test_uncompilable_validation_logic_refuses_to_load_single_node() {
        // Multi-key object: valid JSON, rejected by the datalogic compiler
        // outside templating mode.
        let channel = test_channel(
            "bad-logic-ch",
            r#"{"validation_logic": {"==": [1, 1], "!=": [1, 2]}}"#,
        );
        let (registry, issues) = reload_single_node(channel).await;
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].channel, "bad-logic-ch");
        assert!(
            issues[0]
                .reason
                .contains("validation_logic does not compile"),
            "unexpected reason: {}",
            issues[0].reason
        );
        assert!(registry.get_by_name("bad-logic-ch").is_none());
    }

    #[tokio::test]
    async fn test_valid_validation_logic_still_loads() {
        let channel = test_channel(
            "good-logic-ch",
            r#"{"validation_logic": {"!!": [{"var": "data.id"}]}}"#,
        );
        let (registry, issues) = reload_single_node(channel).await;
        assert!(issues.is_empty(), "unexpected issues: {issues:?}");
        let runtime = registry.get_by_name("good-logic-ch").expect("loaded");
        assert!(runtime.validation_logic.is_some());
    }

    /// A refusal must not partially apply: the previously loaded channels
    /// keep serving with their guards rather than being dropped.
    #[tokio::test]
    async fn test_refusal_leaves_previous_registry_intact() {
        let registry = TestRegistry::new();
        let deps = TestDeps::new();

        deps.reload(&registry, &[test_channel("keep-ch", "{}")])
            .await;
        assert!(registry.quarantined().is_empty());

        deps.reload(
            &registry,
            &[
                test_channel("keep-ch", "{}"),
                test_channel("broken-ch", "{ nope"),
            ],
        )
        .await;
        assert_eq!(registry.quarantined().len(), 1);
        assert!(registry.get_by_name("keep-ch").is_some());
        assert!(registry.get_by_name("broken-ch").is_none());
    }

    // ---- Trace policy: EffectiveTraceConfig::resolve + should_drop --------

    fn global_tracing(
        mode: TraceStorageMode,
        sample_rate: f64,
        errors_only: bool,
    ) -> TraceStorageConfig {
        TraceStorageConfig {
            mode,
            sample_rate,
            errors_only,
            ..Default::default()
        }
    }

    /// Channel-over-global precedence, field by field: every Some on the
    /// channel override wins; every None falls through to the global value;
    /// task_details has no global and defaults to false.
    #[test]
    fn effective_trace_config_overlays_channel_fields_over_global() {
        let global = global_tracing(TraceStorageMode::Sync, 0.5, false);

        // No channel override at all → globals verbatim, task_details off.
        let eff = EffectiveTraceConfig::resolve(&global, None);
        assert!(matches!(eff.mode, TraceStorageMode::Sync));
        assert_eq!(eff.sample_rate, 0.5);
        assert!(!eff.errors_only);
        assert!(!eff.task_details);

        // Channel override with every field set → all channel values.
        let channel = super::super::config::ChannelTracingConfig {
            mode: Some(TraceStorageMode::Off),
            sample_rate: Some(0.1),
            errors_only: Some(true),
            task_details: Some(true),
        };
        let eff = EffectiveTraceConfig::resolve(&global, Some(&channel));
        assert!(matches!(eff.mode, TraceStorageMode::Off));
        assert_eq!(eff.sample_rate, 0.1);
        assert!(eff.errors_only);
        assert!(eff.task_details);

        // Channel override with every field None → globals fall through.
        let channel = super::super::config::ChannelTracingConfig {
            mode: None,
            sample_rate: None,
            errors_only: None,
            task_details: None,
        };
        let eff = EffectiveTraceConfig::resolve(&global, Some(&channel));
        assert!(matches!(eff.mode, TraceStorageMode::Sync));
        assert_eq!(eff.sample_rate, 0.5);
        assert!(!eff.errors_only);
        assert!(!eff.task_details, "task_details has no global to inherit");
    }

    /// Drop-filter precedence: Off beats everything; errors_only spares
    /// error traces; the injected sampling outcome decides last. Fully
    /// deterministic now — N22 moved the coin into [`draw_sample`], so the
    /// filter can be exercised at every combination, not just the extremes.
    #[test]
    fn should_drop_filters_in_precedence_order() {
        let off =
            EffectiveTraceConfig::resolve(&global_tracing(TraceStorageMode::Off, 1.0, false), None);
        assert_eq!(
            off.should_drop(true, true),
            Some("off"),
            "Off wins even for errors"
        );
        assert_eq!(
            off.should_drop(false, false),
            Some("off"),
            "Off outranks sampled_out"
        );

        let errors_only =
            EffectiveTraceConfig::resolve(&global_tracing(TraceStorageMode::Sync, 1.0, true), None);
        assert_eq!(errors_only.should_drop(false, true), Some("errors_only"));
        assert_eq!(
            errors_only.should_drop(true, true),
            None,
            "error traces are spared"
        );

        let sync = EffectiveTraceConfig::resolve(
            &global_tracing(TraceStorageMode::Sync, 0.5, false),
            None,
        );
        assert_eq!(
            sync.should_drop(false, false),
            Some("sampled_out"),
            "a sampled-out trace is dropped"
        );
        assert_eq!(sync.should_drop(false, true), None);
        assert_eq!(sync.should_drop(true, true), None);
    }

    /// N22: the coin itself is deterministic at the extremes, and it is the
    /// only non-deterministic input to the drop filter.
    #[test]
    fn draw_sample_is_deterministic_at_the_extremes() {
        let never = EffectiveTraceConfig::resolve(
            &global_tracing(TraceStorageMode::Sync, 0.0, false),
            None,
        );
        let always = EffectiveTraceConfig::resolve(
            &global_tracing(TraceStorageMode::Sync, 1.0, false),
            None,
        );
        for _ in 0..64 {
            assert!(!never.draw_sample(), "rate 0.0 must never sample in");
            assert!(always.draw_sample(), "rate 1.0 must always sample in");
        }
    }

    /// N22 + R11: an async submission's trace row is the result-delivery
    /// mechanism, so `for_async_submission` pins the sample rate to 1.0
    /// (async traces are never sampled out) exactly as it upgrades
    /// `Off` → `Sync`. `errors_only` keeps its documented result-drop.
    #[test]
    fn for_async_submission_never_samples_out() {
        let eff =
            EffectiveTraceConfig::resolve(&global_tracing(TraceStorageMode::Off, 0.0, true), None)
                .for_async_submission();
        assert!(matches!(eff.mode, TraceStorageMode::Sync));
        assert_eq!(eff.sample_rate, 1.0);
        assert!(eff.draw_sample(), "pinned rate must never enter the roll");
        assert!(eff.errors_only, "errors_only is deliberately left alone");
    }

    // ---- N6: guard state survives a reload -------------------------------

    /// N6: consume a channel's burst, reload with the channel unchanged, and
    /// the next request must still be limited — a fresh limiter per reload
    /// silently refilled every bucket on every admin mutation.
    #[tokio::test]
    async fn test_reload_preserves_rate_limiter_state_when_unchanged() {
        let registry = TestRegistry::new();
        let config = r#"{"rate_limit": {"requests_per_second": 1, "burst": 2}}"#;
        let issues = reload_into(&registry, test_channel("rl-ch", config)).await;
        assert!(issues.is_empty(), "{issues:?}");

        let limiter = registry
            .get_by_name("rl-ch")
            .expect("loaded")
            .rate_limiter
            .clone()
            .expect("limiter configured");
        assert!(limiter.check("k".to_string()).await.expect("test"));
        assert!(limiter.check("k".to_string()).await.expect("test"));
        assert!(
            !limiter.check("k".to_string()).await.expect("test"),
            "burst of 2 must be consumed"
        );

        // Reload with the identical channel — an admin mutation elsewhere.
        let issues = reload_into(&registry, test_channel("rl-ch", config)).await;
        assert!(issues.is_empty(), "{issues:?}");
        let limiter = registry
            .get_by_name("rl-ch")
            .expect("loaded")
            .rate_limiter
            .clone()
            .expect("limiter configured");
        assert!(
            !limiter.check("k".to_string()).await.expect("test"),
            "reload must not refill the consumed burst"
        );
    }

    /// #275: `key_headers` is part of the limiter's identity too. Editing it
    /// changes which values a key can be computed from, so the buckets are
    /// keyed differently and the old per-key state must not carry over —
    /// otherwise a new key inherits an old key's consumption.
    ///
    /// This is the omission that would ship silently: every other test in this
    /// file still passes with `key_headers` missing from the reuse tuple.
    #[tokio::test]
    async fn test_reload_rebuilds_limiter_when_key_headers_change() {
        let registry = TestRegistry::new();
        let before = r#"{"rate_limit": {"requests_per_second": 1, "burst": 2, "key_headers": ["deviceid"]}}"#;
        let issues = reload_into(&registry, test_channel("kh-ch", before)).await;
        assert!(issues.is_empty(), "{issues:?}");

        let limiter = registry
            .get_by_name("kh-ch")
            .expect("loaded")
            .rate_limiter
            .clone()
            .expect("limiter");
        while limiter.check("k".to_string()).await.expect("test") {}

        // An identical reload keeps the spent bucket...
        let _ = reload_into(&registry, test_channel("kh-ch", before)).await;
        let limiter = registry
            .get_by_name("kh-ch")
            .expect("loaded")
            .rate_limiter
            .clone()
            .expect("limiter");
        assert!(
            !limiter.check("k".to_string()).await.expect("test"),
            "an unchanged key_headers list must reuse the limiter"
        );

        // ...while editing the declared list rebuilds it.
        let after = r#"{"rate_limit": {"requests_per_second": 1, "burst": 2, "key_headers": ["x-client-id"]}}"#;
        let _ = reload_into(&registry, test_channel("kh-ch", after)).await;
        let limiter = registry
            .get_by_name("kh-ch")
            .expect("loaded")
            .rate_limiter
            .clone()
            .expect("limiter");
        assert!(
            limiter.check("k".to_string()).await.expect("test"),
            "editing key_headers re-dimensions the buckets, so the limiter must be fresh"
        );
    }

    /// #275: the declared list is lowercased once at load, so the request path
    /// never normalizes a header name per request.
    #[tokio::test]
    async fn test_key_headers_are_lowercased_at_load() {
        let registry = TestRegistry::new();
        let issues = reload_into(
            &registry,
            test_channel(
                "kh-case-ch",
                r#"{"rate_limit": {"requests_per_second": 1, "key_headers": ["DeviceId", "X-Partner"]}}"#,
            ),
        )
        .await;
        assert!(issues.is_empty(), "{issues:?}");
        let runtime = registry.get_by_name("kh-case-ch").expect("loaded");
        assert_eq!(
            runtime
                .rate_limit_key_headers
                .as_deref()
                .expect("declared headers"),
            ["deviceid".to_string(), "x-partner".to_string()]
        );
    }

    /// #275: a `key_logic` naming a header outside the key context is a
    /// warning, never a quarantine — the static walk cannot see a composed
    /// path, so refusing on it would be worse than the defect it reports.
    #[tokio::test]
    async fn test_unreachable_key_logic_header_still_loads() {
        let registry = TestRegistry::new();
        let issues = reload_into(
            &registry,
            test_channel(
                "kh-warn-ch",
                r#"{"rate_limit": {"requests_per_second": 1, "key_logic": {"var": "headers.deviceid"}}}"#,
            ),
        )
        .await;
        assert!(
            issues.is_empty(),
            "an unreachable header must not quarantine the channel: {issues:?}"
        );
        assert!(registry.get_by_name("kh-warn-ch").is_some());
    }

    /// The counterpart: changed limits get a fresh limiter with the new
    /// shape — reuse only applies while (rps, burst, key_logic, key_headers)
    /// hold.
    #[tokio::test]
    async fn test_reload_rebuilds_limiter_when_limits_change() {
        let registry = TestRegistry::new();
        let _ = reload_into(
            &registry,
            test_channel(
                "rl-ch",
                r#"{"rate_limit": {"requests_per_second": 1, "burst": 2}}"#,
            ),
        )
        .await;
        let limiter = registry
            .get_by_name("rl-ch")
            .expect("loaded")
            .rate_limiter
            .clone()
            .expect("limiter");
        while limiter.check("k".to_string()).await.expect("test") {}

        // Raise the burst: the operator's new limit must apply immediately.
        let _ = reload_into(
            &registry,
            test_channel(
                "rl-ch",
                r#"{"rate_limit": {"requests_per_second": 1, "burst": 10}}"#,
            ),
        )
        .await;
        let limiter = registry
            .get_by_name("rl-ch")
            .expect("loaded")
            .rate_limiter
            .clone()
            .expect("limiter");
        assert!(
            limiter.check("k".to_string()).await.expect("test"),
            "a changed limit must take effect on reload"
        );
    }

    /// N6: the backpressure semaphore is the same object across reloads while
    /// `max_concurrent_per_node` is unchanged — otherwise every reload
    /// forgot the in-flight permits — and a fresh one when it changes.
    #[tokio::test]
    async fn test_reload_reuses_backpressure_semaphore_when_unchanged() {
        let registry = TestRegistry::new();
        let config = r#"{"backpressure": {"max_concurrent_per_node": 3}}"#;
        let _ = reload_into(&registry, test_channel("bp-ch", config)).await;
        let sem = registry
            .get_by_name("bp-ch")
            .expect("loaded")
            .backpressure_semaphore
            .clone()
            .expect("semaphore");

        let _ = reload_into(&registry, test_channel("bp-ch", config)).await;
        let sem_after = registry
            .get_by_name("bp-ch")
            .expect("loaded")
            .backpressure_semaphore
            .clone()
            .expect("semaphore");
        assert!(
            Arc::ptr_eq(&sem, &sem_after),
            "unchanged max_concurrent_per_node must keep the same semaphore"
        );

        let _ = reload_into(
            &registry,
            test_channel(
                "bp-ch",
                r#"{"backpressure": {"max_concurrent_per_node": 5}}"#,
            ),
        )
        .await;
        let sem_changed = registry
            .get_by_name("bp-ch")
            .expect("loaded")
            .backpressure_semaphore
            .clone()
            .expect("semaphore");
        assert!(
            !Arc::ptr_eq(&sem, &sem_changed),
            "a changed limit needs a fresh semaphore"
        );
        assert_eq!(sem_changed.available_permits(), 5);
    }

    /// Reloading against a registry with in-flight permits keeps the count:
    /// permits held on the old generation still bound the new one.
    #[tokio::test]
    async fn test_reload_keeps_in_flight_backpressure_permits() {
        let registry = TestRegistry::new();
        let config = r#"{"backpressure": {"max_concurrent_per_node": 2}}"#;
        let _ = reload_into(&registry, test_channel("bp-ch", config)).await;
        let sem = registry
            .get_by_name("bp-ch")
            .expect("loaded")
            .backpressure_semaphore
            .clone()
            .expect("semaphore");
        let _held = sem.clone().try_acquire_owned().expect("permit");

        let _ = reload_into(&registry, test_channel("bp-ch", config)).await;
        let sem_after = registry
            .get_by_name("bp-ch")
            .expect("loaded")
            .backpressure_semaphore
            .clone()
            .expect("semaphore");
        assert_eq!(
            sem_after.available_permits(),
            1,
            "the in-flight permit must survive the reload"
        );
    }

    // ---- N17: rebuild only what changed ----------------------------------

    /// The whole point: an unchanged channel is *carried over*, not rebuilt.
    /// Pointer equality is the observable form of "no JSON parse, no datalogic
    /// compilation, no backend resolution, no `Channel` clone" — the work that
    /// used to run for every channel on every reload, on every node, on every
    /// epoch tick.
    #[tokio::test]
    async fn test_reload_reuses_runtime_config_when_nothing_changed() {
        let registry = TestRegistry::new();
        let deps = TestDeps::new();
        let channel = test_channel(
            "reuse-ch",
            r#"{"validation_logic": {"!!": [{"var": "data.id"}]},
                "rate_limit": {"requests_per_second": 10},
                "deduplication": {"header": "idem"}}"#,
        );

        deps.reload(&registry, std::slice::from_ref(&channel)).await;
        let first = registry.get_by_name("reuse-ch").expect("loaded");

        deps.reload(&registry, std::slice::from_ref(&channel)).await;
        let second = registry.get_by_name("reuse-ch").expect("loaded");

        assert!(
            Arc::ptr_eq(&first, &second),
            "an unchanged channel must keep its runtime config across a reload"
        );
    }

    /// The counterpart: an edited row is a new row, and gets a fresh runtime
    /// config. `updated_at` alone is enough — it is what moves when the stored
    /// bytes change.
    #[tokio::test]
    async fn test_reload_rebuilds_runtime_config_when_the_row_changes() {
        let registry = TestRegistry::new();
        let deps = TestDeps::new();
        let channel = test_channel("edit-ch", r#"{"rate_limit": {"requests_per_second": 10}}"#);

        deps.reload(&registry, std::slice::from_ref(&channel)).await;
        let first = registry.get_by_name("edit-ch").expect("loaded");

        let edited = Channel {
            version: 2,
            config_json: r#"{"rate_limit": {"requests_per_second": 20}}"#.to_string(),
            updated_at: channel.updated_at + chrono::Duration::seconds(1),
            ..channel
        };
        deps.reload(&registry, &[edited]).await;
        let second = registry.get_by_name("edit-ch").expect("loaded");

        assert!(!Arc::ptr_eq(&first, &second), "an edited row must rebuild");
        assert_eq!(
            second
                .parsed_config
                .rate_limit
                .as_ref()
                .expect("rate limit")
                .requests_per_second,
            20
        );
    }

    /// The cache is keyed on the connector token too, because a
    /// `ChannelRuntimeConfig` embeds dedup and response-cache backends
    /// resolved through the connector registry. Reusing it across a connector
    /// change would pin a channel to a backend the operator has replaced —
    /// exactly what the epoch resync's pool eviction exists to prevent.
    #[tokio::test]
    async fn test_reload_rebuilds_when_the_connector_generation_moves() {
        let registry = TestRegistry::new();
        let deps = TestDeps::new();
        let repo = StubConnectorRepo::with(vec![("dedup-cache", "cache", MEMORY_CACHE)]);
        deps.connectors
            .reload(&repo)
            .await
            .expect("connectors load");
        let channel = test_channel(
            "dep-ch",
            r#"{"deduplication": {"header": "idem", "connector": "dedup-cache"}}"#,
        );

        deps.reload(&registry, std::slice::from_ref(&channel)).await;
        let first = registry.get_by_name("dep-ch").expect("loaded");

        // The operator deletes the connector the channel names. Same channel
        // row — the change is entirely behind the connector name, and a
        // reused runtime would keep serving through a backend that no longer
        // has a connector behind it.
        repo.set(vec![]);
        deps.connectors
            .reload(&repo)
            .await
            .expect("connectors load");
        deps.reload(&registry, std::slice::from_ref(&channel)).await;
        let second = registry.get_by_name("dep-ch").expect("loaded");

        assert!(
            !Arc::ptr_eq(&first, &second),
            "a moved connector token must re-resolve the channel's backends"
        );
    }

    /// The other half of the same key, and the one that decides whether N17
    /// saves anything on a node that did not originate the mutation.
    ///
    /// `resync_from_db` reloads the connector registry on *every* epoch tick,
    /// whatever the mutation was — a channel edit, a workflow activation. If
    /// that load moved the connector token, the fingerprint would differ on
    /// every remote node, nothing would ever be reused there, and the saving
    /// would be confined to the one node that made the change. It moves on
    /// *change*, so an unchanged connector set leaves the cache intact.
    #[tokio::test]
    async fn test_reload_reuses_across_a_connector_load_that_changed_nothing() {
        let registry = TestRegistry::new();
        let deps = TestDeps::new();
        let repo = StubConnectorRepo::with(vec![("dedup-cache", "cache", MEMORY_CACHE)]);
        deps.connectors
            .reload(&repo)
            .await
            .expect("connectors load");
        let channel = test_channel(
            "resync-ch",
            r#"{"deduplication": {"header": "idem", "connector": "dedup-cache"},
                "validation_logic": {"!!": [{"var": "data.id"}]}}"#,
        );

        deps.reload(&registry, std::slice::from_ref(&channel)).await;
        let first = registry.get_by_name("resync-ch").expect("loaded");

        // Three epoch ticks' worth of resync, none of which touched a
        // connector.
        for _ in 0..3 {
            deps.connectors
                .reload(&repo)
                .await
                .expect("connectors load");
            deps.reload(&registry, std::slice::from_ref(&channel)).await;
            let next = registry.get_by_name("resync-ch").expect("loaded");
            assert!(
                Arc::ptr_eq(&first, &next),
                "an epoch resync that changed no connector must not re-parse, \
                 re-compile and re-resolve every channel"
            );
        }
    }

    /// `RouteTable::build` parses every pattern and scans for conflicts in
    /// O(routes²), so an unchanged serviceable set must carry the built table
    /// over rather than rebuild it.
    #[tokio::test]
    async fn test_reload_reuses_route_table_when_the_serviceable_set_is_unchanged() {
        let registry = TestRegistry::new();
        let deps = TestDeps::new();
        let alpha = rest_channel("alpha-ch", "/alpha/{id}");

        deps.reload(&registry, std::slice::from_ref(&alpha)).await;
        let first = registry.snapshot().route_table.clone();

        deps.reload(&registry, std::slice::from_ref(&alpha)).await;
        let second = registry.snapshot().route_table.clone();
        assert!(
            Arc::ptr_eq(&first, &second),
            "an unchanged route set must keep the built table"
        );

        deps.reload(&registry, &[alpha, rest_channel("beta-ch", "/beta/{id}")])
            .await;
        let third = registry.snapshot().route_table.clone();
        assert!(
            !Arc::ptr_eq(&first, &third),
            "a new route-bearing channel must rebuild the table"
        );
        assert!(
            registry
                .match_route("GET", "beta/7")
                .expect("valid path")
                .is_some()
        );
    }

    /// N17, atomicity: a reader must never observe a reload half-applied.
    ///
    /// `by_name`, the route table and the quarantine map used to be swapped
    /// under three separate locks, in that order. Between the first swap and
    /// the last, a channel this reload was quarantining was absent from the
    /// serving map *and* absent from the quarantine map — so
    /// `require_serviceable` answered `Ok(None)`, which the data plane turns
    /// into 404 "unknown channel" instead of 503 "quarantined". One snapshot
    /// makes that state unrepresentable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_reader_never_sees_a_half_applied_quarantine() {
        let registry = Arc::new(TestRegistry::new());
        let deps = TestDeps::new();
        let good = test_channel("flip-ch", "{}");
        // A distinct row, so the reuse cache never short-circuits the flip and
        // every reload really does move the channel in or out of quarantine.
        let broken = Channel {
            version: 2,
            updated_at: good.updated_at + chrono::Duration::seconds(1),
            ..test_channel("flip-ch", "{ not json")
        };
        deps.reload(&registry, std::slice::from_ref(&good)).await;

        let reader_registry = registry.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observations = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let reader_stop = stop.clone();
        let reader_observations = observations.clone();
        let reader = tokio::spawn(async move {
            while !reader_stop.load(std::sync::atomic::Ordering::Relaxed) {
                match reader_registry.require_serviceable("flip-ch") {
                    // Serving, or refused as quarantined — both are whole
                    // answers from one generation.
                    Ok(Some(_)) | Err(_) => {
                        reader_observations.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    Ok(None) => unreachable!(
                        "reader saw 'flip-ch' as neither serving nor quarantined: \
                         the serving map and the quarantine map disagreed"
                    ),
                }
                tokio::task::yield_now().await;
            }
        });

        // 300 reloads take well under a tenth of a second, so without this
        // handshake the writer can finish and set `stop` before the reader is
        // ever scheduled — the test would pass having observed nothing.
        while observations.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            tokio::task::yield_now().await;
        }
        let before_flips = observations.load(std::sync::atomic::Ordering::Relaxed);
        for i in 0..300 {
            let channels = if i % 2 == 0 {
                std::slice::from_ref(&broken)
            } else {
                std::slice::from_ref(&good)
            };
            deps.reload(&registry, channels).await;
            tokio::task::yield_now().await;
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        reader.await.expect("reader must not panic");
        assert!(
            observations.load(std::sync::atomic::Ordering::Relaxed) > before_flips,
            "the reader must have read while the registry was being reloaded"
        );
    }

    /// The route-table reuse branch, at the one place it can go wrong.
    ///
    /// Reuse is keyed on the *serviceable* rows, not on the rows supplied. A
    /// channel can stop being serviceable without its row changing at all —
    /// the engine failed to build its workflow this time round — and if the
    /// key ignored that, the carried-over table would keep resolving a route
    /// to a channel that is no longer in the serving map. Everything else
    /// about the pairing is true by construction (the quarantine map
    /// partitions the supplied rows, and both the table and the serving map
    /// are built from the complement), so this is the branch worth a test.
    #[tokio::test]
    async fn test_route_table_reuse_drops_a_newly_quarantined_channels_route() {
        let registry = TestRegistry::new();
        let deps = TestDeps::new();
        let alpha = rest_channel("alpha-ch", "/alpha/{id}");
        let beta = rest_channel("beta-ch", "/beta/{id}");
        let channels = [alpha.clone(), beta.clone()];

        deps.reload(&registry, &channels).await;
        let first = registry.snapshot().route_table.clone();
        assert!(
            registry
                .match_route("GET", "alpha/7")
                .expect("valid path")
                .is_some()
        );

        // Same two rows, byte for byte — only the engine's verdict changed.
        deps.reload_with_issues(
            &registry,
            &channels,
            vec![ChannelLoadIssue {
                channel: "alpha-ch".to_string(),
                reason: "workflow 'wf_alpha' not found".to_string(),
            }],
        )
        .await;

        let second = registry.snapshot().route_table.clone();
        assert!(
            !Arc::ptr_eq(&first, &second),
            "a channel leaving the serviceable set must rebuild the table, \
             however unchanged its row is"
        );
        assert!(
            registry
                .match_route("GET", "alpha/7")
                .expect("valid path")
                .is_none(),
            "a quarantined channel's route must not outlive its runtime config"
        );
        assert!(registry.get_by_name("alpha-ch").is_none());
        // The channel that is still fine keeps serving.
        let matched = registry
            .match_route("GET", "beta/7")
            .expect("valid path")
            .expect("beta still routes");
        assert!(registry.get_by_name(&matched.channel_name).is_some());

        // And it comes back when the engine can build it again.
        deps.reload(&registry, &channels).await;
        assert!(
            registry
                .match_route("GET", "alpha/7")
                .expect("valid path")
                .is_some()
        );
        assert!(registry.get_by_name("alpha-ch").is_some());
    }

    /// F33: an engine-quarantined channel is refused even when its own config
    /// is fine, and a channel broken in *both* stages reports the more
    /// specific config reason.
    ///
    /// The double-failure case is also what made the old
    /// `channels.len() - issues.len()` count underflow: one channel, two
    /// issue entries. The count is off the published maps now.
    ///
    /// The assertions are on the published snapshot, not on the log line —
    /// but the subscriber below still matters, because the old subtraction
    /// lived inside `tracing::error!` and was therefore only evaluated when
    /// that level was enabled. Without it, a revert would pass here and panic
    /// in production.
    #[tokio::test]
    async fn test_engine_quarantine_and_config_failure_on_one_channel() {
        // Thread-local, not global: the current-thread test runtime polls this
        // future on this thread, and other tests are unaffected.
        let _log_guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::ERROR)
                .with_test_writer()
                .finish(),
        );
        let registry = TestRegistry::new();
        let deps = TestDeps::new();
        deps.reload_with_issues(
            &registry,
            &[test_channel("dup-ch", "{ nope")],
            vec![ChannelLoadIssue {
                channel: "dup-ch".to_string(),
                reason: "workflow missing".to_string(),
            }],
        )
        .await;

        let quarantined = registry.quarantined();
        assert_eq!(
            quarantined.len(),
            1,
            "one broken channel is one quarantine entry, however many ways it \
             is broken"
        );
        assert_eq!(quarantined[0].channel, "dup-ch");
        assert!(
            quarantined[0].reason.contains("config_json does not parse"),
            "the channel's own config failure is the more specific reason, \
             got: {}",
            quarantined[0].reason
        );
        assert!(registry.get_by_name("dup-ch").is_none());
        assert!(registry.snapshot().by_name.is_empty());
    }

    /// The engine-quarantine seeding on its own: a channel whose config parses
    /// perfectly still must not serve when the engine could not build it.
    #[tokio::test]
    async fn test_engine_quarantined_channel_does_not_serve() {
        let registry = TestRegistry::new();
        let deps = TestDeps::new();
        deps.reload_with_issues(
            &registry,
            &[test_channel("ok-ch", "{}"), test_channel("orphan-ch", "{}")],
            vec![ChannelLoadIssue {
                channel: "orphan-ch".to_string(),
                reason: "workflow 'wf_gone' not found".to_string(),
            }],
        )
        .await;

        assert!(registry.get_by_name("ok-ch").is_some());
        assert!(
            registry.get_by_name("orphan-ch").is_none(),
            "a channel with no workflow behind it must not be served"
        );
        assert_eq!(
            registry.quarantine_reason("orphan-ch").as_deref(),
            Some("workflow 'wf_gone' not found")
        );
        // Refused at ingress rather than reported unknown (N17 atomicity).
        assert!(registry.require_serviceable("orphan-ch").is_err());
    }

    /// N21: the quarantine set has one representation. `reload` no longer
    /// hands back a `Vec` that says the same thing as the map every caller —
    /// `/health`, the boot log, every ingress — already reads.
    #[tokio::test]
    async fn test_quarantine_set_is_readable_from_the_registry_alone() {
        let registry = TestRegistry::new();
        let deps = TestDeps::new();
        deps.reload(
            &registry,
            &[
                test_channel("ok-ch", "{}"),
                test_channel("bad-ch", "{ nope"),
            ],
        )
        .await;

        let quarantined = registry.quarantined();
        assert_eq!(quarantined.len(), 1);
        assert_eq!(quarantined[0].channel, "bad-ch");
        assert_eq!(
            registry.quarantine_reason("bad-ch"),
            Some(quarantined[0].reason.clone()),
            "the list and the single lookup must be the same map"
        );
        assert!(registry.quarantine_reason("ok-ch").is_none());

        // And a clean reload clears it — the map is the whole state.
        deps.reload(&registry, &[test_channel("ok-ch", "{}")]).await;
        assert!(registry.quarantined().is_empty());
    }
}
