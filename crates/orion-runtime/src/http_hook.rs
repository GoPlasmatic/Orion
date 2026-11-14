//! The node's side of the data-plane HTTP hook: the [`HttpHost`] a hooked
//! channel's driver runs the channel's workflow through.
//!
//! It sits beside [`crate::ingress`], and for the same reason: it consumes
//! the engine, the channel estate and the trace queue. The HTTP layer builds
//! one [`RequestHost`] per request, after the request's own guards (the
//! `HttpHook` row) passed, and hands it to the driver's `serve_http`.
//!
//! Each [`run`](HttpHost::run) is one more ingress, and obeys the rules the
//! others do: one generation, the platform's metadata, the guards of its row
//! (`HttpHookRun`, only the declared `validation_logic`), execution on that
//! generation, a trace with the protocol as its `mode`, the channel's message
//! counters.
//!
//! **Which generation a run executes on.** While `serve_http` is pending,
//! the request is one unit of work: every run executes on the generation the
//! handler loaded, so a GraphQL document's fields all see one engine and one
//! estate. Once a streamed body is returned the host
//! [releases](RequestHost::release_generation) it (holding it for the
//! stream's life would pin the old engine and estate in memory for hours),
//! and each later run is its own unit of work: it loads the current
//! generation once and runs only if the channel's runtime config there is
//! the very one the request was admitted under (`Arc::ptr_eq`). An unchanged
//! channel is carried over by `Arc` across unrelated deploys, so the check
//! holds through them; a changed one (a new workflow, a new auth policy)
//! answers `Unavailable` and the driver ends the stream, so the client
//! reconnects under the channel as it now stands.
//!
//! **What a stream holds.** [`stream_body`] adapts a driver's [`BodyStream`]
//! to the HTTP body. The request's backpressure permit (and its idempotency
//! claim) move into it, so a stream counts against
//! `backpressure.max_concurrent_per_node` until it ends or its client
//! disconnects; so does the open-streams gauge. It ends on its own when the
//! node drains ([`StreamDrain`]), whatever the driver does, so a rolling
//! deploy never waits for the force timeout to cut a stream off.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use orion_extension_api::{
    BodyStream, DispatchOutcome, HttpHookGuards, HttpHost, NodeInfo, ShutdownSignal,
};
use serde_json::{Map, Value, json};

use crate::channel::guards::{GuardContext, GuardRequest, Transport};
use crate::channel::registry::ChannelRuntimeConfig;
use crate::engine::TRIGGER_KEY;
use crate::queue::TracePersistenceQueue;
use crate::runtime::{RuntimeGeneration, RuntimeHandle};

/// What every hooked request's host needs from the node. Built once, at
/// assembly.
pub struct HookDeps {
    pub runtime: Arc<RuntimeHandle>,
    pub traces: Arc<dyn crate::storage::repositories::traces::TraceRepository>,
    pub persistence_queue: TracePersistenceQueue,
    /// The process's expression engine, for the run's guards.
    pub exprs: dataflow_rs::expr::ExprEngine,
    pub vars: Option<Arc<Value>>,
    pub node: NodeInfo,
    /// Cap on a serialized result, shared with the trace queue.
    pub max_result_size_bytes: usize,
    /// Fires when the node stops serving; what [`HttpHost::shutdown`] waits on.
    pub drain: StreamDrain,
}

/// The node's "stop serving" signal for long-lived bodies: fired once the
/// drain gate resolves, after `/readyz` has gone unready and the drain period
/// has let the load balancer stop routing here. Ending a stream any earlier
/// would send its client's reconnect back to this node.
#[derive(Clone)]
pub struct StreamDrain(Arc<tokio::sync::watch::Sender<bool>>);

impl Default for StreamDrain {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::watch::Sender::new(false)))
    }
}

impl StreamDrain {
    /// Tell every open stream the node is draining. Idempotent.
    pub fn fire(&self) {
        self.0.send_replace(true);
    }

    /// Whether [`Self::fire`] was called.
    pub fn is_fired(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once [`Self::fire`] has been called (at once, if it was).
    pub fn signal(&self) -> ShutdownSignal {
        let mut rx = self.0.subscribe();
        Box::pin(async move {
            // An error means the sender is gone: the node is shutting down.
            let _ = rx.wait_for(|fired| *fired).await;
        })
    }
}

/// The [`HttpHost`] for one request to a hooked channel.
pub struct RequestHost {
    deps: Arc<HookDeps>,
    guards: HttpHookGuards,
    /// The runtime config the request was admitted under: the identity a
    /// later run is held to.
    admitted: Arc<ChannelRuntimeConfig>,
    /// The generation the handler loaded, while `serve_http` is pending.
    generation: Mutex<Option<Arc<RuntimeGeneration>>>,
    request_id: String,
    /// The verified claims, merged into every run's metadata.
    claims: Option<Value>,
}

impl RequestHost {
    /// The host for one admitted request, executing on `generation` until
    /// [`Self::release_generation`].
    pub fn new(
        deps: Arc<HookDeps>,
        guards: HttpHookGuards,
        admitted: Arc<ChannelRuntimeConfig>,
        generation: Arc<RuntimeGeneration>,
        request_id: String,
        claims: Option<Value>,
    ) -> Arc<Self> {
        Arc::new(Self {
            deps,
            guards,
            admitted,
            generation: Mutex::new(Some(generation)),
            request_id,
            claims,
        })
    }

    /// Stop executing on the request's generation: called once `serve_http`
    /// has returned. A later run (from a streamed body) loads the current one.
    pub fn release_generation(&self) {
        lock(&self.generation).take();
    }

    /// The runtime config the request was admitted under.
    pub fn admitted(&self) -> &Arc<ChannelRuntimeConfig> {
        &self.admitted
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[async_trait]
impl HttpHost for RequestHost {
    fn protocol(&self) -> &str {
        &self.admitted.channel.protocol
    }

    fn node(&self) -> &NodeInfo {
        &self.deps.node
    }

    async fn run(&self, data: Value, trigger: Map<String, Value>) -> DispatchOutcome {
        run(self, data, trigger).await
    }

    fn shutdown(&self) -> ShutdownSignal {
        self.deps.drain.signal()
    }
}

/// The generation a run executes on, and the channel's config in it; `None`
/// when the channel was redeployed since the request was admitted.
fn generation_for_run(
    host: &RequestHost,
) -> Option<(Arc<RuntimeGeneration>, Arc<ChannelRuntimeConfig>)> {
    if let Some(generation) = lock(&host.generation).clone() {
        return Some((generation, host.admitted.clone()));
    }
    let generation = host.deps.runtime.load();
    let current = generation
        .channels
        .serviceable_by_id(&host.admitted.channel.channel_id)?;
    Arc::ptr_eq(&current, &host.admitted).then_some((generation, current))
}

async fn run(host: &RequestHost, data: Value, mut trigger: Map<String, Value>) -> DispatchOutcome {
    let deps = &host.deps;
    let Some((generation, runtime)) = generation_for_run(host) else {
        return DispatchOutcome::Unavailable {
            reason: format!(
                "channel '{}' was redeployed after this request was admitted; reconnect",
                host.admitted.channel.name
            ),
        };
    };
    let channel = runtime.channel.name.clone();
    let protocol = runtime.channel.protocol.clone();

    // The metadata every ingress builds: platform-owned keys only, with the
    // request's id on the trigger so its runs can be grouped.
    trigger.insert("type".to_string(), Value::String(protocol.clone()));
    trigger.insert(
        "request_id".to_string(),
        Value::String(host.request_id.clone()),
    );
    let mut metadata = json!({ "channel": channel, TRIGGER_KEY: Value::Object(trigger) });
    crate::engine::clear_error_context(&mut metadata);
    crate::engine::stamp_vars(&mut metadata, deps.vars.as_deref());
    if let Some(claims) = host.claims.clone() {
        metadata = crate::channel::guards::merge_auth_claims(metadata, claims);
    }

    let input_json = serde_json::to_string(&data).ok();
    let mut context = GuardContext::new(data, metadata);
    let no_headers = |_: &str| -> Option<&str> { None };
    let channel_runtime = Some(runtime.clone());
    let admission = crate::channel::guards::admit(GuardRequest {
        transport: Transport::HttpHookRun(host.guards),
        channel: &channel,
        runtime: &channel_runtime,
        context: &mut context,
        exprs: &deps.exprs,
        origin: None,
        caller_identity: &channel,
        header: &no_headers,
        auth_backoff: None,
        raw_body: None,
        dedup_key_fallback: None,
        dedup_owner: None,
        // A run's deadline is the channel's `timeout_ms`, as on the sync path.
        default_timeout_ms: None,
        max_timeout_ms: None,
        oauth: None,
    })
    .await;
    let admission = match admission {
        Ok(admission) => admission,
        Err(refused) => return crate::ingress::refusal(&refused),
    };

    let capture = runtime
        .trace_storage
        .task_details
        .then_some(crate::engine::TraceCapture {
            max_snapshot_bytes: deps.max_result_size_bytes,
        });
    let execution = crate::runtime::execute_admitted_on_generation(
        &generation,
        &channel,
        context.data(),
        context.metadata(),
        crate::engine::ExecOpts {
            cancel: None,
            accountant: None,
            timeout_ms: admission.timeout_ms,
            capture,
            // Every run of one request routes alike, so a document's fields
            // never straddle two rollout versions.
            routing_bucket: Some(crate::engine::utils::rollout_bucket_for_identity(Some(
                &host.request_id,
            ))),
            profile: None,
        },
    )
    .await;
    // The generation is not needed past execution; drop it before the trace
    // write, which may wait on the database.
    drop(generation);

    crate::queue::trace_record::record_run(
        crate::queue::trace_record::RunTraceSink {
            trace_repo: &*deps.traces,
            persistence_queue: &deps.persistence_queue,
            max_result_size_bytes: deps.max_result_size_bytes,
        },
        &protocol,
        &runtime,
        &channel,
        input_json,
        &execution,
    )
    .await;
    runtime
        .metrics
        .record_message(&channel, execution.outcome.status_label());
    runtime
        .metrics
        .record_message_duration(&channel, execution.duration.as_secs_f64());
    crate::ingress::outcome_of(execution)
}

/// What a streamed body keeps until it ends: the guards' tokens the request
/// was admitted with.
#[derive(Default)]
pub struct StreamHold {
    /// The channel's concurrency permit: one stream, one slot.
    pub permit: Option<tokio::sync::OwnedSemaphorePermit>,
    /// The request's idempotency claim, which stands for its window.
    pub claim: Option<crate::channel::guards::DedupClaim>,
}

/// Everything a streamed body owns while it is open.
struct OpenBody {
    stream: Box<dyn BodyStream>,
    drain: ShutdownSignal,
    _hold: StreamHold,
    _counted: crate::metrics::OpenStream,
    /// The request's host: what the driver's later runs go through, and the
    /// admitted config they are held to.
    _host: Arc<RequestHost>,
}

/// A driver's streamed body as an HTTP body: its chunks, in order, until the
/// driver ends it, the client goes away (the body is dropped), or the node
/// drains. Whichever ends it releases the permit and counts the stream out.
pub fn stream_body(
    stream: Box<dyn BodyStream>,
    host: Arc<RequestHost>,
    hold: StreamHold,
) -> impl futures::Stream<Item = Result<bytes::Bytes, std::convert::Infallible>> + Send + 'static {
    let open = OpenBody {
        stream,
        drain: host.deps.drain.signal(),
        _hold: hold,
        _counted: crate::metrics::OpenStream::open(&host.admitted.channel.name),
        _host: host,
    };
    futures::stream::unfold(open, |mut open| async move {
        // The drain signal resolves once and the stream ends then, so it is
        // never polled again after resolving.
        let chunk = tokio::select! {
            chunk = open.stream.next() => chunk,
            () = &mut open.drain => None,
        }?;
        Some((Ok(bytes::Bytes::from(chunk)), open))
    })
}
