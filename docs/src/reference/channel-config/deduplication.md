<!-- description: The deduplication block of a channel: idempotency-key replay protection within a window, the backing store, and how Kafka and cluster mode behave. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `deduplication`

`deduplication` extracts an idempotency key from a request header and refuses a repeat of the same key within the window with `409 Conflict`.

## Synopsis

```json
{
  "deduplication": {
    "header": "Idempotency-Key",
    "window_secs": 300,
    "on_backend_error": "deny"
  }
}
```

## Fields

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `header` | string | yes | — | Header carrying the idempotency key. |
| `window_secs` | integer | no | `300` | Seconds a key is remembered. |
| `connector` | string | no | in-memory store | Name of a [cache connector](../connectors/index.md) backing the dedup store. In cluster mode the default is the shared cluster Redis. |
| `on_backend_error` | string | no | `"allow"` | `allow` proceeds without the check when the store cannot answer; `deny` refuses with `503` — never `409`, because the key is unverifiable, not a known duplicate. |

Rules:

- Keys are scoped per channel. A request that does not carry the header is not checked.
- The key is claimed before the workflow runs and settled once the outcome is durable. A delivery that fails without settling is re-processed on retry, not refused as a duplicate of itself. The full claim/settle argument is in [Availability](../../concepts/lifecycle.md).
- `deny` on payment-style workloads trades availability for the guarantee that a duplicate can never slip through an outage.

**Kafka.** Kafka ingest deduplicates too, and needs it most: at-least-once delivery replays records the workflow already ran. The key is the record header named by `header` when the producer sets one, else the record key. A recognized duplicate is skipped and its offset committed — nothing is dead-lettered, because nothing failed. Set the key per logical event, not per entity. Deduplication narrows at-least-once; it does not make Kafka exactly once.

**`channel_call` is exempt.** An in-process call is a step inside a request already deduplicated at its own ingress and carries no key of its own. It would inherit the parent's, so a workflow calling one channel once per line item would see its second call refused.

**Cluster mode.** A channel whose dedup connector is missing, broken, or explicitly in-memory refuses to load instead of silently degrading to per-node state. On a single node, an unusable connector falls back to process memory with a warning.

## Related

- [The entity lifecycle](../../concepts/lifecycle.md): the claim-and-settle argument.
- [Kafka channels](../../guides/patterns/kafka-channels.md): deduplicating at-least-once delivery.
- [Connector types › Cache](../connectors/cache.md): the connector that backs the store.
- [Deploy a cluster](../../operate/deploy/cluster.md): the shared store a cluster requires.
- [Channel configuration](./index.md): every key, with its page.
