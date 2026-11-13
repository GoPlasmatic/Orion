//! The node's side of channel ingress: the [`IngressHost`] an edition's
//! channel driver dispatches through, and the loop that starts, reconciles
//! and stops each running driver.
//!
//! It sits beside `cron`, not inside `extensions`: like the scheduler it is a
//! consumer of the engine, the channel estate and the trace queue, and the
//! extension seams are below all three.
//!
//! [`dispatch`](IngressHost::dispatch) is one more ingress, and obeys the rules
//! the other four (HTTP sync, HTTP `/async`, `channel_call`, cron) do:
//!
//! 1. **Load one generation** for the delivery. The channel resolved from it,
//!    the guards applied from it and the engine that runs it are one build.
//! 2. **Resolve the channel by stable id.** A listener outlives a rename.
//! 3. **Build the metadata** the way every ingress does: `channel`, the
//!    transport's facts at `trigger` (with `type` = the protocol), `vars`
//!    stamped, engine-owned keys cleared.
//! 4. **Apply the guards** through the same [`admit`](crate::channel::guards::admit)
//!    every ingress calls, with [`Transport::Extension`] carrying the row the
//!    driver declared.
//! 5. **Execute** on that generation, holding the backpressure permit.
//! 6. **Trace** the run under the channel's `tracing` block, with the input,
//!    so a delivery the driver dead-letters can be found and replayed.
//! 7. **Count it** on the channel's message counters.
//!
//! What the driver does with the outcome (acknowledge, retry, dead-letter)
//! is the driver's; the dedup claim travels back in the [`Settle`] handle so
//! the driver can report it.
//!
//! The lifecycle: one supervised task per running driver. It starts the
//! driver, reconciles it against the live generation, then follows
//! [`RuntimeHandle::subscribe`] — the latest publish wins, so a burst of
//! reloads may skip a generation but never delivers one out of order — and
//! on shutdown calls the driver's `shutdown`. The task runs outside
//! `reload_lock`: a driver that takes seconds to rebuild a consumer never
//! holds up a deployment apply.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use orion_extension_api::{
    ChannelBinding, ChannelDriver, Delivery, DispatchOutcome, Dispatched, DriverHealth,
    FailureKind, GuardSet, IngressHost, NodeInfo, RunningDriver, Settle,
};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::channel::guards::{DedupClaim, GuardContext, GuardRequest, Transport};
use crate::channel::registry::ChannelRuntimeConfig;
use crate::engine::TRIGGER_KEY;
use crate::errors::{OrionError, StatusCode};
use crate::queue::TracePersistenceQueue;
use crate::runtime::{Criticality, RuntimeGeneration, RuntimeHandle, Shutdown, TaskRegistry};
use crate::storage::repositories::traces::TraceSink;

use crate::extensions::ChannelDriverRegistry;

/// What the ingress host needs from the node.
pub struct IngressDeps {
    pub runtime: Arc<RuntimeHandle>,
    pub trace_repo: Arc<dyn TraceSink>,
    pub persistence_queue: TracePersistenceQueue,
    /// The process's expression engine, for the channel guards.
    pub exprs: dataflow_rs::expr::ExprEngine,
    pub vars: Option<Arc<Value>>,
    pub node: NodeInfo,
    /// Cap on a serialized result, shared with the trace queue.
    pub max_result_size_bytes: usize,
    /// How long a driver's `shutdown` is given.
    pub shutdown_timeout: Duration,
}

/// The [`IngressHost`] for one protocol.
struct Host {
    protocol: String,
    guards: GuardSet,
    deps: Arc<IngressDeps>,
}

#[async_trait]
impl IngressHost for Host {
    fn protocol(&self) -> &str {
        &self.protocol
    }

    fn node(&self) -> &NodeInfo {
        &self.deps.node
    }

    async fn dispatch(&self, delivery: Delivery) -> Dispatched {
        dispatch(self, delivery).await
    }
}

async fn dispatch(host: &Host, delivery: Delivery) -> Dispatched {
    let deps = &host.deps;
    // 1. One generation for this delivery.
    let generation = deps.runtime.load();

    // 2. The channel, by stable id, of this protocol.
    let Some(runtime) = generation.channels.serviceable_by_id(&delivery.channel_id) else {
        let reason = match generation
            .channels
            .quarantine_reason_by_id(&delivery.channel_id)
        {
            Some(why) => format!(
                "channel '{}' failed to load and is not being served: {why}",
                delivery.channel_id
            ),
            None => format!(
                "no channel with id '{}' is served by this node",
                delivery.channel_id
            ),
        };
        return Dispatched::unsettled(DispatchOutcome::Unavailable { reason });
    };
    if runtime.channel.protocol != host.protocol {
        return Dispatched::unsettled(DispatchOutcome::Unavailable {
            reason: format!(
                "channel '{}' is a '{}' channel, not '{}'",
                delivery.channel_id, runtime.channel.protocol, host.protocol
            ),
        });
    }
    let channel = runtime.channel.name.clone();

    // 3. The metadata every ingress builds: platform-owned keys only.
    let mut trigger = delivery.trigger;
    trigger.insert("type".to_string(), Value::String(host.protocol.clone()));
    let mut metadata = json!({ "channel": channel, TRIGGER_KEY: Value::Object(trigger) });
    crate::engine::clear_error_context(&mut metadata);
    crate::engine::stamp_vars(&mut metadata, deps.vars.as_deref());

    let input_json = serde_json::to_string(&delivery.data).ok();
    let mut context = GuardContext::new(delivery.data, metadata);
    let headers: HashMap<String, String> = delivery
        .headers
        .into_iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value))
        .collect();
    let header_lookup = |name: &str| headers.get(&name.to_ascii_lowercase()).map(String::as_str);
    let caller_identity = delivery
        .caller_identity
        .unwrap_or_else(|| runtime.channel.channel_id.clone());
    let channel_runtime = Some(runtime.clone());

    // 4. The protocol's row of the guard matrix.
    let admission = crate::channel::guards::admit(GuardRequest {
        transport: Transport::Extension(host.guards),
        channel: &channel,
        runtime: &channel_runtime,
        context: &mut context,
        exprs: &deps.exprs,
        origin: None,
        caller_identity: &caller_identity,
        header: &header_lookup,
        auth_backoff: None,
        raw_body: None,
        dedup_key_fallback: delivery.dedup_key.as_deref(),
        dedup_owner: delivery.dedup_owner.as_deref(),
        default_timeout_ms: delivery.max_timeout_ms,
        max_timeout_ms: delivery.max_timeout_ms,
        oauth: None,
    })
    .await;
    let admission = match admission {
        Ok(admission) => admission,
        Err(refused) => {
            let outcome = refusal(&refused);
            if matches!(outcome, DispatchOutcome::Duplicate) {
                runtime.metrics.record_message(&channel, "duplicate");
            }
            return Dispatched::unsettled(outcome);
        }
    };
    let _backpressure_permit = admission.backpressure_permit;
    let claim = admission.dedup_claim;

    // 5. Execute on the generation the guards were read from. The rollout
    // bucket hashes the physical delivery, so a redelivery runs the version
    // its first attempt ran; with no owner, a random bucket per delivery.
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
            routing_bucket: Some(crate::engine::utils::rollout_bucket_for_identity(
                delivery.dedup_owner.as_deref(),
            )),
            profile: None,
        },
    )
    .await;

    // 6. The trace, every outcome included.
    record_trace(host, &runtime, &channel, input_json, &execution).await;

    // 7. The channel's counters.
    runtime
        .metrics
        .record_message(&channel, execution.outcome.status_label());
    runtime
        .metrics
        .record_message_duration(&channel, execution.duration.as_secs_f64());

    let outcome = match execution.outcome {
        crate::engine::RunOutcome::Ok => DispatchOutcome::Completed {
            output: execution.data(),
        },
        crate::engine::RunOutcome::Timeout(ms) => DispatchOutcome::Failed {
            kind: FailureKind::Timeout,
            message: format!("processing timed out after {ms}ms"),
        },
        crate::engine::RunOutcome::EngineError(e) => DispatchOutcome::Failed {
            kind: FailureKind::Engine,
            message: e.to_string(),
        },
        crate::engine::RunOutcome::WorkflowErrors(summary) => DispatchOutcome::Failed {
            kind: FailureKind::Workflow,
            message: summary,
        },
    };
    match claim {
        Some(claim) => Dispatched::new(outcome, Box::new(ClaimSettle(claim))),
        None => Dispatched::unsettled(outcome),
    }
}

/// A guard refusal, as a driver acts on it.
///
/// `RateLimitKeyUnavailable` is not grouped with `RateLimited`: being over a
/// limit clears with time, but a `rate_limit.key_logic` that cannot be
/// evaluated against this message fails against every redelivery of it, so
/// deferring it would block the driver's stream for as long as the channel
/// keeps that expression.
fn refusal(error: &OrionError) -> DispatchOutcome {
    match error {
        OrionError::Http { status, .. } if *status == StatusCode::CONFLICT => {
            DispatchOutcome::Duplicate
        }
        OrionError::RateLimited(message) => DispatchOutcome::Deferred {
            reason: message.clone(),
        },
        OrionError::ServiceUnavailable { reason, message } if reason.is_transient() => {
            DispatchOutcome::Deferred {
                reason: message.clone(),
            }
        }
        other => {
            let (_, code, message) = other.response_parts();
            DispatchOutcome::Rejected {
                code: code.to_string(),
                message: message.into_owned(),
            }
        }
    }
}

/// The dedup claim a delivery took at admission, settled by the driver.
struct ClaimSettle(DedupClaim);

#[async_trait]
impl Settle for ClaimSettle {
    async fn committed(self: Box<Self>) {
        self.0.confirm().await;
    }

    async fn abandoned(self: Box<Self>) {
        self.0.release().await;
    }
}

/// Persist the trace for one dispatched delivery, under the channel's own
/// `tracing` block. A trace that cannot be written is logged and dropped: it
/// must not change what the driver does with the delivery.
async fn record_trace(
    host: &Host,
    runtime: &ChannelRuntimeConfig,
    channel: &str,
    input_json: Option<String>,
    execution: &crate::engine::Execution,
) {
    let deps = &host.deps;
    let has_errors = !execution.outcome.is_ok();
    let cfg = runtime.trace_storage;
    if !crate::queue::trace_record::TracePlan::decide(&cfg, has_errors).persists() {
        return;
    }
    let Ok(result_json) = serde_json::to_string(&execution.data()) else {
        tracing::warn!(
            channel,
            "Failed to serialize a delivery's result for its trace"
        );
        return;
    };
    if deps.max_result_size_bytes > 0 && result_json.len() > deps.max_result_size_bytes {
        crate::metrics::record_trace_dropped("result_too_large");
        tracing::warn!(
            channel,
            result_bytes = result_json.len(),
            cap = deps.max_result_size_bytes,
            "Delivery result exceeds trace_queue.max_result_size_bytes; trace not stored"
        );
        return;
    }
    let task_trace_json = crate::engine::utils::serialize_task_trace_capped(
        execution.task_trace.as_ref(),
        deps.max_result_size_bytes,
        channel,
    );
    crate::queue::trace_record::route_store_completed(
        &cfg,
        &*deps.trace_repo,
        &deps.persistence_queue,
        crate::queue::trace_record::CompletedTrace {
            mode: &host.protocol,
            channel,
            channel_id: Some(&runtime.channel.channel_id),
            input_json,
            response_json: std::borrow::Cow::Owned(result_json),
            duration_ms: execution.duration.as_secs_f64() * 1000.0,
            has_errors,
            failed: matches!(
                execution.outcome,
                crate::engine::RunOutcome::Timeout(_) | crate::engine::RunOutcome::EngineError(_)
            ),
            task_trace_json,
        },
    )
    .await;
}

/// The bindings `protocol`'s driver is reconciled against in `generation`.
fn bindings(generation: &RuntimeGeneration, protocol: &str) -> Vec<ChannelBinding> {
    generation
        .channels
        .serviceable_of_protocol(protocol)
        .iter()
        .map(|runtime| {
            let channel = &runtime.channel;
            ChannelBinding::new(
                channel.channel_id.clone(),
                channel.name.clone(),
                fingerprint(runtime),
                serde_json::from_str(&channel.transport_config_json).unwrap_or(Value::Null),
            )
            .with_timeout_ms(runtime.parsed_config.timeout_ms)
        })
        .collect()
}

/// A hash of everything in the stored channel a driver could act on, so a
/// change to any of it — a topic moved to another channel, a new workflow —
/// reads as a changed binding.
fn fingerprint(runtime: &ChannelRuntimeConfig) -> String {
    let channel = &runtime.channel;
    let mut hasher = Sha256::new();
    for part in [
        channel.channel_id.as_str(),
        &channel.version.to_string(),
        &channel.name,
        &channel.protocol,
        &channel.channel_type,
        &channel.transport_config_json,
        &channel.config_json,
        channel.workflow_id.as_deref().unwrap_or(""),
        &channel.status,
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(7 + digest.len() * 2);
    out.push_str("sha256:");
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// One composed driver that serves ingress, as the node tracks it.
struct DriverSlot {
    protocol: String,
    /// Set once `start` returned a running driver.
    running: Mutex<Option<Arc<dyn RunningDriver>>>,
    /// Why the last `start` failed, while it has not succeeded.
    start_error: Mutex<Option<String>>,
}

impl DriverSlot {
    fn health(&self) -> DriverHealth {
        if let Some(running) = lock(&self.running).as_ref() {
            return running.health();
        }
        match lock(&self.start_error).as_ref() {
            Some(why) => DriverHealth::Degraded(format!("driver did not start: {why}")),
            None => DriverHealth::Degraded("driver is starting".to_string()),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The node's ingress drivers: what `/health` and `/readyz` read.
#[derive(Default)]
pub struct IngressDrivers {
    slots: Vec<Arc<DriverSlot>>,
}

impl IngressDrivers {
    /// No driver serves ingress.
    pub fn none() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Every ingress driver's protocol and health, in protocol order.
    pub fn health(&self) -> Vec<(String, DriverHealth)> {
        self.slots
            .iter()
            .map(|slot| (slot.protocol.clone(), slot.health()))
            .collect()
    }

    /// Whether every ingress driver is serving (`/readyz`).
    pub fn all_ready(&self) -> bool {
        self.slots
            .iter()
            .all(|slot| !matches!(slot.health(), DriverHealth::Degraded(_)))
    }
}

/// Start one supervised task per composed driver that declares
/// [`ChannelDriverCapabilities::ingress`](orion_extension_api::ChannelDriverCapabilities::ingress).
/// A driver that does not is the node's own business (the HTTP data plane,
/// the cron scheduler) and gets no task.
pub fn start(
    tasks: &TaskRegistry,
    drivers: &ChannelDriverRegistry,
    deps: IngressDeps,
) -> Arc<IngressDrivers> {
    let deps = Arc::new(deps);
    let mut slots = Vec::new();
    for protocol in drivers.protocol_ids() {
        let Some(driver) = drivers.get(&protocol) else {
            continue;
        };
        let Some(guards) = driver.capabilities().ingress else {
            continue;
        };
        let slot = Arc::new(DriverSlot {
            protocol: protocol.clone(),
            running: Mutex::new(None),
            start_error: Mutex::new(None),
        });
        let host: Arc<dyn IngressHost> = Arc::new(Host {
            protocol: protocol.clone(),
            guards,
            deps: deps.clone(),
        });
        let task = Arc::new(DriverTask {
            driver,
            host,
            slot: slot.clone(),
            deps: deps.clone(),
        });
        // A task name is `'static`, and the set of protocols is fixed at boot
        // (one name per composed driver, leaked once).
        let name: &'static str = Box::leak(format!("channel_driver:{protocol}").into_boxed_str());
        tasks.supervise(name, Criticality::Required, move |shutdown| {
            task.clone().run(shutdown)
        });
        slots.push(slot);
    }
    Arc::new(IngressDrivers { slots })
}

struct DriverTask {
    driver: Arc<dyn ChannelDriver>,
    host: Arc<dyn IngressHost>,
    slot: Arc<DriverSlot>,
    deps: Arc<IngressDeps>,
}

impl DriverTask {
    /// Start (once), reconcile against the live generation, follow publishes,
    /// stop on shutdown. Returning before shutdown is a failure the
    /// supervisor retries with backoff — which is how a driver whose `start`
    /// failed is started again.
    async fn run(self: Arc<Self>, mut shutdown: Shutdown) {
        let running = match self.ensure_started().await {
            Some(running) => running,
            None => return,
        };
        let mut published = self.deps.runtime.subscribe();
        // Mark the current value seen before the first reconcile: a publish
        // that lands after this line is a change the loop below receives.
        published.borrow_and_update();
        let protocol = self.slot.protocol.as_str();
        running
            .reconcile(bindings(&self.deps.runtime.load(), protocol))
            .await;
        loop {
            tokio::select! {
                _ = shutdown.signalled() => break,
                changed = published.changed() => {
                    if changed.is_err() {
                        // The handle is gone: the node is shutting down.
                        break;
                    }
                    published.borrow_and_update();
                    running
                        .reconcile(bindings(&self.deps.runtime.load(), protocol))
                        .await;
                }
            }
        }
        if shutdown.is_signalled() {
            let deadline = Instant::now() + self.deps.shutdown_timeout;
            running.shutdown(deadline).await;
        }
    }

    async fn ensure_started(&self) -> Option<Arc<dyn RunningDriver>> {
        if let Some(running) = lock(&self.slot.running).clone() {
            return Some(running);
        }
        let protocol = self.slot.protocol.as_str();
        match self.driver.start(self.host.clone()).await {
            Ok(Some(running)) => {
                let running: Arc<dyn RunningDriver> = Arc::from(running);
                *lock(&self.slot.start_error) = None;
                *lock(&self.slot.running) = Some(running.clone());
                tracing::info!(protocol, "Channel driver started");
                Some(running)
            }
            Ok(None) => {
                *lock(&self.slot.start_error) =
                    Some("declares ingress but start returned no running driver".to_string());
                tracing::error!(
                    protocol,
                    "Channel driver declares ingress but its start returned no running driver"
                );
                None
            }
            Err(e) => {
                *lock(&self.slot.start_error) = Some(e.to_string());
                tracing::error!(protocol, error = %e, "Channel driver failed to start");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conflict_is_a_duplicate_and_a_busy_node_defers() {
        assert_eq!(
            refusal(&OrionError::conflict("seen")),
            DispatchOutcome::Duplicate
        );
        assert!(matches!(
            refusal(&OrionError::RateLimited("slow down".into())),
            DispatchOutcome::Deferred { .. }
        ));
        assert!(matches!(
            refusal(&OrionError::RateLimitKeyUnavailable("no key".into())),
            DispatchOutcome::Rejected { .. }
        ));
    }

    #[test]
    fn a_rejection_carries_the_envelope_code() {
        let outcome = refusal(&OrionError::validation(
            "validation_logic refused the message",
        ));
        assert!(
            matches!(&outcome, DispatchOutcome::Rejected { code, .. } if !code.is_empty()),
            "a validation refusal is terminal and coded: {outcome:?}"
        );
    }
}
