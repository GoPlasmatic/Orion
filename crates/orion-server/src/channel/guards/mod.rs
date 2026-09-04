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
//! | channel `auth` | ✅ | ✅ | ❌ | ❌ |
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
//! `trace_queue.processing_timeout_ms` over Kafka and `/async`. Every `❌` cell
//! that remains is deliberate, and each is justified on
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
//!
//! ## Where the guards live
//!
//! The matrix above and the order the guards run in are this file's subject;
//! the guards themselves are one module each — `rate_limit`, `dedup`,
//! `response_cache`, and `admission` for the four that answer yes or no
//! without deriving a key or keeping backend bookkeeping (channel `auth`, the
//! origin allow-list, `validation_logic`, backpressure).
//!
//! The split is by concept and not by size: each guard has exactly one caller
//! — [`apply_guards`] — so the seam was already there, and someone changing
//! the dedup policy should not have to scroll past rate limiting to find it.
//! The tests stay here because they test *this*: 52 of them drive
//! `apply_guards` across the transport matrix, and six of the seven guards
//! have no direct test at all, because a guard's contract is what the matrix
//! does with it.

use dataflow_rs::datalogic_rs;
use std::sync::Arc;

use serde_json::Value;

use super::ChannelRuntimeConfig;
use crate::errors::OrionError;

mod admission;
mod dedup;
mod rate_limit;
mod response_cache;

pub use dedup::DedupClaim;
pub(crate) use rate_limit::{COMMON_KEY_HEADERS, key_logic_header_paths};
pub use response_cache::CacheStoreCtx;

use admission::{acquire_backpressure, check_allowed_origin, check_auth, validate_input};
use dedup::check_deduplication;
use rate_limit::check_rate_limit;
use response_cache::{CacheLookup, check_response_cache};

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
    /// Authenticate the caller against the channel's `auth` config.
    pub auth: bool,
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
    /// The inbound OAuth2 sign-in flow (#307): answer the authorize leg with a
    /// `302`, or verify and exchange a callback.
    pub oauth2_login: bool,
}

impl Transport {
    /// This transport's row of the guard matrix.
    ///
    /// Every `false` cell, each for a reason that is a property of the
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
    /// * **authentication off Kafka and `channel_call`.** Both carry a
    ///   credential the channel's `auth` config cannot describe, and both are
    ///   already authenticated by the layer that delivered them. A Kafka
    ///   record's authentication is the broker's (SASL/mTLS on the consumer
    ///   connection); it has no HTTP headers, and no signature over a body the
    ///   producer never signed. A `channel_call` is a step *inside* a request
    ///   that authenticated at its own ingress, and the calling workflow holds
    ///   no credential to present — enforcing here would make composition
    ///   impossible rather than make it safer, the same reasoning that leaves
    ///   deduplication off that transport. A channel that is reachable both
    ///   over HTTP and from a Kafka topic is therefore authenticated on the
    ///   HTTP path and broker-authenticated on the Kafka one.
    /// * **`oauth2_login` on `HttpSync` alone.** Both legs are browser
    ///   redirects. The first *is* a `302`, which `/async` (which answers
    ///   `202` with a trace id), Kafka (which answers nothing) and
    ///   `channel_call` (whose caller is a workflow, not a user agent) have no
    ///   way to express. The callback leg is off those three for the stronger
    ///   reason that admitting it there would run the channel's workflow with
    ///   no grant at `metadata.oauth` — a sign-in that appears to succeed and
    ///   established nothing. The data route refuses `…/callback/async`
    ///   outright rather than letting it through as a grant-less run.
    pub const fn guards(self) -> GuardSet {
        match self {
            Transport::HttpSync => GuardSet {
                auth: true,
                origin_allow_list: true,
                rate_limit: true,
                validation: true,
                deduplication: true,
                response_cache: true,
                backpressure: true,
                oauth2_login: true,
            },
            Transport::HttpAsync => GuardSet {
                auth: true,
                origin_allow_list: true,
                rate_limit: true,
                validation: true,
                deduplication: true,
                response_cache: false,
                backpressure: true,
                oauth2_login: false,
            },
            Transport::Kafka => GuardSet {
                auth: false,
                origin_allow_list: false,
                rate_limit: true,
                validation: true,
                deduplication: true,
                response_cache: false,
                backpressure: true,
                oauth2_login: false,
            },
            Transport::ChannelCall => GuardSet {
                auth: false,
                origin_allow_list: false,
                rate_limit: true,
                validation: true,
                deduplication: false,
                response_cache: false,
                backpressure: true,
                oauth2_login: false,
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
    /// Failed-credential budget for `auth.mode = "api_key"` / `"hmac"` (S12).
    /// `None` for the transports that present no credential — Kafka
    /// authenticates at the broker connection, and an in-process
    /// `channel_call` authenticated at the edge — so there is nothing to
    /// count and no client to count it against.
    pub auth_backoff: Option<&'a crate::auth::FailedAuthTracker>,
    /// The request body exactly as received, for `auth.mode = "hmac"`.
    ///
    /// A webhook signature is computed over these bytes, so verification has to
    /// see them before anything parses them: re-serializing the parsed JSON
    /// reorders keys and drops whitespace, and the signature would never match
    /// again. Only the HTTP transports carry it; the two that authenticate are
    /// the two that supply it.
    pub raw_body: Option<&'a [u8]>,
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

    /// The inbound OAuth2 sign-in leg this request arrived on, when the
    /// channel declares one and the transport can carry it (#307). `None`
    /// everywhere else, which is every request to every channel that does not.
    ///
    /// The query parameters travel here rather than being read back out of
    /// `metadata["query"]`: that key is stamped only when the real query string
    /// is non-empty, so on a query-less request a caller-supplied envelope
    /// `metadata` survives in its place. Everywhere else that would be a
    /// curiosity; here it is `state` and `code`, and a security check must not
    /// read a value the caller could have written.
    pub oauth: Option<OAuthIngress<'a>>,
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
    /// Verified JWT claims (#267), for the caller to place at
    /// `metadata.auth.claims` when building the message. `None` for the
    /// party-level auth modes and for optional-auth requests without a token.
    pub auth_claims: Option<Value>,
    /// The idempotency key this delivery holds, when the channel deduplicates
    /// and a key was resolved.
    ///
    /// A transport that can redeliver **must** settle it: [`DedupClaim::confirm`]
    /// once the message is durably accounted for, [`DedupClaim::release`] when
    /// it is not. A transport that cannot redeliver (one HTTP request, one
    /// delivery) may simply drop it — the claim then stands for the rest of
    /// the window, which is exactly the `409` a replay of that key should get.
    pub dedup_claim: Option<DedupClaim>,

    /// What the `oauth2_login` guard decided, when it ran at all (#307).
    ///
    /// One boxed option rather than three fields, because the three are set
    /// together or not at all and every request to every other channel — which
    /// is nearly all of them — would otherwise pay their width on the stack.
    pub oauth: Option<Box<OAuthAdmission>>,
}

/// What an HTTP ingress knows about a request to an `oauth2_login` channel
/// that the transport-neutral guard layer otherwise could not.
pub struct OAuthIngress<'a> {
    /// Which of the channel's two routes matched.
    pub leg: crate::channel::OAuthLeg,
    /// The request's query string, parsed. The authorize leg reads a
    /// `return_to`; the callback reads `state`, `code` and `error`.
    pub query: &'a std::collections::HashMap<String, String>,
    /// Every `Cookie` header value, for the state cookie. HTTP/2 clients may
    /// split a jar across several headers (RFC 9113 §8.2.3).
    pub jar: Vec<&'a str>,
}

/// The `oauth2_login` guard's contribution to an admitted request.
pub struct OAuthAdmission {
    /// `Set-Cookie` values the platform appends to whatever response the
    /// workflow produces. Today the one entry is the callback retiring the
    /// state cookie it just verified — which has to happen whatever the
    /// workflow answers, including when the workflow fails.
    pub response_cookies: Vec<String>,

    /// The compiled sign-in block, carried only on an authorize leg that runs
    /// its workflow first (`run_workflow_on_authorize`). The redirect is built
    /// after the workflow, so the sync path needs the block.
    pub authorize: Option<std::sync::Arc<crate::channel::CompiledOAuth2Login>>,

    /// The caller's `return_to`, already checked against the channel's
    /// allow-list. Travels with [`Self::authorize`] because the check needs the
    /// request's own query string, which is gone by the time the workflow has
    /// finished.
    pub return_to: Option<String>,

    /// The verified grant from a callback leg, stamped at `metadata.oauth` by
    /// the ingress. Carried here rather than merged in the guard for the same
    /// reason `auth_claims` is: the metadata object belongs to the transport,
    /// so there is one merge point rather than one per ingress.
    pub grant: Option<Value>,
}

/// A complete response a guard built itself.
///
/// Plain strings rather than an `axum::response::Response`, for the reason
/// stated at the top of this module: header-derived inputs are lowered so a
/// non-HTTP caller never needs a `HeaderMap`, and the same has to hold in the
/// other direction. Shaped like `sync.rs`'s `ShapedResponse`, `Vec` and all —
/// a redirect that sets a cookie has two headers whose names may repeat.
pub struct GuardResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// Outcome of [`apply_guards`].
pub enum GuardVerdict {
    /// Every guard the transport applies passed.
    Admitted(Admission),
    /// The response cache answered — carries the pre-serialized body. Only
    /// [`Transport::HttpSync`] can produce this.
    CacheHit(String),
    /// A guard answered the request itself. Today that is the `oauth2_login`
    /// authorize leg, whose whole job is to redirect the browser to the
    /// identity provider — the workflow is not entered, and the engine is
    /// never reached. Only [`Transport::HttpSync`] can produce this.
    Respond(GuardResponse),
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
/// Authentication comes straight after it, and before everything else: an
/// unauthenticated caller must not be able to reach the response-cache lookup
/// (which would let them probe for which requests are cached) or the dedup
/// store (where they could claim an idempotency key belonging to a real
/// caller and have the genuine request answered `409`). Keeping it *after* the
/// rate limit means credential-stuffing is metered like any other traffic.
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
    let auth_claims = if set.auth {
        check_auth(
            req.channel,
            req.runtime,
            req.header,
            req.raw_body,
            req.datalogic,
            req.auth_backoff,
            req.caller_identity,
        )
        .await?
    } else {
        None
    };
    if set.origin_allow_list {
        check_allowed_origin(req.channel, req.runtime, req.origin)?;
    }
    if set.validation {
        // Verified claims join the metadata the channel's own logic sees, so
        // "claim vs request" checks are one-line JSONLogic.
        let metadata_with_auth = auth_claims
            .as_ref()
            .map(|claims| merge_auth_claims(req.metadata.clone(), claims.clone()));
        validate_input(
            req.channel,
            req.runtime,
            req.data,
            metadata_with_auth.as_ref().unwrap_or(req.metadata),
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
        match check_response_cache(
            req.channel,
            req.data,
            req.metadata,
            req.runtime,
            req.datalogic,
        )
        .await
        {
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

    // Last, and after the backpressure permit rather than before it. The
    // callback leg makes a round trip to the identity provider, and that is the
    // only I/O in this chain: run before the permit, a burst of callbacks would
    // open one outbound request each with nothing bounding the concurrency. The
    // cost is that `validation_logic` (step 4) has already run, so it sees the
    // request and not the grant — which is the right split anyway, because the
    // grant is what the workflow is for.
    //
    // A channel that declares `oauth2_login` may not also declare `cache`
    // (refused at create time): the lookup at step 6 would otherwise serve a
    // stored `302` carrying a spent state cookie, or replay one user's
    // callback to the next caller.
    let mut oauth_authorize = None;
    let mut oauth_return_to = None;
    let mut response_cookies = Vec::new();
    let mut oauth_metadata = None;
    if set.oauth2_login
        && let Some(ingress) = req.oauth.as_ref()
        && let Some(login) = req.runtime.as_ref().and_then(|rt| rt.oauth2_login.as_ref())
    {
        match ingress.leg {
            crate::channel::OAuthLeg::Authorize if !login.runs_workflow_on_authorize() => {
                let return_to = login.accepted_return_to(ingress.query);
                let redirect = match login.begin(None, return_to.as_deref()) {
                    Ok(redirect) => redirect,
                    Err(e) => {
                        tracing::error!(
                            channel = %req.channel,
                            error = %e,
                            "Could not build the OAuth2 authorize redirect"
                        );
                        // Nothing ran, so hand the key back — the same rule the
                        // backpressure branch above states and for the same
                        // reason. A sign-in that could not even be started must
                        // not make the user's retry a duplicate of it.
                        if let Some(claim) = dedup_claim {
                            claim.release().await;
                        }
                        return Err(OrionError::internal("could not begin the sign-in"));
                    }
                };
                crate::metrics::record_oauth_login(
                    req.channel,
                    crate::channel::OAuthLeg::Authorize,
                    "ok",
                );
                // The permit and the claim both drop here. The claim standing
                // is correct and matches `CacheHit`: the request *was*
                // answered, so a replay of the key is a duplicate of a
                // delivery that succeeded.
                return Ok(GuardVerdict::Respond(GuardResponse {
                    status: 302,
                    headers: vec![
                        ("location".to_string(), redirect.location),
                        ("set-cookie".to_string(), redirect.set_cookie),
                        // A redirect that mints a per-user nonce must not sit
                        // in any cache between here and the browser.
                        ("cache-control".to_string(), "no-store".to_string()),
                    ],
                    body: String::new(),
                }));
            }
            crate::channel::OAuthLeg::Authorize => {
                // Checked here, where the request's own query string is, and
                // carried to the redirect that is built after the workflow.
                oauth_return_to = login.accepted_return_to(ingress.query);
                oauth_authorize = Some(std::sync::Arc::clone(login));
            }
            crate::channel::OAuthLeg::Callback => {
                let grant = match login.complete(ingress.query, &ingress.jar).await {
                    Ok(grant) => grant,
                    Err(e) => {
                        // Every failure here — a missing or mismatched state, a
                        // bad nonce, a rejected exchange — happens *before* the
                        // workflow, so nothing ran and the key goes back. Held,
                        // it would answer the user's retry `409` for the rest of
                        // the dedup window rather than judging it on its merits:
                        // a sign-in that failed its CSRF check would become a
                        // sign-in that cannot be attempted again.
                        if let Some(claim) = dedup_claim {
                            claim.release().await;
                        }
                        return Err(e);
                    }
                };
                response_cookies.push(grant.clear_cookie);
                oauth_metadata = Some(grant.metadata);
            }
        }
    }

    let oauth = (oauth_authorize.is_some() || oauth_metadata.is_some()).then(|| {
        Box::new(OAuthAdmission {
            response_cookies,
            authorize: oauth_authorize,
            return_to: oauth_return_to,
            grant: oauth_metadata,
        })
    });

    Ok(GuardVerdict::Admitted(Admission {
        backpressure_permit,
        cache_store,
        timeout_ms: effective_timeout_ms(req.runtime, req.default_timeout_ms, req.max_timeout_ms),
        dedup_claim,
        auth_claims,
        oauth,
    }))
}

/// [`apply_guards`] for a transport whose row leaves `response_cache` off —
/// every ingress but [`Transport::HttpSync`].
///
/// [`GuardVerdict`] spans the whole matrix, so its `CacheHit` and `Respond`
/// variants are statically unreachable for those transports. Resolving that
/// impossibility here is the same principle as the matrix itself: the exclusion
/// is read from [`Transport::guards`] once instead of each call site
/// hand-writing its own handling of a branch it can never take. A transport
/// that does cache, or that can carry a redirect, calls [`apply_guards`] and
/// matches every variant.
pub async fn admit(req: GuardRequest<'_>) -> Result<Admission, OrionError> {
    let transport = req.transport;
    match apply_guards(req).await? {
        GuardVerdict::Admitted(admission) => Ok(admission),
        GuardVerdict::CacheHit(_) => Err(OrionError::internal(format!(
            "{transport:?} does not enable the response cache"
        ))),
        GuardVerdict::Respond(_) => Err(OrionError::internal(format!(
            "{transport:?} cannot answer a request from a guard"
        ))),
    }
}

/// Merge verified claims into request metadata at `auth.claims` (#267) — the
/// one definition of the shape that both the channel's own `validation_logic`
/// (in [`apply_guards`]) and the workflow message (the data route) see, so a
/// change to it (say, adding `auth.mode`) cannot skew the two surfaces.
pub fn merge_auth_claims(
    mut metadata: serde_json::Value,
    claims: serde_json::Value,
) -> serde_json::Value {
    if let Some(obj) = metadata.as_object_mut() {
        let mut auth = serde_json::Map::with_capacity(1);
        auth.insert("claims".to_string(), claims);
        obj.insert("auth".to_string(), serde_json::Value::Object(auth));
    }
    metadata
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
pub(crate) fn is_truthy(val: &Value) -> bool {
    match val {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::key_logic_header_paths;
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
                StubOutcome::Duplicate => Ok(Some(super::dedup::DEDUP_SETTLED.to_string())),
                StubOutcome::BackendError => {
                    Err(OrionError::internal("dedup backend down".to_string()))
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
            Err(OrionError::internal("backend down".to_string()))
        }
    }

    /// Builder for a `ChannelRuntimeConfig`, so each test states only the
    /// guard it exercises.
    struct Runtime {
        parsed_config: ChannelConfig,
        rate_limiter: Option<Arc<dyn crate::channel::RateLimitBackend>>,
        rate_limit_key_logic: Option<datalogic_rs::Logic>,
        rate_limit_key_headers: Option<Arc<[String]>>,
        validation_logic: Option<datalogic_rs::Logic>,
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

        fn key_logic(mut self, engine: &datalogic_rs::Engine, logic: serde_json::Value) -> Self {
            self.rate_limit_key_logic = Some(engine.compile(&logic).expect("test logic compiles"));
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
                key_logic: None,
                connector: None,
            });
            self.response_cache = Some(backend);
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
                parsed_config: self.parsed_config,
                rate_limiter: self.rate_limiter,
                rate_limit_key_logic: self.rate_limit_key_logic,
                cache_key_logic: None,
                rate_limit_key_headers: self.rate_limit_key_headers,
                validation_logic: self.validation_logic,
                backpressure_semaphore: self.backpressure_semaphore,
                dedup_store: self.dedup_store,
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
            auth_backoff: None,
            origin: None,
            caller_identity: "10.0.0.1",
            header: NO_HEADERS,
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
        assert!(sync.auth && submit.auth);
        assert!(!kafka.auth && !call.auth);
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
            let present: HeaderLookup<'_> =
                &|name: &str| (name == "X-API-Key").then(|| "s3cret".to_string());
            let mut req = request(transport, &runtime, &dl, &data, &metadata);
            req.header = present;
            assert!(
                apply_guards(req).await.is_ok(),
                "{transport:?} refused a request presenting the right key"
            );
        }
    }

    /// The two transports whose row leaves `auth` off carry no credential to
    /// present, and are authenticated by the layer that delivered them — the
    /// broker for Kafka, the originating ingress for `channel_call`. Enforcing
    /// here would break composition rather than tighten anything.
    #[tokio::test]
    async fn authentication_does_not_apply_to_kafka_or_channel_call() {
        let dl = engine();
        let data = json!({});
        let metadata = json!({});

        for transport in [Transport::Kafka, Transport::ChannelCall] {
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
        req.header = IDEMPOTENCY;
        // `GuardVerdict` is not `Debug`, so `expect_err` is unavailable;
        // `.err().expect(..)` asserts the same thing without it.
        let err = apply_guards(req)
            .await
            .err()
            .expect("an unauthenticated caller must be refused");
        // `Deny` on a dedup backend error is a 503; authentication refuses 401.
        // Seeing the 503 would mean the dedup guard ran first.
        assert!(
            matches!(err, OrionError::Unauthorized(_)),
            "expected a 401 from the auth guard, got {err:?} — the dedup guard ran first"
        );
    }

    /// The channel's `timeout_ms` wins on every transport; the transport's
    /// default only fills in when the channel declares none (N16 — Kafka and
    /// `/async` used to ignore the channel value entirely).
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
    /// it. On Kafka the dispatch blocks the poll loop and an over-long one
    /// gets the consumer evicted from its group mid-message; on the async
    /// path it holds one of a fixed number of queue workers past the
    /// operator's ceiling. `timeout_ms` is unvalidated and unbounded upward,
    /// so the clamp is the only thing standing between a channel config and
    /// those two failures.
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

    /// #275: the defect. A `key_logic` reading a header the context does not
    /// carry resolves to `null` — datalogic returns `null` for a missing path
    /// rather than erroring — and the old code serialized that into the key,
    /// so the bucket became the literal string `"null"` for *every* caller.
    /// An intended per-device quota silently became one shared channel-wide
    /// bucket, with no warning and no metric.
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

        let device = |name: &str| (name == "deviceid").then(|| "phone-1".to_string());
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
    /// are both refused *for key unavailability*. Under the old behaviour the
    /// first was admitted into the shared `"null"` bucket and the second was
    /// refused as over-limit — so asserting the variant, not just the refusal,
    /// is what makes this test meaningful.
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
            let lookup = move |name: &str| (name == "deviceid").then(|| caller.to_string());
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

        let blank = |name: &str| (name == "x-tenant-id").then(|| "   ".to_string());
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
    /// visible to `key_logic` on every transport — the per-device limit that
    /// was previously inexpressible.
    #[tokio::test]
    async fn a_declared_custom_header_is_visible_to_key_logic() {
        for transport in [Transport::HttpSync, Transport::HttpAsync, Transport::Kafka] {
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

            let phone = |name: &str| (name == "deviceid").then(|| "phone".to_string());
            let tablet = |name: &str| (name == "deviceid").then(|| "tablet".to_string());

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
            "deviceid" => Some("phone".to_string()),
            "x-partner" => Some("acme".to_string()),
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

        let acme = |name: &str| (name == "x-tenant-id").then(|| "acme".to_string());
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
                matches!(verdict, Err(OrionError::ServiceUnavailable { .. })),
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
            Transport::Kafka,
            Transport::ChannelCall,
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
            key_logic: None,
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
        super::response_cache::compute_cache_key(
            channel,
            data,
            metadata,
            cfg,
            None,
            &datalogic_rs::Engine::new(),
        )
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

    /// The property the hash choice exists to serve: the same request must
    /// key identically in every process, so a replica sharing a Redis cache
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
                &serde_json::json!({"unrelated": 1}),
                &m,
                &cache_cfg(fields),
                None,
                &datalogic_rs::Engine::new(),
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
