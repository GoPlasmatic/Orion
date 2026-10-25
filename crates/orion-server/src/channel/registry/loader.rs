use super::*;

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
    /// The process's expression engine, which compiles every channel
    /// expression (I188).
    pub exprs: &'a ExprEngine,
    /// The instance's JWKS cache, for `auth.mode = "jwt"` channels.
    pub jwks: &'a std::sync::Arc<crate::jwt::jwks::JwksCache>,
    /// The shared outbound client, for a channel's `oauth2_login` code
    /// exchange (#307).
    pub http_client: &'a reqwest::Client,
    /// `[oauth2_login] allow_private_token_urls`.
    pub allow_private_token_urls: bool,
    /// `[oauth2_login.providers]` — deployment-supplied identity providers (#355),
    /// merged into a channel that opts in with `providers_from_instance`. Read
    /// once at boot, so a carried-over compiled block stays valid across reloads.
    pub instance_oauth_providers:
        &'a std::collections::BTreeMap<String, crate::config::InstanceProviderConfig>,
    /// The instance's OIDC discovery cache, for an `oauth2_login` provider that
    /// names an `issuer` (#355). A process singleton, like `jwks`.
    pub oidc_discovery: &'a std::sync::Arc<crate::channel::oidc_discovery::DiscoveryCache>,
    pub global_trace_storage: &'a TraceStorageConfig,
    /// `[cron] enabled`. A cron channel loaded on a node with the scheduler off
    /// is quarantined rather than served as a schedule that never fires.
    pub cron_enabled: bool,
}

/// Everything [`ChannelLoader::build_runtime`] needs beyond the channel row.
pub(super) struct RuntimeDeps<'a> {
    pub(super) vars: Option<&'a std::sync::Arc<serde_json::Value>>,
    pub(super) connector_registry: &'a ConnectorRegistry,
    pub(super) cache_pool: &'a CachePool,
    pub(super) exprs: &'a ExprEngine,
    /// The instance's JWKS cache, for `auth.mode = "jwt"` channels. Like
    /// `exprs` and `cache_pool` it is a process singleton, so a carried-over
    /// `ChannelRuntimeConfig` holding a compiled verifier built from it stays
    /// valid — see [`DepsFingerprint`].
    pub(super) jwks: &'a std::sync::Arc<crate::jwt::jwks::JwksCache>,
    /// The shared outbound client (#307). A `reqwest::Client` is an `Arc`
    /// internally, so a compiled sign-in block holding a clone holds a handle
    /// to the one pool, not a second one.
    pub(super) http_client: &'a reqwest::Client,
    pub(super) allow_private_token_urls: bool,
    instance_oauth_providers:
        &'a std::collections::BTreeMap<String, crate::config::InstanceProviderConfig>,
    pub(super) oidc_discovery: &'a std::sync::Arc<crate::channel::oidc_discovery::DiscoveryCache>,
    pub(super) global_trace_storage: &'a TraceStorageConfig,
    pub(super) cluster_shared: Option<crate::storage::repositories::shared_state::SqlSharedState>,
    pub(super) cron_enabled: bool,
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

pub(super) fn refuse_multi_key_object(
    logic: &serde_json::Value,
    field: &str,
) -> Result<(), String> {
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
    /// [`ChannelLoader::resolve_backend`]: shared-store defaults and load
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
    /// | cluster mode   | resolve; failure/memory = load error  | shared store (else memory) |
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

    /// Every store a response-cache entry can be sitting in, for namespace
    /// invalidation.
    ///
    /// The cluster's shared store when there is one, and every in-memory
    /// response-cache store this process has made — the built-in one and each
    /// cache connector's. The memory stores are deliberately every one ever
    /// made, not the ones active channels use today: a channel archived with
    /// live entries and reactivated later reads them against its own store's
    /// counters, so an invalidation has to have reached that store too.
    pub fn response_cache_targets(&self, cache_pool: &CachePool) -> Vec<Arc<dyn CacheBackend>> {
        let mut targets = cache_pool.memory_backends_for(CachePurpose::ResponseCache);
        if let Some(shared) = self.cluster.as_ref().and_then(|c| c.default_cache.clone()) {
            targets.push(shared);
        }
        targets
    }

    /// Invalidate `namespaces` in every [`Self::response_cache_targets`] store,
    /// answering how many stores the bump was sent to. The one path behind
    /// both `cache_invalidate` and the admin route, so the two cannot come to
    /// disagree about which stores an invalidation reaches.
    pub async fn invalidate_namespaces(
        &self,
        cache_pool: &CachePool,
        namespaces: &[String],
        source: &'static str,
    ) -> Result<usize, crate::errors::OrionError> {
        let targets = self.response_cache_targets(cache_pool);
        crate::channel::cache_namespace::invalidate(&targets, namespaces, source).await?;
        Ok(targets.len())
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

        // Both limiters are built the same way and reuse the same way; the
        // only difference is which config block they read and which previous
        // backend they may carry forward.
        let build_limiter = |rl: &ChannelRateLimitConfig,
                             prev_cfg: Option<&ChannelRateLimitConfig>,
                             prev_limiter: Option<&Arc<dyn RateLimitBackend>>|
         -> Arc<dyn RateLimitBackend> {
            if let (Some(prev_rl), Some(prev)) = (prev_cfg, prev_limiter)
                && prev_rl.requests_per_second == rl.requests_per_second
                && prev_rl.burst == rl.burst
                && prev_rl.key_logic == rl.key_logic
                && prev_rl.key_headers == rl.key_headers
            {
                return prev.clone();
            }
            let burst = rl.burst.unwrap_or(rl.requests_per_second / 2 + 1);
            match deps.cluster_shared.clone() {
                Some(store) => Arc::new(SharedRateLimitBackend::new(
                    store,
                    channel.name.clone(),
                    rl.requests_per_second,
                    burst,
                )) as Arc<dyn RateLimitBackend>,
                None => Arc::new(LocalRateLimitBackend::new(rl.requests_per_second, burst)),
            }
        };

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
                build_limiter(
                    rl,
                    prior.and_then(|p| p.parsed_config.rate_limit.as_ref()),
                    prior.and_then(|p| p.rate_limiter.as_ref()),
                )
            });

        // The quota half, keyed on the authenticated principal. Its own
        // backend, so a caller's address budget and its principal budget are
        // never counted against one bucket.
        let principal_rate_limiter: Option<Arc<dyn RateLimitBackend>> =
            parsed_config.principal_rate_limit.as_ref().map(|rl| {
                build_limiter(
                    rl,
                    prior.and_then(|p| p.parsed_config.principal_rate_limit.as_ref()),
                    prior.and_then(|p| p.principal_rate_limiter.as_ref()),
                )
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
                deps.exprs.compile(logic).map(Arc::new).map_err(|e| {
                    tracing::error!(
                        channel = %channel.name,
                        error = %e,
                        "Refusing to load channel: rate_limit.key_logic does not compile"
                    );
                    issue(format!("rate_limit.key_logic does not compile: {e}"))
                })
            })
            .transpose()?;

        // Required, not optional, and validated as such at create: the outer
        // limiter can fall back to the caller's address, and there is no
        // equivalent default for a principal — Orion cannot pick the claim
        // that identifies one on the author's behalf.
        let principal_rate_limit_key_logic = parsed_config
            .principal_rate_limit
            .as_ref()
            .and_then(|rl| rl.key_logic.as_ref())
            .map(|logic| {
                refuse_multi_key_object(logic, "principal_rate_limit.key_logic")
                    .map_err(|e| issue(e.clone()))?;
                deps.exprs.compile(logic).map(Arc::new).map_err(|e| {
                    tracing::error!(
                        channel = %channel.name,
                        error = %e,
                        "Refusing to load channel: principal_rate_limit.key_logic does not compile"
                    );
                    issue(format!(
                        "principal_rate_limit.key_logic does not compile: {e}"
                    ))
                })
            })
            .transpose()?;
        if parsed_config.principal_rate_limit.is_some() && principal_rate_limit_key_logic.is_none()
        {
            return Err(issue(
                "principal_rate_limit requires a 'key_logic' naming the claim that identifies \
                 the principal — there is no address to fall back to"
                    .to_string(),
            ));
        }

        // Same call as N5 above: a response-cache key that silently fell back
        // to the whole-payload hash would serve one caller's body to another
        // whenever the key was meant to narrow on something else.
        let cache_key_logic = parsed_config
            .cache
            .as_ref()
            .and_then(|c| c.key_logic.as_ref())
            .map(|logic| {
                refuse_multi_key_object(logic, "cache.key_logic").map_err(|e| issue(e.clone()))?;
                deps.exprs.compile(logic).map(Arc::new).map_err(|e| {
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
            let unreachable: Vec<String> = crate::channel::guards::key_logic_header_paths(logic)
                .into_iter()
                .filter(|name| {
                    !crate::channel::guards::COMMON_KEY_HEADERS.contains(&name.as_str())
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
                deps.exprs.compile(logic).map(Arc::new).map_err(|e| {
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
                crate::channel::auth::CompiledAuth::compile(cfg, Some(deps.exprs), Some(deps.jwks))
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
                        instance_providers: deps.instance_oauth_providers,
                        discovery: deps.oidc_discovery,
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

        let cache_flights = parsed_config
            .cache
            .as_ref()
            .filter(|c| c.enabled && c.coalesce_misses)
            .map(|_| Arc::new(crate::channel::guards::CacheFlights::default()));

        Ok(Arc::new(ChannelRuntimeConfig {
            channel: channel.clone(),
            parsed_config,
            rate_limiter,
            rate_limit_key_logic,
            principal_rate_limiter,
            principal_rate_limit_key_logic,
            rate_limit_key_headers,
            cache_key_logic,
            validation_logic,
            backpressure_semaphore,
            dedup_store,
            cache_flights,
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
    /// expression compilation, no backend resolution, no `Channel` clone. An
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
            exprs,
            jwks,
            http_client,
            allow_private_token_urls,
            instance_oauth_providers,
            oidc_discovery,
            global_trace_storage,
            cron_enabled,
        } = deps;
        let deps = RuntimeDeps {
            vars,
            connector_registry,
            cache_pool,
            exprs,
            jwks,
            http_client,
            allow_private_token_urls,
            instance_oauth_providers,
            oidc_discovery,
            global_trace_storage,
            cluster_shared: self.cluster.as_ref().and_then(|c| c.shared.clone()),
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

        // The one place the identities are attached: every issue names a
        // row being loaded, so its id and workflow come from that row.
        let quarantined: HashMap<String, QuarantineEntry> = issues
            .iter()
            .map(|i| {
                let row = channels.iter().find(|c| c.name == i.channel);
                (
                    i.channel.clone(),
                    QuarantineEntry {
                        reason: i.reason.clone(),
                        channel_id: row.map(|c| c.channel_id.clone()).unwrap_or_default(),
                        workflow_id: row.and_then(|c| c.workflow_id.clone()),
                    },
                )
            })
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
