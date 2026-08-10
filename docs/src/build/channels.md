# Configure Channels

A channel is the endpoint plus the contract it enforces on the way in. This page
is task-by-task: pick the route, then turn on only the guards that channel
needs.

Every key here has a normative entry in
[Channel Configuration](../reference/channel-config.md) with its default and
per-ingress semantics. This page is how to reach for them.

## Choose a route

```json
{
  "channel_id": "orders", "name": "orders",
  "channel_type": "sync", "protocol": "rest",
  "route_pattern": "/orders/{id}", "methods": ["GET"],
  "workflow_id": "order-lookup"
}
```

Path parameters are whole segments written `{name}`, and reach the workflow as
request metadata. Routes match **byte-exactly**, including case — `/Orders` does
not reach a channel declaring `/orders`.

Every REST channel also stays reachable by name at `/api/v1/data/{name}`, which
is what `orion-cli send` and the examples use.

## Go async

Set `channel_type: "async"` and the channel answers `202` with a trace id
instead of the result. Any REST channel also serves its async form at
`/{route_pattern}/async` — so the same endpoint can be called both ways without
a second channel.

**Use sync for:** anything the caller waits on. **Use async for:** work that
outlives a request, where a trace id and a later poll are enough. See
[Traces & Async Processing](../operate/traces.md).

## Authenticate callers

The data plane is open unless the channel says otherwise.

```json
{ "config": { "auth": {
    "mode": "api_key",
    "keys": ["env://ORDERS_API_KEY", "env://ORDERS_API_KEY_PREVIOUS"],
    "header": "X-API-Key"
}}}
```

Listing two keys is how you rotate without a window of refusals. For webhooks,
use `hmac` instead — it verifies a signature over the raw body, before parsing,
which is the scheme Stripe, GitHub, and Shopify send:

```json
{ "config": { "auth": {
    "mode": "hmac",
    "secret": "env://GITHUB_WEBHOOK_SECRET",
    "header": "X-Hub-Signature-256",
    "signature_prefix": "sha256="
}}}
```

Failures are a uniform `401` that never says which part was wrong. An `auth`
block whose `env://` secret is unset **quarantines the channel** rather than
serving it unauthenticated.

## Rate-limit

```json
{ "config": { "rate_limit": { "requests_per_second": 100, "burst": 50 } } }
```

> [!WARNING]
> **That is 100/s *per caller*, not 100/s for the channel.** The default bucket
> key is the caller's identity. For a channel-wide ceiling, or to key on
> something else — a tenant id, an API key — set `key_logic`.

Behind a proxy, also set `rate_limit.trusted_proxies` in the server config, or
every caller collapses into one bucket. See
[Secure an Instance](../operate/security.md#trust-the-right-proxies).

## Validate before the workflow runs

```json
{ "config": { "validation_logic": {
    "and": [
      { "!!": { "var": "data.order_id" } },
      { ">": [{ "var": "data.total" }, 0] }
    ]
}}}
```

A falsy result rejects the request with `400` before any task executes. Use it
for the cheap structural checks every caller must satisfy; leave business rules
to the workflow, where a trace records what happened.

## Deduplicate replays

```json
{ "config": { "deduplication": { "enabled": true, "header": "Idempotency-Key", "ttl_secs": 300 } } }
```

A repeat of a settled key inside the window answers `409` instead of running
twice. On Kafka the record key or a header serves the same purpose.

> [!IMPORTANT]
> **Deduplication narrows at-least-once; it does not make Kafka
> exactly-once.** A duplicate delivery of an *unfinished* attempt re-runs it —
> only a settled key suppresses. If double execution would be harmful, make the
> downstream write idempotent too. The mechanism is in
> [Design Notes](../reference/design-notes.md#deduplication-claim-then-settle).

## Cache responses

```json
{ "config": { "cache": { "enabled": true, "ttl_secs": 60, "cache_key_fields": ["data.customer_id"] } } }
```

A hit skips workflow execution entirely.

> [!NOTE]
> **Request headers are never part of the cache key.** If a response varies by
> something a header carries — a tenant, a locale, an API key — that thing must
> appear in the payload and in `cache_key_fields`, or the channel must not
> cache.

## Shed load instead of queueing it

```json
{ "config": { "backpressure": { "max_concurrent_per_node": 50 } } }
```

Excess requests get an immediate `503` rather than waiting, which protects
latency for the requests already admitted. The name is literal: N replicas admit
up to N× this number in flight.

## Bound execution time

```json
{ "config": { "timeout_ms": 5000 } }
```

This is the promise you make the caller. Set connector timeouts below it — see
[Timeouts, Retries & Circuit Breakers](../operate/failure-handling.md).

## Shape the response

By default a sync channel returns the standard envelope with the final `data`
context. A channel can instead let the workflow control the status code,
headers, and body — for a webhook that must answer `204`, or an endpoint whose
body is not the whole context. See
[Channel Configuration › Response shaping](../reference/channel-config.md#response-shaping).

## Check it before it serves

```bash
# Would this activation succeed? Writes nothing.
curl -s -X PATCH "http://localhost:8080/api/v1/admin/channels/orders/status?dry_run=true" \
  -H 'Content-Type: application/json' -d '{"status":"active"}'
```

Unknown keys anywhere in `config` are refused with a `400` naming the key. That
is deliberate: a silently-ignored guard reads as protection while providing
none.

## Related

- [Channel Configuration](../reference/channel-config.md) — every key, default,
  and which ingresses each guard applies to.
- [Channels](../concepts/channels.md) — the concept, if this page assumed too
  much.
- [Data API](../reference/data-api.md) — how a request resolves to a channel.
- [Secure an Instance](../operate/security.md) — the instance-level half of the
  same job.
