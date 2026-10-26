//! `channel_call`'s host: the serving generation a call dispatches into.
//!
//! The function itself, and every rule about the call position, the child's
//! limits and the refusals' words, is the engine's
//! ([`crate::engine::functions::channel_call`]). What it cannot know is which
//! channels this node serves and under which guards; [`GenerationChannels`]
//! answers that from one published generation per call.

use std::sync::Arc;

use dataflow_rs::datavalue::OwnedDataValue;
use serde_json::Value;

use crate::channel::ChannelRuntimeConfig;
use crate::engine::functions::channel_call::{
    ChannelCallHost, ChannelCallTarget, ChildAdmission, ChildRequest, guard_refused, not_found,
};
use crate::engine::{ErrorClass, HandlerError};
use crate::runtime::{RuntimeGeneration, RuntimeHandle};

/// [`ChannelCallHost`] over the node's serving generation.
pub struct GenerationChannels {
    /// The node's serving generation, loaded once per call.
    pub runtime: Arc<RuntimeHandle>,
    /// The process's expression engine, which compiled the target channel's
    /// guards and evaluates them (I188).
    pub exprs: dataflow_rs::expr::ExprEngine,
}

impl ChannelCallHost for GenerationChannels {
    fn resolve(&self, target: &str) -> Result<Box<dyn ChannelCallTarget>, HandlerError> {
        // Inherits any generation pinned by the parent run (V2-05.3).
        let generation = self.runtime.load_pinned_or_current();
        let channel = serviceable_target(&generation, target)?;
        Ok(Box::new(ResolvedTarget {
            generation,
            channel,
            exprs: self.exprs.clone(),
        }))
    }
}

/// One target and the generation it was read from.
struct ResolvedTarget {
    generation: Arc<RuntimeGeneration>,
    channel: Arc<ChannelRuntimeConfig>,
    exprs: dataflow_rs::expr::ExprEngine,
}

#[async_trait::async_trait]
impl ChannelCallTarget for ResolvedTarget {
    async fn admit(&self, request: ChildRequest<'_>) -> dataflow_rs::Result<ChildAdmission> {
        let ChildRequest {
            target,
            data,
            metadata,
            caller,
            default_timeout_ms,
        } = request;
        let target_runtime = Some(Arc::clone(&self.channel));
        // `validation_logic` is the one guard of this transport that reads
        // the message, as JSON; a target without one is admitted with no
        // copy of either.
        let mut view = if self.channel.validation_logic.is_some() {
            crate::channel::guards::GuardContext::new(Value::from(data), Value::from(metadata))
        } else {
            crate::channel::guards::GuardContext::new(Value::Null, Value::Null)
        };
        // The header view is the metadata inherited from the originating
        // request, so a target whose `rate_limit.key_logic` reads a header
        // still resolves one on this path.
        let header = |name: &str| {
            metadata
                .get("headers")
                .and_then(|headers| headers.get(name))
                .and_then(OwnedDataValue::as_str)
        };
        let admission = crate::channel::guards::admit(crate::channel::guards::GuardRequest {
            transport: crate::channel::guards::Transport::ChannelCall,
            channel: target,
            runtime: &target_runtime,
            context: &mut view,
            exprs: &self.exprs,
            origin: None,
            caller_identity: caller,
            header: &header,
            auth_backoff: None,
            raw_body: None,
            dedup_key_fallback: None,
            dedup_owner: None,
            default_timeout_ms: Some(default_timeout_ms),
            max_timeout_ms: None,
            oauth: None,
        })
        .await
        .map_err(|e| guard_refused(target, e))?;
        Ok(ChildAdmission {
            timeout_ms: admission.timeout_ms,
            permit: admission
                .backpressure_permit
                .map(|permit| Box::new(permit) as Box<dyn std::any::Any + Send>),
        })
    }

    fn engine(&self) -> &dataflow_rs::Engine {
        &self.generation.engine
    }
}

/// The target's runtime config in `generation`, when it may be called: not
/// quarantined (F35), active, and not a cron channel (D6).
///
/// A target absent from the registry is not an active channel, and is
/// refused loudly: a run on an unknown channel matches zero workflows and
/// reports success, which would silently drop the call (a typo'd channel
/// name would "work" with `output` never populated).
///
/// A cron channel is not callable. Its workflow is meant to run once per
/// occurrence, recorded in the ledger and serialised by its singleton key; a
/// `channel_call` would run it with none of that — no occurrence row, and no
/// lock, so a caller could trivially overlap a `forbid` schedule with
/// itself. The refusal is here rather than in the guard matrix because it is
/// about *what the channel is*, not about which guards this transport
/// applies.
fn serviceable_target(
    generation: &RuntimeGeneration,
    target: &str,
) -> Result<Arc<ChannelRuntimeConfig>, HandlerError> {
    let runtime = generation
        .channels
        .require_serviceable(target)
        .map_err(|e| {
            HandlerError::new(
                ErrorClass::Backend,
                format!("channel_call to '{target}': {e}"),
            )
        })?
        .ok_or_else(|| not_found(target))?;
    if runtime.cron.is_some() {
        // 1.12's text, the two runs of spaces included (a line continuation
        // that lost its backslash): a client reads it, so it holds.
        return Err(HandlerError::new(
            ErrorClass::Backend,
            format!(
                "channel_call to '{target}': it is a cron channel, which                          runs only on its own schedule. Call the workflow it names                          directly, or give the shared work a channel of its own."
            ),
        ));
    }
    Ok(runtime)
}

#[cfg(test)]
#[path = "channel_call_tests.rs"]
mod tests;
