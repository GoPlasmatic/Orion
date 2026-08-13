# kafka-order-events

An **async Kafka channel**: the same declarative workflow shape as the REST
examples, consuming from a topic instead of answering a request.
`order-events` reads `orders.events` in consumer group `orion-examples` and
routes each record by its event type.

> [!IMPORTANT]
> **This example is not zero-dependency.** Unlike every other package here, it
> needs a running Kafka broker *and* a server started with Kafka enabled:
>
> ```toml
> [kafka]
> enabled = true
> brokers = "localhost:9092"
> ```
>
> With `kafka.enabled = false` (the default) the channel activates but nothing
> consumes it, and there is no HTTP route to call — a Kafka channel has no
> `route_pattern`, so `deploy.sh` deploys it and stops without sending a
> request. That is expected, not a failure.

From the repository root, with a Kafka-enabled server on
`http://localhost:8080`:

```bash
./examples/deploy.sh kafka-order-events
```

That creates and activates the workflow and channel. To exercise it, produce a
record to the topic rather than sending an HTTP request:

```bash
echo '{"event_type":"order.placed","order_id":"ORD-9182","total":25000}' \
  | kafka-console-producer.sh --broker-list localhost:9092 --topic orders.events
```

Then read the execution back through the trace API, the way you would for any
async channel:

```bash
orion-cli traces list --channel order-events
```

See [Consume from Kafka](https://docs.goplasmatic.io/guides/kafka-channels.html)
for delivery semantics, the DLQ, and how the channel's guards apply to records;
[`examples/README.md`](../../README.md) for the file layout and the full example
list.
