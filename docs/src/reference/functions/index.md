<!-- description: Every built-in Orion task function in one table, with its category, the connector it needs, its page, and whether a retry is safe. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-25 -->

# Task functions

A workflow is an ordered list of **tasks**, and every task invokes one built-in **function** with an `input` object. Functions read and write the [data context](../workflows.md#the-data-context), the JSON document that flows through the pipeline. This hub lists every function the release documented here ships; each name links to its own page.

Some functions are contributed by the [dataflow-rs](https://github.com/GoPlasmatic/dataflow-rs) engine. The rest are Orion handlers that talk to [connectors](../connectors/index.md), compose channels, or compute locally. `GET /api/v1/admin/functions` serves the authoritative list on a running instance; see [Runtime function discovery](./runtime-discovery.md).

<div class="table-filter" data-label="Filter functions"></div>

| Function | Category | Connector | Purpose |
|----------|----------|:---------:|---------|
| [`parse_json`](./parse_json.md) | Data | — | Parse the raw payload into the data context |
| [`parse_xml`](./parse_xml.md) | Data | — | Parse an XML payload into the data context |
| [`map`](./map.md) | Data | — | Transform/reshape data with JSONLogic |
| [`filter`](./filter.md) | Data | — | Gate the pipeline on a JSONLogic condition |
| [`validation`](./validation.md) | Data | — | Collect validation errors from JSONLogic rules |
| [`log`](./log.md) | Data | — | Emit a structured log line |
| [`publish_json`](./publish_json.md) | Data | — | Serialize a context field to a JSON string |
| [`publish_xml`](./publish_xml.md) | Data | — | Serialize a context field to an XML string |
| [`http_call`](./http_call.md) | Connector | HTTP | Call an external API with retry + circuit breaker |
| [`data_query`](./data_query.md) | Connector | SQL / MongoDB / ES | Portable, backend-neutral query |
| [`data_write`](./data_write.md) | Connector | SQL / MongoDB / ES | Portable, backend-neutral insert/update/delete/upsert |
| [`db_read`](./db_read.md) | Connector | SQL | Run a raw `SELECT`, return rows as JSON |
| [`db_write`](./db_write.md) | Connector | SQL | Run raw `INSERT`/`UPDATE`/`DELETE`, return affected count |
| [`cache_read`](./cache_read.md) | Connector | Cache | Read one value, or several in one round trip, from Redis or the in-memory cache |
| [`cache_write`](./cache_write.md) | Connector | Cache | Write a value to cache with optional TTL |
| [`cache_delete`](./cache_delete.md) | Connector | Cache | Delete exact keys, so a write can drop what it made stale |
| [`cache_incr`](./cache_incr.md) | Connector | Cache | Atomically increment an integer — a generation counter |
| [`mongo_read`](./mongo_read.md) | Connector | MongoDB | Run a raw `find()`, return documents as JSON |
| [`mongo_write`](./mongo_write.md) | Connector | MongoDB | Insert/update/replace/delete documents, nested shapes included |
| [`mongo_aggregate`](./mongo_aggregate.md) | Connector | MongoDB | Run a stage-allowlisted aggregation pipeline |
| [`publish_kafka`](./publish_kafka.md) | Connector | Kafka | Publish a message to a Kafka topic |
| [`send_email`](./send_email.md) | Connector | SMTP | Send transactional email through an SMTP connector |
| [`storage_presign`](./storage_presign.md) | Connector | Storage | Compute a time-limited presigned object URL — no data path |
| [`storage_head`](./storage_head.md) | Connector | Storage | Object metadata (exists/size/etag) |
| [`channel_call`](./channel_call.md) | Composition | — | Invoke another channel's workflow in-process |
| [`model_infer`](./model_infer.md) | Compute | — | Run an admitted ONNX model: the manifest's adapters in, tensors through, the result out |
| [`crypto`](./crypto.md) | Utility | — | Digests, HMAC compute/verify, password hashing |
| [`jwt_sign`](./jwt_sign.md) | Utility | — | Mint a signed JWT (login, refresh, client assertions) |
| [`jwt_verify`](./jwt_verify.md) | Utility | — | Verify a JWT against static keys or a JWKS |

> [!NOTE]
> The **Category** column groups the table for reading. It is not the wire value: `GET /api/v1/admin/functions` serves a `category` of `connector`, `control`, `data`, `compute`, or `utility` for every function, so tooling should branch on those rather than on the labels here.

When a function fails, the data-plane response follows [Errors and response envelopes](../errors.md). Use its stable error code and the trace's task ID to identify the failing step; do not parse the message text. Each function's page describes its own validation and runtime failures.

## Related

- [Workflows](../../concepts/workflows.md): the pipeline model every function runs in.
- [Author a workflow](../../guides/author/workflows.md): choosing and combining functions in practice.
- [Workflow definition](../workflows.md): the task object a function sits in, and the data context.
- [Retry safety](./retry-safety.md): what a retry of each function costs.
- [Runtime function discovery](./runtime-discovery.md): the live catalogue, plugin functions included.
