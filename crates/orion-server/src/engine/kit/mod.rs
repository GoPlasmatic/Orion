//! The handler kit (I189): what every dataflow-rs 4 function of Orion's
//! shares.
//!
//! - [`read`]: Orion's field readers, with today's coercions (I183).
//! - [`fields`]: the key-material and detached-expression field types.
//! - [`guarded`]: the connector shell — the circuit breaker and the
//!   connector metrics — around a connector function's work.
//! - [`resolve_connector`] and [`require_connector`]: the connector lookups,
//!   on dataflow-rs 4's error, and [`require_op`] and
//!   [`require_method_allowed`], the connector's operation gates.
//! - [`spec`]: a function's `schema.rs` row as its `FunctionSpec`.
//! - [`ProfileLayer`]: the per-request profile, as one registry layer
//!   around every host call.
//! - [`OfflineDeps`]: inert dependencies, so every function can be
//!   registered with no `AppState`.
//!
//! The errors are [`HandlerError`](crate::engine::HandlerError)s, which `?`
//! turns into dataflow-rs 4's `Error` with the codes 1.12 recorded (I184).

pub mod fields;
pub mod read;

pub use fields::{DetachedExpr, KeyMaterial, KeyMaterialList, key_material, key_material_list};

use std::borrow::Cow;
use std::future::Future;
use std::sync::Arc;

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::ExprEngine;
use dataflow_rs::function::{BoxFuture, CallLayer, FunctionSpec, Next, Reference};
use dataflow_rs::{Ctx, Outcome};

use crate::connector::{ConnectorConfig, ConnectorRegistry, ConnectorTarget, HttpOperationGates};
use crate::engine::functions::schema;
use crate::engine::{HandlerDeps, ResolvedSecrets};

/// The channel a call's run was started on, `"unknown"` when it was started
/// on none — what `extract_channel` answers from the message's metadata.
/// Borrowed for the run, not for `ctx`, so a call holds it while it writes.
pub fn channel<'r>(ctx: &Ctx<'r>) -> &'r str {
    ctx.info().channel.unwrap_or("unknown")
}

/// What `name`'s row in `schema.rs` says about the function, as its
/// [`FunctionSpec`] (I179): the row's description and category, its retry
/// safety on dataflow-rs 4's three answers (`Pure`, `Read` and
/// `IdempotentWrite` are `Idempotent`, `UnsafeWrite` is `NotIdempotent`,
/// `DependsOn` is `Unknown`), and, for a function that takes a connector,
/// its `connector` field as a reference of the function's own kind (I187),
/// which the authoring oracle judges by that function's `ConnectorRule`.
///
/// # Panics
///
/// When `schema.rs` has no row named `name`: a function registered under a
/// name Orion does not declare, which `kit`'s tests refuse for every
/// function `register_functions` registers.
pub fn spec(name: &str) -> FunctionSpec {
    let row = schema::registry()
        .iter()
        .find(|row| row.name == name)
        .expect("every function Orion registers has its schema.rs row");
    let retry = match row.retry_safety {
        schema::RetrySafety::Pure
        | schema::RetrySafety::Read
        | schema::RetrySafety::IdempotentWrite => dataflow_rs::function::RetrySafety::Idempotent,
        schema::RetrySafety::UnsafeWrite => dataflow_rs::function::RetrySafety::NotIdempotent,
        schema::RetrySafety::DependsOn { .. } => dataflow_rs::function::RetrySafety::Unknown,
    };
    let references = match row.connector {
        Some(_) => vec![Reference::new("connector", row.name)],
        None => Vec::new(),
    };
    FunctionSpec::default()
        .with_description(row.description)
        .with_category(row.category)
        .with_retry(retry)
        .with_references(references)
}

/// `connector_helpers::require_op` on dataflow-rs 4's error: a connector
/// whose gate for `op` is off refuses the call before anything is dialled.
///
/// # Errors
///
/// A `connector_detail` refusal when `allowed` is false: the caller is told
/// the request failed validation, and which operation is off rides as the
/// operator-only detail.
pub fn require_op(allowed: bool, op: &str, connector: &str) -> dataflow_rs::Result<()> {
    if allowed {
        return Ok(());
    }
    Err(crate::errors::service::connector_detail(format!(
        "operation '{op}' is disabled on connector '{connector}'"
    )))
}

/// `connector_helpers::require_method_allowed` on dataflow-rs 4's error: an
/// HTTP connector's method allow-list (F22e), where an empty list allows
/// every method.
///
/// # Errors
///
/// A `connector_detail` refusal naming the method and the allowed ones when
/// the list excludes `method`.
pub fn require_method_allowed(
    gates: &HttpOperationGates,
    method: &str,
    connector: &str,
) -> dataflow_rs::Result<()> {
    if gates.allows_method(method) {
        return Ok(());
    }
    Err(crate::errors::service::connector_detail(format!(
        "HTTP method '{method}' is not allowed on connector '{connector}' (allowed: {})",
        gates.methods.join(", ")
    )))
}

/// `guarded_handler` on dataflow-rs 4's types: the circuit breaker and the
/// connector metrics around one connector call's work.
///
/// **Only retryable failures trip the breaker.** A query the backend
/// *rejected* says nothing about the dependency's health, and counting it
/// would let one bad workflow trip the breaker on a healthy database and
/// take down every other channel using it. The breaker is a no-op when
/// `engine.circuit_breaker.enabled` is false (the default); the metrics are
/// unconditional, since observability is not resilience configuration.
///
/// `name` is the function's, for the breaker's log line. The profile sample
/// is not taken here: [`ProfileLayer`] wraps every host call, this one
/// included.
///
/// # Errors
///
/// The work's own error, or the open breaker's `circuit_open`, which is
/// retryable and never reaches the work.
pub async fn guarded<F>(
    name: &'static str,
    registry: &ConnectorRegistry,
    connector: &str,
    channel: &str,
    fut: F,
) -> dataflow_rs::Result<Outcome>
where
    F: Future<Output = dataflow_rs::Result<Outcome>>,
{
    if !registry.circuit_breaker_enabled() {
        return observed(connector, channel, fut).await;
    }

    // The registry composes the `channel:connector` key, so an operator's
    // keys keep meaning what they meant.
    let breaker = registry.get_or_create_breaker(channel, connector).await;
    if !breaker.check() {
        crate::metrics::record_circuit_breaker_rejection(connector, channel);
        return Err(crate::errors::service::circuit_open(connector, channel));
    }

    let result = observed(connector, channel, fut).await;
    match &result {
        Ok(_) => breaker.record_success(),
        Err(e) if e.is_retryable() => {
            if breaker.record_failure() {
                tracing::warn!(
                    function = name,
                    connector = connector,
                    channel = channel,
                    "Circuit breaker tripped"
                );
                crate::metrics::record_circuit_breaker_trip(connector, channel);
            }
        }
        // A failure the caller caused is not evidence about the dependency.
        Err(_) => {}
    }
    result
}

/// The connector request and latency metrics, around the work.
async fn observed<F>(connector: &str, channel: &str, fut: F) -> dataflow_rs::Result<Outcome>
where
    F: Future<Output = dataflow_rs::Result<Outcome>>,
{
    let start = std::time::Instant::now();
    let result = fut.await;
    let status = if result.is_ok() { "ok" } else { "error" };
    crate::metrics::record_connector_request(connector, channel, status);
    crate::metrics::record_connector_duration(connector, channel, start.elapsed().as_secs_f64());
    result
}

/// The per-request profile as one registry [`CallLayer`] (I189): while a
/// [`ProfileCollector`](crate::engine::profile::ProfileCollector) is in scope,
/// each host call is one sample, nested at the collector's depth, named by
/// the function it was called by and attributed to its input's `connector`,
/// or for `channel_call` the `channel` it reaches. With no collector in scope
/// the call is passed on untouched: no input is read and nothing is boxed
/// beyond the chain's own future, so an unprofiled call pays one task-local
/// read.
///
/// One layer for every host function, so the functions outside the
/// connector shell are profiled too, where 1.12 sampled only the connector
/// handlers and `channel_call`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProfileLayer;

impl CallLayer for ProfileLayer {
    fn call<'a>(
        &'a self,
        ctx: &'a mut Ctx<'_>,
        next: Next<'a>,
    ) -> BoxFuture<'a, dataflow_rs::Result<Outcome>> {
        let Ok(collector) = crate::engine::profile::ORION_PROFILE.try_with(Arc::clone) else {
            return next.run(ctx);
        };
        Box::pin(async move {
            let function = Cow::Owned(ctx.info().function.to_owned());
            let target = profile_target(ctx);
            crate::engine::profile::sample(&collector, function, target, next.run(ctx)).await
        })
    }
}

/// What a sample is attributed to: the input's `connector`, else the
/// `channel` a `channel_call` reaches (the label 1.12 gave it). Read from
/// [`Ctx::input`], which evaluates no secret-reading field, and only while
/// profiling.
fn profile_target(ctx: &Ctx<'_>) -> Option<String> {
    let input = ctx.input().ok()?;
    ["connector", "channel"].iter().find_map(|key| {
        input
            .get(*key)
            .and_then(OwnedDataValue::as_str)
            .map(str::to_owned)
    })
}

/// The connector named `name`, `connector_helpers::resolve_connector` on
/// dataflow-rs 4's error.
///
/// # Errors
///
/// A `FUNCTION_ERROR`, not retryable, when no connector has the name.
pub async fn resolve_connector(
    registry: &ConnectorRegistry,
    name: &str,
) -> dataflow_rs::Result<Arc<ConnectorConfig>> {
    registry.get(name).await.ok_or_else(|| {
        crate::engine::HandlerError::new(
            crate::engine::ErrorClass::Backend,
            format!("Connector '{name}' not found"),
        )
        .into()
    })
}

/// The typed config of a connector of kind `K`,
/// `connector_helpers::require_connector` on dataflow-rs 4's error.
///
/// # Errors
///
/// A `connector_detail` refusal when the connector is another kind: the
/// caller is told the request failed validation, and which kind it is rides
/// as the operator-only detail.
pub fn require_connector<'a, K: ConnectorTarget>(
    config: &'a ConnectorConfig,
    name: &str,
) -> dataflow_rs::Result<&'a K::Config> {
    K::extract(config).ok_or_else(|| {
        crate::errors::service::connector_detail(format!("Connector '{name}' is not {}", K::noun()))
    })
}

/// Orion's expressions with no secrets and no budget: `orion_exprs` as an
/// offline surface or a test builds it. Every function test compiles its
/// config and runs its call on it.
pub fn test_exprs() -> ExprEngine {
    crate::engine::operators::orion_exprs(&ResolvedSecrets::empty(), 0)
        .expect("Orion's expression engine builds with no secrets and no budget")
}

/// Inert dependencies for every function: empty pools and registries, a
/// client, the default configuration, and an empty generation. Enough to
/// register every real function with no `AppState`, for a registry that
/// runs offline, where a stubbed call never reaches a dependency.
pub struct OfflineDeps {
    registry: Arc<ConnectorRegistry>,
    client: reqwest::Client,
    runtime: Arc<crate::runtime::RuntimeHandle>,
    exprs: ExprEngine,
    jwks: Arc<crate::jwt::jwks::JwksCache>,
    engine_config: crate::config::EngineConfig,
    query_config: crate::config::QueryConfig,
    write_config: crate::config::WriteConfig,
    cache_pool: Arc<crate::connector::cache_backend::CachePool>,
    channel_loader: Arc<crate::channel::ChannelLoader>,
    sql_pool_cache: Arc<crate::connector::pool_cache::SqlPoolCache>,
    mongo_pool_cache: Arc<crate::connector::mongo_pool::MongoPoolCache>,
    smtp_pool_cache: Arc<crate::connector::smtp_pool::SmtpPoolCache>,
    models_config: crate::config::ModelsConfig,
}

impl OfflineDeps {
    /// Every dependency empty or default. Nothing is spawned and nothing is
    /// connected: each pool opens on first use, which offline never comes.
    pub fn new() -> Self {
        let engine_config = crate::config::EngineConfig::default();
        let client = reqwest::Client::new();
        let entries = engine_config.max_pool_cache_entries;
        Self {
            registry: Arc::new(ConnectorRegistry::new(
                engine_config.circuit_breaker.clone(),
            )),
            runtime: Arc::new(crate::runtime::RuntimeHandle::new(
                Arc::new(
                    dataflow_rs_v3::Engine::builder()
                        .build()
                        .expect("an empty engine builds"),
                ),
                Arc::new(crate::channel::ChannelSnapshot::empty()),
                crate::engine::FunctionRegistry::builtin().clone(),
            )),
            exprs: test_exprs(),
            jwks: Arc::new(crate::jwt::jwks::JwksCache::new(client.clone(), false)),
            query_config: crate::config::QueryConfig::default(),
            write_config: crate::config::WriteConfig::default(),
            cache_pool: Arc::new(crate::connector::cache_backend::CachePool::new(
                entries,
                engine_config.cache_cleanup_interval_secs,
                engine_config.max_memory_cache_entries,
            )),
            channel_loader: Arc::new(crate::channel::ChannelLoader::new()),
            sql_pool_cache: Arc::new(crate::connector::pool_cache::SqlPoolCache::new(entries)),
            mongo_pool_cache: Arc::new(crate::connector::mongo_pool::MongoPoolCache::new(entries)),
            smtp_pool_cache: Arc::new(crate::connector::smtp_pool::SmtpPoolCache::new(entries)),
            models_config: crate::config::ModelsConfig::default(),
            client,
            engine_config,
        }
    }

    /// The dependencies, as `register_functions` and
    /// `build_custom_functions` take them. No model runtime: `model_infer`
    /// answers `unavailable`, as with `models.enabled = false`.
    pub fn deps(&self) -> HandlerDeps<'_> {
        HandlerDeps {
            registry: Arc::clone(&self.registry),
            client: self.client.clone(),
            runtime: Arc::clone(&self.runtime),
            exprs: self.exprs.clone(),
            jwks: Arc::clone(&self.jwks),
            engine_config: &self.engine_config,
            query_config: &self.query_config,
            write_config: &self.write_config,
            cache_pool: Arc::clone(&self.cache_pool),
            channel_loader: Arc::clone(&self.channel_loader),
            sql_pool_cache: Arc::clone(&self.sql_pool_cache),
            mongo_pool_cache: Arc::clone(&self.mongo_pool_cache),
            smtp_pool_cache: Arc::clone(&self.smtp_pool_cache),
            models: None,
            models_config: &self.models_config,
        }
    }
}

impl Default for OfflineDeps {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for OfflineDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineDeps").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::circuit_breaker::CircuitBreakerConfig;
    use crate::engine::functions::connector_helpers as v3;
    use crate::engine::{ErrorClass, HandlerError};
    use dataflow_rs::Function;
    use dataflow_rs::datavalue::OwnedDataValue;
    use dataflow_rs::function::testing;
    use dataflow_rs_v3::engine::task_outcome::TaskOutcome;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// One call as its caller sees it: whether the work ran, and the error's
    /// code, retryability and the text its record carries (a dataflow-rs 3
    /// error's `Display`, a dataflow-rs 4 service error's message).
    type Seen = (bool, Option<(String, bool, String)>);

    /// What each call of a plan does: succeed, or fail with a class.
    type Plan<'p> = &'p [Option<ErrorClass>];

    fn breakers(threshold: u32) -> ConnectorRegistry {
        ConnectorRegistry::new(CircuitBreakerConfig {
            enabled: true,
            failure_threshold: threshold,
            recovery_timeout_secs: 300,
            max_breakers: 100,
        })
    }

    /// `plan` through the kit's shell, connector `c` on channel `ch`.
    async fn through_guarded(registry: &ConnectorRegistry, plan: Plan<'_>) -> Vec<Seen> {
        let mut seen = Vec::new();
        for step in plan {
            let ran = AtomicBool::new(false);
            let result = guarded("probe", registry, "c", "ch", async {
                ran.store(true, Ordering::Relaxed);
                match step {
                    None => Ok(Outcome::Ok),
                    Some(class) => Err(HandlerError::new(*class, "down").into()),
                }
            })
            .await;
            let error = result.err().map(|e| {
                (
                    e.code().to_string(),
                    e.is_retryable(),
                    e.message().to_string(),
                )
            });
            seen.push((ran.load(Ordering::Relaxed), error));
        }
        seen
    }

    /// The same plan through 1.12's `guarded_handler`.
    async fn through_guarded_handler(registry: &ConnectorRegistry, plan: Plan<'_>) -> Vec<Seen> {
        let mut seen = Vec::new();
        for step in plan {
            let ran = AtomicBool::new(false);
            let result = v3::guarded_handler("probe", registry, "c", "ch", async {
                ran.store(true, Ordering::Relaxed);
                match step {
                    None => Ok(TaskOutcome::Success),
                    Some(class) => Err(HandlerError::new(*class, "down").into()),
                }
            })
            .await;
            let error = result.err().map(|e| {
                let code = dataflow_rs_v3::ErrorInfo::new(None, None, e.clone()).code;
                (code, e.retryable(), e.to_string())
            });
            seen.push((ran.load(Ordering::Relaxed), error));
        }
        seen
    }

    /// `plan` through the kit's shell when `kit`, else through 1.12's.
    async fn through(kit: bool, registry: &ConnectorRegistry, plan: Plan<'_>) -> Vec<Seen> {
        if kit {
            through_guarded(registry, plan).await
        } else {
            through_guarded_handler(registry, plan).await
        }
    }

    /// The exposition lines of the connector and breaker counters `run`
    /// emitted, on a recorder of its own: what Prometheus is told, less the
    /// latencies, which differ run to run.
    fn counters(run: impl FnOnce() + Send) -> Vec<String> {
        let exposition = crate::metrics::render_local(run);
        let mut lines: Vec<String> = exposition
            .lines()
            .filter(|l| {
                l.starts_with("orion_connector_requests_total")
                    || l.starts_with("orion_circuit_breaker_")
            })
            .map(str::to_string)
            .collect();
        lines.sort();
        lines
    }

    fn block_on<T>(f: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(f)
    }

    /// `test_breaker_trips_after_threshold`, `test_breaker_reset_via_admin_api`
    /// and the shed request's code, at the shell: three retryable failures
    /// open the breaker, the next calls never reach the work and fail
    /// `circuit_open`, and a reset closes it — every outcome, every breaker
    /// state and every counter as `guarded_handler`'s over the same plan.
    #[test]
    fn guarded_trips_and_resets_the_breaker_as_guarded_handler_does() {
        let connect = Some(ErrorClass::Connector);
        let plan = [connect, connect, connect, None, None];
        let run = |kit: bool| {
            let mut seen = Vec::new();
            let mut states = Vec::new();
            let lines = counters(|| {
                block_on(async {
                    let registry = breakers(3);
                    seen.extend(through(kit, &registry, &plan).await);
                    states.push(registry.circuit_breaker_states().await);
                    assert!(registry.reset_circuit_breaker("ch:c").await);
                    seen.extend(through(kit, &registry, &[None]).await);
                    states.push(registry.circuit_breaker_states().await);
                });
            });
            (seen, states, lines)
        };
        let (kit, v3) = (run(true), run(false));
        assert_eq!(kit, v3);

        let (seen, states, lines) = kit;
        assert_eq!(seen.iter().filter(|(ran, _)| *ran).count(), 4, "{seen:?}");
        let shed = seen[3].1.as_ref().expect("shed");
        assert_eq!(shed.0, crate::errors::kind::CIRCUIT_OPEN);
        assert!(shed.1, "an open breaker is retryable");
        assert_eq!(
            shed.2,
            "Circuit breaker open for connector 'c' on channel 'ch'"
        );
        assert_eq!(states[0].get("ch:c").map(String::as_str), Some("open"));
        assert_eq!(states[1].get("ch:c").map(String::as_str), Some("closed"));
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("orion_circuit_breaker_trips_total"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("orion_circuit_breaker_rejections_total"))
        );
    }

    /// `test_breaker_disabled_by_default`: with the breaker off, every call
    /// reaches the work, and no breaker is made.
    #[tokio::test]
    async fn guarded_is_a_pass_through_with_the_breaker_off() {
        let plan = [Some(ErrorClass::Connector); 10];
        let registry = ConnectorRegistry::new(CircuitBreakerConfig::default());
        let kit = through_guarded(&registry, &plan).await;
        assert!(registry.circuit_breaker_states().await.is_empty());
        let registry = ConnectorRegistry::new(CircuitBreakerConfig::default());
        assert_eq!(kit, through_guarded_handler(&registry, &plan).await);
        assert!(kit.iter().all(|(ran, _)| *ran));
    }

    /// `a_rejected_query_does_not_trip_the_breaker`: a failure the caller
    /// caused is no evidence about the dependency, at a threshold of one.
    #[tokio::test]
    async fn a_failure_that_is_not_retryable_never_trips_the_breaker() {
        let plan = [
            Some(ErrorClass::CallerInput),
            Some(ErrorClass::Backend),
            Some(ErrorClass::Limit),
            Some(ErrorClass::Backend),
        ];
        let registry = breakers(1);
        let kit = through_guarded(&registry, &plan).await;
        let states = registry.circuit_breaker_states().await;
        assert_eq!(states.get("ch:c").map(String::as_str), Some("closed"));
        assert_eq!(kit, through_guarded_handler(&breakers(1), &plan).await);
        assert!(kit.iter().all(|(ran, _)| *ran));
    }

    /// `test_breaker_half_open_recovery`: past the cooldown the next call is
    /// let through as a probe, and a failed probe opens the breaker again.
    #[tokio::test(start_paused = true)]
    async fn after_the_cooldown_a_probe_reaches_the_work() {
        let connect = Some(ErrorClass::Connector);
        let mut sides = Vec::new();
        for kit in [true, false] {
            let registry = breakers(2);
            let mut seen = through(kit, &registry, &[connect, connect, connect]).await;
            tokio::time::advance(std::time::Duration::from_secs(301)).await;
            seen.extend(through(kit, &registry, &[connect, connect]).await);
            sides.push((seen, registry.circuit_breaker_states().await));
        }
        assert_eq!(sides[0], sides[1]);
        let ran: Vec<bool> = sides[0].0.iter().map(|(ran, _)| *ran).collect();
        assert_eq!(ran, [true, true, false, true, false]);
        assert_eq!(sides[0].1.get("ch:c").map(String::as_str), Some("open"));
    }

    /// Writes `kit::channel(ctx)` at `data.channel`.
    struct Channel;

    impl Function for Channel {
        type Config = ();

        async fn call(&self, ctx: &mut Ctx<'_>, _cfg: &()) -> dataflow_rs::Result<Outcome> {
            let channel = OwnedDataValue::String(channel(ctx).to_string());
            ctx.set(&"data.channel".parse()?, channel)?;
            Ok(Outcome::Ok)
        }
    }

    #[tokio::test]
    async fn channel_reads_the_runs_channel_and_falls_back_to_unknown() {
        let at: dataflow_rs::Path = "data.channel".parse().expect("a path");
        let written = |called: &testing::Called| called.message.context().get(&at).map(Value::from);
        let on = testing::call(Channel, json!({})).channel("orders").await;
        assert_eq!(written(&on), Some(json!("orders")));
        let off = testing::call(Channel, json!({})).await;
        assert_eq!(written(&off), Some(json!("unknown")));
        // 1.12's reading of a message with no channel.
        let message = dataflow_rs_v3::Message::from_value(&json!({}));
        assert_eq!(
            crate::engine::functions::extract_channel(&message),
            "unknown"
        );
    }

    /// The code, retryability, recorded text and detail of a v4 and a v3
    /// error.
    fn facts_v4(e: &dataflow_rs::Error) -> (String, bool, String, Option<String>) {
        (
            e.code().to_string(),
            e.is_retryable(),
            e.message().to_string(),
            crate::errors::service::detail_of(e).map(str::to_string),
        )
    }

    fn facts_v3(e: dataflow_rs_v3::DataflowError) -> (String, bool, String, Option<String>) {
        let detail = e.detail().map(str::to_string);
        let code = dataflow_rs_v3::ErrorInfo::new(None, None, e.clone()).code;
        (code, e.retryable(), e.to_string(), detail)
    }

    #[tokio::test]
    async fn the_connector_lookups_refuse_as_1_12_did() {
        let registry = ConnectorRegistry::new(CircuitBreakerConfig::default());
        let kit = resolve_connector(&registry, "missing")
            .await
            .expect_err("absent");
        let v3 = v3::resolve_connector(&registry, "missing")
            .await
            .expect_err("absent");
        assert_eq!(facts_v4(&kit), facts_v3(v3));

        let es = ConnectorConfig::Es(crate::connector::EsConnectorConfig {
            max_response_size: 1024,
            url: "http://127.0.0.1:9200".to_string(),
            auth: None,
            request_timeout_ms: None,
            allow_private_urls: false,
            operations: crate::connector::OperationGates::default(),
            dialect: crate::connector::DialectGuards::default(),
        });
        let kit = require_connector::<crate::connector::kind::Db>(&es, "search")
            .expect_err("not a database");
        let v3 = v3::require_connector::<crate::connector::kind::Db>(&es, "search")
            .expect_err("not a database");
        assert_eq!(facts_v4(&kit), facts_v3(v3));
        assert!(require_connector::<crate::connector::kind::Es>(&es, "search").is_ok());
    }

    /// A probe with the fields a sample is attributed by.
    #[derive(Debug, dataflow_rs::function::FunctionConfig)]
    struct Target {
        connector: Option<String>,
        channel: Option<dataflow_rs::Expr<String>>,
    }

    /// Answers `Ok`, after a nested sample of its own when asked.
    struct Probe {
        nested: bool,
    }

    impl Function for Probe {
        type Config = Target;

        async fn call(&self, _ctx: &mut Ctx<'_>, _cfg: &Target) -> dataflow_rs::Result<Outcome> {
            if self.nested {
                crate::engine::profile::record("db_read", Some("inner-db"), async {}).await;
            }
            Ok(Outcome::Ok)
        }
    }

    /// One call of `probe` under the layer, as `name`, with a collector in
    /// scope when `profiled`; the collector's rendered profile.
    async fn profiled(probe: Probe, name: &str, input: Value, profiled: bool) -> Value {
        let collector = crate::engine::profile::ProfileCollector::new();
        let call = testing::call(probe, input).name(name).layer(ProfileLayer);
        let called = if profiled {
            crate::engine::profile::ORION_PROFILE
                .scope(Arc::clone(&collector), call.into_future())
                .await
        } else {
            call.await
        };
        assert!(called.issues.is_empty(), "{:?}", called.issues);
        assert_eq!(called.outcome.expect("the probe answers"), Outcome::Ok);
        collector.to_json()
    }

    #[tokio::test]
    async fn profile_layer_records_a_sample_per_call_with_a_collector_in_scope() {
        let profile = profiled(
            Probe { nested: false },
            "db_read",
            json!({"connector": "db1"}),
            true,
        )
        .await;
        let handlers = profile["handlers"].as_array().expect("handlers");
        assert_eq!(handlers.len(), 1, "{profile}");
        assert_eq!(handlers[0]["function"], "db_read", "named by ctx.info()");
        assert_eq!(handlers[0]["connector"], "db1", "attributed by ctx.input()");
        assert_eq!(profile["by_function"]["db_read"]["count"], 1);
        assert_eq!(profile["by_connector"]["db1"]["count"], 1);

        // `channel_call`'s label is the channel it reaches, computed or not.
        let profile = profiled(
            Probe { nested: false },
            "channel_call",
            json!({"channel": {"cat": ["downstream", "-ch"]}}),
            true,
        )
        .await;
        assert_eq!(
            profile["by_connector"]["downstream-ch"]["count"], 1,
            "{profile}"
        );

        // A function with neither field is profiled too, unattributed.
        let profile = profiled(Probe { nested: false }, "crypto", json!({}), true).await;
        assert_eq!(profile["handlers"][0]["function"], "crypto");
        assert!(profile["handlers"][0].get("connector").is_none());
    }

    /// A sample taken inside the call nests under the layer's, as a
    /// `channel_call`'s child calls nest under it.
    #[tokio::test]
    async fn a_sample_inside_the_call_nests_under_the_layers() {
        let profile = profiled(
            Probe { nested: true },
            "channel_call",
            json!({"channel": "orders"}),
            true,
        )
        .await;
        let handlers = profile["handlers"].as_array().expect("handlers");
        assert_eq!(
            handlers.len(),
            1,
            "only the outer call is top-level: {profile}"
        );
        assert_eq!(handlers[0]["function"], "channel_call");
        assert_eq!(handlers[0]["nested"][0]["function"], "db_read");
        assert_eq!(handlers[0]["nested"][0]["depth"], 1);
    }

    #[tokio::test]
    async fn profile_layer_records_nothing_without_a_collector() {
        let profile = profiled(
            Probe { nested: false },
            "db_read",
            json!({"connector": "db1"}),
            false,
        )
        .await;
        assert_eq!(profile["handlers"], json!([]), "{profile}");
    }

    /// The gates refuse as 1.12's did: the code, retryability, recorded text
    /// and operator-only detail, open and closed.
    #[test]
    fn the_gates_refuse_as_1_12_did() {
        for allowed in [true, false] {
            let kit = require_op(allowed, "publish", "events").map_err(|e| facts_v4(&e));
            let v3 = v3::require_op(allowed, "publish", "events").map_err(facts_v3);
            assert_eq!(kit, v3, "allowed: {allowed}");
        }
        for methods in [
            vec![],
            vec!["GET".to_string()],
            vec!["get".to_string(), "PUT".to_string()],
        ] {
            let gates = HttpOperationGates { methods };
            for method in ["GET", "POST", "PUT"] {
                let kit = require_method_allowed(&gates, method, "api").map_err(|e| facts_v4(&e));
                let v3 = v3::require_method_allowed(&gates, method, "api").map_err(facts_v3);
                assert_eq!(kit, v3, "{method} against {:?}", gates.methods);
            }
        }
    }

    /// Each function `register_functions` registers carries its `schema.rs`
    /// row: the description, the category, the retry safety mapped (I179),
    /// and the connector reference (I187).
    #[test]
    fn spec_is_the_schema_row() {
        use dataflow_rs::function::RetrySafety;
        let offline = OfflineDeps::new();
        let mut registry = dataflow_rs::Registry::new(test_exprs());
        crate::engine::register_functions(&mut registry, offline.deps());
        let mut seen = 0;
        for info in registry.catalogue().filter(|info| !info.builtin) {
            let row = schema::registry()
                .iter()
                .find(|row| row.name == info.name)
                .expect("a row");
            assert_eq!(info.spec.description, row.description, "{}", info.name);
            assert_eq!(info.spec.category, row.category, "{}", info.name);
            let retry = match row.retry_safety {
                schema::RetrySafety::UnsafeWrite => RetrySafety::NotIdempotent,
                schema::RetrySafety::DependsOn { .. } => RetrySafety::Unknown,
                _ => RetrySafety::Idempotent,
            };
            assert_eq!(info.spec.retry, retry, "{}", info.name);
            let references: Vec<_> = info
                .spec
                .references
                .iter()
                .map(|r| (r.field.to_string(), r.kind.to_string()))
                .collect();
            let expected: Vec<_> = row
                .connector
                .iter()
                .map(|_| ("connector".to_string(), row.name.to_string()))
                .collect();
            assert_eq!(references, expected, "{}", info.name);
            seen += 1;
        }
        assert!(seen > 0, "register_functions registered nothing");
    }

    /// The functions ported so far, in `build_custom_functions`' order.
    const PORTED: &[&str] = &[
        "http_call",
        "send_email",
        "storage_presign",
        "storage_head",
        "publish_kafka",
    ];

    /// Every function 1.12 registers can be built over the inert
    /// dependencies, and `register_functions` registers the ones ported so
    /// far, each under the name `build_custom_functions` gives its twin.
    #[tokio::test]
    async fn offline_deps_build_every_function_and_register_the_ported_ones() {
        let offline = OfflineDeps::new();
        let built = crate::engine::build_custom_functions(offline.deps());
        for name in [
            "http_call",
            "channel_call",
            "db_read",
            "cache_read",
            "jwt_verify",
            "model_infer",
        ] {
            assert!(built.contains_key(name), "{name}");
        }
        let mut registry = dataflow_rs::Registry::new(test_exprs());
        crate::engine::register_functions(&mut registry, offline.deps());
        let hosts: Vec<&str> = registry
            .catalogue()
            .filter(|info| !info.builtin)
            .map(|info| info.name)
            .collect();
        let mut ported = PORTED.to_vec();
        ported.sort_unstable();
        assert_eq!(hosts, ported, "the catalogue lists host functions by name");
        for name in PORTED {
            assert!(built.contains_key(*name), "{name} has a dataflow-rs 3 twin");
        }
        // The engine every test compiles on: Orion's operators, no secrets.
        assert!(
            test_exprs()
                .operators()
                .iter()
                .any(|op| op.name == "hex_encode")
        );
        assert!(test_exprs().secrets().names().next().is_none());
    }
}
