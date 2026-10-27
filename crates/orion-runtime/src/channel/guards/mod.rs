//! Transport-neutral per-channel ingress guards.
//!
//! Four ingresses dispatch a message to a channel — synchronous HTTP, HTTP
//! `/async` submission, in-process `channel_call`, and a cron channel's
//! claimed occurrence — and a channel's declared contract must hold whichever
//! one the message arrived on. [`apply_guards`] is the one function that
//! enforces it, and the per-transport [`GuardSet`] returned by
//! [`Transport::guards`] decides which guards run, so the deliberate
//! exclusions are **data** rather than a comment repeated at four call sites.
//!
//! | Guard | HTTP sync | HTTP `/async` | `channel_call` | cron |
//! |---|---|---|---|---|
//! | rate limit | ✅ | ✅ | ✅ | ❌ |
//! | channel `auth` | ✅ | ✅ | ❌ | ❌ |
//! | origin allow-list | ✅ | ✅ | ❌ | ❌ |
//! | `validation_logic` | ✅ | ✅ | ✅ | ✅ |
//! | deduplication | ✅ | ✅ | ❌ | ❌ |
//! | response cache | ✅ | ❌ | ❌ | ❌ |
//! | backpressure | ✅ | ✅ | ✅ | ✅ |
//! | channel `timeout_ms` | ✅ | ✅¹ | ✅ | ✅ |
//!
//! ¹ clamped to the transport's ceiling — see [`effective_timeout_ms`]. The
//! async worker caps the channel value at `trace_queue.processing_timeout_ms`,
//! because on that path the deadline protects a shared resource (a queue
//! worker) rather than one caller's patience. A channel can shorten its
//! deadline everywhere; it can only lengthen it where nothing else depends on
//! it.
//!
//! N16: every `❌` cell is a decision, not an omission, and each is justified
//! on [`Transport::guards`]. An omission is how a guard goes missing on one
//! path — `channel_call` with no rate limit, or a channel timing out at its
//! `timeout_ms` over HTTP and at the global `trace_queue.processing_timeout_ms`
//! over `/async`.
//!
//! A ✅ in the rate-limit row means *the same limiter is consulted*, not that
//! the ingresses share one bucket. The bucket key defaults to whatever
//! caller identity the transport has — client IP over HTTP, calling channel
//! for `channel_call` — so a channel's
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

use std::sync::Arc;

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::expr::ExprEngine;
use serde_json::Value;

use super::ChannelRuntimeConfig;
use crate::errors::OrionError;

mod admission;
mod dedup;
mod rate_limit;
mod response_cache;

pub use dedup::DedupClaim;
pub(crate) use rate_limit::{COMMON_KEY_HEADERS, key_logic_header_paths};
pub use response_cache::{CacheFlights, CacheStoreCtx};

use admission::{acquire_backpressure, check_allowed_origin, check_auth, validate_input};
use dedup::check_deduplication;
use rate_limit::{check_principal_rate_limit, check_rate_limit};
use response_cache::{CacheLookup, check_response_cache};

/// Which ingress carried a message to a channel. Selects the [`GuardSet`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// `POST /api/v2/data/{channel}` — the synchronous HTTP data plane.
    HttpSync,
    /// `POST /api/v2/data/{channel}/async` — HTTP submission to the trace queue.
    HttpAsync,
    /// In-process `channel_call` from another channel's workflow.
    ChannelCall,
    /// One occurrence of a cron channel's schedule, claimed from the durable
    /// ledger by a worker.
    Cron,
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
    /// Per-channel token bucket / shared-store window.
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
    /// * **origin allow-list off `channel_call`.** The check reads an HTTP
    ///   `Origin` header. An in-process call has no browsing context and no
    ///   origin to check; a channel that lists origins is constraining its
    ///   *HTTP* surface.
    /// * **deduplication off `channel_call`.** Deduplication suppresses a
    ///   redelivery of the same *ingress* event. A `channel_call` is a step
    ///   inside a request that was already deduplicated at its own ingress,
    ///   and it carries no key of its own — the child would inherit the
    ///   parent's, so a workflow that fans out to the same channel twice
    ///   (one call per line item) would see its second call rejected as a
    ///   duplicate.
    /// * **response cache off `/async` and `channel_call`.** An `/async`
    ///   submission answers `202` with a trace id and never has a body to
    ///   serve or to store. `channel_call` returns its result into the caller's message, and
    ///   caching that would pin one workflow run's output across callers
    ///   with none of the request identity (method, path, query) the cache
    ///   key is built from.
    /// * **authentication off `channel_call`.** It is already authenticated
    ///   by the layer that delivered it: a `channel_call` is a step *inside* a
    ///   request that authenticated at its own ingress, and the calling
    ///   workflow holds no credential to present — enforcing here would make
    ///   composition impossible rather than make it safer, the same reasoning
    ///   that leaves deduplication off that transport.
    /// * **`oauth2_login` on `HttpSync` alone.** Both legs are browser
    ///   redirects. The first *is* a `302`, which `/async` (which answers
    ///   `202` with a trace id), `channel_call` (whose caller is a workflow,
    ///   not a user agent) and cron (which has no caller) have no way to
    ///   express. The callback leg is off those three for the stronger
    ///   reason that admitting it there would run the channel's workflow with
    ///   no grant at `metadata.oauth` — a sign-in that appears to succeed and
    ///   established nothing. The data route refuses `…/callback/async`
    ///   outright rather than letting it through as a grant-less run.
    /// * **almost everything off `Cron`.** This transport has no caller at all,
    ///   which is a stronger statement than `channel_call`'s "the caller
    ///   authenticated elsewhere". There is nobody to authenticate, no origin, no response to
    ///   cache or shape, and no redirect to follow. Rate limiting is off for a
    ///   different reason than the rest: a limit would be a second, contradictory
    ///   answer to the question the cron expression already answers, and the
    ///   loser would be silent. **Deduplication is off because the durable
    ///   ledger does its job better**: `UNIQUE(channel_id, scheduled_for)` makes
    ///   an occurrence unique by construction, for all time rather than for a
    ///   window, and a retry of an occurrence is *meant* to run. What is left on
    ///   is what a schedule can still mean: `validation` (a payload that no
    ///   longer satisfies the channel's own rule should fail visibly, not run)
    ///   and `backpressure` (the permit is per channel, so scheduled work draws
    ///   from the same budget as everything else rather than around it).
    ///
    ///   Every `false` here is refused at authoring time by
    ///   `validation::cron`, so an author cannot write a guard this row will
    ///   silently ignore. `cron_guards_and_validation_agree` below pins the two
    ///   lists to each other.
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
            Transport::Cron => GuardSet {
                auth: false,
                origin_allow_list: false,
                rate_limit: false,
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
/// HTTP passes a view over the `HeaderMap`; `channel_call` a view over the
/// inherited `metadata.headers`.
/// Used by deduplication (the idempotency header is per-channel config) and
/// by `rate_limit.key_logic`.
///
/// The value is lent from the transport's own storage — the `HeaderMap`, or
/// the metadata — for as long as the request: a guard that only compares or
/// hashes a header (every credential check) copies nothing, and one that
/// keeps it owns it explicitly. `'r` is the lookup's own borrow and `'v` the
/// values', kept apart so a lookup built for one call can lend values that
/// outlive it.
pub type HeaderLookup<'r, 'v> = &'r (dyn Fn(&str) -> Option<&'v str> + Send + Sync + 'r);

/// The message as a channel's own expressions read it: one
/// `{"data": …, "metadata": …}` object, built once per message by moving the
/// transport's payload and metadata into it.
///
/// `validation_logic` and the response cache's `key_logic` both evaluate
/// against exactly this shape. Building it per reader with
/// `json!({"data": data, "metadata": metadata})` would deep-copy the payload
/// and every request header each time, plus once more to merge verified
/// claims. Instead the transport moves its two values in, the guards read and
/// (for the claims) edit them in place, and [`Self::into_parts`] hands them
/// back for the engine.
#[derive(Debug)]
pub struct GuardContext(Value);

/// What [`GuardContext::data`] and [`GuardContext::metadata`] lend when a part
/// is absent — which only a context built from something other than
/// [`GuardContext::new`] could be.
static NULL: Value = Value::Null;

impl GuardContext {
    /// The context for one message, taking ownership of both halves.
    pub fn new(data: Value, metadata: Value) -> Self {
        let mut root = serde_json::Map::with_capacity(2);
        root.insert("data".to_string(), data);
        root.insert("metadata".to_string(), metadata);
        Self(Value::Object(root))
    }

    /// The payload, as `validation_logic` and the cache key see it.
    pub fn data(&self) -> &Value {
        self.0.get("data").unwrap_or(&NULL)
    }

    /// The message metadata, as `validation_logic` sees it.
    pub fn metadata(&self) -> &Value {
        self.0.get("metadata").unwrap_or(&NULL)
    }

    /// The whole object, the root an expression is evaluated against.
    pub(crate) fn root(&self) -> &Value {
        &self.0
    }

    /// Merge verified claims into the metadata at `auth.claims`, in place —
    /// [`merge_auth_claims`]'s shape, without a copy of the metadata.
    fn merge_auth_claims(&mut self, claims: Value) {
        if let Some(metadata) = self.0.get_mut("metadata") {
            *metadata = merge_auth_claims(std::mem::take(metadata), claims);
        }
    }

    /// The payload and the metadata, handed back for the engine.
    pub fn into_parts(self) -> (Value, Value) {
        match self.0 {
            Value::Object(mut root) => (
                root.remove("data").unwrap_or(Value::Null),
                root.remove("metadata").unwrap_or(Value::Null),
            ),
            _ => (Value::Null, Value::Null),
        }
    }
}

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
    /// The message as the channel's own expressions read it — the payload
    /// and the metadata, built once by the transport (see [`GuardContext`]).
    pub context: &'a mut GuardContext,
    /// The process's expression engine, which compiled the channel's
    /// expressions and evaluates them (I188).
    pub exprs: &'a ExprEngine,
    /// `Origin` header value, when the transport carries one.
    pub origin: Option<&'a str>,
    /// Default rate-limit bucket key, used when the channel declares no
    /// `key_logic`: the client IP over HTTP, the calling channel for
    /// `channel_call`.
    pub caller_identity: &'a str,
    /// Named header lookup (see [`HeaderLookup`]).
    pub header: HeaderLookup<'a, 'a>,
    /// Failed-credential budget for `auth.mode = "api_key"` / `"hmac"` (S12).
    /// `None` for the transports that present no credential — an in-process
    /// `channel_call` authenticated at the edge, and a cron occurrence has no
    /// caller — so there is nothing to count and no client to count it
    /// against.
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
    /// configured dedup header is absent. No built-in transport carries one;
    /// every call site passes `None`.
    pub dedup_key_fallback: Option<&'a str>,
    /// Stable identity of *this physical delivery*, for transports that can
    /// redeliver the same message. No built-in transport redelivers, so every
    /// call site passes `None` and gets a one-shot token minted per call. See
    /// `check_deduplication` for why the distinction matters.
    pub dedup_owner: Option<&'a str>,
    /// Timeout applied when the channel declares no `timeout_ms` of its own.
    ///
    /// Per call site: `None` for synchronous HTTP (which has no deadline
    /// beyond the channel's) and for the `/async` submission (the worker
    /// re-resolves at dequeue time), and
    /// `engine.default_channel_call_timeout_ms` for `channel_call`. The
    /// async worker calls [`effective_timeout_ms`] directly with
    /// `trace_queue.processing_timeout_ms`.
    pub default_timeout_ms: Option<u64>,
    /// Hard ceiling on the resolved deadline: the channel's `timeout_ms` may
    /// shorten a message's deadline below this, never lengthen it past.
    ///
    /// `Some(…)` only where the transport's deadline is a *safety* property
    /// rather than a default. The async worker passes
    /// `trace_queue.processing_timeout_ms` because that setting is the operator's cap on how long one queue
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
    /// The `{provider}` route segment, when the channel is multi-provider
    /// (#355). `None` for a single-provider block. It picks the entry on both
    /// legs; an unknown slug is a `404`.
    pub provider: Option<&'a str>,
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

    /// The `{provider}` slug for the deferred authorize redirect (#355), carried
    /// beside [`Self::authorize`] for the same reason [`Self::return_to`] is: it
    /// is the request's own route parameter, gone by the time the workflow has
    /// run.
    pub provider: Option<String>,

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

    /// The normalised identity from a callback leg (#355), stamped at
    /// `metadata.identity` by the ingress beside the grant.
    pub identity: Option<Value>,
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

impl From<crate::channel::oauth2_login::Redirect> for GuardResponse {
    fn from(redirect: crate::channel::oauth2_login::Redirect) -> Self {
        Self {
            status: 302,
            headers: vec![
                ("location".to_string(), redirect.location),
                ("set-cookie".to_string(), redirect.set_cookie),
                // A redirect that mints a per-user nonce must not sit
                // in any cache between here and the browser.
                ("cache-control".to_string(), "no-store".to_string()),
            ],
            body: String::new(),
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
            req.exprs,
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
            req.exprs,
            req.auth_backoff,
            req.caller_identity,
        )
        .await?
    } else {
        None
    };
    // The quota, keyed on who the caller turned out to be. It runs here rather
    // than beside the address limit because the principal does not exist until
    // `check_auth` has returned, and it runs before everything below for the
    // same reason that one does: a caller over its quota must not reach the
    // dedup store or the response cache.
    // Gated on `auth`, not on `rate_limit`: the principal only exists where
    // authentication ran. `channel_call` and cron carry no credential, so a
    // quota keyed on claims there could never compute a key and would refuse
    // every message — the transport matrix answers this, not the config.
    if set.auth {
        check_principal_rate_limit(
            req.channel,
            req.runtime,
            req.exprs,
            req.caller_identity,
            req.header,
            auth_claims.as_ref(),
        )
        .await?;
    }
    if set.origin_allow_list {
        check_allowed_origin(req.channel, req.runtime, req.origin)?;
    }
    // Verified claims join the metadata the channel's own logic sees, so
    // "claim vs request" checks are one-line JSONLogic. Merged once, into the
    // one context both readers below evaluate against: `validation_logic` and
    // the response cache's `key_logic`. The cache key must read this view and
    // never the metadata before the merge — keyed on the subject before it, it
    // read whatever the caller wrote at `metadata.auth` and stored one
    // caller's body under another's key (#354). Only a channel with an
    // expression to read the claims pays for the copy of the claims:
    // `validate_input` returns before looking without `validation_logic`, and
    // the cache key reads metadata beyond params and query only through
    // `key_logic`. The transport merges the claims into the message itself
    // afterwards (`merge_auth_claims`), the same key with the same value, so
    // what the engine sees does not depend on whether this merge ran.
    let reads_claims = req.runtime.as_ref().is_some_and(|rt| {
        (set.validation && rt.validation_logic.is_some())
            || (set.response_cache && rt.cache_key_logic.is_some())
    });
    if reads_claims && let Some(claims) = auth_claims.as_ref() {
        req.context.merge_auth_claims(claims.clone());
    }
    if set.validation {
        validate_input(req.channel, req.runtime, req.context, req.exprs)?;
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
    // Every guard below runs with the idempotency key claimed. A refusal from
    // any of them means nothing ran, so the key goes back — once, here, for
    // every refusal, so a new refusal cannot forget to: held, it would answer
    // the caller's retry `409` for the rest of the window instead of judging
    // it on its merits. An answer — a cache hit, the sign-in redirect — leaves it
    // standing: a replay of the key is a duplicate of a delivery that
    // succeeded.
    match after_dedup(&req, set, auth_claims).await {
        Ok(GuardVerdict::Admitted(admission)) => Ok(GuardVerdict::Admitted(Admission {
            dedup_claim,
            ..admission
        })),
        Ok(answered) => Ok(answered),
        Err(e) => {
            if let Some(claim) = dedup_claim {
                claim.release().await;
            }
            Err(e)
        }
    }
}

/// The guards [`apply_guards`] runs after claiming the idempotency key: the
/// response cache, the backpressure permit and the OAuth2 sign-in legs. An
/// admission it returns carries no claim; the caller attaches it.
async fn after_dedup(
    req: &GuardRequest<'_>,
    set: GuardSet,
    auth_claims: Option<Value>,
) -> Result<GuardVerdict, OrionError> {
    let cache_store = if set.response_cache {
        match check_response_cache(req.channel, req.context, req.runtime, req.exprs).await {
            // The request was answered, so the claim stands: a later replay
            // of the same key is a duplicate of a delivery that succeeded.
            CacheLookup::Hit(body) => return Ok(GuardVerdict::CacheHit(body)),
            CacheLookup::Miss(ctx) => ctx,
        }
    } else {
        None
    };
    let backpressure_permit = if set.backpressure {
        acquire_backpressure(req.channel, req.runtime)?
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
    let mut oauth_provider = None;
    let mut oauth_return_to = None;
    let mut response_cookies = Vec::new();
    let mut oauth_metadata = None;
    let mut oauth_identity = None;
    if set.oauth2_login
        && let Some(ingress) = req.oauth.as_ref()
        && let Some(login) = req.runtime.as_ref().and_then(|rt| rt.oauth2_login.as_ref())
    {
        match ingress.leg {
            crate::channel::OAuthLeg::Authorize if !login.runs_workflow_on_authorize() => {
                let return_to = login.accepted_return_to(ingress.query);
                let redirect = match login.begin(ingress.provider, None, return_to.as_deref()) {
                    Ok(redirect) => redirect,
                    Err(e) => {
                        // An unknown provider is the caller's 404; anything else
                        // (a URL that will not parse, a signing failure) is ours.
                        if matches!(
                            e,
                            OrionError::Http {
                                status: orion_common::errors::StatusCode::NOT_FOUND,
                                ..
                            }
                        ) {
                            return Err(e);
                        }
                        tracing::error!(
                            channel = %req.channel,
                            error = %e,
                            "Could not build the OAuth2 authorize redirect"
                        );
                        return Err(OrionError::internal("could not begin the sign-in"));
                    }
                };
                // The permit drops here; the caller leaves the claim standing,
                // as for `CacheHit`: the request *was* answered.
                return Ok(GuardVerdict::Respond(GuardResponse::from(redirect)));
            }
            crate::channel::OAuthLeg::Authorize => {
                // The redirect is built after the workflow, but an unknown
                // provider must 404 before the workflow runs — a sign-in to a
                // provider this channel does not serve should execute nothing.
                login.require_provider(ingress.provider)?;
                // Checked here, where the request's own query string is, and
                // carried to the redirect that is built after the workflow.
                oauth_return_to = login.accepted_return_to(ingress.query);
                oauth_provider = ingress.provider.map(str::to_string);
                oauth_authorize = Some(std::sync::Arc::clone(login));
            }
            crate::channel::OAuthLeg::Callback => {
                // Every failure here — a missing or mismatched state, a bad
                // nonce, a rejected exchange — happens before the workflow, so
                // the caller hands the key back: a sign-in that failed its
                // CSRF check must stay one the user can attempt again.
                let grant = login
                    .complete(ingress.provider, ingress.query, &ingress.jar)
                    .await?;
                response_cookies.push(grant.clear_cookie);
                oauth_metadata = Some(grant.metadata);
                oauth_identity = grant.identity;
            }
        }
    }

    let oauth = (oauth_authorize.is_some() || oauth_metadata.is_some()).then(|| {
        Box::new(OAuthAdmission {
            response_cookies,
            authorize: oauth_authorize,
            provider: oauth_provider,
            identity: oauth_identity,
            return_to: oauth_return_to,
            grant: oauth_metadata,
        })
    });

    Ok(GuardVerdict::Admitted(Admission {
        backpressure_permit,
        cache_store,
        timeout_ms: effective_timeout_ms(req.runtime, req.default_timeout_ms, req.max_timeout_ms),
        dedup_claim: None,
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
        obj.insert(
            crate::engine::AUTH_KEY.to_string(),
            serde_json::Value::Object(auth),
        );
    }
    metadata
}

/// The deadline for a message on this channel: the channel's declared
/// `timeout_ms` when it has one, else the transport's default — clamped to
/// `max_timeout_ms` where the transport has a ceiling.
///
/// N16: one resolution for every path, so a channel never times out at its
/// configured value over HTTP and at the global
/// `trace_queue.processing_timeout_ms` over `/async`. Both the guard chain
/// and the async worker (which re-resolves at dequeue time, when the config
/// may have moved on) call this.
///
/// The clamp is what keeps that unification from turning an operator's
/// transport ceiling into a mere default. Channel `timeout_ms` is
/// unvalidated and unbounded upward; on the async path a channel asking for
/// ten minutes would occupy a queue worker past
/// `trace_queue.processing_timeout_ms`. A channel can therefore
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
///
/// Read off the evaluated value as 1.12 read the JSON it became
/// ([`result_json`](crate::engine::operators::result_json)): a number JSON
/// cannot spell — an infinity or a NaN — was `null` there and is falsy here,
/// and a datetime, a duration or a tensor was an object and is truthy.
pub(crate) fn is_truthy(val: &OwnedDataValue) -> bool {
    match val {
        OwnedDataValue::Null => false,
        OwnedDataValue::Bool(b) => *b,
        OwnedDataValue::Number(n) => {
            let f = n.as_f64();
            f.is_finite() && f != 0.0
        }
        OwnedDataValue::String(s) => !s.is_empty(),
        OwnedDataValue::Array(a) => !a.is_empty(),
        _ => true,
    }
}

#[cfg(test)]
mod tests;
