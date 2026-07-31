//! Transport-neutral per-channel ingress guards.
//!
//! Four ingresses dispatch a message to a channel — synchronous HTTP, HTTP
//! `/async` submission, Kafka ingest, and in-process `channel_call` — and a
//! channel's declared contract must hold whichever one the message arrived
//! on. [`apply_guards`] is the one function that enforces it, and the
//! per-transport [`GuardSet`] returned by [`Transport::guards`] decides which
//! guards run, so the deliberate exclusions are **data** rather than a
//! comment repeated at four call sites.
//!
//! | Guard | HTTP sync | HTTP `/async` | Kafka | `channel_call` |
//! |---|---|---|---|---|
//! | rate limit | ✅ | ✅ | ✅ | ✅ |
//! | origin allow-list | ✅ | ✅ | ❌ | ❌ |
//! | `validation_logic` | ✅ | ✅ | ✅ | ✅ |
//! | deduplication | ✅ | ✅ | ✅ | ❌ |
//! | response cache | ✅ | ❌ | ❌ | ❌ |
//! | backpressure | ✅ | ✅ | ✅ | ✅ |
//! | channel `timeout_ms` | ✅ | ✅ | ✅¹ | ✅ |
//!
//! ¹ clamped to the transport's ceiling — see [`effective_timeout_ms`]. Kafka
//! and the async worker cap the channel value at `kafka.processing_timeout_ms`
//! and `trace_queue.processing_timeout_ms` respectively, because on those
//! paths the deadline protects a shared resource (the consumer's poll loop, a
//! queue worker) rather than one caller's patience. A channel can shorten its
//! deadline everywhere; it can only lengthen it where nothing else depends on
//! it.
//!
//! N16: three of those cells used to be `❌` by omission rather than by
//! decision. Kafka — the highest-volume ingress — applied neither
//! backpressure nor a rate limit nor deduplication, so `max_concurrent_per_node`
//! bounded every path except the one that needed it most and an at-least-once
//! redelivery ran the workflow twice; `channel_call` applied no rate limit;
//! and a channel's `timeout_ms` was honoured on two paths of four, so the
//! same channel timed out at its configured value over HTTP and at the global
//! `trace_queue.processing_timeout_ms` over Kafka and `/async`. Only the four
//! `❌` cells that remain are deliberate, and each is justified on
//! [`Transport::guards`].
//!
//! A ✅ in the rate-limit row means *the same limiter is consulted*, not that
//! the four ingresses share one bucket. The bucket key defaults to whatever
//! caller identity the transport has — client IP over HTTP, topic on Kafka,
//! calling channel for `channel_call` — so a channel's
//! `requests_per_second` is a per-identity rate on each ingress, and only a
//! `rate_limit.key_logic` that returns a transport-independent value makes it
//! one shared throughput cap.
//!
//! Header-derived inputs are lowered to plain strings and a [`HeaderLookup`]
//! closure, so non-HTTP callers never need an `axum::http::HeaderMap`.

use dataflow_rs::datalogic_rs;
use std::sync::Arc;

use serde_json::{Value, json};

use super::ChannelRuntimeConfig;
use crate::connector::cache_backend::CacheBackend;
use crate::errors::OrionError;
use crate::metrics;

/// Which ingress carried a message to a channel. Selects the [`GuardSet`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// `POST /api/v1/data/{channel}` — the synchronous HTTP data plane.
    HttpSync,
    /// `POST /api/v1/data/{channel}/async` — HTTP submission to the trace queue.
    HttpAsync,
    /// Kafka ingest (`kafka.topics`, or a channel's own `topic`).
    Kafka,
    /// In-process `channel_call` from another channel's workflow.
    ChannelCall,
}

/// The guards one transport applies.
///
/// Constructed only by [`Transport::guards`] — a transport's row in the
/// matrix is a single `const` value, which is what makes the omissions
/// reviewable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardSet {
    /// Reject an `Origin` header the channel does not list (N24: a
    /// server-side allow-list, not CORS negotiation).
    pub origin_allow_list: bool,
    /// Per-channel token bucket / shared Redis window.
    pub rate_limit: bool,
    /// The channel's compiled `validation_logic`.
    pub validation: bool,
    /// Idempotency-key deduplication.
    pub deduplication: bool,
    /// Response-cache lookup and store.
    pub response_cache: bool,
    /// Per-channel concurrency permit.
    pub backpressure: bool,
}

impl Transport {
    /// This transport's row of the guard matrix.
    ///
    /// The four `false` cells, each for a reason that is a property of the
    /// transport rather than an oversight:
    ///
    /// * **origin allow-list off Kafka and `channel_call`.** The check reads
    ///   an HTTP `Origin` header. A Kafka record and an in-process call have
    ///   no browsing context and no origin to check; a channel that lists
    ///   origins is constraining its *HTTP* surface.
    /// * **deduplication off `channel_call`.** Deduplication suppresses a
    ///   redelivery of the same *ingress* event. A `channel_call` is a step
    ///   inside a request that was already deduplicated at its own ingress,
    ///   and it carries no key of its own — the child would inherit the
    ///   parent's, so a workflow that fans out to the same channel twice
    ///   (one call per line item) would see its second call rejected as a
    ///   duplicate. Kafka is the opposite case and does dedupe: the
    ///   transport is at-least-once by design, and the record key is a
    ///   natural idempotency key.
    /// * **response cache off `/async`, Kafka and `channel_call`.** An
    ///   `/async` submission answers `202` with a trace id and never has a
    ///   body to serve or to store; a Kafka record has no response at all.
    ///   `channel_call` returns its result into the caller's message, and
    ///   caching that would pin one workflow run's output across callers
    ///   with none of the request identity (method, path, query) the cache
    ///   key is built from.
    pub const fn guards(self) -> GuardSet {
        match self {
            Transport::HttpSync => GuardSet {
                origin_allow_list: true,
                rate_limit: true,
                validation: true,
                deduplication: true,
                response_cache: true,
                backpressure: true,
            },
            Transport::HttpAsync => GuardSet {
                origin_allow_list: true,
                rate_limit: true,
                validation: true,
                deduplication: true,
                response_cache: false,
                backpressure: true,
            },
            Transport::Kafka => GuardSet {
                origin_allow_list: false,
                rate_limit: true,
                validation: true,
                deduplication: true,
                response_cache: false,
                backpressure: true,
            },
            Transport::ChannelCall => GuardSet {
                origin_allow_list: false,
                rate_limit: true,
                validation: true,
                deduplication: false,
                response_cache: false,
                backpressure: true,
            },
        }
    }
}

/// A named lookup over the transport's headers.
///
/// HTTP passes a view over the `HeaderMap`; Kafka a view over the record
/// headers; `channel_call` a view over the inherited `metadata.headers`.
/// Used by deduplication (the idempotency header is per-channel config) and
/// by `rate_limit.key_logic`.
pub type HeaderLookup<'a> = &'a (dyn Fn(&str) -> Option<String> + Send + Sync);

/// Everything [`apply_guards`] needs, lowered to transport-neutral values.
pub struct GuardRequest<'a> {
    /// Which ingress this is — selects the [`GuardSet`].
    pub transport: Transport,
    /// Resolved channel name (registry-confirmed, so it is safe as a label).
    pub channel: &'a str,
    /// The channel's runtime config. `None` means the channel is not in the
    /// registry, and every guard is a no-op — callers reject that case
    /// before getting here.
    pub runtime: &'a Option<Arc<ChannelRuntimeConfig>>,
    /// The payload, as `validation_logic` and the cache key see it.
    pub data: &'a Value,
    /// The message metadata, as `validation_logic` sees it.
    pub metadata: &'a Value,
    /// Compiled-logic evaluator.
    pub datalogic: &'a datalogic_rs::Engine,
    /// `Origin` header value, when the transport carries one.
    pub origin: Option<&'a str>,
    /// Default rate-limit bucket key, used when the channel declares no
    /// `key_logic`: the client IP over HTTP, the topic for Kafka, the
    /// calling channel for `channel_call`.
    pub caller_identity: &'a str,
    /// Named header lookup (see [`HeaderLookup`]).
    pub header: HeaderLookup<'a>,
    /// Idempotency key the transport carries out of band, used when the
    /// configured dedup header is absent. Kafka passes the record key.
    pub dedup_key_fallback: Option<&'a str>,
    /// Stable identity of *this physical delivery*, for transports that can
    /// redeliver the same message. Kafka passes `topic/partition/offset`;
    /// every other transport passes `None` and gets a one-shot token minted
    /// per call. See `check_deduplication` for why the distinction matters.
    pub dedup_owner: Option<&'a str>,
    /// Timeout applied when the channel declares no `timeout_ms` of its own.
    ///
    /// Per call site: `None` for synchronous HTTP (which has no deadline
    /// beyond the channel's) and for the `/async` submission (the worker
    /// re-resolves at dequeue time), `kafka.processing_timeout_ms` for Kafka,
    /// and `engine.default_channel_call_timeout_ms` for `channel_call`. The
    /// async worker calls [`effective_timeout_ms`] directly with
    /// `trace_queue.processing_timeout_ms`.
    pub default_timeout_ms: Option<u64>,
    /// Hard ceiling on the resolved deadline: the channel's `timeout_ms` may
    /// shorten a message's deadline below this, never lengthen it past.
    ///
    /// `Some(…)` only where the transport's deadline is a *safety* property
    /// rather than a default. Kafka passes `kafka.processing_timeout_ms`
    /// because the consumer blocks its poll loop for the whole dispatch and
    /// must return before librdkafka's `max.poll.interval.ms` evicts it from
    /// the group; the async worker passes `trace_queue.processing_timeout_ms`
    /// because that setting is the operator's cap on how long one queue
    /// worker may be occupied. Synchronous HTTP and `channel_call` pass
    /// `None` — neither has a ceiling to protect, and a `channel_call` task's
    /// own `timeout_ms` outranks everything anyway.
    pub max_timeout_ms: Option<u64>,
}

/// What the caller must carry for the rest of the request once every guard
/// has passed.
pub struct Admission {
    /// Backpressure permit; hold it for the duration of processing.
    pub backpressure_permit: Option<tokio::sync::OwnedSemaphorePermit>,
    /// Where to store the response on success, when the transport caches.
    pub cache_store: Option<CacheStoreCtx>,
    /// The deadline for this message: the channel's `timeout_ms` when it
    /// declares one, else the transport's default — clamped to the
    /// transport's ceiling in either case.
    pub timeout_ms: Option<u64>,
    /// The idempotency key this delivery holds, when the channel deduplicates
    /// and a key was resolved.
    ///
    /// A transport that can redeliver **must** settle it: [`DedupClaim::confirm`]
    /// once the message is durably accounted for, [`DedupClaim::release`] when
    /// it is not. A transport that cannot redeliver (one HTTP request, one
    /// delivery) may simply drop it — the claim then stands for the rest of
    /// the window, which is exactly the `409` a replay of that key should get.
    pub dedup_claim: Option<DedupClaim>,
}

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
const DEDUP_SETTLED: &str = "settled";

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

/// Outcome of [`apply_guards`].
pub enum GuardVerdict {
    /// Every guard the transport applies passed.
    Admitted(Admission),
    /// The response cache answered — carries the pre-serialized body. Only
    /// [`Transport::HttpSync`] can produce this.
    CacheHit(String),
}

/// Apply the target channel's ingress guards for one transport.
///
/// The single enforcement point for N16's matrix: which guards run is read
/// from [`Transport::guards`], so adding an ingress is adding a row, not
/// remembering four call sites. Only a transport that caches calls this
/// directly; the other three go through [`admit`], which is this function
/// with the unreachable [`GuardVerdict::CacheHit`] already resolved.
///
/// Order is deliberate. The rate limit is first so a refusal costs the least
/// work and so rejected requests (a disallowed origin, a failing predicate)
/// still consume a token — otherwise an attacker gets unmetered rejections.
/// Deduplication precedes the response-cache lookup so a replayed
/// idempotency key is answered `409` rather than served a cached body. The
/// backpressure permit is acquired last, after the cache lookup, so a cache
/// hit does not consume a concurrency slot it never needed — and because the
/// permit is the only guard that can refuse *after* the idempotency key has
/// been claimed, its refusal releases the claim before returning, so a shed
/// request does not burn the key it never got to use.
pub async fn apply_guards(req: GuardRequest<'_>) -> Result<GuardVerdict, OrionError> {
    let set = req.transport.guards();

    if set.rate_limit {
        check_rate_limit(
            req.channel,
            req.runtime,
            req.datalogic,
            req.caller_identity,
            req.header,
        )
        .await?;
    }
    if set.origin_allow_list {
        check_allowed_origin(req.channel, req.runtime, req.origin)?;
    }
    if set.validation {
        validate_input(
            req.channel,
            req.runtime,
            req.data,
            req.metadata,
            req.datalogic,
        )?;
    }
    let dedup_claim = if set.deduplication {
        check_deduplication(
            req.channel,
            req.runtime,
            req.header,
            req.dedup_key_fallback,
            req.dedup_owner,
        )
        .await?
    } else {
        None
    };
    let cache_store = if set.response_cache {
        match check_response_cache(req.channel, req.data, req.metadata, req.runtime).await {
            // The request was answered, so the claim stands: a later replay
            // of the same key is a duplicate of a delivery that succeeded.
            CacheLookup::Hit(body) => return Ok(GuardVerdict::CacheHit(body)),
            CacheLookup::Miss(ctx) => ctx,
        }
    } else {
        None
    };
    let backpressure_permit = if set.backpressure {
        match acquire_backpressure(req.channel, req.runtime) {
            Ok(permit) => permit,
            Err(e) => {
                // Nothing ran. Hand the key back so the caller's retry — or,
                // on Kafka, the redelivery of an offset that was never
                // committed — is judged on its own merits rather than as a
                // duplicate of an attempt that was shed.
                if let Some(claim) = dedup_claim {
                    claim.release().await;
                }
                return Err(e);
            }
        }
    } else {
        None
    };

    Ok(GuardVerdict::Admitted(Admission {
        backpressure_permit,
        cache_store,
        timeout_ms: effective_timeout_ms(req.runtime, req.default_timeout_ms, req.max_timeout_ms),
        dedup_claim,
    }))
}

/// [`apply_guards`] for a transport whose row leaves `response_cache` off —
/// every ingress but [`Transport::HttpSync`].
///
/// [`GuardVerdict`] spans the whole matrix, so its `CacheHit` variant is
/// statically unreachable for those transports. Resolving that impossibility
/// here is the same principle as the matrix itself: the exclusion is read
/// from [`Transport::guards`] once instead of each call site hand-writing its
/// own handling of a branch it can never take. A transport that does cache
/// calls [`apply_guards`] and matches both variants.
pub async fn admit(req: GuardRequest<'_>) -> Result<Admission, OrionError> {
    let transport = req.transport;
    match apply_guards(req).await? {
        GuardVerdict::Admitted(admission) => Ok(admission),
        GuardVerdict::CacheHit(_) => Err(OrionError::Internal(format!(
            "{transport:?} does not enable the response cache"
        ))),
    }
}

/// The deadline for a message on this channel: the channel's declared
/// `timeout_ms` when it has one, else the transport's default — clamped to
/// `max_timeout_ms` where the transport has a ceiling.
///
/// N16: a channel's `timeout_ms` used to be read only by the synchronous
/// HTTP path and `channel_call`, so the same channel timed out at its
/// configured value over HTTP and at the global
/// `trace_queue.processing_timeout_ms` over `/async` and Kafka. Both the
/// guard chain and the async worker (which re-resolves at dequeue time, when
/// the config may have moved on) call this.
///
/// The clamp is what keeps that unification from turning an operator's
/// transport ceiling into a mere default. Channel `timeout_ms` is
/// unvalidated and unbounded upward; on Kafka the dispatch blocks the poll
/// loop, so a channel asking for ten minutes would get the consumer evicted
/// from its group mid-message, and on the async path it would occupy a queue
/// worker past `trace_queue.processing_timeout_ms`. A channel can therefore
/// shorten its deadline below the transport's, never lengthen it past.
pub fn effective_timeout_ms(
    runtime: &Option<Arc<ChannelRuntimeConfig>>,
    default_timeout_ms: Option<u64>,
    max_timeout_ms: Option<u64>,
) -> Option<u64> {
    let resolved = runtime
        .as_ref()
        .and_then(|c| c.parsed_config.timeout_ms)
        .or(default_timeout_ms)?;
    Some(match max_timeout_ms {
        Some(max) => resolved.min(max),
        None => resolved,
    })
}

/// JSONLogic truthiness: false, null, 0, "", and [] are falsy; everything else is truthy.
fn is_truthy(val: &Value) -> bool {
    match val {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(_) => true,
    }
}

/// Reject the request when an `Origin` header is present and the channel's
/// allow-list does not name it.
///
/// N24: this is a **server-side origin allow-list**, not CORS. It sets no
/// `Access-Control-*` header and takes no part in the preflight handshake —
/// the browser handshake is the platform's `[cors]` section, applied by the
/// router's CORS layer to every route.
///
/// The two do different jobs, and this one is the only enforcement. The
/// platform layer short-circuits a *genuine preflight* (`OPTIONS` carrying
/// `Access-Control-Request-Method`) from a disallowed origin, but a
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
fn check_allowed_origin(
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

/// Check the channel's rate limit.
///
/// S15: this used to live in `server::rate_limit`, which pulls `AppState`
/// and resolves the target channel from the URI — so the limiter a channel
/// declared applied to HTTP ingress and to nothing else. Kafka and
/// `channel_call` reached the workflow with the limit unenforced. Lifting it
/// here is what lets [`apply_guards`] run it on every transport.
///
/// `Err(RateLimited)` = over limit; `Err(ServiceUnavailable)` = the backend
/// could not answer and the channel is configured to fail closed (N7 —
/// deliberately not a `429`: the caller is not over any limit, the control is
/// unavailable).
async fn check_rate_limit(
    channel: &str,
    channel_config: &Option<Arc<ChannelRuntimeConfig>>,
    datalogic: &datalogic_rs::Engine,
    caller_identity: &str,
    header: HeaderLookup<'_>,
) -> Result<(), OrionError> {
    let Some(cfg) = channel_config else {
        return Ok(());
    };
    let Some(ref limiter) = cfg.rate_limiter else {
        return Ok(());
    };

    // Compute the bucket key from `key_logic`, defaulting to the transport's
    // caller identity.
    let key = if let Some(ref compiled) = cfg.rate_limit_key_logic {
        let context = rate_limit_context(caller_identity, channel, header);
        match datalogic
            .session()
            .eval_into::<serde_json::Value, _>(compiled, &context)
        {
            Ok(val) => val
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| serde_json::to_string(&val).unwrap_or_default()),
            // N5: falling back to the caller identity here would silently
            // re-dimension the limit mid-flight. The key is part of the
            // control, so a request whose key cannot be computed is
            // rejected rather than counted in the wrong bucket.
            //
            // Its own variant, not `RateLimited`: over-limit is transient and
            // worth retrying, while a key expression that cannot be evaluated
            // against this message will fail against every copy of it. Both
            // answer `429` over HTTP; only this one is terminal on Kafka, so
            // the record is dead-lettered instead of head-of-line blocking
            // its partition for as long as the channel keeps the expression.
            Err(e) => {
                tracing::warn!(
                    channel = %channel,
                    error = %e,
                    "rate_limit.key_logic evaluation failed; rejecting request"
                );
                metrics::record_rate_limit_rejected(channel);
                return Err(OrionError::RateLimitKeyUnavailable(
                    "Too many requests".to_string(),
                ));
            }
        }
    } else {
        caller_identity.to_string()
    };

    let policy = cfg
        .parsed_config
        .rate_limit
        .as_ref()
        .map(|rl| rl.on_backend_error)
        .unwrap_or_default();
    match limiter.check(key).await {
        Ok(true) => Ok(()),
        Ok(false) => {
            metrics::record_rate_limit_rejected(channel);
            Err(OrionError::RateLimited("Too many requests".to_string()))
        }
        Err(e) => {
            metrics::record_error("rate_limit_backend");
            match policy {
                crate::channel::BackendErrorPolicy::Allow => {
                    tracing::warn!(
                        channel = %channel,
                        error = %e,
                        "Rate-limit backend error; failing open (request allowed)"
                    );
                    Ok(())
                }
                crate::channel::BackendErrorPolicy::Deny => {
                    tracing::warn!(
                        channel = %channel,
                        error = %e,
                        "Rate-limit backend error; failing closed (request refused)"
                    );
                    metrics::record_rate_limit_rejected(channel);
                    Err(OrionError::ServiceUnavailable(format!(
                        "Channel '{channel}' cannot check its rate limit: the backend is \
                         unavailable and the channel is configured to fail closed"
                    )))
                }
            }
        }
    }
}

/// Build the context object `rate_limit.key_logic` is evaluated against.
///
/// `client_ip` is the transport's caller identity — the HTTP client IP, the
/// Kafka topic, or the calling channel — and keeps its historical name so
/// stored `key_logic` expressions keep resolving. Headers are limited to a
/// fixed list of the ones key expressions actually use, so a request does not
/// pay an allocation per header for a key that references two of them.
fn rate_limit_context(caller_identity: &str, channel: &str, header: HeaderLookup<'_>) -> Value {
    const COMMON_HEADERS: &[&str] = &[
        "authorization",
        "x-api-key",
        "x-forwarded-for",
        "x-real-ip",
        "user-agent",
        "content-type",
        "origin",
        "x-tenant-id",
    ];
    let mut headers = serde_json::Map::with_capacity(COMMON_HEADERS.len());
    for &name in COMMON_HEADERS {
        if let Some(value) = header(name) {
            headers.insert(name.to_string(), Value::String(value));
        }
    }
    json!({
        "client_ip": caller_identity,
        "channel": channel,
        "headers": headers,
    })
}

/// Evaluate per-channel input validation logic (JSONLogic). Returns `Ok(())` when
/// validation passes or no validation is configured.
fn validate_input(
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
                    return Err(OrionError::BadRequest(
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
                return Err(OrionError::BadRequest(
                    "Input validation failed".to_string(),
                ));
            }
        }
    }
    Ok(())
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
async fn check_deduplication(
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
                    return Err(OrionError::ServiceUnavailable(format!(
                        "Channel '{channel}' cannot verify the idempotency key: the \
                         deduplication backend is unavailable and the channel is \
                         configured to fail closed"
                    )));
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

/// Acquire a per-channel backpressure permit. Returns `Err(ServiceUnavailable)`
/// when the channel's concurrency limit has been reached. The caller must
/// hold the returned permit for the duration of processing.
fn acquire_backpressure(
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
                Err(OrionError::ServiceUnavailable(format!(
                    "Channel '{channel}' is at capacity"
                )))
            }
        }
    } else {
        Ok(None)
    }
}

use crate::engine::utils::{FNV1A_SEED, fnv1a_feed};

/// Resolve one `cache_key_fields` entry against the request payload.
///
/// Three spellings resolve, tried in this order:
///
/// 1. The **literal key** — `data.get(f)`. This was the only spelling the
///    original implementation supported, so trying it first keeps every stored
///    channel keying exactly as it did, including a payload whose top-level key
///    genuinely contains a dot.
/// 2. A **dotted path** from the payload root — `user.id` walks
///    `{"user": {"id": …}}`.
/// 3. The same path with a leading `data.` stripped — the spelling the docs
///    have always shown (`data.user_id`), which resolved to nothing under (1)
///    because the payload *is* `data` and has no member of that name.
///
/// (3) is what made this worth fixing: a channel configured from the
/// documented example matched no field at all, and a field that matches
/// nothing contributed nothing to the hash, so every request on the channel
/// collapsed onto one cache entry and the first caller's body was served to
/// everyone for the TTL. [`compute_cache_key`] now refuses to build a key at
/// all in that case rather than building a meaningless one.
fn resolve_key_field<'a>(data: &'a Value, field: &str) -> Option<&'a Value> {
    fn walk<'a>(mut cur: &'a Value, path: &str) -> Option<&'a Value> {
        for segment in path.split('.') {
            if segment.is_empty() {
                return None;
            }
            cur = cur.get(segment)?;
        }
        Some(cur)
    }

    if let Some(v) = data.get(field) {
        return Some(v);
    }
    if !field.contains('.') {
        return None;
    }
    walk(data, field).or_else(|| field.strip_prefix("data.").and_then(|p| walk(data, p)))
}

/// Compute a deterministic cache key from channel name and request data.
///
/// `None` means **this request has no meaningful cache key** and must neither
/// be served from the cache nor stored in it. That happens when the channel
/// declares `cache_key_fields` and not one of them resolves against the
/// payload: the request is then indistinguishable from every other request on
/// the channel, which is precisely when a cache entry is dangerous rather than
/// merely useless. Bypassing the cache costs one workflow run; keying on
/// nothing costs correctness.
///
/// Uses FNV-1a (64-bit) rather than `std::collections::hash_map::DefaultHasher`
/// because the cache key must be **stable across processes** (multiple
/// orion-server instances sharing a Redis cache must agree on the key for the
/// same request). `DefaultHasher` is `SipHash` keyed by a per-process random
/// seed and would produce different keys per process. `ahash` likewise
/// randomises its seed on construction. FNV-1a is unkeyed and deterministic.
fn compute_cache_key(
    channel: &str,
    data: &Value,
    metadata: &Value,
    cache_cfg: &crate::channel::ChannelCacheConfig,
) -> Option<String> {
    let mut h: u64 = FNV1A_SEED;

    // The request's route identity must always distinguish keys: for a REST
    // channel like `GET /orders/{id}` the body is empty, so hashing only the
    // body would serve the first caller's response to every id.
    if let Some(method) = metadata.get("http_method").and_then(Value::as_str) {
        fnv1a_feed(&mut h, method.as_bytes());
    }
    fnv1a_feed(&mut h, &[0]);
    fnv1a_feed_object_sorted(&mut h, metadata.get("params"));
    fnv1a_feed(&mut h, &[0]);
    fnv1a_feed_object_sorted(&mut h, metadata.get("query"));
    fnv1a_feed(&mut h, &[0]);

    if let Some(ref fields) = cache_cfg.cache_key_fields {
        // Hash selected fields directly — no intermediate Map or clones. An
        // absent field feeds its *name* and a marker byte rather than nothing,
        // so `{"a": 1}` and `{"b": 1}` under fields `["a", "b"]` cannot land on
        // the same key by each contributing one term and skipping the other.
        let mut resolved = 0usize;
        for f in fields {
            fnv1a_feed(&mut h, f.as_bytes());
            match resolve_key_field(data, f) {
                Some(v) => {
                    resolved += 1;
                    fnv1a_feed(&mut h, &[1]);
                    let v_bytes = serde_json::to_vec(v).unwrap_or_default();
                    fnv1a_feed(&mut h, &v_bytes);
                }
                None => fnv1a_feed(&mut h, &[0]),
            }
        }
        if resolved == 0 {
            return None;
        }
    } else {
        let bytes = serde_json::to_vec(data).unwrap_or_default();
        fnv1a_feed(&mut h, &bytes);
    };

    Some(format!("cache:{channel}:{h:016x}"))
}

/// Feed an optional JSON object into the FNV-1a state in sorted-key order,
/// so the key is independent of map iteration and query-string order.
fn fnv1a_feed_object_sorted(h: &mut u64, v: Option<&Value>) {
    if let Some(Value::Object(map)) = v {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort_unstable();
        for k in keys {
            fnv1a_feed(h, k.as_bytes());
            let bytes = serde_json::to_vec(&map[k.as_str()]).unwrap_or_default();
            fnv1a_feed(h, &bytes);
        }
    }
}

/// Context carried from cache pre-check to post-success cache store:
/// (cache key, backend, TTL seconds).
pub type CacheStoreCtx = (String, Arc<dyn CacheBackend>, u64);

/// Outcome of the response-cache pre-check.
enum CacheLookup {
    /// Cache hit — carries the cached pre-serialized JSON body.
    Hit(String),
    /// No cache hit. Carries the (key, backend, ttl) needed to store the
    /// computed response on success, or `None` if caching is disabled.
    Miss(Option<CacheStoreCtx>),
}

/// Check the response cache; return a hit or the context needed to store
/// the eventual response on success.
async fn check_response_cache(
    channel: &str,
    data: &Value,
    metadata: &Value,
    channel_config: &Option<Arc<ChannelRuntimeConfig>>,
) -> CacheLookup {
    let Some(cfg) = channel_config else {
        return CacheLookup::Miss(None);
    };
    let Some(ref cache_cfg) = cfg.parsed_config.cache else {
        return CacheLookup::Miss(None);
    };
    if !cache_cfg.enabled {
        return CacheLookup::Miss(None);
    }
    let Some(ref cache) = cfg.response_cache else {
        return CacheLookup::Miss(None);
    };
    let Some(key) = compute_cache_key(channel, data, metadata, cache_cfg) else {
        // Every declared key field was absent from this payload, so any key we
        // built would be shared with every other request on the channel. Run
        // the workflow and store nothing.
        tracing::warn!(
            channel = %channel,
            fields = ?cache_cfg.cache_key_fields,
            "No cache_key_fields resolved against the request payload; bypassing the \
             response cache. Field names are literal payload keys or dotted paths \
             (`user.id`, or `data.user_id` for a top-level `user_id`)."
        );
        return CacheLookup::Miss(None);
    };
    match cache.get(&key).await {
        Ok(Some(cached)) => {
            metrics::record_cache_hit(channel);
            CacheLookup::Hit(cached)
        }
        _ => {
            metrics::record_cache_miss(channel);
            CacheLookup::Miss(Some((
                key,
                cache.clone(),
                cache_cfg.ttl_secs.unwrap_or(300),
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use dataflow_rs::datalogic_rs;
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
                StubOutcome::Duplicate => Ok(Some(super::DEDUP_SETTLED.to_string())),
                StubOutcome::BackendError => {
                    Err(OrionError::Internal("dedup backend down".to_string()))
                }
            }
        }
    }

    /// A real, per-test dedup store: claims are held in a map, so a claim, a
    /// release and a re-claim behave as they do against Redis.
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

    /// Rate-limit backend whose store never answers — a Redis blip, minus
    /// the Redis.
    struct FailingLimiter;

    #[async_trait]
    impl crate::channel::RateLimitBackend for FailingLimiter {
        async fn check(&self, _key: String) -> Result<bool, OrionError> {
            Err(OrionError::Internal("backend down".to_string()))
        }
    }

    /// Builder for a `ChannelRuntimeConfig`, so each test states only the
    /// guard it exercises.
    struct Runtime {
        parsed_config: ChannelConfig,
        rate_limiter: Option<Arc<dyn crate::channel::RateLimitBackend>>,
        rate_limit_key_logic: Option<datalogic_rs::Logic>,
        validation_logic: Option<datalogic_rs::Logic>,
        backpressure_semaphore: Option<Arc<tokio::sync::Semaphore>>,
        dedup_store: Option<Arc<dyn CacheBackend>>,
        response_cache: Option<Arc<dyn CacheBackend>>,
    }

    impl Runtime {
        fn new() -> Self {
            Self {
                parsed_config: ChannelConfig::default(),
                rate_limiter: None,
                rate_limit_key_logic: None,
                validation_logic: None,
                backpressure_semaphore: None,
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
                on_backend_error: policy,
            });
            self.rate_limiter = Some(backend);
            self
        }

        fn key_logic(mut self, engine: &datalogic_rs::Engine, logic: serde_json::Value) -> Self {
            self.rate_limit_key_logic = Some(engine.compile(&logic).expect("test logic compiles"));
            self
        }

        fn validation(mut self, engine: &datalogic_rs::Engine, logic: serde_json::Value) -> Self {
            self.validation_logic = Some(engine.compile(&logic).expect("test logic compiles"));
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
                connector: None,
            });
            self.response_cache = Some(backend);
            self
        }

        fn timeout_ms(mut self, ms: u64) -> Self {
            self.parsed_config.timeout_ms = Some(ms);
            self
        }

        fn build(self) -> Option<Arc<ChannelRuntimeConfig>> {
            let now = chrono::Utc::now().naive_utc();
            Some(Arc::new(ChannelRuntimeConfig {
                channel: Channel {
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
                parsed_config: self.parsed_config,
                rate_limiter: self.rate_limiter,
                rate_limit_key_logic: self.rate_limit_key_logic,
                validation_logic: self.validation_logic,
                backpressure_semaphore: self.backpressure_semaphore,
                dedup_store: self.dedup_store,
                response_cache: self.response_cache,
                trace_storage: EffectiveTraceConfig::resolve(&TraceStorageConfig::default(), None),
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
    fn idempotency_lookup(name: &str) -> Option<String> {
        (name == "idempotency-key").then(|| "token-1".to_string())
    }

    fn no_headers(_name: &str) -> Option<String> {
        None
    }

    const IDEMPOTENCY: HeaderLookup<'static> = &idempotency_lookup;
    const NO_HEADERS: HeaderLookup<'static> = &no_headers;

    fn engine() -> datalogic_rs::Engine {
        datalogic_rs::Engine::new()
    }

    /// One guard request with everything defaulted, so a test names only
    /// what it is about.
    fn request<'a>(
        transport: Transport,
        runtime: &'a Option<Arc<ChannelRuntimeConfig>>,
        datalogic: &'a datalogic_rs::Engine,
        data: &'a Value,
        metadata: &'a Value,
    ) -> GuardRequest<'a> {
        GuardRequest {
            transport,
            channel: "orders",
            runtime,
            data,
            metadata,
            datalogic,
            origin: None,
            caller_identity: "10.0.0.1",
            header: NO_HEADERS,
            dedup_key_fallback: None,
            dedup_owner: None,
            default_timeout_ms: None,
            max_timeout_ms: None,
        }
    }

    /// `Some` when the guards admitted the message; `None` when the response
    /// cache answered instead. Call sites `.expect(...)` the arm they mean.
    fn admitted(verdict: GuardVerdict) -> Option<Admission> {
        match verdict {
            GuardVerdict::Admitted(a) => Some(a),
            GuardVerdict::CacheHit(_) => None,
        }
    }

    // ---- The matrix itself (N16) ----

    /// The matrix is the specification: these are the cells the module doc
    /// documents and `apply_guards` reads. A change here is a change to the
    /// contract, not an implementation detail.
    #[test]
    fn the_guard_matrix_is_what_the_docs_claim() {
        let sync = Transport::HttpSync.guards();
        let submit = Transport::HttpAsync.guards();
        let kafka = Transport::Kafka.guards();
        let call = Transport::ChannelCall.guards();

        // Universal guards: every transport, no exceptions. Kafka lacked the
        // rate limit, dedup and backpressure; channel_call lacked the rate
        // limit (N16).
        for set in [sync, submit, kafka, call] {
            assert!(set.rate_limit);
            assert!(set.validation);
            assert!(set.backpressure);
        }

        // Deliberate exclusions.
        assert!(sync.origin_allow_list && submit.origin_allow_list);
        assert!(!kafka.origin_allow_list && !call.origin_allow_list);
        assert!(sync.deduplication && submit.deduplication && kafka.deduplication);
        assert!(!call.deduplication);
        assert!(sync.response_cache);
        assert!(!submit.response_cache && !kafka.response_cache && !call.response_cache);
    }

    /// The channel's `timeout_ms` wins on every transport; the transport's
    /// default only fills in when the channel declares none (N16 — Kafka and
    /// `/async` used to ignore the channel value entirely).
    #[test]
    fn the_channel_timeout_outranks_the_transport_default() {
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
    /// it. On Kafka the dispatch blocks the poll loop and an over-long one
    /// gets the consumer evicted from its group mid-message; on the async
    /// path it holds one of a fixed number of queue workers past the
    /// operator's ceiling. `timeout_ms` is unvalidated and unbounded upward,
    /// so the clamp is the only thing standing between a channel config and
    /// those two failures.
    #[test]
    fn a_transport_ceiling_clamps_an_over_long_channel_timeout() {
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
            Transport::Kafka,
            Transport::ChannelCall,
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

    /// The per-channel limit used to be enforced by the HTTP middleware and
    /// therefore by nothing else. Every transport now consults the same
    /// limiter.
    #[tokio::test]
    async fn the_rate_limit_applies_on_every_transport() {
        let dl = engine();
        let (data, meta) = (json!({}), json!({}));
        for transport in [
            Transport::HttpSync,
            Transport::HttpAsync,
            Transport::Kafka,
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
    /// key is the transport's caller identity — client IP, topic, calling
    /// channel — so a `requests_per_second` is a per-identity rate on each
    /// ingress and not a channel-wide throughput cap. Stating that here keeps
    /// the test above from being read as the stronger claim it does not make
    /// (it passes the same identity for all four transports).
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

        // ...while the Kafka topic's bucket is untouched, because the
        // identity — and therefore the key — is different.
        let mut req = request(Transport::Kafka, &runtime, &dl, &data, &meta);
        req.caller_identity = "orders-topic";
        assert!(
            apply_guards(req).await.is_ok(),
            "a Kafka record meters under its topic, not the HTTP client's bucket"
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
        let mut req = request(Transport::Kafka, &shared, &dl, &data, &meta);
        req.caller_identity = "orders-topic";
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
    /// copy of it. Over HTTP both answer `429`; on Kafka only this one is
    /// terminal, so a record whose key cannot be computed is dead-lettered
    /// instead of retried forever at the head of its partition.
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
        let verdict = apply_guards(request(Transport::Kafka, &allow, &dl, &data, &meta)).await;
        assert!(verdict.is_ok(), "allow must fail open");

        let deny = Runtime::new()
            .limiter(Arc::new(FailingLimiter), BackendErrorPolicy::Deny)
            .build();
        let verdict = apply_guards(request(Transport::Kafka, &deny, &dl, &data, &meta)).await;
        assert!(
            matches!(verdict, Err(OrionError::ServiceUnavailable(_))),
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

        let acme = |name: &str| (name == "x-tenant-id").then(|| "acme".to_string());
        let globex = |name: &str| (name == "x-tenant-id").then(|| "globex".to_string());

        let mut req = request(Transport::Kafka, &runtime, &dl, &data, &meta);
        req.header = &acme;
        assert!(apply_guards(req).await.is_ok());

        // A different tenant has its own bucket, so it is not refused...
        let mut req = request(Transport::Kafka, &runtime, &dl, &data, &meta);
        req.header = &globex;
        assert!(apply_guards(req).await.is_ok());

        // ...while the first tenant's bucket is now empty.
        let mut req = request(Transport::Kafka, &runtime, &dl, &data, &meta);
        req.header = &acme;
        assert!(matches!(
            apply_guards(req).await,
            Err(OrionError::RateLimited(_))
        ));
    }

    // ---- Backpressure, on every transport ----

    /// The permit is per channel, not per transport: Kafka work counts
    /// against the same `max_concurrent_per_node` as HTTP work (N16 — Kafka
    /// took no permit at all, so the bound applied to everything except the
    /// highest-volume path).
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

        // ...so a Kafka record and a channel_call are both shed.
        for transport in [Transport::Kafka, Transport::ChannelCall] {
            let verdict = apply_guards(request(transport, &runtime, &dl, &data, &meta)).await;
            assert!(
                matches!(verdict, Err(OrionError::ServiceUnavailable(_))),
                "{transport:?} must be shed while the permit is held"
            );
        }

        // Releasing it re-admits them.
        drop(held);
        assert!(
            apply_guards(request(Transport::Kafka, &runtime, &dl, &data, &meta))
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
                matches!(apply_guards(req).await, Err(OrionError::Forbidden(_))),
                "{transport:?} must refuse an unlisted origin"
            );
        }
        // A Kafka record has no browsing context; an origin the caller
        // asserts there means nothing and is not checked.
        for transport in [Transport::Kafka, Transport::ChannelCall] {
            let mut req = request(transport, &runtime, &dl, &data, &meta);
            req.origin = Some("https://evil.example");
            assert!(
                apply_guards(req).await.is_ok(),
                "{transport:?} does not check origins"
            );
        }
    }

    /// N24: the deprecated `{"cors": {"allowed_origins": [...]}}` spelling
    /// keeps working — stored channel configs must not silently lose the
    /// check when the key is renamed.
    #[tokio::test]
    async fn the_deprecated_cors_spelling_still_enforces() {
        let dl = engine();
        let mut runtime = Runtime::new();
        runtime.parsed_config.cors = Some(crate::channel::ChannelCorsConfig {
            allowed_origins: Some(vec!["https://allowed.example".to_string()]),
        });
        let runtime = runtime.build();
        let (data, meta) = (json!({}), json!({}));

        let mut req = request(Transport::HttpSync, &runtime, &dl, &data, &meta);
        req.origin = Some("https://evil.example");
        assert!(matches!(
            apply_guards(req).await,
            Err(OrionError::Forbidden(_))
        ));

        let mut req = request(Transport::HttpSync, &runtime, &dl, &data, &meta);
        req.origin = Some("https://allowed.example");
        assert!(apply_guards(req).await.is_ok());
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
            Transport::Kafka,
            Transport::ChannelCall,
        ] {
            assert!(
                matches!(
                    apply_guards(request(transport, &runtime, &dl, &bad, &meta)).await,
                    Err(OrionError::BadRequest(_))
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
            super::check_deduplication("test-channel", &cfg, IDEMPOTENCY, None, None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dedup_duplicate_rejected() {
        let cfg = dedup_runtime(StubOutcome::Duplicate);
        let result =
            super::check_deduplication("test-channel", &cfg, IDEMPOTENCY, None, None).await;
        assert!(matches!(result, Err(OrionError::Conflict(_))));
    }

    #[tokio::test]
    async fn test_dedup_fails_open_on_backend_error_by_default() {
        let cfg = dedup_runtime(StubOutcome::BackendError);
        let result =
            super::check_deduplication("test-channel", &cfg, IDEMPOTENCY, None, None).await;
        assert!(result.is_ok(), "backend errors must fail open, not 409");
    }

    /// N7: `on_backend_error = "deny"` fails closed — a 503, never a 409,
    /// because the request is unverifiable rather than a known duplicate.
    #[tokio::test]
    async fn test_dedup_fails_closed_when_policy_is_deny() {
        let cfg = dedup_runtime_with_policy(StubOutcome::BackendError, BackendErrorPolicy::Deny);
        let result =
            super::check_deduplication("test-channel", &cfg, IDEMPOTENCY, None, None).await;
        assert!(
            matches!(result, Err(OrionError::ServiceUnavailable(_))),
            "deny must refuse with 503"
        );
    }

    /// The deny policy only fires on backend errors — a healthy backend
    /// answers normally under either policy.
    #[tokio::test]
    async fn test_deny_policy_does_not_affect_healthy_backend() {
        let cfg = dedup_runtime_with_policy(StubOutcome::New, BackendErrorPolicy::Deny);
        let result =
            super::check_deduplication("test-channel", &cfg, IDEMPOTENCY, None, None).await;
        assert!(result.is_ok());
        let cfg = dedup_runtime_with_policy(StubOutcome::Duplicate, BackendErrorPolicy::Deny);
        let result =
            super::check_deduplication("test-channel", &cfg, IDEMPOTENCY, None, None).await;
        assert!(matches!(result, Err(OrionError::Conflict(_))));
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
        super::check_deduplication("orders", &cfg, IDEMPOTENCY, None, None)
            .await
            .expect("dedup check should pass");
        let keys = seen.lock().expect("test lock poisoned");
        assert_eq!(keys.as_slice(), ["dedup:orders:token-1"]);
    }

    /// N16: on Kafka the record key is the idempotency key when the record
    /// carries no header of the configured name — at-least-once redelivery
    /// of the same key must not run the workflow twice.
    #[tokio::test]
    async fn the_kafka_record_key_is_the_fallback_idempotency_key() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cfg = Runtime::new()
            .dedup(
                Arc::new(CapturingDedupBackend { seen: seen.clone() }),
                BackendErrorPolicy::Allow,
            )
            .build();
        super::check_deduplication("orders", &cfg, NO_HEADERS, Some("ORD-77"), None)
            .await
            .expect("dedup check should pass");
        assert_eq!(
            seen.lock().expect("test lock poisoned").as_slice(),
            ["dedup:orders:ORD-77"]
        );

        // An explicit header still wins over the record key.
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cfg = Runtime::new()
            .dedup(
                Arc::new(CapturingDedupBackend { seen: seen.clone() }),
                BackendErrorPolicy::Allow,
            )
            .build();
        super::check_deduplication("orders", &cfg, IDEMPOTENCY, Some("ORD-77"), None)
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
        req.header = IDEMPOTENCY;
        assert!(
            apply_guards(req).await.is_ok(),
            "channel_call must not consult the dedup store"
        );

        // The same store refuses the same key over HTTP and Kafka.
        for transport in [Transport::HttpSync, Transport::HttpAsync, Transport::Kafka] {
            let mut req = request(transport, &runtime, &dl, &data, &meta);
            req.header = IDEMPOTENCY;
            assert!(
                matches!(apply_guards(req).await, Err(OrionError::Conflict(_))),
                "{transport:?} must reject a duplicate"
            );
        }
    }

    // ---- The dedup claim: who holds a key, and until when ----

    /// The bug the claim exists to prevent. Claiming a key at admission and
    /// never revisiting it destroys every message a redelivering transport
    /// has to retry: attempt 0 registers the key, the attempt fails without
    /// committing, and the redelivery reads the key its own previous attempt
    /// wrote. The Kafka ingress translates that `409` into "already handled",
    /// commits the offset, and the record is gone — never processed, never
    /// dead-lettered.
    #[tokio::test]
    async fn a_kafka_redelivery_recognises_its_own_unsettled_claim() {
        let dl = engine();
        let store = Arc::new(InMemoryDedupBackend::default());
        let runtime = Runtime::new()
            .dedup(store.clone(), BackendErrorPolicy::Allow)
            .build();
        let (data, meta) = (json!({}), json!({}));

        // Attempt 0 of the record at offset 7 claims the key...
        let mut req = request(Transport::Kafka, &runtime, &dl, &data, &meta);
        req.dedup_key_fallback = Some("ORD-77");
        req.dedup_owner = Some("kafka:orders/0/7");
        let admission = admitted(apply_guards(req).await.expect("first attempt admitted"))
            .expect("an admission");
        // ...and fails, so the offset is not committed and the claim is
        // handed back.
        admission
            .dedup_claim
            .expect("a claim was taken")
            .release()
            .await;

        // The retry is the same record, so it must run.
        let mut req = request(Transport::Kafka, &runtime, &dl, &data, &meta);
        req.dedup_key_fallback = Some("ORD-77");
        req.dedup_owner = Some("kafka:orders/0/7");
        assert!(
            apply_guards(req).await.is_ok(),
            "a redelivery of an uncommitted offset must be processed, not committed as a duplicate"
        );
    }

    /// The same, without the release: a consumer killed mid-dispatch never
    /// runs `release`, and the claim it left behind is still in the store
    /// when the record comes back. It must recognise the claim as its own.
    #[tokio::test]
    async fn a_claim_left_behind_by_a_dead_attempt_does_not_suppress_the_record() {
        let dl = engine();
        let store = Arc::new(InMemoryDedupBackend::default());
        let runtime = Runtime::new()
            .dedup(store.clone(), BackendErrorPolicy::Allow)
            .build();
        let (data, meta) = (json!({}), json!({}));

        for _ in 0..3 {
            let mut req = request(Transport::Kafka, &runtime, &dl, &data, &meta);
            req.dedup_key_fallback = Some("ORD-77");
            req.dedup_owner = Some("kafka:orders/0/7");
            let admission = admitted(apply_guards(req).await.expect("admitted"))
                .expect("an admission, not a cache hit");
            // Claim dropped, never settled — the process died here.
            drop(admission);
        }
    }

    /// Confirming is what makes the guard useful on Kafka: once a record's
    /// offset is committed, a *later* record carrying the same key — a
    /// producer retry, a mirrored partition, or this record replayed because
    /// its commit was lost — is the duplicate the channel asked to suppress.
    #[tokio::test]
    async fn a_confirmed_claim_suppresses_every_later_delivery() {
        let dl = engine();
        let store = Arc::new(InMemoryDedupBackend::default());
        let runtime = Runtime::new()
            .dedup(store.clone(), BackendErrorPolicy::Allow)
            .build();
        let (data, meta) = (json!({}), json!({}));

        let mut req = request(Transport::Kafka, &runtime, &dl, &data, &meta);
        req.dedup_key_fallback = Some("ORD-77");
        req.dedup_owner = Some("kafka:orders/0/7");
        let admission = admitted(apply_guards(req).await.expect("admitted")).expect("an admission");
        admission
            .dedup_claim
            .expect("a claim was taken")
            .confirm()
            .await;

        // The producer re-sends the same key at a later offset.
        let mut req = request(Transport::Kafka, &runtime, &dl, &data, &meta);
        req.dedup_key_fallback = Some("ORD-77");
        req.dedup_owner = Some("kafka:orders/0/12");
        assert!(
            matches!(apply_guards(req).await, Err(OrionError::Conflict(_))),
            "a settled key must refuse a later record carrying it"
        );

        // And so is a replay of the original record whose commit was lost:
        // "settled" belongs to no owner, so nobody reclaims it.
        let mut req = request(Transport::Kafka, &runtime, &dl, &data, &meta);
        req.dedup_key_fallback = Some("ORD-77");
        req.dedup_owner = Some("kafka:orders/0/7");
        assert!(
            matches!(apply_guards(req).await, Err(OrionError::Conflict(_))),
            "a settled key must refuse a replay of the record that settled it"
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
        req.header = IDEMPOTENCY;
        assert!(apply_guards(req).await.is_ok(), "the first request passes");

        let mut req = request(Transport::HttpSync, &runtime, &dl, &data, &meta);
        req.header = IDEMPOTENCY;
        assert!(
            matches!(apply_guards(req).await, Err(OrionError::Conflict(_))),
            "the second must be refused"
        );
    }

    /// Backpressure is the one guard that can refuse *after* the key is
    /// claimed. A shed request ran nothing, so it must not burn the key: the
    /// caller's retry (or, on Kafka, the redelivery of the offset it never
    /// committed) has to be judged on its own merits.
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
        req.header = IDEMPOTENCY;
        assert!(matches!(
            apply_guards(req).await,
            Err(OrionError::ServiceUnavailable(_))
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
        req.header = IDEMPOTENCY;
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
            Transport::Kafka,
            Transport::ChannelCall,
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
        req.header = IDEMPOTENCY;
        assert!(matches!(
            apply_guards(req).await,
            Err(OrionError::Conflict(_))
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
            connector: None,
        }
    }

    /// `compute_cache_key` for the cases that must produce one.
    fn key(
        channel: &str,
        data: &serde_json::Value,
        metadata: &serde_json::Value,
        cfg: &crate::channel::ChannelCacheConfig,
    ) -> String {
        super::compute_cache_key(channel, data, metadata, cfg)
            .expect("this request must have a cache key")
    }

    fn meta(
        method: &str,
        params: serde_json::Value,
        query: serde_json::Value,
    ) -> serde_json::Value {
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

    /// A payload that resolves *no* declared field has no meaningful key, so
    /// the request is refused the cache rather than given a key it shares with
    /// every other request on the channel.
    #[test]
    fn a_payload_matching_no_declared_field_has_no_key() {
        let m = meta("POST", serde_json::json!({}), serde_json::json!({}));
        let fields = Some(vec!["user_id".to_string(), "action".to_string()]);
        assert!(
            super::compute_cache_key(
                "acct",
                &serde_json::json!({"unrelated": 1}),
                &m,
                &cache_cfg(fields)
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
}
