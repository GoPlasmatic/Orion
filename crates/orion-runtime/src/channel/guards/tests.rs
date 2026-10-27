use super::key_logic_header_paths;
use dataflow_rs::Expr;
use dataflow_rs::expr::ExprEngine;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use super::{
    Admission, GuardRequest, GuardVerdict, HeaderLookup, Transport, Value, apply_guards,
    effective_timeout_ms,
};
use crate::channel::registry::EffectiveTraceConfig;
use crate::channel::{
    BackendErrorPolicy, ChannelConfig, ChannelRuntimeConfig, DeduplicationConfig,
};
use crate::config::TraceStorageConfig;
use crate::connector::cache_backend::CacheBackend;
use crate::errors::OrionError;
use crate::storage::models::Channel;

/// Dedup-store stub with a fixed `claim_dedup_key` outcome.
enum StubOutcome {
    New,
    /// Held by an earlier, *settled* delivery — a real duplicate.
    Duplicate,
    BackendError,
}

struct StubDedupBackend {
    outcome: StubOutcome,
}

#[async_trait]
impl CacheBackend for StubDedupBackend {
    async fn get(&self, _key: &str) -> Result<Option<String>, OrionError> {
        Ok(None)
    }
    async fn set(&self, _key: &str, _value: &str) -> Result<(), OrionError> {
        Ok(())
    }
    async fn set_ex(&self, _key: &str, _value: &str, _ttl: u64) -> Result<(), OrionError> {
        Ok(())
    }
    async fn remove(&self, _key: &str) -> Result<(), OrionError> {
        Ok(())
    }
    async fn claim_dedup_key(
        &self,
        _key: &str,
        _owner: &str,
        _window: u64,
    ) -> Result<Option<String>, OrionError> {
        match self.outcome {
            StubOutcome::New => Ok(None),
            StubOutcome::Duplicate => Ok(Some(super::dedup::DEDUP_SETTLED.to_string())),
            StubOutcome::BackendError => {
                Err(OrionError::internal("dedup backend down".to_string()))
            }
        }
    }
}

/// A real, per-test dedup store: claims are held in a map, so a claim, a
/// release and a re-claim behave as they do against a real store.
#[derive(Default)]
struct InMemoryDedupBackend {
    held: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

#[async_trait]
impl CacheBackend for InMemoryDedupBackend {
    async fn get(&self, key: &str) -> Result<Option<String>, OrionError> {
        Ok(self
            .held
            .lock()
            .expect("test lock poisoned")
            .get(key)
            .cloned())
    }
    async fn set(&self, key: &str, value: &str) -> Result<(), OrionError> {
        self.held
            .lock()
            .expect("test lock poisoned")
            .insert(key.to_string(), value.to_string());
        Ok(())
    }
    async fn set_ex(&self, key: &str, value: &str, _ttl: u64) -> Result<(), OrionError> {
        self.set(key, value).await
    }
    async fn remove(&self, key: &str) -> Result<(), OrionError> {
        self.held.lock().expect("test lock poisoned").remove(key);
        Ok(())
    }
    async fn claim_dedup_key(
        &self,
        key: &str,
        owner: &str,
        _window: u64,
    ) -> Result<Option<String>, OrionError> {
        let mut held = self.held.lock().expect("test lock poisoned");
        match held.get(key) {
            Some(holder) => Ok(Some(holder.clone())),
            None => {
                held.insert(key.to_string(), owner.to_string());
                Ok(None)
            }
        }
    }
}

/// Dedup-store stub that records every key passed to `claim_dedup_key`.
struct CapturingDedupBackend {
    seen: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl CacheBackend for CapturingDedupBackend {
    async fn get(&self, _key: &str) -> Result<Option<String>, OrionError> {
        Ok(None)
    }
    async fn set(&self, _key: &str, _value: &str) -> Result<(), OrionError> {
        Ok(())
    }
    async fn set_ex(&self, _key: &str, _value: &str, _ttl: u64) -> Result<(), OrionError> {
        Ok(())
    }
    async fn remove(&self, _key: &str) -> Result<(), OrionError> {
        Ok(())
    }
    async fn claim_dedup_key(
        &self,
        key: &str,
        _owner: &str,
        _window: u64,
    ) -> Result<Option<String>, OrionError> {
        self.seen
            .lock()
            .expect("test lock poisoned")
            .push(key.to_string());
        Ok(None)
    }
}

/// Response-cache stub that always hits with a fixed body.
struct AlwaysHitCache;

#[async_trait]
impl CacheBackend for AlwaysHitCache {
    async fn get(&self, _key: &str) -> Result<Option<String>, OrionError> {
        Ok(Some(r#"{"cached":true}"#.to_string()))
    }
    async fn set(&self, _key: &str, _value: &str) -> Result<(), OrionError> {
        Ok(())
    }
    async fn set_ex(&self, _key: &str, _value: &str, _ttl: u64) -> Result<(), OrionError> {
        Ok(())
    }
    async fn remove(&self, _key: &str) -> Result<(), OrionError> {
        Ok(())
    }
    async fn claim_dedup_key(
        &self,
        _key: &str,
        _owner: &str,
        _window: u64,
    ) -> Result<Option<String>, OrionError> {
        Ok(None)
    }
}

/// Rate-limit backend whose store never answers — a shared-store outage,
/// minus the store.
struct FailingLimiter;

#[async_trait]
impl crate::channel::RateLimitBackend for FailingLimiter {
    async fn check(&self, _key: String) -> Result<bool, OrionError> {
        Err(OrionError::internal("backend down".to_string()))
    }
}

/// Builder for a `ChannelRuntimeConfig`, so each test states only the
/// guard it exercises.
struct Runtime {
    parsed_config: ChannelConfig,
    rate_limiter: Option<Arc<dyn crate::channel::RateLimitBackend>>,
    rate_limit_key_logic: Option<Arc<Expr>>,
    rate_limit_key_headers: Option<Arc<[String]>>,
    principal_rate_limiter: Option<Arc<dyn crate::channel::RateLimitBackend>>,
    principal_rate_limit_key_logic: Option<Arc<Expr>>,
    validation_logic: Option<Arc<Expr>>,
    backpressure_semaphore: Option<Arc<tokio::sync::Semaphore>>,
    dedup_store: Option<Arc<dyn CacheBackend>>,
    response_cache: Option<Arc<dyn CacheBackend>>,
    auth: Option<crate::channel::auth::CompiledAuth>,
}

impl Runtime {
    fn new() -> Self {
        Self {
            parsed_config: ChannelConfig::default(),
            rate_limiter: None,
            rate_limit_key_logic: None,
            rate_limit_key_headers: None,
            principal_rate_limiter: None,
            principal_rate_limit_key_logic: None,
            validation_logic: None,
            backpressure_semaphore: None,
            auth: None,
            dedup_store: None,
            response_cache: None,
        }
    }

    fn dedup(mut self, store: Arc<dyn CacheBackend>, policy: BackendErrorPolicy) -> Self {
        self.parsed_config.deduplication = Some(DeduplicationConfig {
            header: "idempotency-key".to_string(),
            window_secs: Some(60),
            connector: None,
            on_backend_error: policy,
        });
        self.dedup_store = Some(store);
        self
    }

    fn origins(mut self, origins: &[&str]) -> Self {
        self.parsed_config.origin_allow_list =
            Some(origins.iter().map(|o| o.to_string()).collect());
        self
    }

    fn limiter(
        mut self,
        backend: Arc<dyn crate::channel::RateLimitBackend>,
        policy: BackendErrorPolicy,
    ) -> Self {
        self.parsed_config.rate_limit = Some(crate::channel::ChannelRateLimitConfig {
            requests_per_second: 1,
            burst: Some(1),
            key_logic: None,
            key_headers: None,
            on_backend_error: policy,
        });
        self.rate_limiter = Some(backend);
        self
    }

    /// The post-auth quota: its own backend and a required key.
    fn principal_limiter(
        mut self,
        engine: &ExprEngine,
        backend: Arc<dyn crate::channel::RateLimitBackend>,
        logic: serde_json::Value,
    ) -> Self {
        self.parsed_config.principal_rate_limit = Some(crate::channel::ChannelRateLimitConfig {
            requests_per_second: 1,
            burst: Some(1),
            key_logic: Some(logic.clone()),
            key_headers: None,
            on_backend_error: BackendErrorPolicy::default(),
        });
        self.principal_rate_limiter = Some(backend);
        self.principal_rate_limit_key_logic = Some(Arc::new(
            engine.compile(&logic).expect("test logic compiles"),
        ));
        self
    }

    fn key_logic(mut self, engine: &ExprEngine, logic: serde_json::Value) -> Self {
        self.rate_limit_key_logic = Some(Arc::new(
            engine.compile(&logic).expect("test logic compiles"),
        ));
        self
    }

    /// Declare extra headers for `key_logic`, as the registry would after
    /// lowercasing them.
    fn key_headers(mut self, names: &[&str]) -> Self {
        let lowered: Vec<String> = names.iter().map(|n| n.to_ascii_lowercase()).collect();
        if let Some(ref mut rl) = self.parsed_config.rate_limit {
            rl.key_headers = Some(lowered.clone());
        }
        self.rate_limit_key_headers = Some(lowered.into());
        self
    }

    fn validation(mut self, engine: &ExprEngine, logic: serde_json::Value) -> Self {
        self.validation_logic = Some(Arc::new(
            engine.compile(&logic).expect("test logic compiles"),
        ));
        self
    }

    fn backpressure(mut self, permits: usize) -> Self {
        self.parsed_config.backpressure = Some(crate::channel::BackpressureConfig {
            max_concurrent_per_node: permits,
        });
        self.backpressure_semaphore = Some(Arc::new(tokio::sync::Semaphore::new(permits)));
        self
    }

    fn cache(mut self, backend: Arc<dyn CacheBackend>) -> Self {
        self.parsed_config.cache = Some(crate::channel::ChannelCacheConfig {
            enabled: true,
            ttl_secs: Some(60),
            cache_key_fields: None,
            key_logic: None,
            connector: None,
            namespaces: None,
            coalesce_misses: false,
        });
        self.response_cache = Some(backend);
        self
    }

    /// Turn on `cache.coalesce_misses` for a cache added by [`Self::cache`].
    fn coalesce(mut self) -> Self {
        if let Some(ref mut cache) = self.parsed_config.cache {
            cache.coalesce_misses = true;
        }
        self
    }

    /// Declare `cache.namespaces` on a cache added by [`Self::cache`].
    fn namespaces(mut self, names: &[&str]) -> Self {
        if let Some(ref mut cache) = self.parsed_config.cache {
            cache.namespaces = Some(names.iter().map(|n| n.to_string()).collect());
        }
        self
    }

    fn timeout_ms(mut self, ms: u64) -> Self {
        self.parsed_config.timeout_ms = Some(ms);
        self
    }

    /// An `api_key` policy accepting exactly `key`, presented bare in
    /// `X-API-Key` so tests need no scheme prefix.
    async fn api_key(mut self, key: &str) -> Self {
        let cfg = crate::channel::config::ChannelAuthConfig {
            mode: crate::channel::config::AuthMode::ApiKey,
            keys: Some(vec![key.to_string()]),
            header: Some("X-API-Key".to_string()),
            ..Default::default()
        };
        self.auth = Some(
            crate::channel::auth::CompiledAuth::compile(&cfg, None, None)
                .await
                .expect("test auth compiles"),
        );
        self.parsed_config.auth = Some(cfg);
        self
    }

    fn build(self) -> Option<Arc<ChannelRuntimeConfig>> {
        let now = chrono::Utc::now().naive_utc();
        let cache_flights = self
            .parsed_config
            .cache
            .as_ref()
            .filter(|c| c.coalesce_misses)
            .map(|_| Arc::new(super::response_cache::CacheFlights::default()));
        Some(Arc::new(ChannelRuntimeConfig {
            channel: Channel {
                tags_json: "[]".to_string(),
                channel_id: "ch_test".to_string(),
                version: 1,
                name: "test-channel".to_string(),
                description: None,
                channel_type: "sync".to_string(),
                protocol: "rest".to_string(),
                methods_json: None,
                route_pattern: None,
                topic: None,
                consumer_group: None,
                transport_config_json: "{}".to_string(),
                workflow_id: None,
                config_json: "{}".to_string(),
                status: "active".to_string(),
                priority: 0,
                created_at: now,
                updated_at: now,
            },
            cron: None,
            metrics: Default::default(),
            parsed_config: self.parsed_config,
            rate_limiter: self.rate_limiter,
            rate_limit_key_logic: self.rate_limit_key_logic,
            principal_rate_limiter: self.principal_rate_limiter,
            principal_rate_limit_key_logic: self.principal_rate_limit_key_logic,
            cache_key_logic: None,
            rate_limit_key_headers: self.rate_limit_key_headers,
            validation_logic: self.validation_logic,
            backpressure_semaphore: self.backpressure_semaphore,
            dedup_store: self.dedup_store,
            cache_flights,
            response_cache: self.response_cache,
            trace_storage: EffectiveTraceConfig::resolve(&TraceStorageConfig::default(), None),
            auth: self.auth,
            oauth2_login: None,
        }))
    }
}

fn dedup_runtime(outcome: StubOutcome) -> Option<Arc<ChannelRuntimeConfig>> {
    dedup_runtime_with_policy(outcome, BackendErrorPolicy::Allow)
}

fn dedup_runtime_with_policy(
    outcome: StubOutcome,
    policy: BackendErrorPolicy,
) -> Option<Arc<ChannelRuntimeConfig>> {
    Runtime::new()
        .dedup(Arc::new(StubDedupBackend { outcome }), policy)
        .build()
}

/// Header-lookup view matching what the HTTP path passes: the
/// idempotency header resolves to "token-1", everything else to None.
// Generic over the value lifetime rather than `'static`: the lookup's
// value type is invariant inside the `dyn Fn`, so a `'static` one would
// pin every request it is set on to `'static` too.
fn idempotency_lookup<'v>(name: &str) -> Option<&'v str> {
    (name == "idempotency-key").then_some("token-1")
}

fn no_headers<'v>(_name: &str) -> Option<&'v str> {
    None
}

/// The process's expression engine as boot builds it with no secrets and
/// no budget.
fn engine() -> ExprEngine {
    crate::engine::operators::orion_exprs(&crate::engine::ResolvedSecrets::empty(), 0)
        .expect("Orion's expression engine builds")
}

/// The context hands back exactly what the transport moved in, and its
/// root is the `{"data", "metadata"}` object every reader evaluates against.
#[test]
fn a_guard_context_hands_back_what_it_was_given() {
    let data = json!({"order": {"id": 7}});
    let metadata = json!({"headers": {"x-tenant-id": "acme"}, "channel": "orders"});
    let context = super::GuardContext::new(data.clone(), metadata.clone());
    assert_eq!(context.data(), &data);
    assert_eq!(context.metadata(), &metadata);
    assert_eq!(
        context.root(),
        &json!({"data": data.clone(), "metadata": metadata.clone()})
    );
    assert_eq!(context.into_parts(), (data, metadata));
}

/// The two readers that moved off the JSON 1.12 evaluated into each give,
/// for every shape of result, what 1.12's reader gave for the JSON that
/// result became (`result_json`): `is_truthy` for `validation_logic` and
/// `authorization_logic`, `key_text` for both rate-limit keys. The shapes
/// the JSON changed are the point: a non-finite number was `null`, and a
/// datetime or a duration was a one-key object.
#[test]
fn the_readers_answer_as_they_did_over_json() {
    use dataflow_rs::datavalue::{DataDateTime, DataDuration, NumberValue, OwnedDataValue};

    fn truthy_1_12(val: &Value) -> bool {
        match val {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
            Value::String(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Object(_) => true,
        }
    }
    fn key_1_12(val: Value) -> Result<String, &'static str> {
        match val {
            Value::Null => Err("expression resolved to null"),
            Value::String(s) if s.trim().is_empty() => {
                Err("expression resolved to an empty string")
            }
            val => Ok(val
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| serde_json::to_string(&val).unwrap_or_default())),
        }
    }

    let float = |f: f64| OwnedDataValue::Number(NumberValue::Float(f));
    let from = |v: Value| OwnedDataValue::from(&v);
    let values = [
        OwnedDataValue::Null,
        OwnedDataValue::Bool(true),
        OwnedDataValue::Bool(false),
        from(json!(0)),
        from(json!(7)),
        from(json!(-1)),
        from(json!(u64::MAX)),
        float(0.0),
        float(-0.0),
        float(2.5),
        float(f64::NAN),
        float(f64::INFINITY),
        float(f64::NEG_INFINITY),
        from(json!("")),
        from(json!("  ")),
        from(json!("tenant-a")),
        from(json!([])),
        from(json!([0])),
        from(json!({})),
        from(json!({"b": 1, "a": [1.5, null, "x"]})),
        OwnedDataValue::DateTime(DataDateTime::parse("2024-01-15T12:30:45Z").expect("parses")),
        OwnedDataValue::Duration(DataDuration::parse("3d:4h").expect("parses")),
    ];
    for value in values {
        let json = crate::engine::operators::result_json(&value);
        assert_eq!(super::is_truthy(&value), truthy_1_12(&json), "{value:?}");
        assert_eq!(
            super::rate_limit::key_text(value.clone()),
            key_1_12(json),
            "{value:?}"
        );
    }
}

/// One guard request with everything defaulted, so a test names only
/// what it is about.
///
/// The context is a copy of `data` and `metadata`, leaked: a test calls
/// this many times over the same two values, and a `&'static mut` keeps
/// every call site the one line it was before the guards took the message
/// as one owned [`GuardContext`]. Test-only, and a few bytes per call.
fn request<'a>(
    transport: Transport,
    runtime: &'a Option<Arc<ChannelRuntimeConfig>>,
    exprs: &'a ExprEngine,
    data: &'a Value,
    metadata: &'a Value,
) -> GuardRequest<'a> {
    GuardRequest {
        transport,
        channel: "orders",
        runtime,
        context: Box::leak(Box::new(super::GuardContext::new(
            data.clone(),
            metadata.clone(),
        ))),
        exprs,
        auth_backoff: None,
        origin: None,
        caller_identity: "10.0.0.1",
        header: &no_headers,
        raw_body: None,
        dedup_key_fallback: None,
        dedup_owner: None,
        default_timeout_ms: None,
        max_timeout_ms: None,
        oauth: None,
    }
}

/// `Some` when the guards admitted the message; `None` when a guard
/// answered instead — the response cache, or an OAuth2 authorize leg. Call
/// sites `.expect(...)` the arm they mean.
fn admitted(verdict: GuardVerdict) -> Option<Admission> {
    match verdict {
        GuardVerdict::Admitted(a) => Some(a),
        GuardVerdict::CacheHit(_) | GuardVerdict::Respond(_) => None,
    }
}

// ---- The matrix itself (N16) ----

/// The matrix is the specification: these are the cells the module doc
/// documents and `apply_guards` reads. A change here is a change to the
/// contract, not an implementation detail.
#[tokio::test]
async fn the_guard_matrix_is_what_the_docs_claim() {
    let sync = Transport::HttpSync.guards();
    let submit = Transport::HttpAsync.guards();
    let call = Transport::ChannelCall.guards();
    let cron = Transport::Cron.guards();

    // Universal guards: every transport, no exceptions. channel_call
    // lacked the rate limit (N16).
    for set in [sync, submit, call, cron] {
        assert!(set.validation);
        assert!(set.backpressure);
    }
    // Rate limiting is universal among the transports that have a *caller*.
    // Cron is the exception, and the only one: its schedule already fixes
    // how often it runs.
    for set in [sync, submit, call] {
        assert!(set.rate_limit);
    }
    assert!(!cron.rate_limit);

    // Deliberate exclusions.
    assert!(sync.origin_allow_list && submit.origin_allow_list);
    assert!(!call.origin_allow_list && !cron.origin_allow_list);
    assert!(sync.deduplication && submit.deduplication);
    assert!(!call.deduplication && !cron.deduplication);
    assert!(sync.response_cache);
    assert!(!submit.response_cache && !call.response_cache && !cron.response_cache);
    assert!(sync.auth && submit.auth);
    assert!(!call.auth && !cron.auth);
    assert!(sync.oauth2_login);
    assert!(!cron.oauth2_login);
}

/// The refused-config list and the guard row are two statements of one
/// decision, and a disagreement between them is a config Orion accepts and
/// silently ignores — which is exactly what refusing the keys was for.
///
/// Only one direction can be checked mechanically from here: every guard
/// this row switches **off** must be a key authoring refuses. The reverse
/// (a refused key whose guard is on) would be a key an author cannot set,
/// which is harmless and is covered by the validation module's own tests.
#[test]
fn cron_guards_and_validation_agree() {
    use crate::extensions::ChannelDriver;
    let cron = Transport::Cron.guards();
    // Asked of the cron channel driver, which is what validates a cron
    // channel at upload — not of a helper beside it.
    let schedule = serde_json::json!({"schedule": "0 15 2 * * *"});
    assert!(
        crate::extensions::channel_driver::CronChannelDriver
            .validate_transport(&schedule, &serde_json::json!({}), None)
            .is_empty(),
        "the probe schedule itself must be valid"
    );
    let refused = |key: &str| {
        !crate::extensions::channel_driver::CronChannelDriver
            .validate_transport(&schedule, &serde_json::json!({ key: {} }), None)
            .is_empty()
    };
    for (guard_on, key) in [
        (cron.auth, "auth"),
        (cron.origin_allow_list, "origin_allow_list"),
        (cron.rate_limit, "rate_limit"),
        (cron.deduplication, "deduplication"),
        (cron.response_cache, "cache"),
        (cron.oauth2_login, "oauth2_login"),
    ] {
        assert!(
            guard_on || refused(key),
            "Transport::Cron leaves `{key}` off, so authoring must refuse it — \
                 otherwise a cron channel can declare it and Orion ignores it"
        );
    }
    // And the two that stay on are settable.
    assert!(cron.validation && cron.backpressure);
    assert!(!refused("validation_logic") && !refused("backpressure"));
}

/// Authentication is enforced on both HTTP ingresses, not just the one a
/// test happened to exercise.
///
/// `/async` bypassing a guard the sync path applies is the exact shape of
/// S1, and it is the shape an authentication guard can least afford: a
/// channel that refuses anonymous callers on `POST /orders` while
/// accepting them on `POST /orders/async` is not authenticated at all.
#[tokio::test]
async fn authentication_applies_to_every_http_ingress() {
    let dl = engine();
    let data = json!({});
    let metadata = json!({});

    for transport in [Transport::HttpSync, Transport::HttpAsync] {
        let runtime = Runtime::new().api_key("s3cret").await.build();
        let req = request(transport, &runtime, &dl, &data, &metadata);
        assert!(
            apply_guards(req).await.is_err(),
            "{transport:?} admitted a request presenting no key"
        );

        let runtime = Runtime::new().api_key("s3cret").await.build();
        let present: HeaderLookup<'_, '_> = &|name: &str| (name == "X-API-Key").then_some("s3cret");
        let mut req = request(transport, &runtime, &dl, &data, &metadata);
        req.header = present;
        assert!(
            apply_guards(req).await.is_ok(),
            "{transport:?} refused a request presenting the right key"
        );
    }
}

/// The transports whose row leaves `auth` off carry no credential to
/// present. `channel_call` is authenticated by the layer that delivered
/// it — the originating ingress; a cron occurrence has no caller in the
/// first place. Enforcing here would break composition rather than tighten
/// anything.
#[tokio::test]
async fn authentication_does_not_apply_to_channel_call_or_cron() {
    let dl = engine();
    let data = json!({});
    let metadata = json!({});

    for transport in [Transport::ChannelCall, Transport::Cron] {
        let runtime = Runtime::new().api_key("s3cret").await.build();
        let req = request(transport, &runtime, &dl, &data, &metadata);
        assert!(
            apply_guards(req).await.is_ok(),
            "{transport:?} must not require an HTTP credential"
        );
    }
}

/// A failed authentication must not reach the guards behind it.
///
/// Ordering is the whole control here: if an anonymous caller got as far as
/// the dedup store they could claim a real caller's idempotency key and have
/// the genuine request answered `409`, and if they reached the response
/// cache they could probe which requests are cached. Both are behind
/// `check_auth` in `apply_guards`, and this is what says so.
#[tokio::test]
async fn a_refused_caller_never_reaches_dedup_or_cache() {
    let dl = engine();
    let data = json!({});
    let metadata = json!({});

    // A dedup backend that panics if consulted: reaching it at all is the
    // failure this test is looking for.
    let runtime = Runtime::new()
        .api_key("s3cret")
        .await
        .dedup(
            Arc::new(StubDedupBackend {
                outcome: StubOutcome::BackendError,
            }),
            BackendErrorPolicy::Deny,
        )
        .build();

    let mut req = request(Transport::HttpSync, &runtime, &dl, &data, &metadata);
    req.header = &idempotency_lookup;
    // `GuardVerdict` is not `Debug`, so `expect_err` is unavailable;
    // `.err().expect(..)` asserts the same thing without it.
    let err = apply_guards(req)
        .await
        .err()
        .expect("an unauthenticated caller must be refused");
    // `Deny` on a dedup backend error is a 503; authentication refuses 401.
    // Seeing the 503 would mean the dedup guard ran first.
    assert!(
        matches!(
            err,
            OrionError::Http {
                status: orion_common::errors::StatusCode::UNAUTHORIZED,
                ..
            }
        ),
        "expected a 401 from the auth guard, got {err:?} — the dedup guard ran first"
    );
}

/// The channel's `timeout_ms` wins on every transport; the transport's
/// default only fills in when the channel declares none (N16 — `/async`
/// included).
#[tokio::test]
async fn the_channel_timeout_outranks_the_transport_default() {
    let with_timeout = Runtime::new().timeout_ms(2_000).build();
    let without = Runtime::new().build();
    assert_eq!(
        effective_timeout_ms(&with_timeout, Some(60_000), None),
        Some(2_000)
    );
    assert_eq!(effective_timeout_ms(&with_timeout, None, None), Some(2_000));
    assert_eq!(
        effective_timeout_ms(&without, Some(60_000), None),
        Some(60_000)
    );
    assert_eq!(effective_timeout_ms(&without, None, None), None);
    assert_eq!(
        effective_timeout_ms(&None, Some(60_000), None),
        Some(60_000)
    );
}

/// A channel may shorten the transport's deadline; it may not lengthen
/// it. On the async path an over-long one holds one of a fixed number of
/// queue workers past the operator's ceiling. `timeout_ms` is unvalidated
/// and unbounded upward, so the clamp is the only thing standing between a
/// channel config and that failure.
#[tokio::test]
async fn a_transport_ceiling_clamps_an_over_long_channel_timeout() {
    let over = Runtime::new().timeout_ms(600_000).build();
    let under = Runtime::new().timeout_ms(2_000).build();
    let none = Runtime::new().build();

    assert_eq!(
        effective_timeout_ms(&over, Some(30_000), Some(30_000)),
        Some(30_000),
        "a channel asking for 10 minutes gets the transport's ceiling"
    );
    assert_eq!(
        effective_timeout_ms(&under, Some(30_000), Some(30_000)),
        Some(2_000),
        "a shorter channel deadline is still honoured"
    );
    assert_eq!(
        effective_timeout_ms(&none, Some(30_000), Some(30_000)),
        Some(30_000)
    );
    // No ceiling declared: synchronous HTTP and `channel_call` protect no
    // shared resource, so the channel value stands whatever it is.
    assert_eq!(
        effective_timeout_ms(&over, None, None),
        Some(600_000),
        "without a ceiling the channel value is unchanged"
    );
}

#[tokio::test]
async fn every_transport_carries_the_channel_timeout_into_its_admission() {
    let dl = engine();
    let runtime = Runtime::new().timeout_ms(2_000).build();
    let (data, meta) = (json!({}), json!({}));
    for transport in [
        Transport::HttpSync,
        Transport::HttpAsync,
        Transport::ChannelCall,
        Transport::Cron,
    ] {
        let mut req = request(transport, &runtime, &dl, &data, &meta);
        req.default_timeout_ms = Some(60_000);
        let admission = admitted(apply_guards(req).await.expect("guards pass"))
            .expect("an admission, not a cache hit");
        assert_eq!(
            admission.timeout_ms,
            Some(2_000),
            "{transport:?} must honour the channel's timeout_ms"
        );
    }
}

// ---- Rate limit, on every transport (S15) ----

/// The per-channel limit is a guard, not HTTP middleware, so every transport
/// consults the same limiter.
#[tokio::test]
async fn the_rate_limit_applies_on_every_transport() {
    let dl = engine();
    let (data, meta) = (json!({}), json!({}));
    for transport in [
        Transport::HttpSync,
        Transport::HttpAsync,
        Transport::ChannelCall,
    ] {
        // A fresh 1-rps/1-burst limiter per transport: the first call
        // passes, the second is refused.
        let runtime = Runtime::new()
            .limiter(
                Arc::new(crate::channel::LocalRateLimitBackend::new(1, 1)),
                BackendErrorPolicy::Allow,
            )
            .build();
        let first = apply_guards(request(transport, &runtime, &dl, &data, &meta)).await;
        assert!(first.is_ok(), "{transport:?} first call must pass");
        let second = apply_guards(request(transport, &runtime, &dl, &data, &meta)).await;
        assert!(
            matches!(second, Err(OrionError::RateLimited(_))),
            "{transport:?} must refuse the second call"
        );
    }
}

/// …but "the same limiter" is not "one bucket". Without `key_logic` the
/// key is the transport's caller identity — client IP, calling channel —
/// so a `requests_per_second` is a per-identity rate on each ingress and
/// not a channel-wide throughput cap. Stating that here keeps the test
/// above from being read as the stronger claim it does not make (it
/// passes the same identity for every transport).
#[tokio::test]
async fn the_default_bucket_key_is_the_transports_own_caller_identity() {
    let dl = engine();
    let (data, meta) = (json!({}), json!({}));
    let runtime = Runtime::new()
        .limiter(
            Arc::new(crate::channel::LocalRateLimitBackend::new(1, 1)),
            BackendErrorPolicy::Allow,
        )
        .build();

    // One HTTP client spends its token...
    let mut req = request(Transport::HttpSync, &runtime, &dl, &data, &meta);
    req.caller_identity = "203.0.113.7";
    assert!(apply_guards(req).await.is_ok());
    let mut req = request(Transport::HttpSync, &runtime, &dl, &data, &meta);
    req.caller_identity = "203.0.113.7";
    assert!(matches!(
        apply_guards(req).await,
        Err(OrionError::RateLimited(_))
    ));

    // ...while the calling channel's bucket is untouched, because the
    // identity — and therefore the key — is different.
    let mut req = request(Transport::ChannelCall, &runtime, &dl, &data, &meta);
    req.caller_identity = "checkout";
    assert!(
        apply_guards(req).await.is_ok(),
        "a channel_call meters under its calling channel, not the HTTP client's bucket"
    );

    // A `key_logic` returning a transport-independent value is what makes
    // the limit one shared cap.
    let shared = Runtime::new()
        .limiter(
            Arc::new(crate::channel::LocalRateLimitBackend::new(1, 1)),
            BackendErrorPolicy::Allow,
        )
        .key_logic(&dl, json!({"var": "channel"}))
        .build();
    let mut req = request(Transport::HttpSync, &shared, &dl, &data, &meta);
    req.caller_identity = "203.0.113.7";
    assert!(apply_guards(req).await.is_ok());
    let mut req = request(Transport::ChannelCall, &shared, &dl, &data, &meta);
    req.caller_identity = "checkout";
    assert!(
        matches!(apply_guards(req).await, Err(OrionError::RateLimited(_))),
        "a channel-keyed limit is spent by whichever ingress gets there first"
    );
}

/// N5: a `key_logic` that cannot be evaluated rejects rather than
/// silently falling back to the caller identity, which would merge every
/// tenant into one bucket.
///
/// It rejects with its own variant, because the condition is not "over a
/// limit": the expression fails on this message and will fail on every
/// copy of it. Over HTTP both answer `429`; a transport that retries can
/// tell from the variant that this one is terminal.
#[tokio::test]
async fn an_unevaluable_rate_limit_key_rejects_as_its_own_condition() {
    let dl = engine();
    let runtime = Runtime::new()
        .limiter(
            Arc::new(crate::channel::LocalRateLimitBackend::new(1000, 1000)),
            BackendErrorPolicy::Allow,
        )
        .key_logic(&dl, json!({"throw": "key_unavailable"}))
        .build();
    let (data, meta) = (json!({}), json!({}));
    let verdict = apply_guards(request(Transport::HttpSync, &runtime, &dl, &data, &meta)).await;
    assert!(
        matches!(verdict, Err(OrionError::RateLimitKeyUnavailable(_))),
        "an unevaluable key must reject as unevaluable, not as over-limit"
    );
    // The caller cannot tell the two apart, and should not have to.
    let (status, code, _) = verdict.err().expect("a refusal").response_parts();
    assert_eq!(status, axum::http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(code, "RATE_LIMITED");
}

/// N7: a limiter backend outage resolves through the channel's policy —
/// `allow` fails open, `deny` refuses with 503 and never 429 (the caller
/// is not over any limit; the control is unavailable).
#[tokio::test]
async fn a_limiter_backend_outage_follows_the_channel_policy() {
    let dl = engine();
    let (data, meta) = (json!({}), json!({}));

    let allow = Runtime::new()
        .limiter(Arc::new(FailingLimiter), BackendErrorPolicy::Allow)
        .build();
    let verdict = apply_guards(request(Transport::ChannelCall, &allow, &dl, &data, &meta)).await;
    assert!(verdict.is_ok(), "allow must fail open");

    let deny = Runtime::new()
        .limiter(Arc::new(FailingLimiter), BackendErrorPolicy::Deny)
        .build();
    let verdict = apply_guards(request(Transport::ChannelCall, &deny, &dl, &data, &meta)).await;
    assert!(
        matches!(verdict, Err(OrionError::ServiceUnavailable { .. })),
        "deny must refuse with 503, not 429"
    );
}

/// `key_logic` reads the transport's headers, whatever the transport is.
#[tokio::test]
async fn the_rate_limit_key_can_be_computed_from_transport_headers() {
    let dl = engine();
    let runtime = Runtime::new()
        .limiter(
            Arc::new(crate::channel::LocalRateLimitBackend::new(1, 1)),
            BackendErrorPolicy::Allow,
        )
        .key_logic(&dl, json!({"var": "headers.x-tenant-id"}))
        .build();
    let (data, meta) = (json!({}), json!({}));

    let acme = |name: &str| (name == "x-tenant-id").then_some("acme");
    let globex = |name: &str| (name == "x-tenant-id").then_some("globex");

    let mut req = request(Transport::ChannelCall, &runtime, &dl, &data, &meta);
    req.header = &acme;
    assert!(apply_guards(req).await.is_ok());

    // A different tenant has its own bucket, so it is not refused...
    let mut req = request(Transport::ChannelCall, &runtime, &dl, &data, &meta);
    req.header = &globex;
    assert!(apply_guards(req).await.is_ok());

    // ...while the first tenant's bucket is now empty.
    let mut req = request(Transport::ChannelCall, &runtime, &dl, &data, &meta);
    req.header = &acme;
    assert!(matches!(
        apply_guards(req).await,
        Err(OrionError::RateLimited(_))
    ));
}

/// G6: the address limit runs before `check_auth` and therefore cannot know
/// who the caller is. The principal limit is the second bucket, and it is a
/// separate one — two callers sharing an address are metered apart, which is
/// the whole point of a quota.
#[tokio::test]
async fn the_principal_limit_is_a_second_bucket_after_the_address_limit() {
    let dl = engine();
    let runtime = Runtime::new()
        // Generous on the address, so any refusal below is the quota's.
        .limiter(
            Arc::new(crate::channel::LocalRateLimitBackend::new(100, 100)),
            BackendErrorPolicy::Allow,
        )
        .principal_limiter(
            &dl,
            Arc::new(crate::channel::LocalRateLimitBackend::new(1, 1)),
            json!({"var": "headers.x-tenant-id"}),
        )
        .build();
    let (data, meta) = (json!({}), json!({}));
    let acme = |name: &str| (name == "x-tenant-id").then_some("acme");
    let globex = |name: &str| (name == "x-tenant-id").then_some("globex");

    let mut req = request(Transport::HttpAsync, &runtime, &dl, &data, &meta);
    req.header = &acme;
    assert!(apply_guards(req).await.is_ok());

    // A different principal from the same address has its own bucket.
    let mut req = request(Transport::HttpAsync, &runtime, &dl, &data, &meta);
    req.header = &globex;
    assert!(apply_guards(req).await.is_ok());

    // The first principal's is now spent, though the address limit is not.
    let mut req = request(Transport::HttpAsync, &runtime, &dl, &data, &meta);
    req.header = &acme;
    assert!(matches!(
        apply_guards(req).await,
        Err(OrionError::RateLimited(_))
    ));
}

/// The claims `check_auth` verified reach the key expression at `auth`,
/// which is the identity the address limiter could never see.
#[tokio::test]
async fn the_principal_key_reads_the_verified_claims() {
    let dl = engine();
    let runtime = Runtime::new()
        .principal_limiter(
            &dl,
            Arc::new(crate::channel::LocalRateLimitBackend::new(1, 1)),
            json!({"var": "auth.sub"}),
        )
        .build();
    let none = |_: &str| None;
    let claims = json!({"sub": "user-1", "scope": "read"});
    let other = json!({"sub": "user-2"});

    let check = async |claims: Option<&serde_json::Value>| {
        super::check_principal_rate_limit("ch", &runtime, &dl, "10.0.0.1", &none, claims).await
    };

    assert!(check(Some(&claims)).await.is_ok());
    // A different subject from the same address is a different bucket.
    assert!(check(Some(&other)).await.is_ok());
    // The first subject is spent.
    assert!(matches!(
        check(Some(&claims)).await,
        Err(OrionError::RateLimited(_))
    ));
}

/// No claims means no principal, so the key cannot be computed. Refused as
/// its own condition rather than falling back to the address, which would
/// silently turn a per-user quota into a per-address one. Validation keeps
/// this state off a stored channel; the guard does not rely on that.
#[tokio::test]
async fn a_principal_key_with_no_claims_refuses_rather_than_falling_back() {
    let dl = engine();
    let runtime = Runtime::new()
        .principal_limiter(
            &dl,
            Arc::new(crate::channel::LocalRateLimitBackend::new(100, 100)),
            json!({"var": "auth.sub"}),
        )
        .build();
    let none = |_: &str| None;
    assert!(matches!(
        super::check_principal_rate_limit("ch", &runtime, &dl, "10.0.0.1", &none, None).await,
        Err(OrionError::RateLimitKeyUnavailable(_))
    ));
}

/// #275: a `key_logic` reading a header the context does not carry resolves
/// to `null` — datalogic returns `null` for a missing path rather than
/// erroring. Serialized into the key, that makes the bucket the literal
/// string `"null"` for *every* caller: an intended per-device quota silently
/// becomes one shared channel-wide bucket, with no warning and no metric.
///
/// A key that resolved to nothing has not been computed, so it takes the
/// same path as an evaluation error: refuse, per N5.
#[tokio::test]
async fn a_null_rate_limit_key_refuses_rather_than_collapsing() {
    let dl = engine();
    let runtime = Runtime::new()
        .limiter(
            // Deliberately generous: if the request is refused it is
            // because the key is unavailable, never because a bucket ran
            // dry.
            Arc::new(crate::channel::LocalRateLimitBackend::new(1000, 1000)),
            BackendErrorPolicy::Allow,
        )
        // `deviceid` is neither a built-in nor declared.
        .key_logic(&dl, json!({"var": "headers.deviceid"}))
        .build();
    let (data, meta) = (json!({}), json!({}));

    let device = |name: &str| (name == "deviceid").then_some("phone-1");
    let mut req = request(Transport::HttpSync, &runtime, &dl, &data, &meta);
    req.header = &device;

    let verdict = apply_guards(req).await;
    assert!(
        matches!(verdict, Err(OrionError::RateLimitKeyUnavailable(_))),
        "an unreachable header must refuse, not collapse every caller into one bucket"
    );
    // The caller learns nothing about the channel's configuration.
    let (status, code, _) = verdict.err().expect("a refusal").response_parts();
    assert_eq!(status, axum::http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(code, "RATE_LIMITED");
}

/// The behavioural statement behind the fix, phrased so it fails loudly if
/// the collapse ever returns: two distinct callers on a **1 rps** limiter
/// are both refused *for key unavailability*. With a shared `"null"` bucket
/// the first would be admitted and the second refused as over-limit — so
/// asserting the variant, not just the refusal, is what makes this test
/// meaningful.
#[tokio::test]
async fn two_callers_with_a_null_key_do_not_share_a_bucket() {
    let dl = engine();
    let runtime = Runtime::new()
        .limiter(
            Arc::new(crate::channel::LocalRateLimitBackend::new(1, 1)),
            BackendErrorPolicy::Allow,
        )
        .key_logic(&dl, json!({"var": "headers.deviceid"}))
        .build();
    let (data, meta) = (json!({}), json!({}));

    for caller in ["phone-1", "phone-2"] {
        let lookup = move |name: &str| (name == "deviceid").then_some(caller);
        let mut req = request(Transport::HttpSync, &runtime, &dl, &data, &meta);
        req.header = &lookup;
        assert!(
            matches!(
                apply_guards(req).await,
                Err(OrionError::RateLimitKeyUnavailable(_))
            ),
            "{caller} must be refused for an uncomputable key, never counted \
                 in a shared bucket"
        );
    }
}

/// The same collapse by a different route: the header is present but its
/// value is empty, so every caller keys on `""`. Refused for the same
/// reason a `null` is.
#[tokio::test]
async fn an_empty_rate_limit_key_refuses() {
    let dl = engine();
    let runtime = Runtime::new()
        .limiter(
            Arc::new(crate::channel::LocalRateLimitBackend::new(1000, 1000)),
            BackendErrorPolicy::Allow,
        )
        .key_logic(&dl, json!({"var": "headers.x-tenant-id"}))
        .build();
    let (data, meta) = (json!({}), json!({}));

    let blank = |name: &str| (name == "x-tenant-id").then_some("   ");
    let mut req = request(Transport::HttpSync, &runtime, &dl, &data, &meta);
    req.header = &blank;
    assert!(
        matches!(
            apply_guards(req).await,
            Err(OrionError::RateLimitKeyUnavailable(_))
        ),
        "a blank key is as unusable as a missing one"
    );
}

/// #275 part 2: a channel declares the header it keys on, and it becomes
/// visible to `key_logic` on every transport, which is what makes a
/// per-device limit expressible.
#[tokio::test]
async fn a_declared_custom_header_is_visible_to_key_logic() {
    for transport in [
        Transport::HttpSync,
        Transport::HttpAsync,
        Transport::ChannelCall,
    ] {
        let dl = engine();
        let runtime = Runtime::new()
            .limiter(
                Arc::new(crate::channel::LocalRateLimitBackend::new(1, 1)),
                BackendErrorPolicy::Allow,
            )
            .key_headers(&["deviceid"])
            .key_logic(&dl, json!({"var": "headers.deviceid"}))
            .build();
        let (data, meta) = (json!({}), json!({}));

        let phone = |name: &str| (name == "deviceid").then_some("phone");
        let tablet = |name: &str| (name == "deviceid").then_some("tablet");

        // Each device gets its own bucket...
        let mut req = request(transport, &runtime, &dl, &data, &meta);
        req.header = &phone;
        assert!(
            apply_guards(req).await.is_ok(),
            "{transport:?}: first device"
        );

        let mut req = request(transport, &runtime, &dl, &data, &meta);
        req.header = &tablet;
        assert!(
            apply_guards(req).await.is_ok(),
            "{transport:?}: a second device must not share the first's bucket"
        );

        // ...and the first device's is now spent.
        let mut req = request(transport, &runtime, &dl, &data, &meta);
        req.header = &phone;
        assert!(
            matches!(apply_guards(req).await, Err(OrionError::RateLimited(_))),
            "{transport:?}: the first device's own bucket must be empty"
        );
    }
}

/// Declaring one header must not open the whole header map: an undeclared
/// name still resolves to `null` and still refuses. Without this, the
/// declaration mechanism could silently become "expose everything".
#[tokio::test]
async fn declaring_one_header_does_not_expose_the_others() {
    let dl = engine();
    let runtime = Runtime::new()
        .limiter(
            Arc::new(crate::channel::LocalRateLimitBackend::new(1000, 1000)),
            BackendErrorPolicy::Allow,
        )
        .key_headers(&["deviceid"])
        .key_logic(&dl, json!({"var": "headers.x-partner"}))
        .build();
    let (data, meta) = (json!({}), json!({}));

    let both = |name: &str| match name {
        "deviceid" => Some("phone"),
        "x-partner" => Some("acme"),
        _ => None,
    };
    let mut req = request(Transport::HttpSync, &runtime, &dl, &data, &meta);
    req.header = &both;
    assert!(
        matches!(
            apply_guards(req).await,
            Err(OrionError::RateLimitKeyUnavailable(_))
        ),
        "an undeclared header stays invisible even when another is declared"
    );
}

/// Redundantly declaring a built-in changes nothing — it must not produce
/// a duplicate entry or a second lookup.
#[tokio::test]
async fn redeclaring_a_builtin_header_is_harmless() {
    let dl = engine();
    let runtime = Runtime::new()
        .limiter(
            Arc::new(crate::channel::LocalRateLimitBackend::new(1, 1)),
            BackendErrorPolicy::Allow,
        )
        .key_headers(&["x-tenant-id"])
        .key_logic(&dl, json!({"var": "headers.x-tenant-id"}))
        .build();
    let (data, meta) = (json!({}), json!({}));

    let acme = |name: &str| (name == "x-tenant-id").then_some("acme");
    let mut req = request(Transport::HttpSync, &runtime, &dl, &data, &meta);
    req.header = &acme;
    assert!(apply_guards(req).await.is_ok());

    let mut req = request(Transport::HttpSync, &runtime, &dl, &data, &meta);
    req.header = &acme;
    assert!(
        matches!(apply_guards(req).await, Err(OrionError::RateLimited(_))),
        "the bucket must behave exactly as an undeclared built-in does"
    );
}

/// The load-time warning's input: which `headers.*` names a stored
/// expression statically reads. A dynamically-composed path is skipped
/// rather than guessed, so the warning never fires on a false positive.
#[test]
fn key_logic_header_paths_reads_only_static_var_nodes() {
    assert_eq!(
        key_logic_header_paths(&json!({"var": "headers.deviceid"})),
        vec!["deviceid".to_string()]
    );
    // Nested inside another operator, and with a default value.
    assert_eq!(
        key_logic_header_paths(&json!({
            "cat": [{"var": "client_ip"}, ":", {"var": ["headers.X-Partner", "none"]}]
        })),
        vec!["x-partner".to_string()]
    );
    // Two names, deduplicated, order preserved.
    assert_eq!(
        key_logic_header_paths(&json!({
            "cat": [{"var": "headers.a"}, {"var": "headers.b"}, {"var": "headers.a"}]
        })),
        vec!["a".to_string(), "b".to_string()]
    );
    // Not a header path, or not statically knowable: no claim made.
    assert!(key_logic_header_paths(&json!({"var": "client_ip"})).is_empty());
    assert!(key_logic_header_paths(&json!({"var": "headers."})).is_empty());
    assert!(
        key_logic_header_paths(&json!({"var": {"cat": ["headers.", {"var": "x"}]}})).is_empty(),
        "a composed path must not be guessed at"
    );
}

// ---- Backpressure, on every transport ----

/// The permit is per channel, not per transport: `channel_call` and cron
/// work count against the same `max_concurrent_per_node` as HTTP work.
#[tokio::test]
async fn backpressure_permits_are_shared_across_transports() {
    let dl = engine();
    let runtime = Runtime::new().backpressure(1).build();
    let (data, meta) = (json!({}), json!({}));

    // One HTTP request holds the channel's only permit...
    let held = admitted(
        apply_guards(request(Transport::HttpSync, &runtime, &dl, &data, &meta))
            .await
            .expect("first admission"),
    )
    .expect("an admission, not a cache hit");
    assert!(held.backpressure_permit.is_some());

    // ...so a channel_call and a cron occurrence are both shed.
    for transport in [Transport::ChannelCall, Transport::Cron] {
        let verdict = apply_guards(request(transport, &runtime, &dl, &data, &meta)).await;
        assert!(
            matches!(verdict, Err(OrionError::ServiceUnavailable { .. })),
            "{transport:?} must be shed while the permit is held"
        );
    }

    // Releasing it re-admits them.
    drop(held);
    assert!(
        apply_guards(request(Transport::ChannelCall, &runtime, &dl, &data, &meta))
            .await
            .is_ok()
    );
}

// ---- Origin allow-list (N24) ----

#[tokio::test]
async fn the_origin_allow_list_applies_to_http_only() {
    let dl = engine();
    let runtime = Runtime::new().origins(&["https://allowed.example"]).build();
    let (data, meta) = (json!({}), json!({}));

    for transport in [Transport::HttpSync, Transport::HttpAsync] {
        let mut req = request(transport, &runtime, &dl, &data, &meta);
        req.origin = Some("https://evil.example");
        assert!(
            matches!(
                apply_guards(req).await,
                Err(OrionError::Http {
                    status: orion_common::errors::StatusCode::FORBIDDEN,
                    ..
                })
            ),
            "{transport:?} must refuse an unlisted origin"
        );
    }
    // An in-process call has no browsing context; an origin asserted
    // there means nothing and is not checked.
    for transport in [Transport::ChannelCall, Transport::Cron] {
        let mut req = request(transport, &runtime, &dl, &data, &meta);
        req.origin = Some("https://evil.example");
        assert!(
            apply_guards(req).await.is_ok(),
            "{transport:?} does not check origins"
        );
    }
}

/// N24: the pre-1.0 `{"cors": {"allowed_origins": [...]}}` spelling cannot
/// reach this layer at all.
///
/// The failure mode this pins is the one that makes the rename a security
/// question rather than a documentation one: if the old key merely parsed
/// and was dropped, the channel would arrive here with `allowed_origins()`
/// returning `None` — indistinguishable from a channel that deliberately
/// checks nothing — and every unlisted origin would be admitted. Refusing
/// the config means no such runtime exists, so the guard can never be
/// silently absent; the channel is quarantined instead.
#[tokio::test]
async fn the_pre_1_0_cors_spelling_cannot_produce_a_runtime() {
    let stored = r#"{"cors": {"allowed_origins": ["https://allowed.example"]}}"#;
    assert!(
        serde_json::from_str::<crate::channel::ChannelConfig>(stored).is_err(),
        "the old spelling must fail the config, not silently drop the allow-list"
    );

    // And the shape it would have degraded to — no allow-list at all —
    // admits the origin it was written to refuse. This is what the parse
    // refusal above prevents from ever being reached.
    let dl = engine();
    let runtime = Runtime::new().build();
    let (data, meta) = (json!({}), json!({}));
    let mut req = request(Transport::HttpSync, &runtime, &dl, &data, &meta);
    req.origin = Some("https://evil.example");
    assert!(
        apply_guards(req).await.is_ok(),
        "a channel with no allow-list checks nothing — which is why the \
             old spelling must not degrade into one"
    );
}

// ---- Validation ----

#[tokio::test]
async fn validation_logic_applies_on_every_transport() {
    let dl = engine();
    let runtime = Runtime::new()
        .validation(&dl, json!({"!!": {"var": "data.order_id"}}))
        .build();
    let bad = json!({});
    let good = json!({"order_id": "ORD-1"});
    let meta = json!({});
    for transport in [
        Transport::HttpSync,
        Transport::HttpAsync,
        Transport::ChannelCall,
        Transport::Cron,
    ] {
        assert!(
            matches!(
                apply_guards(request(transport, &runtime, &dl, &bad, &meta)).await,
                Err(OrionError::Validation { .. })
            ),
            "{transport:?} must reject"
        );
        assert!(
            apply_guards(request(transport, &runtime, &dl, &good, &meta))
                .await
                .is_ok(),
            "{transport:?} must accept"
        );
    }
}

// ---- Deduplication ----

#[tokio::test]
async fn test_dedup_new_key_passes() {
    let cfg = dedup_runtime(StubOutcome::New);
    let result =
        super::check_deduplication("test-channel", &cfg, &idempotency_lookup, None, None).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_dedup_duplicate_rejected() {
    let cfg = dedup_runtime(StubOutcome::Duplicate);
    let result =
        super::check_deduplication("test-channel", &cfg, &idempotency_lookup, None, None).await;
    assert!(matches!(
        result,
        Err(OrionError::Http {
            status: orion_common::errors::StatusCode::CONFLICT,
            ..
        })
    ));
}

#[tokio::test]
async fn test_dedup_fails_open_on_backend_error_by_default() {
    let cfg = dedup_runtime(StubOutcome::BackendError);
    let result =
        super::check_deduplication("test-channel", &cfg, &idempotency_lookup, None, None).await;
    assert!(result.is_ok(), "backend errors must fail open, not 409");
}

/// N7: `on_backend_error = "deny"` fails closed — a 503, never a 409,
/// because the request is unverifiable rather than a known duplicate.
#[tokio::test]
async fn test_dedup_fails_closed_when_policy_is_deny() {
    let cfg = dedup_runtime_with_policy(StubOutcome::BackendError, BackendErrorPolicy::Deny);
    let result =
        super::check_deduplication("test-channel", &cfg, &idempotency_lookup, None, None).await;
    assert!(
        matches!(result, Err(OrionError::ServiceUnavailable { .. })),
        "deny must refuse with 503"
    );
}

/// The deny policy only fires on backend errors — a healthy backend
/// answers normally under either policy.
#[tokio::test]
async fn test_deny_policy_does_not_affect_healthy_backend() {
    let cfg = dedup_runtime_with_policy(StubOutcome::New, BackendErrorPolicy::Deny);
    let result =
        super::check_deduplication("test-channel", &cfg, &idempotency_lookup, None, None).await;
    assert!(result.is_ok());
    let cfg = dedup_runtime_with_policy(StubOutcome::Duplicate, BackendErrorPolicy::Deny);
    let result =
        super::check_deduplication("test-channel", &cfg, &idempotency_lookup, None, None).await;
    assert!(matches!(
        result,
        Err(OrionError::Http {
            status: orion_common::errors::StatusCode::CONFLICT,
            ..
        })
    ));
}

#[tokio::test]
async fn test_dedup_key_is_channel_scoped() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let cfg = Runtime::new()
        .dedup(
            Arc::new(CapturingDedupBackend { seen: seen.clone() }),
            BackendErrorPolicy::Allow,
        )
        .build();
    super::check_deduplication("orders", &cfg, &idempotency_lookup, None, None)
        .await
        .expect("dedup check should pass");
    let keys = seen.lock().expect("test lock poisoned");
    assert_eq!(keys.as_slice(), ["dedup:orders:token-1"]);
}

/// N16: a key the transport carries out of band is the idempotency key
/// when the message carries no header of the configured name — an
/// at-least-once redelivery of the same key must not run the workflow
/// twice.
#[tokio::test]
async fn an_out_of_band_key_is_the_fallback_idempotency_key() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let cfg = Runtime::new()
        .dedup(
            Arc::new(CapturingDedupBackend { seen: seen.clone() }),
            BackendErrorPolicy::Allow,
        )
        .build();
    super::check_deduplication("orders", &cfg, &no_headers, Some("ORD-77"), None)
        .await
        .expect("dedup check should pass");
    assert_eq!(
        seen.lock().expect("test lock poisoned").as_slice(),
        ["dedup:orders:ORD-77"]
    );

    // An explicit header still wins over the out-of-band key.
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let cfg = Runtime::new()
        .dedup(
            Arc::new(CapturingDedupBackend { seen: seen.clone() }),
            BackendErrorPolicy::Allow,
        )
        .build();
    super::check_deduplication("orders", &cfg, &idempotency_lookup, Some("ORD-77"), None)
        .await
        .expect("dedup check should pass");
    assert_eq!(
        seen.lock().expect("test lock poisoned").as_slice(),
        ["dedup:orders:token-1"]
    );
}

/// A `channel_call` inherits the originating request's idempotency key,
/// so deduplicating it would reject the second call of a fan-out that
/// legitimately calls one channel twice. The exclusion is enforced, not
/// merely documented.
#[tokio::test]
async fn channel_call_is_not_deduplicated() {
    let dl = engine();
    let runtime = Runtime::new()
        .dedup(
            Arc::new(StubDedupBackend {
                outcome: StubOutcome::Duplicate,
            }),
            BackendErrorPolicy::Allow,
        )
        .build();
    let (data, meta) = (json!({}), json!({}));

    let mut req = request(Transport::ChannelCall, &runtime, &dl, &data, &meta);
    req.header = &idempotency_lookup;
    assert!(
        apply_guards(req).await.is_ok(),
        "channel_call must not consult the dedup store"
    );

    // The same store refuses the same key over HTTP.
    for transport in [Transport::HttpSync, Transport::HttpAsync] {
        let mut req = request(transport, &runtime, &dl, &data, &meta);
        req.header = &idempotency_lookup;
        assert!(
            matches!(
                apply_guards(req).await,
                Err(OrionError::Http {
                    status: orion_common::errors::StatusCode::CONFLICT,
                    ..
                })
            ),
            "{transport:?} must reject a duplicate"
        );
    }
}

// ---- The dedup claim: who holds a key, and until when ----

/// The bug the claim exists to prevent. Claiming a key at admission and
/// never revisiting it destroys every message a redelivering transport
/// has to retry: attempt 0 registers the key, the attempt fails without
/// committing, and the redelivery reads the key its own previous attempt
/// wrote. A transport that reads that `409` as "already handled" drops
/// the message — never processed, never dead-lettered.
#[tokio::test]
async fn a_redelivery_recognises_its_own_unsettled_claim() {
    let dl = engine();
    let store = Arc::new(InMemoryDedupBackend::default());
    let runtime = Runtime::new()
        .dedup(store.clone(), BackendErrorPolicy::Allow)
        .build();
    let (data, meta) = (json!({}), json!({}));

    // Attempt 0 of delivery 7 claims the key...
    let mut req = request(Transport::HttpAsync, &runtime, &dl, &data, &meta);
    req.dedup_key_fallback = Some("ORD-77");
    req.dedup_owner = Some("delivery-7");
    let admission =
        admitted(apply_guards(req).await.expect("first attempt admitted")).expect("an admission");
    // ...and fails, so the delivery is not acknowledged and the claim is
    // handed back.
    admission
        .dedup_claim
        .expect("a claim was taken")
        .release()
        .await;

    // The retry is the same delivery, so it must run.
    let mut req = request(Transport::HttpAsync, &runtime, &dl, &data, &meta);
    req.dedup_key_fallback = Some("ORD-77");
    req.dedup_owner = Some("delivery-7");
    assert!(
        apply_guards(req).await.is_ok(),
        "a redelivery of an unacknowledged message must be processed, not dropped as a duplicate"
    );
}

/// The same, without the release: a process killed mid-dispatch never
/// runs `release`, and the claim it left behind is still in the store
/// when the message comes back. It must recognise the claim as its own.
#[tokio::test]
async fn a_claim_left_behind_by_a_dead_attempt_does_not_suppress_the_message() {
    let dl = engine();
    let store = Arc::new(InMemoryDedupBackend::default());
    let runtime = Runtime::new()
        .dedup(store.clone(), BackendErrorPolicy::Allow)
        .build();
    let (data, meta) = (json!({}), json!({}));

    for _ in 0..3 {
        let mut req = request(Transport::HttpAsync, &runtime, &dl, &data, &meta);
        req.dedup_key_fallback = Some("ORD-77");
        req.dedup_owner = Some("delivery-7");
        let admission = admitted(apply_guards(req).await.expect("admitted"))
            .expect("an admission, not a cache hit");
        // Claim dropped, never settled — the process died here.
        drop(admission);
    }
}

/// Confirming is what makes the guard useful on a redelivering transport:
/// once a delivery is acknowledged, a *later* message carrying the same
/// key — a producer retry, or this message replayed because its
/// acknowledgement was lost — is the duplicate the channel asked to
/// suppress.
#[tokio::test]
async fn a_confirmed_claim_suppresses_every_later_delivery() {
    let dl = engine();
    let store = Arc::new(InMemoryDedupBackend::default());
    let runtime = Runtime::new()
        .dedup(store.clone(), BackendErrorPolicy::Allow)
        .build();
    let (data, meta) = (json!({}), json!({}));

    let mut req = request(Transport::HttpAsync, &runtime, &dl, &data, &meta);
    req.dedup_key_fallback = Some("ORD-77");
    req.dedup_owner = Some("delivery-7");
    let admission = admitted(apply_guards(req).await.expect("admitted")).expect("an admission");
    admission
        .dedup_claim
        .expect("a claim was taken")
        .confirm()
        .await;

    // The producer re-sends the same key as a later delivery.
    let mut req = request(Transport::HttpAsync, &runtime, &dl, &data, &meta);
    req.dedup_key_fallback = Some("ORD-77");
    req.dedup_owner = Some("delivery-12");
    assert!(
        matches!(
            apply_guards(req).await,
            Err(OrionError::Http {
                status: orion_common::errors::StatusCode::CONFLICT,
                ..
            })
        ),
        "a settled key must refuse a later message carrying it"
    );

    // And so is a replay of the original delivery whose acknowledgement
    // was lost:
    // "settled" belongs to no owner, so nobody reclaims it.
    let mut req = request(Transport::HttpAsync, &runtime, &dl, &data, &meta);
    req.dedup_key_fallback = Some("ORD-77");
    req.dedup_owner = Some("delivery-7");
    assert!(
        matches!(
            apply_guards(req).await,
            Err(OrionError::Http {
                status: orion_common::errors::StatusCode::CONFLICT,
                ..
            })
        ),
        "a settled key must refuse a replay of the delivery that settled it"
    );
}

/// A transport that cannot redeliver gets a token nothing can present
/// twice, so two HTTP requests carrying one idempotency key are the
/// `409` they have always been — the owner comparison must not turn every
/// duplicate into a "redelivery".
#[tokio::test]
async fn two_http_requests_with_one_key_are_still_a_duplicate() {
    let dl = engine();
    let store = Arc::new(InMemoryDedupBackend::default());
    let runtime = Runtime::new()
        .dedup(store.clone(), BackendErrorPolicy::Allow)
        .build();
    let (data, meta) = (json!({}), json!({}));

    let mut req = request(Transport::HttpSync, &runtime, &dl, &data, &meta);
    req.header = &idempotency_lookup;
    assert!(apply_guards(req).await.is_ok(), "the first request passes");

    let mut req = request(Transport::HttpSync, &runtime, &dl, &data, &meta);
    req.header = &idempotency_lookup;
    assert!(
        matches!(
            apply_guards(req).await,
            Err(OrionError::Http {
                status: orion_common::errors::StatusCode::CONFLICT,
                ..
            })
        ),
        "the second must be refused"
    );
}

/// Backpressure is the one guard that can refuse *after* the key is
/// claimed. A shed request ran nothing, so it must not burn the key: the
/// caller's retry has to be judged on its own merits.
#[tokio::test]
async fn a_shed_request_releases_the_key_it_claimed() {
    let dl = engine();
    let store = Arc::new(InMemoryDedupBackend::default());
    let runtime = Runtime::new()
        .dedup(store.clone(), BackendErrorPolicy::Allow)
        .backpressure(1)
        .build();
    let (data, meta) = (json!({}), json!({}));

    // Hold the channel's only permit with a request carrying no key.
    let held = admitted(
        apply_guards(request(Transport::HttpSync, &runtime, &dl, &data, &meta))
            .await
            .expect("first admission"),
    )
    .expect("an admission");

    // A keyed request is shed...
    let mut req = request(Transport::HttpSync, &runtime, &dl, &data, &meta);
    req.header = &idempotency_lookup;
    assert!(matches!(
        apply_guards(req).await,
        Err(OrionError::ServiceUnavailable { .. })
    ));
    assert!(
        store
            .get("dedup:orders:token-1")
            .await
            .expect("store readable")
            .is_none(),
        "a shed request must leave no claim behind"
    );

    // ...and its retry, once capacity is back, is not a duplicate.
    drop(held);
    let mut req = request(Transport::HttpSync, &runtime, &dl, &data, &meta);
    req.header = &idempotency_lookup;
    assert!(
        apply_guards(req).await.is_ok(),
        "the retry of a shed request must not be refused as a duplicate"
    );
}

// ---- Response cache ----

/// Only the synchronous HTTP path may be answered from the cache; the
/// other three have no response to serve or store.
#[tokio::test]
async fn only_sync_http_reads_the_response_cache() {
    let dl = engine();
    let runtime = Runtime::new().cache(Arc::new(AlwaysHitCache)).build();
    let (data, meta) = (json!({}), json!({}));

    let verdict = apply_guards(request(Transport::HttpSync, &runtime, &dl, &data, &meta))
        .await
        .expect("guards pass");
    assert!(matches!(verdict, GuardVerdict::CacheHit(ref body) if body.contains("cached")));

    for transport in [
        Transport::HttpAsync,
        Transport::ChannelCall,
        Transport::Cron,
    ] {
        let verdict = apply_guards(request(transport, &runtime, &dl, &data, &meta))
            .await
            .expect("guards pass");
        let admission = admitted(verdict).expect("an admission, not a cache hit");
        assert!(
            admission.cache_store.is_none(),
            "{transport:?} must neither read nor write the response cache"
        );
    }
}

/// #354: a namespaced entry is judged against the namespace versions, and
/// is stored under the versions read at *lookup*. An invalidation landing
/// while the workflow runs must leave the entry that run stores stale.
#[tokio::test]
async fn a_namespaced_entry_is_retired_by_a_version_bump() {
    use crate::channel::cache_namespace::{invalidate, version_key};
    use crate::connector::cache_backend::MemoryCacheBackend;

    let dl = engine();
    let store: Arc<dyn CacheBackend> = MemoryCacheBackend::new(60, 0);
    let runtime = Runtime::new()
        .cache(store.clone())
        .namespaces(&["ladder", "season"])
        .build();
    let (data, meta) = (json!({"q": 1}), json!({}));
    let lookup = || async {
        apply_guards(request(Transport::HttpSync, &runtime, &dl, &data, &meta))
            .await
            .expect("guards pass")
    };
    let ns = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();

    // Miss, store, hit.
    let ctx = admitted(lookup().await)
        .and_then(|a| a.cache_store)
        .expect("a miss carries a store context");
    ctx.store("{\"v\":1}").await.expect("store");
    assert!(matches!(lookup().await, GuardVerdict::CacheHit(ref b) if b == "{\"v\":1}"));

    // Bumping one namespace of the two retires the entry.
    invalidate(std::slice::from_ref(&store), &ns(&["season"]), "workflow")
        .await
        .expect("bump");
    let ctx = admitted(lookup().await)
        .and_then(|a| a.cache_store)
        .expect("stale after the bump");

    // An invalidation lands while this request's workflow runs: what the
    // run stores carries the versions it looked up under, so it is stale
    // on arrival rather than pinned under the new version.
    invalidate(std::slice::from_ref(&store), &ns(&["ladder"]), "workflow")
        .await
        .expect("bump");
    ctx.store("{\"v\":2}").await.expect("store");
    assert!(
        admitted(lookup().await).is_some(),
        "an entry computed before an invalidation must not be served after it"
    );
    assert_eq!(
        store.get(&version_key("ladder")).await.expect("get"),
        Some("1".to_string())
    );
}

/// #354: with `coalesce_misses`, the first miss runs the workflow and the
/// concurrent ones wait for it, holding nothing, and are served its entry.
#[tokio::test]
async fn coalesced_misses_wait_for_the_leader_and_share_its_entry() {
    use crate::connector::cache_backend::MemoryCacheBackend;
    let dl = engine();
    let store: Arc<dyn CacheBackend> = MemoryCacheBackend::new(60, 0);
    let runtime = Runtime::new()
        .cache(store)
        .coalesce()
        .backpressure(1)
        .build();
    let (data, meta) = (json!({"q": 1}), json!({}));
    let lookup = || async {
        apply_guards(request(Transport::HttpSync, &runtime, &dl, &data, &meta))
            .await
            .expect("guards pass")
    };

    let leader = admitted(lookup().await).expect("the first miss leads");
    assert!(leader.cache_store.is_some());
    let flights = runtime
        .as_ref()
        .and_then(|r| r.cache_flights.clone())
        .expect("coalescing builds a flight table");
    assert_eq!(flights.in_flight(), 1);

    let lead = async move {
        // The followers are parked by now; none has run, and none holds
        // the one backpressure permit, which the leader has.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let ctx = leader.cache_store.as_ref().expect("ctx");
        ctx.store("{\"run\":1}").await.expect("store");
        drop(leader);
    };
    let (a, b, c, ()) = tokio::join!(lookup(), lookup(), lookup(), lead);
    for verdict in [a, b, c] {
        assert!(
            matches!(verdict, GuardVerdict::CacheHit(ref body) if body == "{\"run\":1}"),
            "a follower is served the leader's entry"
        );
    }
    assert_eq!(flights.in_flight(), 0, "the flight ends with its leader");
}

/// A leader that stores nothing — its workflow failed — releases the
/// followers to run the workflow themselves rather than hold them.
#[tokio::test]
async fn a_leader_that_stores_nothing_releases_its_followers() {
    use crate::connector::cache_backend::MemoryCacheBackend;
    let dl = engine();
    let store: Arc<dyn CacheBackend> = MemoryCacheBackend::new(60, 0);
    let runtime = Runtime::new().cache(store).coalesce().build();
    let (data, meta) = (json!({}), json!({}));
    let lookup = || async {
        apply_guards(request(Transport::HttpSync, &runtime, &dl, &data, &meta))
            .await
            .expect("guards pass")
    };
    let leader = admitted(lookup().await).expect("leads");
    let lead = async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(leader);
    };
    let started = std::time::Instant::now();
    let (a, b, ()) = tokio::join!(lookup(), lookup(), lead);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "released, not timed out"
    );
    for verdict in [a, b] {
        let admission = admitted(verdict).expect("a miss, run by the follower");
        assert!(
            admission.cache_store.is_some(),
            "and it may store its own result"
        );
    }
}

/// A counter that is not an integer cannot judge an entry, so the request
/// bypasses the cache and nothing is stored.
#[tokio::test]
async fn a_foreign_namespace_counter_bypasses_the_cache() {
    use crate::connector::cache_backend::MemoryCacheBackend;
    let dl = engine();
    let store: Arc<dyn CacheBackend> = MemoryCacheBackend::new(60, 0);
    store
        .set(
            &crate::channel::cache_namespace::version_key("ladder"),
            "\"x\"",
        )
        .await
        .expect("set");
    let runtime = Runtime::new().cache(store).namespaces(&["ladder"]).build();
    let (data, meta) = (json!({}), json!({}));
    let admission = admitted(
        apply_guards(request(Transport::HttpSync, &runtime, &dl, &data, &meta))
            .await
            .expect("guards pass"),
    )
    .expect("admitted");
    assert!(admission.cache_store.is_none());
}

/// A duplicate is answered `409` before the cache is consulted: a
/// replayed idempotency key must not be served a cached success.
#[tokio::test]
async fn dedup_precedes_the_cache_lookup() {
    let dl = engine();
    let runtime = Runtime::new()
        .dedup(
            Arc::new(StubDedupBackend {
                outcome: StubOutcome::Duplicate,
            }),
            BackendErrorPolicy::Allow,
        )
        .cache(Arc::new(AlwaysHitCache))
        .build();
    let (data, meta) = (json!({}), json!({}));

    let mut req = request(Transport::HttpSync, &runtime, &dl, &data, &meta);
    req.header = &idempotency_lookup;
    assert!(matches!(
        apply_guards(req).await,
        Err(OrionError::Http {
            status: orion_common::errors::StatusCode::CONFLICT,
            ..
        })
    ));
}

/// A cache hit must not take a backpressure permit — it did no work
/// worth shedding.
#[tokio::test]
async fn a_cache_hit_takes_no_backpressure_permit() {
    let dl = engine();
    let runtime = Runtime::new()
        .cache(Arc::new(AlwaysHitCache))
        .backpressure(1)
        .build();
    let (data, meta) = (json!({}), json!({}));

    for _ in 0..3 {
        let verdict = apply_guards(request(Transport::HttpSync, &runtime, &dl, &data, &meta))
            .await
            .expect("guards pass");
        assert!(matches!(verdict, GuardVerdict::CacheHit(_)));
    }
    // The single permit is still free.
    let semaphore = runtime
        .as_ref()
        .expect("runtime")
        .backpressure_semaphore
        .as_ref()
        .expect("semaphore");
    assert_eq!(semaphore.available_permits(), 1);
}

// ---- Response cache key (proposal N1) ----

fn cache_cfg(fields: Option<Vec<String>>) -> crate::channel::ChannelCacheConfig {
    crate::channel::ChannelCacheConfig {
        enabled: true,
        ttl_secs: Some(60),
        cache_key_fields: fields,
        key_logic: None,
        connector: None,
        namespaces: None,
        coalesce_misses: false,
    }
}

/// `compute_cache_key` for the cases that must produce one.
fn key(
    channel: &str,
    data: &serde_json::Value,
    metadata: &serde_json::Value,
    cfg: &crate::channel::ChannelCacheConfig,
) -> String {
    super::response_cache::compute_cache_key(
        channel,
        &super::GuardContext::new(data.clone(), metadata.clone()),
        cfg,
        None,
        &engine(),
    )
    .expect("this request must have a cache key")
}

fn meta(method: &str, params: serde_json::Value, query: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "http_method": method,
        "params": params,
        "query": query,
        "headers": {},
    })
}

#[test]
fn test_cache_key_distinguishes_route_params() {
    let data = serde_json::json!({});
    let a = key(
        "orders",
        &data,
        &meta("GET", serde_json::json!({"id": "1"}), serde_json::json!({})),
        &cache_cfg(None),
    );
    let b = key(
        "orders",
        &data,
        &meta("GET", serde_json::json!({"id": "2"}), serde_json::json!({})),
        &cache_cfg(None),
    );
    assert_ne!(a, b, "different path params must not share a cache entry");
}

#[test]
fn test_cache_key_distinguishes_query_and_method() {
    let data = serde_json::json!({});
    let base = meta(
        "GET",
        serde_json::json!({}),
        serde_json::json!({"page": "1"}),
    );
    let a = key("orders", &data, &base, &cache_cfg(None));
    let b = key(
        "orders",
        &data,
        &meta(
            "GET",
            serde_json::json!({}),
            serde_json::json!({"page": "2"}),
        ),
        &cache_cfg(None),
    );
    let c = key(
        "orders",
        &data,
        &meta(
            "POST",
            serde_json::json!({}),
            serde_json::json!({"page": "1"}),
        ),
        &cache_cfg(None),
    );
    assert_ne!(a, b);
    assert_ne!(a, c);
}

#[test]
fn test_cache_key_stable_for_identical_requests() {
    let data = serde_json::json!({"order_id": 7});
    let m = meta(
        "GET",
        serde_json::json!({"id": "1"}),
        serde_json::json!({"expand": "items"}),
    );
    let a = key("orders", &data, &m, &cache_cfg(None));
    let b = key("orders", &data, &m, &cache_cfg(None));
    assert_eq!(a, b);
}

/// The spelling the docs have always shown. It resolved to nothing under
/// the original literal-key lookup, and a field that matches nothing
/// contributed nothing to the hash — so two different users landed on one
/// cache entry and the first response was served to both for the TTL.
#[test]
fn documented_data_prefixed_paths_distinguish_callers() {
    let m = meta("POST", serde_json::json!({}), serde_json::json!({}));
    let fields = Some(vec!["data.user_id".to_string(), "data.action".to_string()]);
    let alice = serde_json::json!({"user_id": "alice", "action": "balance"});
    let bob = serde_json::json!({"user_id": "bob", "action": "balance"});
    assert_ne!(
        key("acct", &alice, &m, &cache_cfg(fields.clone())),
        key("acct", &bob, &m, &cache_cfg(fields)),
        "distinct users must not share a cache entry"
    );
}

/// Nested paths walk the payload.
#[test]
fn dotted_paths_walk_into_nested_objects() {
    let m = meta("POST", serde_json::json!({}), serde_json::json!({}));
    let fields = Some(vec!["user.id".to_string()]);
    let a = serde_json::json!({"user": {"id": 1}});
    let b = serde_json::json!({"user": {"id": 2}});
    assert_ne!(
        key("c", &a, &m, &cache_cfg(fields.clone())),
        key("c", &b, &m, &cache_cfg(fields))
    );
}

/// The literal lookup is tried first, so a payload key that genuinely
/// contains a dot keeps resolving exactly as it did before paths existed.
#[test]
fn a_literal_dotted_payload_key_still_wins() {
    let m = meta("POST", serde_json::json!({}), serde_json::json!({}));
    let fields = Some(vec!["a.b".to_string()]);
    let flat_1 = serde_json::json!({"a.b": 1});
    let flat_2 = serde_json::json!({"a.b": 2});
    assert_ne!(
        key("c", &flat_1, &m, &cache_cfg(fields.clone())),
        key("c", &flat_2, &m, &cache_cfg(fields.clone()))
    );
    // The nested spelling resolves through the path fallback to the same
    // value, and therefore to the same key. That is the contract, not a
    // collision: `cache_key_fields` declares that these fields determine
    // the response, so two payloads agreeing on all of them are the same
    // request as far as the cache is concerned.
    let nested = serde_json::json!({"a": {"b": 1}});
    assert_eq!(
        key("c", &flat_1, &m, &cache_cfg(fields.clone())),
        key("c", &nested, &m, &cache_cfg(fields))
    );
}

/// The property the hash choice exists to serve: the same request must
/// key identically in every process, so a replica sharing a cache store
/// agrees with its peers. A per-process-seeded hash would pass every other
/// test in this module and fail only in production, on more than one node.
#[test]
fn the_key_is_stable_across_processes() {
    let m = meta(
        "POST",
        serde_json::json!({"id": "7"}),
        serde_json::json!({"expand": "items"}),
    );
    let data = serde_json::json!({"order_id": 7, "nested": {"a": [1, 2, 3]}});
    // Pinned literal: this value was produced by this code, and changing
    // the hash or the framing changes it. That is the point — a silent
    // change cold-starts every deployment's cache, so it should be a
    // deliberate edit to this line rather than a surprise in production.
    assert_eq!(
        key("orders", &data, &m, &cache_cfg(None)),
        "cache:orders:47396736ec3c2fde9455d2f9a9161e91"
    );
}

/// Map iteration order must not reach the key: `params` and `query` are
/// fed in sorted order so two spellings of one query string agree.
#[test]
fn key_ignores_map_ordering() {
    let data = serde_json::json!({});
    let a = meta(
        "GET",
        serde_json::json!({}),
        serde_json::json!({"a": "1", "b": "2"}),
    );
    let b = meta(
        "GET",
        serde_json::json!({}),
        serde_json::json!({"b": "2", "a": "1"}),
    );
    assert_eq!(
        key("c", &data, &a, &cache_cfg(None)),
        key("c", &data, &b, &cache_cfg(None))
    );
}

/// Length-prefixed framing: no rearrangement of adjacent chunks can be
/// re-read as a different request. Without it, a `params` key/value pair
/// could be shifted into the neighbouring field and hash alike.
#[test]
fn framing_separates_adjacent_chunks() {
    let data = serde_json::json!({});
    let a = meta("GET", serde_json::json!({"ab": "c"}), serde_json::json!({}));
    let b = meta("GET", serde_json::json!({"a": "bc"}), serde_json::json!({}));
    assert_ne!(
        key("c", &data, &a, &cache_cfg(None)),
        key("c", &data, &b, &cache_cfg(None))
    );
}

/// A payload that resolves *no* declared field has no meaningful key, so
/// the request is refused the cache rather than given a key it shares with
/// every other request on the channel.
#[test]
fn a_payload_matching_no_declared_field_has_no_key() {
    let m = meta("POST", serde_json::json!({}), serde_json::json!({}));
    let fields = Some(vec!["user_id".to_string(), "action".to_string()]);
    assert!(
        super::response_cache::compute_cache_key(
            "acct",
            &super::GuardContext::new(serde_json::json!({"unrelated": 1}), m),
            &cache_cfg(fields),
            None,
            &engine(),
        )
        .is_none()
    );
}

/// A field that is absent still feeds its name, so two payloads that each
/// resolve a *different* half of the field list cannot collide.
#[test]
fn absent_fields_are_not_silently_skipped() {
    let m = meta("POST", serde_json::json!({}), serde_json::json!({}));
    let fields = Some(vec!["a".to_string(), "b".to_string()]);
    assert_ne!(
        key(
            "c",
            &serde_json::json!({"a": 1}),
            &m,
            &cache_cfg(fields.clone())
        ),
        key("c", &serde_json::json!({"b": 1}), &m, &cache_cfg(fields))
    );
}

/// With both declared, `key_logic` decides the key and `cache_key_fields`
/// is ignored — the documented precedence (#354).
#[test]
fn key_logic_takes_precedence_over_cache_key_fields() {
    let engine = engine();
    let logic: Expr = engine
        .compile(&serde_json::json!({"var": "data.b"}))
        .expect("compiles");
    let m = meta("POST", serde_json::json!({}), serde_json::json!({}));
    let cfg = cache_cfg(Some(vec!["a".to_string()]));
    let k = |data: serde_json::Value| {
        super::response_cache::compute_cache_key(
            "c",
            &super::GuardContext::new(data, m.clone()),
            &cfg,
            Some(&logic),
            &engine,
        )
        .expect("key_logic resolves")
    };
    assert_ne!(
        k(serde_json::json!({"a": 1, "b": 1})),
        k(serde_json::json!({"a": 1, "b": 2})),
        "a differing key_logic value must yield a differing key"
    );
    assert_eq!(
        k(serde_json::json!({"a": 1, "b": 1})),
        k(serde_json::json!({"a": 2, "b": 1})),
        "cache_key_fields must not contribute once key_logic is declared"
    );
}

#[test]
fn test_cache_key_folds_route_identity_with_key_fields() {
    // Even with cache_key_fields selecting body fields, route identity
    // must still distinguish keys.
    let data = serde_json::json!({"tenant": "acme"});
    let fields = Some(vec!["tenant".to_string()]);
    let a = key(
        "orders",
        &data,
        &meta("GET", serde_json::json!({"id": "1"}), serde_json::json!({})),
        &cache_cfg(fields.clone()),
    );
    let b = key(
        "orders",
        &data,
        &meta("GET", serde_json::json!({"id": "2"}), serde_json::json!({})),
        &cache_cfg(fields),
    );
    assert_ne!(a, b);
}
