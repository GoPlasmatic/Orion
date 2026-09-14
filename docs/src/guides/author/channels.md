<!-- description: Configure an Orion channel: pick the REST, HTTP, Kafka or cron ingress, then enable only the guards it needs — auth, rate limits, dedup, caching, validation. -->
<!-- type: guide -->
<!-- last_verified: 2026-09-14 -->

# Configure a channel

A channel is the endpoint plus the contract it enforces on the way in. This guide is task by task: pick the route, then turn on only the guards that channel needs. Every key has a normative entry in [Channel configuration](../../reference/channel-config/index.md), with its default and its per-ingress semantics.

## Before you start

You need an active workflow for the channel to point at, and a server on `http://localhost:8080`. The channel is created through the admin API and activated like any other entity; [Build your first service](../../get-started/tutorials/first-service.md) shows those calls.

## Choose a route

Declare the method, the path pattern, and the workflow:

```json
{
  "channel_id": "orders", "name": "orders",
  "channel_type": "sync", "protocol": "rest",
  "route_pattern": "/orders/{id}", "methods": ["GET"],
  "workflow_id": "order-lookup"
}
```

Path parameters are whole segments written `{name}`, and reach the workflow as request metadata. Routes match byte-exactly, including case: `/Orders` does not reach a channel declaring `/orders`. Every REST channel also stays reachable by name at `/api/v1/data/{name}`, which is what `orion-cli send` and the examples use.

## Go async

Set `channel_type: "async"` and the channel answers `202` with a trace id instead of the result. Any REST channel also serves its async form at `/{route_pattern}/async`, so the same endpoint can be called both ways without a second channel.

Use sync for anything the caller waits on. Use async for work that outlives a request, where a trace id and a later poll are enough. See [Traces and async processing](../../operate/run/traces.md).

## Authenticate callers

The data plane is open unless the channel says otherwise:

```json
{ "config": { "auth": {
    "mode": "api_key",
    "keys": ["env://ORDERS_API_KEY", "env://ORDERS_API_KEY_PREVIOUS"],
    "header": "X-API-Key"
}}}
```

Listing two keys is how you rotate without a window of refusals. For webhooks, use `hmac` instead. It verifies a signature over the raw body, before parsing, which is the scheme Stripe, GitHub and Shopify send:

```json
{ "config": { "auth": {
    "mode": "hmac",
    "secret": "env://GITHUB_WEBHOOK_SECRET",
    "header": "X-Hub-Signature-256",
    "signature_prefix": "sha256="
}}}
```

Failures are a uniform `401` that never says which part was wrong. An `auth` block whose `env://` secret is unset quarantines the channel rather than serving it unauthenticated.

## Rate-limit

Declare a token bucket:

```json
{ "config": { "rate_limit": { "requests_per_second": 100, "burst": 50 } } }
```

> [!WARNING]
> That is 100/s *per caller*, not 100/s for the channel. The default bucket key is the caller's identity. For a channel-wide ceiling, or to key on something else such as a tenant id or an API key, set `key_logic`.

Behind a proxy, also set `rate_limit.trusted_proxies` in the server config, or every caller collapses into one bucket. See [Trust the right proxies](../../operate/run/security.md#trust-the-right-proxies).

## Validate before the workflow runs

A JSONLogic predicate over the request:

```json
{ "config": { "validation_logic": {
    "and": [
      { "!!": { "var": "data.order_id" } },
      { ">": [{ "var": "data.total" }, 0] }
    ]
}}}
```

A falsy result rejects the request with `400` before any task executes. Use it for the cheap structural checks every caller must satisfy. Leave business rules to the workflow, where a trace records what happened.

## Deduplicate replays

Name the idempotency header and the window:

```json
{ "config": { "deduplication": { "header": "Idempotency-Key", "window_secs": 300 } } }
```

A repeat of a settled key inside the window answers `409` instead of running twice. On Kafka the record key or a header serves the same purpose.

> [!NOTE]
> Deduplication narrows at-least-once; it does not make Kafka exactly once. A duplicate delivery of an *unfinished* attempt re-runs it, because only a settled key suppresses. If double execution would be harmful, make the downstream write idempotent too. The mechanism is in [Design notes](../../concepts/design-notes.md#deduplication-claim-then-settle).

## Cache responses

Turn the cache on and say which payload fields identify a request:

```json
{ "config": { "cache": { "enabled": true, "ttl_secs": 60, "cache_key_fields": ["data.customer_id"] } } }
```

A hit skips workflow execution entirely.

> [!NOTE]
> Request headers are never part of the cache key. If a response varies by something a header carries, such as a tenant, a locale or an API key, that thing must appear in the payload and in `cache_key_fields`, or the channel must not cache.

## Shed load instead of queueing it

Cap in-flight work per node:

```json
{ "config": { "backpressure": { "max_concurrent_per_node": 50 } } }
```

Excess requests get an immediate `503` rather than waiting, which protects latency for the requests already admitted. The name is literal: N replicas admit up to N times this number in flight.

## Bound execution time

Declare the deadline:

```json
{ "config": { "timeout_ms": 5000 } }
```

This is the promise you make the caller. Set connector timeouts below it. See [Timeouts, retries and circuit breakers](../../operate/run/failure-handling.md).

## Shape the response

By default a sync channel returns the standard envelope with the final `data` context. A channel can instead let the workflow control the status code, headers and body. That suits a webhook that must answer `204`, or an endpoint whose body is not the whole context. See [Response shaping](../../reference/channel-config/response.md).

## Verify

Ask whether the activation would succeed, without making it:

```bash
curl -s -X PATCH "http://localhost:8080/api/v1/admin/channels/orders/status?dry_run=true" \
  -H 'Content-Type: application/json' -d '{"status":"active"}'
```

Unknown keys anywhere in `config` are refused with a `400` naming the key. That is deliberate: a silently ignored guard reads as protection while providing none.

## Next steps

- [Channel configuration](../../reference/channel-config/index.md): every key, default, and which ingresses each guard applies to.
- [Channels](../../concepts/channels.md): the concept, if this page assumed too much.
- [Data API](../../reference/data-api.md): how a request resolves to a channel.
- [Secure an instance](../../operate/run/security.md): the instance-level half of the same job.
