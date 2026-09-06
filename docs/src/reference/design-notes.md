<!-- description: The reasoning behind Orion's rules — read a note when a rule looks arbitrary and you want the argument, or when you are weighing whether its trade-off fits. -->
# Design Notes

You do not need this page to run Orion. Every rule below is stated in one sentence on the page that owns it; this page holds the reasoning. Read a note when a rule looks arbitrary and you want the argument, or when you are deciding whether the rule's trade-off fits your case.

Each note opens with the rule as shipped, makes the case, and links the page that states the rule normatively. This is also the one page in the reference where runtime internals (`ArcSwap`, semaphores, hash framing) are fair game.

## Deduplication: claim, then settle

**The rule as shipped:** a delivery whose idempotency key was settled inside the window is refused; a retry of a delivery that never settled runs.

Deduplication matters most on Kafka ingest, because Kafka is at-least-once by design: a rebalance or a lost offset commit replays records the workflow already ran. The key is the record header named by the channel's `deduplication.header` when the producer sets one, else the record key — the natural idempotency key. A record recognized as a duplicate is skipped and its offset committed; nothing is dead-lettered, because nothing failed.

"Recognized as a duplicate" means something precise, and the distinction is what keeps deduplication from eating traffic. The key is **claimed** before the workflow runs — the only point at which the check is atomic against a concurrent delivery. It is marked **settled** only when the delivery is durably accounted for: the workflow completed, or the record was preserved in the dead-letter queue. That is exactly the condition under which the record's offset is committed. A record that is refused, times out, or fails without a confirmed DLQ write releases its claim and leaves its offset uncommitted, so Kafka redelivers it. The redelivery presents the same delivery identity (`topic/partition/offset`) the claim was made under, is recognized as the *same* delivery rather than a second one, and runs. Only a delivery that arrives after the key was settled is skipped.

Without that distinction, the first attempt's own claim would refuse its retry. The ingress would read the refusal as "already handled" and commit the offset — a record committed having never run, which is the failure mode deduplication exists to prevent, inverted.

Over HTTP there is no settle step to reason about. One request is one delivery; the claim simply stands for the rest of the window, and a replay of the key is answered `409 Conflict`, which is what a replay should get.

The residual risk is the transport's, not the guard's. A record whose workflow completed but whose offset commit was lost is redelivered and *is* suppressed — its key was settled. A record that never reached a committed outcome is reprocessed. Deduplication narrows at-least-once; it does not make Kafka exactly-once.

Two boundary decisions follow from the same logic:

- **`channel_call` is never deduplicated.** An in-process call is a step inside a request that was already deduplicated at its own ingress, and it carries no key of its own — it would inherit the parent's. A workflow that calls one channel once per line item would see its second call refused as a duplicate of the first.
- **A backend outage is a policy, not a guess.** The default, `on_backend_error: "allow"`, fails open: if the dedup store cannot answer, the request proceeds without the check — availability wins. Payment-style channels can set `"deny"` to fail closed with `503 Service Unavailable` — never `409`, because the key is unverifiable, not a known duplicate.

Stated normatively in: [Channel Configuration](./channel-config.md).

## Why response-cache keys ignore headers

**The rule as shipped:** the response-cache key is the channel, the HTTP method, the route parameters, the query string, and the payload (or the subset named by `cache_key_fields`). Request headers never contribute.

Orion does not inspect headers to decide cache identity and will not grow a `cache_key_headers` setting. A cached entry is shared by every caller whose method, route, query, and payload agree, whatever headers they sent.

The consequence is the one rule to design around: **if a response varies by anything a header carries, that thing must appear in the payload, or the channel must not cache.** A channel whose workflow reads `metadata.headers` or `metadata.cookies` — through `validation_logic` or a task, and lets one of them change the response body is a channel whose cache will serve one caller's body to another. A channel that emits a `Set-Cookie` through response shaping is the same hazard in reverse: a cached response replays one caller's cookie to the next. Fold the distinguishing value into the payload and name it in `cache_key_fields`, or leave `cache` disabled.

This is a deliberate boundary rather than a gap. Orion is single-tenant by design, and the key strategy is the workflow author's to define, expressed in the payload they control — not inferred by the runtime from transport headers whose meaning it cannot know. A runtime that guessed — hashing `Authorization`, say — would fragment the cache for channels where the header does not matter and still miss the channels where some other header does.

The same conservatism runs in the other direction. A request that resolves **none** of the declared `cache_key_fields` has no distinguishing key, so it bypasses the cache: the workflow runs and nothing is stored. The alternative would be one shared key for every such request — one caller's response served to everyone on the channel. Orion logs a warning naming the channel and the fields when this happens, because it almost always means the field names do not match the payload shape.

Stated normatively in: [Channel Configuration](./channel-config.md).

## Kafka's timeout clamp

**The rule as shipped:** on the Kafka and `/async` ingresses, a channel's `timeout_ms` may shorten the transport's deadline but never lengthen it — the value is clamped to `kafka.processing_timeout_ms` and `trace_queue.processing_timeout_ms` respectively.

On the synchronous HTTP path a channel's `timeout_ms` is honored as declared: nothing but the caller waits on it. On the two queued paths the deadline protects something shared. A Kafka dispatch blocks the consumer's poll loop, so a channel asking for ten minutes on a topic would take the consumer past librdkafka's `max.poll.interval.ms` (300 s by default) and get it evicted from its consumer group mid-record. An `/async` dispatch occupies one of a fixed number of queue workers, so an over-long channel deadline starves every other channel's queued work.

`timeout_ms` is channel configuration: mutable at runtime through the admin API, and not validated against either ceiling. The clamp is what keeps a config change from becoming an outage. The worst a channel can do is time out sooner than the operator's ceiling — never hold the poll loop or a worker past it. A channel that genuinely needs longer on those paths needs the transport setting raised, and that decision belongs to the operator who owns the consumer and the worker pool, not to the channel author.

Stated normatively in: [Timeouts, Retries & Circuit Breakers](../operate/failure-handling.md#bound-how-long-anything-may-take), with both ceilings in [Configuration](./configuration.md).

## Why forwarded headers are ignored by default

**The rule as shipped:** the direct peer IP is authoritative; `X-Forwarded-For` and `X-Real-IP` are honored only when the peer falls inside `rate_limit.trusted_proxies`, and the default is empty — forwarded headers are never trusted.

A forwarded header is a claim, not a fact: any client can send one. Only a proxy you operate can be trusted to append to it honestly. So the question Orion answers is not "what does the header say" but "who handed me this connection". When the peer is inside a listed CIDR block (bare IPs are read as `/32` or `/128`), the client is the **rightmost** `X-Forwarded-For` hop that is not itself a trusted proxy — the hop your own proxy appended. The leftmost elements arrive from the client verbatim and are never used, so a forged prefix cannot mint an identity.

The default is empty because the two failure modes are not symmetric:

- **Behind a load balancer with the list unset**, every request appears to come from the balancer. All clients share one rate-limit bucket, and the limit applies to the whole fleet at once. This is degraded, but visible and recoverable: list the balancer's subnet — `trusted_proxies = ["10.0.0.0/8"]`, and per-client limiting returns.
- **List a network you do not control**, and clients on it can spoof `X-Forwarded-For` to mint a fresh identity per request, which is no rate limiting at all, and forged addresses in the audit trail besides. This failure is silent.

An overly cautious default costs a config line; an overly trusting one is a security hole. That asymmetry is the design.

The key is spelled under `[rate_limit]`, but what it configures is broader: "may a forwarded header name the client". Four things resolve the caller's address with it — the platform rate limiter, the failed-admin-auth backoff, the `details.client_ip` on every audit row, and a channel's own `rate_limit` block, which the ingress guards enforce whether or not the platform limiter is enabled. On a proxied deployment, leaving it empty means audit rows record the load balancer's address and every client behind the proxy shares one per-channel bucket — even with `rate_limit.enabled = false`.

Stated normatively in: [Configuration](./configuration.md#rate-limiting).

## Cursor paging

**The rule as shipped:** the trace list pages with an opaque `next_cursor` in the default `created_at` ordering; `cursor` is refused with a 400 alongside `offset` or any other `sort_by`, and `total` is opt-in.

Offset paging is the intuitive mode and the one that degrades. `OFFSET n` does not seek: the database walks and discards `n` rows to find the page start, so the cost of fetching a page grows linearly with its depth, and a loop that drains a large traces table pays more for every page than for the one before it. A keyset cursor turns the same request into an index seek — "rows strictly after this position" — whose cost is flat no matter how deep the loop is. The two modes disagree about what a position means, so passing both is refused rather than guessed at.

Two decisions inside the cursor:

- **It orders by `created_at` and refuses everything else.** A cursor is a position in an ordering, and a position is only meaningful if rows hold still. `created_at` is immutable. `updated_at` is rewritten in place by every status change, so a row can cross the cursor's position between two page fetches — a trace that moves from ahead of the cursor to behind it is silently skipped; one that moves the other way is served twice. That is why `cursor` with any `sort_by` but `created_at` is a 400, not a best effort.
- **It carries a tie-break.** `created_at` alone is not unique: two traces can share a timestamp on backends whose defaults store second precision. The cursor therefore encodes the pair `(created_at, id)`, and the page condition compares on the pair, so a page boundary that falls inside a shared timestamp neither skips nor repeats the rows sharing it. (The comparison is spelled out as two clauses rather than a row-value `(a, b) < (?, ?)`, because MySQL does not turn the row-value form into an index range scan.)

`total` is opt-in (`?include_total=true`) for the same economics: counting the filtered set is a full scan on PostgreSQL and InnoDB, and a paging loop that asked for the count on every page would pay that scan once per page for a number that rarely changes between them.

The cursor encodes `<microseconds since the epoch>.<trace id>` today. Treat it as opaque: the encoding is not part of the API contract and may change.

The same shape applies to a page read through the portable dialect, where
`after` derives the comparison from the sort — including the null arm above —
and to a raw `db_read`, where you write it yourself. See
[Paging](./data-dialect.md#paging).

Stated normatively in: [Data API](./data-api.md#paging-a-large-traces-table).

## How hot reload swaps the runtime generation

**The rule as shipped:** a reload builds the new engine *and* the new channel estate beside the running ones and publishes the pair with a single atomic pointer swap; requests already executing finish on the generation they started with, and no request is dropped or held off.

A **generation** is those two halves together — the engine (workflows) and the channel estate (route table, rate limiters, compiled validation logic, dedup stores, response caches, backpressure semaphores, quarantine set) — built from one read of the database. The server shares one `Arc<ArcSwap<RuntimeGeneration>>`, an atomic cell holding the current one. A request loads it once, at admission, and runs against that generation for its whole lifetime: the route it matched, the guards it passed and the engine it executes on all come from the same build. Publishing is one atomic store into the cell. There is no lock, so there is no window in which a reader is held off and no writer waiting for readers to drain: loads before the store return the old generation, loads after it return the new one. The old one stays alive by reference count until its last in-flight request completes, then is freed.

**Why one value and not two.** The engine and the estate were published separately until 1.6, each with an atomic store of its own, the estate first — "guards before reachability", so a channel could never be reachable before its guards existed. Each store was atomic and the *pair* was not, and no ordering could fix that; it could only choose which mismatch the window produced. Publishing the estate first meant a just-activated channel was routable while the old engine had no workflow for it: the request was admitted, matched zero workflows, and came back `200` with the caller's own input and nothing done. Publishing the engine first would have traded that for a channel reachable before its guards. One value has no ordering to get right.

The sequence is:

1. **Build off to the side.** The reload reads every active channel and workflow from the database and constructs both halves outside any lock. If the engine cannot be built, the reload aborts with nothing published and the running generation is untouched — the failure mode is "the old configuration keeps serving", never "nothing serves", and never a node left half-updated.
2. **Publish once.** One store makes both halves visible together. A channel that fails to load is quarantined — refused at every ingress, reported through `GET /health` — and the reload proceeds for every other channel. The alternative, aborting the whole reload for one broken `config_json`, would make every activate, archive, and delete hostage to the estate's worst row.
3. **Then tend the consumer.** After the store, the Kafka consumer is restarted only if the reload changed the set of subscribed topics; otherwise it is paused and resumed around the swap. In cluster mode, epoch-driven reloads add per-node jitter to a full restart so the replicas do not all leave and rejoin the consumer group at the same instant.

A cluster resync — triggered when another node's mutation advances the shared config epoch — is this same reload plus a connector-registry refresh and eviction of all cached connector pools, because a remote node cannot know *which* connector changed; pools rebuild lazily on next use.

Stated normatively in: [The Entity Lifecycle](../concepts/lifecycle.md#what-moves-the-engine).

## The guard pipeline

**The rule as shipped:** one enforcement point applies a channel's ingress guards on every transport, in a fixed order — rate limit, authentication, origin allow-list, `validation_logic`, deduplication, response-cache lookup, backpressure.

There are two decisions here: guards as data, and the order.

Four ingresses can dispatch a message to a channel — synchronous HTTP, `/async` submission, Kafka, and in-process `channel_call`, and the channel's declared contract must hold on whichever one the message arrived on. A single function applies the guards, and *which* guards run is data: a per-transport guard set, one constant row per ingress, rather than a comment repeated at four call sites. Adding an ingress means adding a row, and every exclusion in the matrix is a property of the transport rather than an oversight. A Kafka record has no `Origin` header to check and is already authenticated by the broker connection. An `/async` submission answers `202` with a trace ID and has no response body to cache. A `channel_call` is a step inside a request that was authenticated and deduplicated at its own ingress — enforcing either again would break composition without making it safer.

The order is deliberate:

- **Rate limit first**, so a refusal costs the least work, and so rejected requests still consume a token. If any other check ran before the limiter, an attacker probing that check would get unmetered rejections.
- **Authentication immediately after, and before anything stateful.** An unauthenticated caller must not reach the response-cache lookup, which would let them probe which requests are cached. Nor may they reach the dedup store, where they could claim an idempotency key belonging to a real caller and have the genuine request refused as a duplicate. Keeping authentication *after* the rate limit means credential-stuffing is metered like any other traffic.
- **Origin allow-list, then `validation_logic`**: refusals of requests that would never be admitted, before any shared state is touched on their behalf.
- **Deduplication before the cache lookup**, so a replayed idempotency key is answered `409` rather than served a cached body. The two guards answer different questions — "did this delivery already happen?" versus "do we already know the answer?", and the idempotency contract outranks the optimization.
- **Backpressure last.** A cache hit should not consume a concurrency permit it never needed. And because the permit is the only guard that can refuse *after* the idempotency key has been claimed, its refusal releases the claim before returning — a shed request does not burn a key it never got to use.

An admitted request leaves the pipeline carrying what the rest of processing needs: the backpressure permit, the context for storing the response in the cache, the resolved deadline (see [Kafka's timeout clamp](#kafkas-timeout-clamp)), and the dedup claim to settle.

One nuance surprises often enough to state here: applying the rate limit on every transport means the same limiter is *consulted* everywhere, not that the four ingresses share one bucket. The default bucket key is whatever caller identity the transport has — client IP over HTTP, topic on Kafka, the calling channel for `channel_call`, so `requests_per_second` is a per-identity rate on each ingress. Only a `rate_limit.key_logic` that returns a transport-independent value turns it into one shared throughput cap.

Stated normatively in: [How Orion Works](../concepts/how-orion-works.md#one-requests-journey) and [Channel Configuration](./channel-config.md).

## Related

- [How Orion Works](../concepts/how-orion-works.md): the request flow these internals sit behind.
- [Channel Configuration](./channel-config.md): every channel key named above: `deduplication`, `cache`, `timeout_ms`, `rate_limit`, `validation_logic`.
- [Configuration](./configuration.md): the server-level ceilings and `trusted_proxies`.
- [Data API](./data-api.md): the trace endpoints the cursor pages.
