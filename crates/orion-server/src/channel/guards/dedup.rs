//! Request deduplication: the idempotency claim a channel takes on a key, and
//! the settlement that decides whether a redelivery is a duplicate.
//!
//! Split out of `guards` as one concept. [`DedupClaim`] lives here rather than
//! beside the other public types because it is this guard's own contract with
//! its caller — a claim is settled by whoever admitted it.

use std::sync::Arc;

use super::ChannelRuntimeConfig;
use super::*;
use crate::connector::cache_backend::CacheBackend;
use crate::errors::OrionError;
use crate::metrics;

/// A held deduplication key, from admission until the delivery is settled.
///
/// The claim exists because inserting the key at admission and never
/// revisiting it loses messages on any transport that redelivers: the first
/// attempt registers the key, the attempt then fails without committing, and
/// the redelivery is refused as a duplicate of *itself* — never processed,
/// never dead-lettered. [`confirm`](Self::confirm) and
/// [`release`](Self::release) are the two ways a delivery ends.
pub struct DedupClaim {
    store: Arc<dyn CacheBackend>,
    key: String,
    window_secs: u64,
}

/// Value written by [`DedupClaim::confirm`]. Never equal to any owner token,
/// so a later delivery — including a replay of the same physical message —
/// reads it as "someone else finished this key" and is suppressed.
pub(super) const DEDUP_SETTLED: &str = "settled";

impl DedupClaim {
    /// The delivery is durably accounted for (processed, or preserved in a
    /// dead-letter queue). Mark the key settled for the rest of the window so
    /// any further delivery carrying it — a producer-side duplicate, or a
    /// replay of this very record whose offset commit was lost — is
    /// suppressed.
    pub async fn confirm(self) {
        if let Err(e) = self
            .store
            .set_ex(&self.key, DEDUP_SETTLED, self.window_secs)
            .await
        {
            // Not fatal: the key still holds this delivery's owner token, so
            // a replay of the same message reprocesses it (at-least-once)
            // rather than being dropped.
            tracing::warn!(
                key = %self.key,
                error = %e,
                "Could not mark the idempotency key settled; a redelivery of this message would be reprocessed"
            );
        }
    }

    /// The delivery did not happen and the message is coming back. Free the
    /// key so the redelivery is processed on its merits.
    pub async fn release(self) {
        if let Err(e) = self.store.remove(&self.key).await {
            // Also not fatal, for the same reason: the owner token is still
            // in place and the redelivery recognises it as its own.
            tracing::warn!(
                key = %self.key,
                error = %e,
                "Could not release the idempotency key after an unsettled delivery"
            );
        }
    }
}

/// Check per-channel request deduplication, and claim the key for this
/// delivery when it is free.
///
/// `header` is a lookup view over the transport's headers (the idempotency
/// header name is per-channel config); `key_fallback` is the key a transport
/// carries out of band — Kafka's record key — consulted only when that header
/// is absent. Returns `Err(Conflict)` when the key is held by an earlier
/// delivery, and otherwise the [`DedupClaim`] the caller must settle.
///
/// **Why the claim carries an owner.** The key is claimed *before* the
/// message runs, because that is the only moment at which the check is
/// atomic against a concurrent delivery. On a transport that redelivers,
/// that alone loses messages: attempt 0 registers the key, the attempt fails
/// without committing its offset, and the redelivery reads the key its own
/// previous attempt wrote — a `409` that the Kafka ingress translates into
/// "already handled", commits, and drops. So each delivery claims under an
/// owner token, and `owner` is that token:
///
/// * A transport that can redeliver the same physical message passes a
///   *stable* token for it (Kafka: `topic/partition/offset`). A replay finds
///   its own token and proceeds — it is the same delivery, not a second one.
/// * Every other transport passes `None` and gets a fresh token per call, so
///   any second delivery sees a token that is not its own and is refused.
///
/// [`DedupClaim::confirm`] then overwrites the token with a settled marker
/// once the delivery is accounted for, which is what suppresses a replay of a
/// message that *did* run but whose offset commit was lost.
pub(super) async fn check_deduplication(
    channel: &str,
    channel_config: &Option<Arc<ChannelRuntimeConfig>>,
    header: HeaderLookup<'_>,
    key_fallback: Option<&str>,
    owner: Option<&str>,
) -> Result<Option<DedupClaim>, OrionError> {
    let Some(cfg) = channel_config else {
        return Ok(None);
    };
    let Some(ref dedup) = cfg.parsed_config.deduplication else {
        return Ok(None);
    };
    let Some(ref store) = cfg.dedup_store else {
        return Ok(None);
    };
    let Some(key) = header(&dedup.header).or_else(|| key_fallback.map(str::to_string)) else {
        return Ok(None);
    };

    let window = dedup.window_secs.unwrap_or(300);
    // Scope the key per channel (same format family as the response cache
    // key at `compute_cache_key`) — raw tokens would collide across
    // channels sharing a backend.
    let scoped_key = format!("dedup:{channel}:{key}");
    // A one-shot delivery gets a token nothing else can ever present, so
    // the owner comparison below can only ever match a transport that
    // deliberately supplied a stable one.
    let one_shot;
    let owner = match owner {
        Some(owner) => owner,
        None => {
            one_shot = uuid::Uuid::new_v4().simple().to_string();
            one_shot.as_str()
        }
    };
    // N7: a backend error is resolved by the channel's `on_backend_error`
    // policy. The default (`allow`) fails open — availability wins over
    // strict idempotency. `deny` fails closed with 503, never 409: the
    // request is not a known duplicate, it is *unverifiable*.
    let holder = match store.claim_dedup_key(&scoped_key, owner, window).await {
        Ok(holder) => holder,
        Err(e) => {
            metrics::record_error("dedup_backend");
            match dedup.on_backend_error {
                crate::channel::BackendErrorPolicy::Allow => {
                    tracing::warn!(
                        channel = %channel,
                        error = %e,
                        header = %dedup.header,
                        "Dedup backend error; failing open (request allowed without dedup check)"
                    );
                    // Nothing was stored, so there is nothing to settle.
                    return Ok(None);
                }
                crate::channel::BackendErrorPolicy::Deny => {
                    tracing::warn!(
                        channel = %channel,
                        error = %e,
                        header = %dedup.header,
                        "Dedup backend error; failing closed (request refused)"
                    );
                    return Err(OrionError::unavailable(
                        crate::errors::Unavailable::GuardBackend,
                        format!(
                            "Channel '{channel}' cannot verify the idempotency key: the \
                             deduplication backend is unavailable and the channel is \
                             configured to fail closed"
                        ),
                    ));
                }
            }
        }
    };
    match holder {
        // The key was free and is ours for the window.
        None => {}
        // Our own token: this is the same delivery arriving again because
        // the last attempt never settled. Reprocess it — refusing here is
        // how a message gets committed without ever running.
        Some(ref held) if held == owner => {
            tracing::debug!(
                channel = %channel,
                key = %key,
                "Redelivery of an unsettled message; the idempotency claim is its own"
            );
        }
        Some(_) => {
            return Err(OrionError::Conflict(format!(
                "Duplicate request: idempotency key '{key}' already seen"
            )));
        }
    }
    Ok(Some(DedupClaim {
        store: store.clone(),
        key: scoped_key,
        window_secs: window,
    }))
}
