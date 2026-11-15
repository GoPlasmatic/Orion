//! The handler kit: what every dataflow-rs 4 function of Orion's shares.
//!
//! - [`read`]: Orion's field readers, and the coercions they apply.
//! - [`fields`]: the key-material and detached-expression field types.
//! - [`guarded`]: the connector shell — the circuit breaker and the
//!   connector metrics — around a connector function's work.
//! - [`resolve_connector`] and [`require_connector`]: the connector lookups,
//!   on dataflow-rs 4's error, and [`require_op`] and
//!   [`require_method_allowed`], the connector's operation gates.
//! - [`retry`]: Orion's retry safety on dataflow-rs 4's answers; each
//!   function owns its spec in `functions::definition`.
//! - [`output_path`]: where a call's result goes, resolved before its work;
//!   [`root_output_defect`], the Defect for a result no context root takes;
//!   and [`root_output_advisory`] for one a root may not take.
//! - [`rendered`]: a document field rendered for this call, as JSON.
//! - [`ProfileLayer`]: the per-request profile, as one registry layer
//!   around every host call.
//! - [`OfflineDeps`]: inert dependencies, so every function can be
//!   registered with no `AppState`.
//!
//! The errors are [`HandlerError`](crate::HandlerError)s, which `?`
//! turns into dataflow-rs 4's `Error` with the code and retryability its
//! class names.

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

/// Where a call's result goes, resolved before the call does any work: an
/// `output` that names no path fails before anything is sent or signed, and
/// the call writes to the path given here with `Ctx::set` once its work is
/// done.
///
/// A literal output is its compiled path, read at no cost. A computed one
/// is evaluated now, against the context as the call sees it: a dotted path
/// is the path; `null` is `on_null` (the output's default), or refused when
/// there is none to apply; anything else is refused. A string taken as a
/// dotted path but naming no context root (`"result"`) is refused here too:
/// it would write to a context key no response shows, and dataflow-rs 4 has
/// no such key to write.
///
/// # Errors
///
/// A [`ErrorClass::CallerInput`](crate::ErrorClass::CallerInput) error for a
/// result that is not a path, `VALIDATION_ERROR`; the evaluation's own error
/// when the expression fails, `LOGIC_ERROR`.
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
    // Not a path, or not evaluated at all: the value. It compiled when the
    // output did, on the same engine, so this compiles too; and only a call
    // that is about to fail pays for it.
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

/// The Defect, from a function's `prepare`, for a result that is never an
/// object (an array of rows, a URL, a token): an `output` that is a literal
/// context root, the default `data` included, fails every call, since a root
/// takes only an object. `message` names the function and the fix. A
/// computed output is judged per call, when its path is known.
///
/// # Errors
///
/// The one Defect, `INPUT_PARSE` at `output`, when the output is a literal
/// root.
pub(crate) fn root_output_defect(out: &Output, message: &str) -> Result<(), Vec<Issue>> {
    root_output_issue(out, message, Severity::Defect)
}

/// [`root_output_defect`]'s Advisory, for a result that may be an object
/// and may not (one key's cached value): an `output` that is a literal
/// context root, the default `data` included, fails each call whose result
/// is not an object. The call is not refused, since an object is written
/// fine, so it never quarantines.
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

/// `doc` rendered for this call, serialized straight into the JSON
/// (`resolve_value`'s fold of the same document), with no tree of the
/// rendered document's own.
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

/// Orion's retry safety on dataflow-rs 4's three answers: `Pure`,
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
    Typed, require_connector, require_method_allowed, require_op, resolve_connector, with_connector,
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
pub async fn guarded<T, F>(
    name: &'static str,
    registry: &ConnectorRegistry,
    connector: &str,
    channel: &str,
    fut: F,
) -> dataflow_rs::Result<T>
where
    F: Future<Output = dataflow_rs::Result<T>>,
{
    let metrics = registry.connector_metrics(channel, connector);
    if !registry.circuit_breaker_enabled() {
        return observed(metrics, connector, channel, fut).await;
    }

    // The registry composes the `channel:connector` key, so an operator's
    // keys keep meaning what they meant.
    let breaker = registry.get_or_create_breaker(channel, connector);
    if !breaker.check() {
        crate::metrics::record_circuit_breaker_rejection(connector, channel);
        return Err(crate::service::circuit_open(connector, channel));
    }

    let result = observed(metrics, connector, channel, fut).await;
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
async fn observed<T, F>(
    metrics: Arc<crate::metrics::ConnectorMetrics>,
    connector: &str,
    channel: &str,
    fut: F,
) -> dataflow_rs::Result<T>
where
    F: Future<Output = dataflow_rs::Result<T>>,
{
    let start = std::time::Instant::now();
    let result = fut.await;
    metrics.record(
        connector,
        channel,
        result.is_ok(),
        start.elapsed().as_secs_f64(),
    );
    result
}

/// The per-request profile as one registry [`CallLayer`]: while a
/// [`ProfileCollector`](crate::profile::ProfileCollector) is in scope,
/// each host call is one sample, nested at the collector's depth, named by
/// the function it was called by and attributed to its input's `connector`,
/// or for `channel_call` the `channel` it reaches. With no collector in scope
/// the call is passed on untouched: no input is read and nothing is boxed
/// beyond the chain's own future, so an unprofiled call pays one task-local
/// read.
///
/// One layer for every host function, so the functions outside the
/// connector shell are profiled too.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ProfileLayer;

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

/// What a sample is attributed to: the input's `connector` (or `connection`
/// for `connector_call`), else the `channel` a `channel_call` reaches. Read
/// from [`Ctx::input`], which evaluates no
/// secret-reading field, and only while profiling.
fn profile_target(ctx: &Ctx<'_>) -> Option<String> {
    let input = ctx.input().ok()?;
    ["connector", "connection", "channel"]
        .iter()
        .find_map(|key| {
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
pub(crate) struct NoChannels;

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
pub(crate) struct LocalNamespaces;

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
pub(crate) struct NoProviders;

#[async_trait::async_trait]
impl ConnectorCallHost for NoProviders {
    fn literal_input_keys(
        &self,
        _kind: orion_extension_api::ImplementationKind,
        _target: &str,
        _operation: &str,
    ) -> Vec<String> {
        Vec::new()
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
#[path = "tests.rs"]
mod tests;
