<!-- description: Which channel guard runs on which of the five ingresses (HTTP sync, /async, Kafka, channel_call, cron), and the fixed order the guards apply in. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Guards by ingress

Which of a channel's guards run on which ingress, and the order they apply in. A channel is reachable on up to five ingresses, and each guard runs on the ones marked Yes.

## Description

| Guard | HTTP sync | HTTP `/async` | Kafka | `channel_call` | Cron |
|---|---|---|---|---|---|
| `rate_limit` | Yes | Yes | Yes | Yes | No |
| `principal_rate_limit` | Yes | Yes | No | No | No |
| `auth` | Yes | Yes | No | No | No |
| `origin_allow_list` | Yes | Yes | No | No | No |
| `validation_logic` | Yes | Yes | Yes | Yes | Yes |
| `deduplication` | Yes | Yes | Yes | No | No |
| `cache` | Yes | No | No | No | No |
| `backpressure` | Yes | Yes | Yes | Yes | Yes |
| `oauth2_login` | Yes | No | No | No | No |
| `timeout_ms` | Yes | Yes | Yes¹ | Yes | Yes |

¹ Clamped to a transport ceiling. See [Timeouts](./timeout_ms.md). Every No cell is deliberate; the owning section below states why.

The Cron column is mostly No because that ingress has no caller at all, which is a stronger statement than Kafka's "the caller authenticated elsewhere". A cron channel is refused these keys at authoring time rather than storing them and ignoring them — see [Cron transport](./cron.md).

## Order of application

Guards run in a fixed order: rate limit → auth → origin allow-list → validation → deduplication → cache lookup → backpressure → `oauth2_login`. Four consequences follow.

- A rejected request (bad origin, failed validation) still consumes a rate-limit token.
- A replayed idempotency key answers `409` before the cache is consulted.
- A cache hit never consumes a backpressure permit, and a request shed by backpressure releases its idempotency claim.
- `oauth2_login` runs last, *after* the backpressure permit, because its callback leg makes a round trip to the identity provider. That call must be bounded by the channel's concurrency cap. The consequence is that `validation_logic` sees the request and not the grant — the grant is what the workflow is for.

## Related

- [Channels](../../concepts/channels.md): what a channel is, and its ingresses.
- [Configure a channel](../../guides/author/channels.md): the guards, as a walkthrough.
- [Data API](../data-api.md): the HTTP ingress in full.
- [Channel configuration](./index.md): every key, with its page.
