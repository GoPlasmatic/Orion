<!-- description: Whether a retry of each Orion task function is free, a re-read, an idempotent write or a duplicated effect, and which input decides when it depends. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Retry safety

Orion retries a task in more places than a workflow author has in mind. The [trace DLQ](../admin-api/trace-dlq.md) replays a failed async delivery,
a Kafka redelivery re-runs everything after an uncommitted offset, and
`http_call` retries its own transport failures. Whether that is harmless
depends on the function.

## Description

This is a different question from whether an *error* was transient. Orion
already classifies that per error — a connection failure is retryable, a
rejected query is not. The table below answers the other half: **if the retry
happens, what does it cost?**
`GET /api/v1/admin/functions` serves the same answer per function as
`retry_safety`, so tooling can read it rather than hard-coding this table.

## Answers

| Answer | Meaning |
|---|---|
| `pure` | No effect outside the message. Free to retry. |
| `read` | Observes state without changing it. A retry costs a round trip and may see a newer value. |
| `idempotent_write` | Writes, but a second run lands the same end state. |
| `unsafe_write` | Writes, and a second run duplicates the effect — the second email, the second record. |
| `depends_on` | The task decides, and the answer carries the input to look at. |

## Functions

| Function | Retry safety | Notes |
|---|---|---|
| [`crypto`](./crypto.md) | `pure` | Local computation. |
| [`jwt_sign`](./jwt_sign.md) | `pure` | Local signing. |
| [`model_infer`](./model_infer.md) | `pure` | The graph is a function of its inputs and its weights; a retry re-runs it and lands the same tensors. |
| [`storage_presign`](./storage_presign.md) | `pure` | SigV4 arithmetic over the connector's credentials; zero bytes move. |
| [`cache_read`](./cache_read.md) | `read` | |
| [`db_read`](./db_read.md) | `read` | |
| [`data_query`](./data_query.md) | `read` | |
| [`mongo_read`](./mongo_read.md) | `read` | |
| [`mongo_aggregate`](./mongo_aggregate.md) | `read` | Aggregation pipelines with `$out`/`$merge` are refused by the stage allowlist, so this stays a read. |
| [`storage_head`](./storage_head.md) | `read` | Metadata only. |
| [`jwt_verify`](./jwt_verify.md) | `read` | May fetch a JWKS document; the cache usually answers. |
| [`cache_write`](./cache_write.md) | `idempotent_write` | The same key and value land the same entry. A `ttl` restarts from the retry. |
| [`cache_delete`](./cache_delete.md) | `idempotent_write` | A second delete of the same keys leaves them deleted; only the reported count differs. |
| [`cache_incr`](./cache_incr.md) | `unsafe_write` | A retry adds `by` again. Harmless for a generation counter (one extra miss); wrong for a count someone reads. |
| [`send_email`](./send_email.md) | `unsafe_write` | A retry sends a second message. |
| [`publish_kafka`](./publish_kafka.md) | `unsafe_write` | A retry publishes a second record. Consumers that need exactly once delivery should dedupe on a key the workflow sets. |
| [`http_call`](./http_call.md) | `depends_on` `method` | `GET`/`HEAD` are safe; `POST`/`PATCH` may already have been applied. This is what the connector's `retry_non_idempotent` flag is about — off by default. |
| [`db_write`](./db_write.md) | `depends_on` `sql` | Raw SQL: an `UPDATE … SET x = 1` is idempotent, an `INSERT` is not. |
| [`data_write`](./data_write.md) | `depends_on` `op` | `upsert` and `delete` are idempotent; `insert` is not; `update` depends on the expression. |
| [`mongo_write`](./mongo_write.md) | `depends_on` `op` | Same split: an upsert repeats safely, an insert does not. |
| [`channel_call`](./channel_call.md) | `depends_on` `channel` | The answer is whatever the target channel's workflow does. |

Every engine built-in (`map`, `filter`, `parse_json`, …) is `pure`: they read
and write the message and nothing else. `log` writes to this node's own
observability output, which a retry repeating is not a duplicated effect in the
sense above.

## Related

- [Task functions](./index.md): every function, with its page.
- [Timeouts, retries and circuit breakers](../../operate/run/failure-handling.md): where Orion retries, and what it deliberately does not re-drive.
- [Traces and async processing](../../operate/run/traces.md): the trace DLQ replay that re-runs a delivery.
- [Kafka channels](../../guides/patterns/kafka-channels.md): the redelivery a consumer must expect.
