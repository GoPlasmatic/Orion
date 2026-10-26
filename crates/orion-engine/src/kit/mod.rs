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
//! - [`retry`]: Orion's retry safety on dataflow-rs 4's answers; each
//!   function owns its spec in `functions::definition`.
//! - [`output_path`]: where a call's result goes, resolved before its work,
//!   as 1.12's connector shell resolved it; [`root_output_defect`], I185's
//!   Defect for a result no context root takes; and [`root_output_advisory`]
//!   for one a root may not take.
//! - [`rendered`]: a document field rendered for this call, as JSON.
//! - [`ProfileLayer`]: the per-request profile, as one registry layer
//!   around every host call.
//! - [`OfflineDeps`]: inert dependencies, so every function can be
//!   registered with no `AppState`.
//!
//! The errors are [`HandlerError`](crate::HandlerError)s, which `?`
//! turns into dataflow-rs 4's `Error` with the codes 1.12 recorded (I184).

pub mod fields;
pub mod read;

pub use fields::{KeyMaterial, KeyMaterialList, KeyRef, Members, key_material, key_material_list};

use std::borrow::Cow;
use std::future::Future;
use std::sync::{Arc, LazyLock};

use dataflow_rs::Path;
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::{ExprEngine, Interpolated, Output};
use dataflow_rs::function::{BoxFuture, CallLayer, Next};
use dataflow_rs::program::{AuthoredPath, IssueCode, Severity};
use dataflow_rs::{Ctx, Issue, Outcome};

use crate::connector::ConnectorRegistry;
use crate::functions::cache_invalidate::NamespaceInvalidator;
use crate::functions::channel_call::ChannelCallHost;
use crate::functions::connector_call::ConnectorCallHost;
use crate::functions::schema;
use crate::{HandlerDeps, ResolvedSecrets};

/// The channel a call's run was started on, `"unknown"` when it was started
/// on none — what `extract_channel` answers from the message's metadata.
/// Borrowed for the run, not for `ctx`, so a call holds it while it writes.
pub fn channel<'r>(ctx: &Ctx<'r>) -> &'r str {
    ctx.info().channel.unwrap_or("unknown")
}

/// The `data` root: the default `output` of the connector functions.
pub static DATA: LazyLock<Path> = LazyLock::new(|| "data".parse().expect("`data` is a path"));

/// Where a call's result goes, resolved before the call does any work, as
/// 1.12's connector shell resolved `output` in its prologue (I183): an
/// output that names no path fails before anything is sent or signed, and
/// the call writes to the path given here with `Ctx::set` once its work is
/// done.
///
/// A literal output is its compiled path, read at no cost. A computed one
/// is evaluated now, against the context as the call sees it, and read as
/// 1.12's `resolve_output_path` read it: a dotted path is the path; `null`
/// is `on_null` (the output's default, which 1.12 applied), or refused when
/// there is none to apply; anything else is refused with 1.12's text. A
/// string 1.12 took as a dotted path but that names no context root
/// (`"result"`) is refused here too: 1.12 wrote it to a context key no
/// response shows, and dataflow-rs 4 has no such key to write.
///
/// # Errors
///
/// A [`ErrorClass::CallerInput`](crate::ErrorClass::CallerInput)
/// error for a result that is not a path, `VALIDATION_ERROR` as 1.12's was;
/// the evaluation's own error when the expression fails, `LOGIC_ERROR` as
/// 1.12's was.
pub fn output_path<'o>(
    ctx: &Ctx<'_>,
    out: &'o Output,
    on_null: Option<&'o Path>,
) -> dataflow_rs::Result<Cow<'o, Path>> {
    if let Some(path) = out.constant() {
        return Ok(Cow::Borrowed(path));
    }
    if let Ok(path) = ctx.eval(out.path()) {
        return Ok(path);
    }
    // Not a path, or not evaluated at all: the value, as 1.12 judged it. It
    // compiled when the output did, on the same engine, so this compiles too;
    // and only a call that is about to fail pays for it.
    let value = ctx.exprs().compile::<OwnedDataValue>(out.path().raw())?;
    let value = ctx.eval(&value)?;
    let not_a_path = |got: &str| -> dataflow_rs::Error {
        crate::HandlerError::new(
            crate::ErrorClass::CallerInput,
            format!("'output' must resolve to a non-empty dotted path, got {got}"),
        )
        .into()
    };
    match &*value {
        OwnedDataValue::Null => on_null.map(Cow::Borrowed).ok_or_else(|| not_a_path("null")),
        OwnedDataValue::String(dotted) if !dotted.is_empty() => {
            dotted.parse::<Path>().map(Cow::Owned).map_err(|e| {
                crate::HandlerError::new(
                    crate::ErrorClass::CallerInput,
                    format!(
                        "'output' must resolve to a path in the context: {}",
                        e.message()
                    ),
                )
                .into()
            })
        }
        other => Err(not_a_path(
            crate::functions::connector_helpers::json_type_name(&serde_json::Value::from(other)),
        )),
    }
}

/// I185's Defect, from a function's `prepare`, for a result that is never
/// an object (an array of rows, a URL, a token): an `output` that is a
/// literal context root, the default `data` included, fails every call,
/// since a root takes only an object (break 21), where 1.12 replaced the root
/// with the result. `message` names the function and the fix. A computed
/// output is judged per call, when its path is known.
///
/// # Errors
///
/// The one Defect, `INPUT_PARSE` at `output`, when the output is a literal
/// root.
pub fn root_output_defect(out: &Output, message: &str) -> Result<(), Vec<Issue>> {
    root_output_issue(out, message, Severity::Defect)
}

/// [`root_output_defect`]'s Advisory, for a result that may be an object
/// and may not (one key's cached value): an `output` that is a literal
/// context root, the default `data` included, fails each call whose result
/// is not an object (break 21), where 1.12 replaced the root. The call is
/// not refused, since an object is written fine, so it never quarantines.
///
/// # Errors
///
/// The one Advisory, `INPUT_PARSE` at `output`, when the output is a
/// literal root.
pub fn root_output_advisory(out: &Output, message: &str) -> Result<(), Vec<Issue>> {
    root_output_issue(out, message, Severity::Advisory)
}

/// One issue of `severity` at `output` when the output is a literal root.
fn root_output_issue(out: &Output, message: &str, severity: Severity) -> Result<(), Vec<Issue>> {
    match out.constant() {
        Some(path) if path.is_root() => {
            let mut at = AuthoredPath::root();
            at.push("output");
            Err(vec![
                Issue::new(IssueCode::InputParse, at, message).with_severity(severity),
            ])
        }
        _ => Ok(()),
    }
}

/// `doc` rendered for this call, serialized straight into the JSON a
/// dataflow-rs 3 reader took (`resolve_value`'s fold of the same document),
/// with no tree of the rendered document's own.
///
/// # Errors
///
/// A [`ErrorClass::Backend`](crate::ErrorClass::Backend) error for
/// a document that does not serialize, which a rendered document always
/// does.
pub fn rendered(
    ctx: &Ctx<'_>,
    doc: &Interpolated,
) -> Result<serde_json::Value, crate::HandlerError> {
    serde_json::to_value(ctx.render(doc))
        .map_err(|e| crate::HandlerError::new(crate::ErrorClass::Backend, e))
}

/// Orion's retry safety on dataflow-rs 4's three answers (I179): `Pure`,
/// `Read` and `IdempotentWrite` are `Idempotent`, `UnsafeWrite` is
/// `NotIdempotent`, `DependsOn` is `Unknown`. A host function reads it from
/// its declaration, and a plugin function from its registry entry.
pub fn retry(safety: schema::RetrySafety) -> dataflow_rs::function::RetrySafety {
    match safety {
        schema::RetrySafety::Pure
        | schema::RetrySafety::Read
        | schema::RetrySafety::IdempotentWrite => dataflow_rs::function::RetrySafety::Idempotent,
        schema::RetrySafety::UnsafeWrite => dataflow_rs::function::RetrySafety::NotIdempotent,
        schema::RetrySafety::DependsOn { .. } => dataflow_rs::function::RetrySafety::Unknown,
    }
}

/// The connector gates and lookups every connector function opens with. They
/// return a [`HandlerError`], which converts into the engine's error with the
/// code and retryability its class names.
///
/// [`HandlerError`]: crate::HandlerError
pub use crate::functions::connector_helpers::{
    require_connector, require_method_allowed, require_op, resolve_connector,
};

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
        return Err(crate::service::circuit_open(connector, channel));
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
/// [`ProfileCollector`](crate::profile::ProfileCollector) is in scope,
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
        let Ok(collector) = crate::profile::ORION_PROFILE.try_with(Arc::clone) else {
            return next.run(ctx);
        };
        Box::pin(async move {
            let function = Cow::Owned(ctx.info().function.to_owned());
            let target = profile_target(ctx);
            crate::profile::sample(&collector, function, target, next.run(ctx)).await
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

/// Orion's expressions with no secrets and no budget: `orion_exprs` as an
/// offline surface or a test builds it. Every function test compiles its
/// config and runs its call on it.
pub fn test_exprs() -> ExprEngine {
    crate::operators::orion_exprs(&ResolvedSecrets::empty(), 0)
        .expect("Orion's expression engine builds with no secrets and no budget")
}

/// Inert dependencies for every function: empty pools and registries, a
/// client, the default configuration, and hosts that serve nothing. Enough to
/// register every real function with no `AppState`, for a registry that
/// runs offline, where a stubbed call never reaches a dependency.
///
/// The hosts default to [`NoChannels`], [`LocalNamespaces`] and
/// [`NoProviders`]; the offline tools hand it the node-shaped ones with
/// [`OfflineDeps::with_hosts`], so a `connector_call` to a built-in provider
/// is still checked at compile.
pub struct OfflineDeps {
    registry: Arc<ConnectorRegistry>,
    client: reqwest::Client,
    exprs: ExprEngine,
    jwks: Arc<crate::jwt::jwks::JwksCache>,
    engine_config: crate::config::EngineConfig,
    query_config: crate::config::QueryConfig,
    write_config: crate::config::WriteConfig,
    cache_pool: Arc<crate::connector::cache_backend::CachePool>,
    sql_pool_cache: Arc<crate::connector::pool_cache::SqlPoolCache>,
    channel_calls: Arc<dyn ChannelCallHost>,
    namespaces: Arc<dyn NamespaceInvalidator>,
    connector_calls: Arc<dyn ConnectorCallHost>,
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
            exprs: test_exprs(),
            jwks: Arc::new(crate::jwt::jwks::JwksCache::new(client.clone(), false)),
            query_config: crate::config::QueryConfig::default(),
            write_config: crate::config::WriteConfig::default(),
            cache_pool: Arc::new(crate::connector::cache_backend::CachePool::new(
                engine_config.cache_cleanup_interval_secs,
                engine_config.max_memory_cache_entries,
            )),
            sql_pool_cache: Arc::new(crate::connector::pool_cache::SqlPoolCache::new(entries)),
            channel_calls: Arc::new(NoChannels),
            namespaces: Arc::new(LocalNamespaces),
            connector_calls: Arc::new(NoProviders),
            client,
            engine_config,
        }
    }

    /// The same, with these hosts.
    pub fn with_hosts(
        mut self,
        channel_calls: Arc<dyn ChannelCallHost>,
        namespaces: Arc<dyn NamespaceInvalidator>,
        connector_calls: Arc<dyn ConnectorCallHost>,
    ) -> Self {
        self.channel_calls = channel_calls;
        self.namespaces = namespaces;
        self.connector_calls = connector_calls;
        self
    }

    /// The dependencies, as `register_functions` takes them.
    pub fn deps(&self) -> HandlerDeps<'_> {
        HandlerDeps {
            registry: Arc::clone(&self.registry),
            client: self.client.clone(),
            channel_calls: Arc::clone(&self.channel_calls),
            exprs: self.exprs.clone(),
            jwks: Arc::clone(&self.jwks),
            engine_config: &self.engine_config,
            query_config: &self.query_config,
            write_config: &self.write_config,
            cache_pool: Arc::clone(&self.cache_pool),
            namespaces: Arc::clone(&self.namespaces),
            sql_pool_cache: Arc::clone(&self.sql_pool_cache),
            connector_calls: Arc::clone(&self.connector_calls),
        }
    }
}

/// A node with no channels: every `channel_call` target is not found.
pub struct NoChannels;

impl ChannelCallHost for NoChannels {
    fn resolve(
        &self,
        target: &str,
    ) -> Result<Box<dyn crate::functions::channel_call::ChannelCallTarget>, crate::HandlerError>
    {
        Err(crate::functions::channel_call::not_found(target))
    }
}

/// A node's own response-cache stores, with no cluster store beside them: a
/// bump reaches every in-memory response cache the pool holds.
pub struct LocalNamespaces;

#[async_trait::async_trait]
impl NamespaceInvalidator for LocalNamespaces {
    async fn invalidate_namespaces(
        &self,
        cache_pool: &crate::connector::cache_backend::CachePool,
        namespaces: &[String],
        source: &'static str,
    ) -> Result<usize, crate::errors::OrionError> {
        let targets = cache_pool
            .memory_backends_for(crate::connector::cache_backend::CachePurpose::ResponseCache);
        crate::connector::cache_namespace::invalidate(&targets, namespaces, source).await?;
        Ok(targets.len())
    }
}

/// A node with no providers and no deployment: nothing is registered, and a
/// call is refused as one made outside a served run.
pub struct NoProviders;

#[async_trait::async_trait]
impl ConnectorCallHost for NoProviders {
    fn literal_input_keys(
        &self,
        _kind: orion_extension_api::ImplementationKind,
        _target: &str,
        _operation: &str,
    ) -> &'static [&'static str] {
        &[]
    }

    async fn invoke(
        &self,
        call: crate::functions::connector_call::ConnectorInvocation<'_>,
    ) -> Result<serde_json::Value, crate::HandlerError> {
        Err(crate::functions::connector_call::forbidden(format!(
            "CONNECTOR_CONNECTION_FORBIDDEN: connector_call runs only inside a served run; \
             '{}' on '{}' was called outside one",
            call.config.operation, call.config.connection
        )))
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
    use crate::connector::{ConnectorConfig, HttpOperationGates};
    use crate::{ErrorClass, HandlerError};
    use dataflow_rs::Function;
    use dataflow_rs::datavalue::OwnedDataValue;
    use dataflow_rs::function::testing;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// One call as its caller sees it: whether the work ran, and the error's
    /// code, retryability and the text its record carries.
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
    /// state and every counter as 1.12's `guarded_handler` gave them.
    #[test]
    fn guarded_trips_and_resets_the_breaker() {
        let connect = Some(ErrorClass::Connector);
        let plan = [connect, connect, connect, None, None];
        let mut seen = Vec::new();
        let mut states = Vec::new();
        let lines = counters(|| {
            block_on(async {
                let registry = breakers(3);
                seen.extend(through_guarded(&registry, &plan).await);
                states.push(registry.circuit_breaker_states().await);
                assert!(registry.reset_circuit_breaker("ch:c").await);
                seen.extend(through_guarded(&registry, &[None]).await);
                states.push(registry.circuit_breaker_states().await);
            });
        });

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
        assert!(kit.iter().all(|(ran, _)| *ran));
        assert!(
            kit.iter()
                .all(|(_, e)| e.as_ref().map(|e| e.0.as_str()) == Some("IO_ERROR")),
            "{kit:?}"
        );
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
        assert!(kit.iter().all(|(ran, _)| *ran));
    }

    /// `test_breaker_half_open_recovery`: past the cooldown the next call is
    /// let through as a probe, and a failed probe opens the breaker again.
    #[tokio::test(start_paused = true)]
    async fn after_the_cooldown_a_probe_reaches_the_work() {
        let connect = Some(ErrorClass::Connector);
        let registry = breakers(2);
        let mut seen = through_guarded(&registry, &[connect, connect, connect]).await;
        tokio::time::advance(std::time::Duration::from_secs(301)).await;
        seen.extend(through_guarded(&registry, &[connect, connect]).await);
        let ran: Vec<bool> = seen.iter().map(|(ran, _)| *ran).collect();
        assert_eq!(ran, [true, true, false, true, false]);
        let states = registry.circuit_breaker_states().await;
        assert_eq!(states.get("ch:c").map(String::as_str), Some("open"));
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
    }

    /// The code, retryability, recorded text and detail of an error.
    fn facts(e: &dataflow_rs::Error) -> (String, bool, String, Option<String>) {
        (
            e.code().to_string(),
            e.is_retryable(),
            e.message().to_string(),
            crate::service::detail_of(e).map(str::to_string),
        )
    }

    #[tokio::test]
    async fn the_connector_lookups_refuse_as_1_12_did() {
        let registry = ConnectorRegistry::new(CircuitBreakerConfig::default());
        let kit = resolve_connector(&registry, "missing")
            .await
            .expect_err("absent");
        assert_eq!(
            facts(&kit.into()),
            (
                "FUNCTION_ERROR".to_string(),
                false,
                "Function execution error: Connector 'missing' not found".to_string(),
                None
            )
        );

        let cache: ConnectorConfig =
            serde_json::from_str(r#"{"type":"cache","backend":"memory"}"#).expect("cache config");
        let kit = require_connector::<crate::connector::kind::Db>(&cache, "search")
            .expect_err("not a database");
        assert_eq!(
            facts(&kit.into()),
            (
                crate::errors::kind::CONNECTOR_DETAIL.to_string(),
                false,
                "Request validation failed".to_string(),
                Some(format!(
                    "Connector 'search' is not {}",
                    <crate::connector::kind::Db as crate::connector::kind::ConnectorTarget>::noun()
                ))
            )
        );
        assert!(require_connector::<crate::connector::kind::Cache>(&cache, "search").is_ok());
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
                crate::profile::record("db_read", Some("inner-db"), async {}).await;
            }
            Ok(Outcome::Ok)
        }
    }

    /// One call of `probe` under the layer, as `name`, with a collector in
    /// scope when `profiled`; the collector's rendered profile.
    async fn profiled(probe: Probe, name: &str, input: Value, profiled: bool) -> Value {
        let collector = crate::profile::ProfileCollector::new();
        let call = testing::call(probe, input).name(name).layer(ProfileLayer);
        let called = if profiled {
            crate::profile::ORION_PROFILE
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
        let refused = |detail: &str| {
            Err((
                crate::errors::kind::CONNECTOR_DETAIL.to_string(),
                false,
                "Request validation failed".to_string(),
                Some(detail.to_string()),
            ))
        };
        assert_eq!(
            require_op(true, "publish", "events").map_err(|e| facts(&e.into())),
            Ok(())
        );
        assert_eq!(
            require_op(false, "publish", "events").map_err(|e| facts(&e.into())),
            refused("operation 'publish' is disabled on connector 'events'")
        );
        // An empty list allows everything; a listed method matches whatever
        // its case.
        for (methods, allowed) in [
            (vec![], ["GET", "POST", "PUT"].as_slice()),
            (vec!["GET".to_string()], ["GET"].as_slice()),
            (
                vec!["get".to_string(), "PUT".to_string()],
                ["GET", "PUT"].as_slice(),
            ),
        ] {
            let gates = HttpOperationGates { methods };
            for method in ["GET", "POST", "PUT"] {
                let got =
                    require_method_allowed(&gates, method, "api").map_err(|e| facts(&e.into()));
                let expected = if allowed.contains(&method) {
                    Ok(())
                } else {
                    refused(&format!(
                        "HTTP method '{method}' is not allowed on connector 'api' (allowed: {})",
                        gates.methods.join(", ")
                    ))
                };
                assert_eq!(got, expected, "{method} against {:?}", gates.methods);
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
        crate::register_functions(&mut registry, offline.deps());
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

    /// `register_functions` registers every host function over the inert
    /// dependencies, each under its 1.12 name.
    #[tokio::test]
    async fn offline_deps_register_every_function() {
        let offline = OfflineDeps::new();
        let mut registry = dataflow_rs::Registry::new(test_exprs());
        crate::register_functions(&mut registry, offline.deps());
        let hosts: Vec<&str> = registry
            .catalogue()
            .filter(|info| !info.builtin)
            .map(|info| info.name)
            .collect();
        let mut ported: Vec<_> = schema::registry().iter().map(|row| row.name).collect();
        ported.sort_unstable();
        assert_eq!(hosts, ported, "the catalogue lists host functions by name");
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
