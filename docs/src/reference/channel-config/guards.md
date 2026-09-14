<!-- description: The ingress guards of an Orion channel: auth, rate_limit, principal_rate_limit, backpressure, deduplication, validation_logic and origin_allow_list. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Ingress guards

The `config` blocks that admit or refuse a request before its workflow runs.

| Page | Holds |
|---|---|
| [`auth`](./auth.md) | the `api_key`, `hmac` and `jwt` modes, every field each takes, the webhook presets, and the rules a failure and a rotation follow. |
| [`rate_limit`](./rate_limit.md) | the token bucket, the key_logic context and key_headers, cross-ingress semantics, and cluster-wide enforcement. |
| [`principal_rate_limit`](./principal_rate_limit.md) | a quota keyed on the verified JWT claims, applied after authentication on top of the address-keyed rate_limit. |
| [`backpressure`](./backpressure.md) | a per-node concurrency permit shared by every ingress, with excess shed as 503 rather than queued. |
| [`deduplication`](./deduplication.md) | idempotency-key replay protection within a window, the backing store, and how Kafka and cluster mode behave. |
| [`validation_logic`](./validation_logic.md) | a JSONLogic predicate over data and metadata that rejects a request with 400 before its workflow runs. |
| [`origin_allow_list`](./origin_allow_list.md) | a server-side Origin header check on the HTTP ingresses, and how it differs from the platform CORS layer. |

## Related

- [Channel configuration](./index.md): every key, with its page.
- [Guards by ingress](./guards-by-ingress.md): which guard runs where, and in what order.
- [Configure a channel](../../guides/author/channels.md): the same keys, as a walkthrough.
