<!-- description: Every key of an Orion channel's config object and its routing fields, one page per guard, with the ingresses each applies to and what each refuses. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Channel configuration

A channel's `config` object (stored as `config_json`) declares every per-channel guard, from authentication to tracing. All keys are optional and an empty `{}` is valid; unknown keys are refused at every level. A stored config that no longer parses is quarantined at load rather than served with a guard silently missing.

| Key | Purpose | Page |
|---|---|---|
| `channel_type`, `protocol`, `methods`, `route_pattern`, `topic`, `priority` | How requests reach the channel | [Routing and protocol](./routing.md) |
| `transport_config` | The schedule of a cron channel | [Cron transport](./cron.md) |
| `auth` | Authenticate HTTP callers of this channel | [`auth`](./auth.md) |
| `rate_limit` | Token-bucket admission rate per caller | [`rate_limit`](./rate_limit.md) |
| `principal_rate_limit` | A second limit, keyed on the verified principal | [`principal_rate_limit`](./principal_rate_limit.md) |
| `backpressure` | Per-node concurrency cap; excess is shed with `503` | [`backpressure`](./backpressure.md) |
| `deduplication` | Idempotency-key replay protection | [`deduplication`](./deduplication.md) |
| `cache` | Serve repeated identical requests from a response cache | [`cache`](./cache.md) |
| `request` | How the HTTP request body becomes `data` and `metadata` | [`request`](./request.md) |
| `response` | Shaped status, headers, body and cookies; per-status error bodies | [`response`](./response.md) |
| `validation_logic` | JSONLogic predicate; a falsy result rejects with `400` | [`validation_logic`](./validation_logic.md) |
| `timeout_ms` | Deadline on workflow execution | [`timeout_ms`](./timeout_ms.md) |
| `origin_allow_list` | Server-side `Origin` header check | [`origin_allow_list`](./origin_allow_list.md) |
| `tracing` | Per-channel override of the trace-storage policy | [`tracing`](./tracing.md) |
| `oauth2_login` | Complete a browser OAuth2 authorization-code grant | [`oauth2_login`](./oauth2_login.md) |

## Related

- [Guards by ingress](./guards-by-ingress.md): which guard runs on which ingress, and in what order.
- [Channels](../../concepts/channels.md): what a channel is.
- [Configure a channel](../../guides/author/channels.md): the same keys, as a walkthrough.
- [Data API](../data-api.md): how requests resolve to channels, and what traces carry.
- [Reference conventions](../conventions.md): how to read the field tables, and the `Required` legend.
