<!-- description: The [kafka] settings: brokers, group id, topic mappings, timeouts, the dead-letter topic, SASL and TLS broker authentication and raw librdkafka properties. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Kafka settings

Consumer and producer are compiled into every binary and gated at runtime by `kafka.enabled`.

## Synopsis

```toml
[kafka]
enabled = false
brokers = ["localhost:9092"]
group_id = "orion"
topics = []
processing_timeout_ms = 60000
lag_poll_interval_secs = 30
session_timeout_ms = 45000
# extra_config = …   # no default

[kafka.dlq]
enabled = false
topic = "orion-dlq"

[kafka.auth]
# security_protocol = …   # no default
# sasl_mechanism = …   # no default
# sasl_username = …   # no default
# sasl_password = …   # no default
# ssl_ca_location = …   # no default
```

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `kafka.enabled` | `false` | `ORION_KAFKA__ENABLED` | Enable to consume from Kafka topics. |
| `kafka.brokers` | `["localhost:9092"]` | `ORION_KAFKA__BROKERS` | Comma-separated in the env var. Each entry must be `host:port`. |
| `kafka.group_id` | `"orion"` | `ORION_KAFKA__GROUP_ID` | Give each deployment its own group so they do not share offsets. |
| `kafka.topics` | `[]` | `ORION_KAFKA__TOPICS` | Topic-to-channel mappings; see [Topic mappings](#topic-mappings). |
| `kafka.processing_timeout_ms` | `60000` | `ORION_KAFKA__PROCESSING_TIMEOUT_MS` | Per-message deadline. |
| `kafka.lag_poll_interval_secs` | `30` | `ORION_KAFKA__LAG_POLL_INTERVAL_SECS` | `0` disables consumer-lag metrics. |
| `kafka.session_timeout_ms` | `45000` | `ORION_KAFKA__SESSION_TIMEOUT_MS` | Consumer group session timeout; applied whether or not cluster mode is on. In cluster mode it pairs with static group membership (`group.instance.id`) so rolling restarts rejoin without a full rebalance. |

Messages are processed **strictly sequentially** per consumer. The at-least-once delivery contract requires it, because committing an offset implicitly commits every earlier offset on that partition. Scale throughput by running more instances in the same consumer group. (The pre-1.0 `kafka.max_inflight` setting is gone; it advertised concurrency that never existed.)

### Topic mappings

Topic mappings are TOML array-of-tables. Channels with a Kafka protocol contribute their own topics from the database, and the two sets are merged at startup:

```toml
[[kafka.topics]]
topic = "incoming-orders"
channel = "orders"
```

The env-var form is a comma-separated `topic:channel` list: `ORION_KAFKA__TOPICS="incoming-orders:orders,events:event-handler"`.

### Dead-letter queue

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `kafka.dlq.enabled` | `false` | `ORION_KAFKA__DLQ__ENABLED` | Enable so poison messages stop blocking a partition. |
| `kafka.dlq.topic` | `"orion-dlq"` | `ORION_KAFKA__DLQ__TOPIC` | To match an existing naming convention. |

Delivery is at-least-once: an offset advances only on successful processing or a *confirmed* DLQ write. With the DLQ disabled a failing message is retried in place with capped backoff rather than lost. That is safe, but it means one poison message can stall its partition until you enable this.

### Broker authentication

This is what makes managed brokers reachable — Confluent Cloud, MSK, Aiven. Settings apply to every Kafka client Orion creates: the ingest consumer, the `publish_kafka` producer, and the DLQ producer. Each maps 1:1 onto a librdkafka property, and an unset field leaves librdkafka's default (plaintext, no auth) alone.

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `kafka.auth.security_protocol` | — | `ORION_KAFKA__AUTH__SECURITY_PROTOCOL` | `plaintext`, `ssl`, `sasl_plaintext`, or `sasl_ssl`. Any managed broker needs `sasl_ssl`. |
| `kafka.auth.sasl_mechanism` | — | `ORION_KAFKA__AUTH__SASL_MECHANISM` | `PLAIN`, `SCRAM-SHA-256`, or `SCRAM-SHA-512`. |
| `kafka.auth.sasl_username` | — | `ORION_KAFKA__AUTH__SASL_USERNAME` | The API key on Confluent Cloud. |
| `kafka.auth.sasl_password` | — | `ORION_KAFKA__AUTH__SASL_PASSWORD` | The API secret. Prefer the env var or a `${VAR}` placeholder over a literal. |
| `kafka.auth.ssl_ca_location` | — | `ORION_KAFKA__AUTH__SSL_CA_LOCATION` | Path to a CA bundle for broker verification; unset uses the system trust store. |

Choosing a `security_protocol` starting with `sasl` requires `sasl_mechanism`, `sasl_username`, and `sasl_password`. Startup fails if any is missing, rather than falling back to an unauthenticated connection. GSSAPI and OAUTHBEARER are not available: librdkafka is built without libsasl2.

Confluent Cloud, end to end:

```toml
[kafka]
enabled = true
brokers = ["pkc-abc12.us-east-1.aws.confluent.cloud:9092"]
group_id = "orion-prod"

[[kafka.topics]]
topic = "orders"
channel = "order-processor"

[kafka.auth]
security_protocol = "sasl_ssl"
sasl_mechanism = "PLAIN"
sasl_username = "${CONFLUENT_API_KEY}"
sasl_password = "${CONFLUENT_API_SECRET}"
```

AWS MSK with IAM authentication is not supported; use MSK's SCRAM credentials with `sasl_mechanism = "SCRAM-SHA-512"`.

### Raw librdkafka properties

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `kafka.extra_config` | — | — | For any librdkafka property Orion has no first-class setting for. |

Applied to every client *after* everything Orion sets, so entries here override anything — including `[kafka.auth]`. Free-form maps do not fit the `ORION_SECTION__KEY` scheme, so there is no environment variable: this is config file only.

```toml
[kafka.extra_config]
"client.id" = "orion-prod-1"
"socket.keepalive.enable" = "true"
```

## Related

- [Kafka channels](../../guides/patterns/kafka-channels.md): consuming a topic end to end.
- [Connector types › Kafka](../connectors/kafka.md): the producer side, as a connector.
- [Channel configuration › Routing and protocol](../channel-config/routing.md): how a channel declares its topic.
- [Server configuration](./index.md): every section, by what you are configuring.
