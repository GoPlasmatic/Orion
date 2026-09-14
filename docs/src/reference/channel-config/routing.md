<!-- description: The channel fields beside config that decide how requests arrive: channel_type, protocol, methods, route_pattern, topic, consumer_group and priority. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Routing and protocol

These fields sit on the channel object itself, beside `config`. They decide how requests reach the channel; [Route Resolution](../data-api.md#route-resolution) in the Data API reference specifies how a request path resolves to a channel.

## Synopsis

```json
{{#include ../../../../examples/packages/webhook-transform/channel.json}}
```

## Fields

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `channel_type` | string | yes | — | `sync` (the caller waits for the result) or `async` (queued; answers `202` with a trace id). Case-insensitive. A `cron` channel must be `async`. |
| `protocol` | string | yes | — | `rest`, `http`, `kafka`, or `cron`. Case-insensitive. Immutable across versions. |
| `methods` | array of strings | `rest`, `http` | — | HTTP methods the route answers. Valid values: `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS`. An unknown or duplicated method is refused. |
| `route_pattern` | string | `rest`, `http` | — | Path pattern, for example `/orders/{id}`. Grammar below. At most 255 characters. |
| `topic` | string | `kafka` | — | Kafka topic the channel consumes. At most 255 characters. Refused on a `cron` channel. |
| `consumer_group` | string | no | — | Kafka consumer group name. At most 255 characters. |
| `priority` | number | no | `0` | Route-match precedence. Routes match by priority descending, then segment count descending, then channel name — deterministic on every node. |

`rest` and `http` route identically: both must declare `methods` and `route_pattern`, both register in the route table, and both stay reachable by name at `/api/v1/data/{name}`. An async channel's pattern serves at `/{pattern}/async`, whatever its `channel_type`. A `kafka` channel registers its `topic` as a consumer at startup and on engine reload. Config-file topic mappings take precedence over channel-declared ones; see [Kafka settings](../configuration/kafka.md).

A `cron` channel declares none of those four fields, and each is refused. It registers no HTTP route and no Kafka subscription, and it is **not** reachable by name at `/api/v1/data/{name}` either. Its schedule is the only thing that starts it. See [Cron transport](./cron.md).

**`route_pattern` grammar.** The pattern must start with `/`. It must not contain whitespace, `?`, `#`, or `%`. No segment may be empty (no `//`, no trailing `/`). A parameter is a whole segment written `{name}`; the name must match `[A-Za-z_][A-Za-z0-9_]*` and be unique within the pattern. Captured parameters reach the workflow as `metadata.params`. See the [Workflow Schema](../workflows.md).

A channel names its workflow with a top-level `workflow_id`; how conditions and rollout percentages select a workflow version is specified in the [Workflow Schema](../workflows.md). Activation requires that workflow to be active.

Guard keys go in a `config` object beside these fields:

```json
{
  "name": "orders",
  "channel_type": "sync",
  "protocol": "rest",
  "methods": ["POST"],
  "route_pattern": "/orders",
  "workflow_id": "order-processing",
  "config": { "timeout_ms": 5000 }
}
```

## Related

- [Channels](../../concepts/channels.md): what a channel is.
- [Configure a channel](../../guides/author/channels.md): declaring a route in practice.
- [Data API › Route resolution](../data-api.md#route-resolution): how a request path resolves to a channel.
- [Kafka settings](../configuration/kafka.md): the config-file topic mappings that merge with a channel's topic.
- [Channel configuration](./index.md): every key, with its page.
