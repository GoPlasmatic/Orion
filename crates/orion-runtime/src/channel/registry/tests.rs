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

    fn quarantined(&self) -> Vec<QuarantinedChannel> {
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
    // The entry carries the row's identities, so a caller matches on
    // them rather than on the reason's prose.
    assert_eq!(issues[0].channel, "nightly");
    assert!(!issues[0].channel_id.is_empty(), "{issues:?}");
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

/// A channel's protocol is served by its driver: one this node has no driver
/// for is quarantined by name rather than loaded and left unrouted.
#[tokio::test]
async fn a_channel_whose_protocol_has_no_driver_is_quarantined() {
    let registry = TestRegistry::new();
    let channel = Channel {
        protocol: "kafka".to_string(),
        ..rest_channel("orders-stream", "/orders-stream")
    };
    TestDeps::new().reload(&registry, &[channel]).await;

    let issues = registry.quarantined();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert!(
        issues[0].reason.contains("no channel driver"),
        "the reason must name the missing driver: {}",
        issues[0].reason
    );
    assert!(registry.get_by_name("orders-stream").is_none());
    assert!(matches!(
        registry.match_route("POST", "orders-stream"),
        Ok(None)
    ));
}

/// The route table asks the driver, not a list of protocol names: a driver
/// registered on the composition root that declares `registers_http_routes`
/// gets its channel's route served.
#[tokio::test]
async fn a_registered_driver_that_registers_routes_is_routed() {
    use orion_extension_api::{ChannelDriver, ChannelDriverCapabilities, ChannelProtocolFields};
    struct Graphql;
    impl ChannelDriver for Graphql {
        fn protocol_id(&self) -> &'static str {
            "graphql"
        }
        fn capabilities(&self) -> ChannelDriverCapabilities {
            ChannelDriverCapabilities {
                supports_sync: true,
                supports_async: false,
                registers_http_routes: true,
                supports_oauth2_login: false,
            }
        }
        fn validate_fields(&self, _: &ChannelProtocolFields<'_>) -> Vec<crate::errors::FieldError> {
            Vec::new()
        }
    }
    let registry = TestRegistry::new();
    let channel = Channel {
        protocol: "graphql".to_string(),
        ..rest_channel("graph", "/graph")
    };
    let mut deps = TestDeps::new();
    let mut drivers = crate::extensions::channel_driver::community_drivers();
    drivers.push(Arc::new(Graphql));
    deps.channel_drivers =
        crate::extensions::ChannelDriverRegistry::new(drivers).expect("distinct protocols");
    deps.reload(&registry, &[channel]).await;

    assert!(
        registry.quarantined().is_empty(),
        "{:?}",
        registry.quarantined()
    );
    let matched = registry
        .match_route("POST", "graph")
        .expect("valid path")
        .expect("the driver's route is served");
    assert_eq!(matched.channel_name, "graph");
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
    exprs: ExprEngine,
    jwks: std::sync::Arc<crate::jwt::jwks::JwksCache>,
    http_client: reqwest::Client,
    trace_storage: TraceStorageConfig,
    /// `[cron] enabled` as this harness's node sees it. Default `true`;
    /// the D3 quarantine test flips it.
    cron_enabled: bool,
    /// The node's channel drivers. Community by default; the driver tests
    /// register one.
    channel_drivers: crate::extensions::ChannelDriverRegistry,
}

impl TestDeps {
    fn new() -> Self {
        Self {
            connectors: ConnectorRegistry::new(
                crate::config::EngineConfig::default().circuit_breaker,
            ),
            cache_pool: CachePool::new(60, 1000),
            exprs: test_exprs(),
            jwks: test_jwks(),
            http_client: reqwest::Client::new(),
            trace_storage: TraceStorageConfig::default(),
            cron_enabled: true,
            channel_drivers: crate::extensions::ChannelDriverRegistry::community(),
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
                    exprs: &self.exprs,
                    jwks: &self.jwks,
                    http_client: &self.http_client,
                    allow_private_token_urls: false,
                    instance_oauth_providers: &std::collections::BTreeMap::new(),
                    oidc_discovery: &std::sync::Arc::new(
                        crate::channel::oidc_discovery::DiscoveryCache::new(
                            reqwest::Client::new(),
                            false,
                        ),
                    ),
                    global_trace_storage: &self.trace_storage,
                    cron_enabled: self.cron_enabled,
                    channel_drivers: &self.channel_drivers,
                },
                engine_issues,
            )
            .await;
    }
}

/// The process's expression engine as boot builds it with no secrets and
/// no budget.
fn test_exprs() -> ExprEngine {
    crate::engine::operators::orion_exprs(&crate::engine::ResolvedSecrets::empty(), 0)
        .expect("Orion's expression engine builds")
}

/// A JWKS cache for tests: no channel config here sets `auth.jwks_url`,
/// so nothing is ever fetched through it.
fn test_jwks() -> std::sync::Arc<crate::jwt::jwks::JwksCache> {
    std::sync::Arc::new(crate::jwt::jwks::JwksCache::new(
        reqwest::Client::new(),
        false,
    ))
}

fn test_discovery() -> std::sync::Arc<crate::channel::oidc_discovery::DiscoveryCache> {
    std::sync::Arc::new(crate::channel::oidc_discovery::DiscoveryCache::new(
        reqwest::Client::new(),
        false,
    ))
}

fn cluster_backends() -> ClusterBackends {
    ClusterBackends {
        // Stands in for the cluster's shared store, which serves both
        // internal purposes (their keys are structurally prefixed).
        default_cache: Some(CachePool::new(60, 1000).default_memory(CachePurpose::Dedup)),
        shared: None,
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
    let registry = ConnectorRegistry::new(crate::config::EngineConfig::default().circuit_breaker);
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
/// at the connector would keep writing to it — which is exactly what the
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
                cache_pool: &CachePool::new(60, 1000),
                exprs: &test_exprs(),
                jwks: &test_jwks(),
                http_client: &reqwest::Client::new(),
                allow_private_token_urls: false,
                instance_oauth_providers: &std::collections::BTreeMap::new(),
                oidc_discovery: &test_discovery(),
                global_trace_storage: &TraceStorageConfig::default(),
                cron_enabled: true,
                channel_drivers: &crate::extensions::ChannelDriverRegistry::community(),
            },
            Vec::new(),
        )
        .await;
    // N21: the issues are read from the registry, the one place they are kept.
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
                cache_pool: &CachePool::new(60, 1000),
                exprs: &test_exprs(),
                jwks: &test_jwks(),
                http_client: &reqwest::Client::new(),
                allow_private_token_urls: false,
                instance_oauth_providers: &std::collections::BTreeMap::new(),
                oidc_discovery: &test_discovery(),
                global_trace_storage: &TraceStorageConfig::default(),
                cron_enabled: true,
                channel_drivers: &crate::extensions::ChannelDriverRegistry::community(),
            },
            Vec::new(),
        )
        .await;
    // N21: the issues are read from the registry, the one place they are kept.
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
                cache_pool: &CachePool::new(60, 1000),
                exprs: &test_exprs(),
                jwks: &test_jwks(),
                http_client: &reqwest::Client::new(),
                allow_private_token_urls: false,
                instance_oauth_providers: &std::collections::BTreeMap::new(),
                oidc_discovery: &test_discovery(),
                global_trace_storage: &TraceStorageConfig::default(),
                cron_enabled: true,
                channel_drivers: &crate::extensions::ChannelDriverRegistry::community(),
            },
            Vec::new(),
        )
        .await;
    // N21: the issues are read from the registry, the one place they are kept.
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

async fn reload_single_node(channel: Channel) -> (TestRegistry, Vec<QuarantinedChannel>) {
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
async fn reload_into(registry: &TestRegistry, channel: Channel) -> Vec<QuarantinedChannel> {
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
    // Multi-key object: valid JSON, and a legal template on the engine
    // (templating mode), so `refuse_multi_key_object` is what refuses it.
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

    let sync =
        EffectiveTraceConfig::resolve(&global_tracing(TraceStorageMode::Sync, 0.5, false), None);
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
    let never =
        EffectiveTraceConfig::resolve(&global_tracing(TraceStorageMode::Sync, 0.0, false), None);
    let always =
        EffectiveTraceConfig::resolve(&global_tracing(TraceStorageMode::Sync, 1.0, false), None);
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
    let before =
        r#"{"rate_limit": {"requests_per_second": 1, "burst": 2, "key_headers": ["deviceid"]}}"#;
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
    let after =
        r#"{"rate_limit": {"requests_per_second": 1, "burst": 2, "key_headers": ["x-client-id"]}}"#;
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
/// compilation, no backend resolution, no `Channel` clone" — work a full
/// rebuild would repeat for every channel on every reload, on every node, on
/// every epoch tick.
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
    deps.connectors.load(repo.stored()).await;
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
    deps.connectors.load(repo.stored()).await;
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
    deps.connectors.load(repo.stored()).await;
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
        deps.connectors.load(repo.stored()).await;
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
/// Were `by_name`, the route table and the quarantine map swapped one at a
/// time, then between the first swap and the last a channel this reload is
/// quarantining would be absent from the serving map *and* absent from the
/// quarantine map — so `require_serviceable` would answer `Ok(None)`, which
/// the data plane turns into 404 "unknown channel" instead of 503
/// "quarantined". One snapshot makes that state unrepresentable.
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
    // A real OS thread, not a task: `require_serviceable` is a synchronous
    // snapshot read, so the reader needs no runtime, and preemption is what
    // lets it race the writer instead of depending on the writer to yield.
    let reader = std::thread::spawn(move || {
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
            // Spin rather than yield: the writer below waits on this
            // counter after every flip, and parking for a scheduler
            // quantum each time made the test take three seconds.
            std::hint::spin_loop();
        }
    });

    // Wait for the reader to be running before timing anything from it.
    while observations.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        tokio::task::yield_now().await;
    }
    let before_flips = observations.load(std::sync::atomic::Ordering::Relaxed);
    // All 300 flips together take a few milliseconds — less than one
    // scheduler quantum — so nothing makes the reader run *during* them on
    // its own, and an assertion that it had would be flaky. The writer waits
    // for an answer against each generation instead, which is what turns
    // the assertion below into a statement about the registry rather than
    // about how the two happened to be scheduled.
    'flips: for i in 0..300 {
        let channels = if i % 2 == 0 {
            std::slice::from_ref(&broken)
        } else {
            std::slice::from_ref(&good)
        };
        deps.reload(&registry, channels).await;
        let seen = observations.load(std::sync::atomic::Ordering::Relaxed);
        // Bounded, because the reader panicking on `Ok(None)` is the very
        // failure this test exists to catch: leave the loop so `join`
        // reports it, rather than waiting here for an answer never coming.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while observations.load(std::sync::atomic::Ordering::Relaxed) == seen {
            if std::time::Instant::now() >= deadline {
                break 'flips;
            }
            tokio::task::yield_now().await;
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    reader.join().expect("reader must not panic");
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
/// The double-failure case is also what would underflow a
/// `channels.len() - issues.len()` count: one channel, two issue entries.
/// The count is taken off the published maps.
///
/// The assertions are on the published snapshot, not on the log line —
/// but the subscriber below still matters, because the count is computed
/// for a `tracing::error!`, and a subtraction inside the macro is only
/// evaluated when that level is enabled. Without it, a regression would pass
/// here and panic in production.
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

/// N21: the quarantine set has one representation. `reload` hands back no
/// `Vec` restating the map every caller — `/health`, the boot log, every
/// ingress — already reads.
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
