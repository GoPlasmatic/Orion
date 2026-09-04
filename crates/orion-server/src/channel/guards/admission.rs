//! The four guards that answer yes or no.
//!
//! Channel `auth`, the origin allow-list, `validation_logic` and backpressure
//! share a shape the other three do not: no key to derive, no backend entry to
//! write or settle. A refusal is the whole result, and the only state any of
//! them holds is the semaphore permit backpressure hands to the caller.

use dataflow_rs::datalogic_rs;
use std::sync::Arc;

use serde_json::{Value, json};

use super::ChannelRuntimeConfig;
use super::*;
use crate::errors::OrionError;
use crate::metrics;

/// Reject the request when an `Origin` header is present and the channel's
/// allow-list does not name it.
///
/// Authenticate the caller against the channel's compiled `auth` policy.
///
/// A channel with no `auth` config is unauthenticated, which is what every
/// channel was before 1.0 and what every stored channel still is until an
/// operator adds the key. That default is why this guard is a no-op rather
/// than a refusal when the policy is absent.
///
/// The refusal is `401` with one message for every cause. Distinguishing
/// "no header" from "wrong key" from "malformed signature" would tell an
/// unauthenticated caller which half of the credential they had right.
pub(super) async fn check_auth(
    channel: &str,
    channel_config: &Option<Arc<ChannelRuntimeConfig>>,
    header: HeaderLookup<'_>,
    raw_body: Option<&[u8]>,
    datalogic: &datalogic_rs::Engine,
    backoff: Option<&crate::auth::FailedAuthTracker>,
    client: &str,
) -> Result<Option<Value>, OrionError> {
    let Some(cfg) = channel_config else {
        return Ok(None);
    };
    let Some(ref auth) = cfg.auth else {
        return Ok(None);
    };

    // S12, extended to the data plane. The admin API key has had a
    // failed-attempt budget since S12; a channel's own `auth.keys` had none,
    // so the *public* credential faced unlimited online guessing at whatever
    // rate the channel's rate limit allowed — and that limit is off by
    // default. Counted only for the modes where every failure is a wrong
    // credential (see `CompiledAuth::failures_are_guesses`).
    // `channel\u{1f}client`, not the client alone: a shared egress address is
    // the norm on the data plane, so a per-client lockout would let one
    // misconfigured integration lock its whole NAT out of every *other*
    // channel too. The cost is that an attacker who knows N channel names gets
    // N independent budgets — bounded, and each of those channels has its own
    // unrelated key.
    let budget = backoff
        .filter(|_| auth.failures_are_guesses())
        .map(|tracker| (tracker, format!("{channel}\u{1f}{client}")));
    if let Some((tracker, key)) = &budget
        && let Some(remaining) = tracker.locked_for(key)
    {
        crate::metrics::record_error("channel_auth_locked_out");
        tracing::warn!(
            channel = %channel,
            remaining_ms = remaining.as_millis() as u64,
            "Channel authentication refused: client is in failed-auth backoff"
        );
        // The same refusal as a wrong key: a caller must not learn from the
        // response that it is being rate-limited on credentials.
        return Err(crate::channel::auth::refused());
    }

    let outcome = auth
        .authenticate(header, raw_body, datalogic)
        .await
        .map(|outcome| outcome.claims)
        .inspect_err(|_| {
            metrics::record_message(channel, "unauthorized");
            tracing::warn!(channel = %channel, "Channel authentication failed");
        });

    if let Some((tracker, key)) = &budget {
        match &outcome {
            Ok(_) => tracker.record_success(key),
            Err(_) => {
                if let Some(lockout) = tracker.record_failure(key) {
                    tracing::warn!(
                        channel = %channel,
                        lockout_ms = lockout.as_millis() as u64,
                        "Channel authentication: client entered failed-auth backoff"
                    );
                }
            }
        }
    }
    outcome
}

/// N24: this is a **server-side origin allow-list**, not CORS. It sets no
/// `Access-Control-*` header and takes no part in the preflight handshake —
/// the browser handshake is the platform's `[cors]` section, applied by the
/// router's CORS layer to every route.
///
/// The two do different jobs, and this one is the only enforcement. The
/// platform layer short-circuits **every `OPTIONS`** from a disallowed origin
/// — tower-http 0.7 tests the method alone, not the presence of
/// `Access-Control-Request-Method`, so preflights are also unmetered by the
/// rate limiter, which sits inside CORS — but a
/// non-preflighted cross-origin request is passed through with the
/// `Access-Control-Allow-Origin` header simply omitted — the workflow runs
/// and only the browser discards the answer — and a non-browser client is
/// outside `[cors]` entirely. This check runs on every request that reaches
/// the handler and refuses `403` before dispatch, which is what makes it the
/// server-side control rather than a narrowing of the platform policy.
///
/// It is not authentication: `Origin` is client-supplied, and a request that
/// sends none is not checked at all.
///
/// The key was called `cors.allowed_origins`, which promised a handshake it
/// never performed; it is `origin_allow_list` now, with the old spelling
/// still accepted (see [`crate::channel::ChannelConfig::allowed_origins`]).
pub(super) fn check_allowed_origin(
    channel: &str,
    channel_config: &Option<Arc<ChannelRuntimeConfig>>,
    origin: Option<&str>,
) -> Result<(), OrionError> {
    if let Some(cfg) = channel_config
        && let Some(allowed_origins) = cfg.parsed_config.allowed_origins()
        && let Some(origin) = origin
        && !allowed_origins.iter().any(|o| o == "*" || o == origin)
    {
        return Err(OrionError::Forbidden(format!(
            "Origin '{origin}' is not allowed for channel '{channel}'"
        )));
    }
    Ok(())
}

/// Evaluate per-channel input validation logic (JSONLogic). Returns `Ok(())` when
/// validation passes or no validation is configured.
pub(super) fn validate_input(
    channel: &str,
    channel_config: &Option<Arc<ChannelRuntimeConfig>>,
    data: &Value,
    metadata: &Value,
    datalogic: &datalogic_rs::Engine,
) -> Result<(), OrionError> {
    if let Some(cfg) = channel_config
        && let Some(ref compiled) = cfg.validation_logic
    {
        let context = json!({ "data": data, "metadata": metadata });
        match datalogic
            .session()
            .eval_into::<serde_json::Value, _>(compiled, &context)
        {
            Ok(result) => {
                if !is_truthy(&result) {
                    return Err(OrionError::validation(
                        "Input validation failed".to_string(),
                    ));
                }
            }
            Err(e) => {
                // The detail is logged, not returned: it describes the shape of
                // the channel's own `validation_logic`, and the data plane is
                // anonymous (proposal G4). The failed-predicate arm above is
                // already opaque; these two must agree.
                tracing::warn!(channel = %channel, error = %e, "validation_logic evaluation failed, rejecting");
                return Err(OrionError::validation(
                    "Input validation failed".to_string(),
                ));
            }
        }
    }
    Ok(())
}

/// Acquire a per-channel backpressure permit. Returns `Err(ServiceUnavailable)`
/// when the channel's concurrency limit has been reached. The caller must
/// hold the returned permit for the duration of processing.
pub(super) fn acquire_backpressure(
    channel: &str,
    channel_config: &Option<Arc<ChannelRuntimeConfig>>,
) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, OrionError> {
    if let Some(cfg) = channel_config
        && let Some(ref semaphore) = cfg.backpressure_semaphore
    {
        match semaphore.clone().try_acquire_owned() {
            Ok(permit) => Ok(Some(permit)),
            Err(_) => {
                metrics::record_error("backpressure");
                Err(OrionError::unavailable(
                    crate::errors::Unavailable::AtCapacity,
                    format!("Channel '{channel}' is at capacity"),
                ))
            }
        }
    } else {
        Ok(None)
    }
}
