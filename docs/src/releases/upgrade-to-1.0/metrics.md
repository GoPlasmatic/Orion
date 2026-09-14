<!-- description: Three metric families changed name or labels in Orion 1.0, and /metrics answers 404 when metrics are off: what to change in dashboards and alerts. -->
<!-- type: migration -->
<!-- last_verified: 2026-09-14 -->

# Metrics: dashboards and alerts

Break 2 of eleven in the 0.3.0 → 1.0.0 upgrade.

## Before you start

Read [Upgrade to 1.0.0](./index.md) first: it carries the checklist, the backup step and the `preflight` scan.

**What changed.** Two label changes, both intended to bound Prometheus
cardinality.

**`orion_rate_limit_rejections_total` lost its `client` label and gained `scope`.**
The metric name is unchanged. The old label carried a raw client IP — unbounded cardinality. `scope` takes one of a small, bounded set of values:

| `scope` value | Meaning |
|---------------|---------|
| *(channel name)* | A per-channel limiter defined in that channel's `config_json` |
| `admin` | The platform limiter for `/api/v1/admin*` |
| `data` | The platform limiter for `/api/v1/data*` |
| `operational` | Everything else |

**Channel-labelled metrics now use `_unknown` for unregistered channels.** When a request names a channel the registry does not hold, the `channel` label is the literal string `_unknown`. The leading underscore keeps it out of the caller-supplied namespace. Otherwise anyone could inflate the metric cardinality by POSTing to arbitrary paths. Two metrics are capped:

- `orion_messages_total{channel, status}`
- `orion_message_duration_seconds{channel}`

The cap applies to the HTTP and async-queue paths. Kafka ingest is deliberately exempt — its channel set comes from operator configuration and is already bounded.

**How you'll notice.** Silently. A PromQL selector on a label that no longer
exists returns an empty result rather than an error. A `by (client)` breakdown renders as an empty panel and an alert built on it **stops firing** instead of erroring.

**What to do.** Grep your dashboards and rules for the old label before you
upgrade:

```bash
grep -rn 'rate_limit_rejections_total' \
  grafana/ dashboards/ prometheus/ *.rules.yml
```

Then rewrite the selectors:

```promql
# before
sum by (client) (rate(rate_limit_rejections_total[5m]))
# after
sum by (scope) (rate(rate_limit_rejections_total[5m]))
```

If you are already running Prometheus, you can confirm which series the old label produced before cutting over:

```promql
count by (client) (rate_limit_rejections_total)
```

**Four metrics are new** and worth adding to your dashboards while you are in
there:

| Metric | Type | Why you want it |
|--------|------|-----------------|
| `orion_trace_queue_rejected_total{reason}` | counter | `reason="full"` or `"memory"` — async submissions being shed with a `503`. See [Queue-full](./api-and-response-shape.md#queue-full-now-returns-503-instead-of-hanging) |
| `orion_trace_dlq_depth` | gauge | Backlog of failed traces. **Only refreshed by the DLQ retry loop, so it stops updating when `trace_queue.dlq_retry_enabled = false`** — exactly the setting that lets the backlog grow |
| `orion_trace_dlq_retries_total{outcome}` | counter | `retried` / `exhausted` / `failed` |
| `orion_trace_persistence_failures_total` | counter | The only signal that trace writes are being dropped |

**Three metric families changed name or labels.** Rewrite these selectors
before upgrading.

The failure mode is the same silent one described above. A PromQL selector on a name or label that no longer exists returns an empty result, not an error. A panel renders blank, and an alert built on it **stops firing** rather than erroring.

| Before | After |
|---|---|
| `orion_channel_executions_total{channel}` | *removed* — use `sum by (channel) (orion_messages_total)` |
| `orion_errors_total{type="…"}` | `orion_errors_total{reason="…"}` |
| `kafka_consumer_lag{topic, partition}` | `orion_kafka_consumer_lag_messages{topic, partition}` |

```promql
# before
sum by (channel) (rate(orion_channel_executions_total[5m]))
# after — a superset, not an identity: the removed counter had two call sites,
# both on the HTTP path, and never saw the Kafka ingest or DLQ paths
sum by (channel) (rate(orion_messages_total[5m]))
# ...or, for what it actually counted:
sum by (channel) (rate(orion_messages_total{status="ok"}[5m]))

# before
sum by (type) (rate(orion_errors_total[5m]))
# after — the label *values* are unchanged, only the key moved
sum by (reason) (rate(orion_errors_total[5m]))

# before
max by (topic, partition) (kafka_consumer_lag)
# after
max by (topic, partition) (orion_kafka_consumer_lag_messages)
```

Find them — note `kafka_consumer_lag` is a substring of its own replacement, so check each hit rather than blanket-replacing:

```bash
grep -rn 'channel_executions_total\|errors_total{type\|by (type)\|kafka_consumer_lag' \
  grafana/ dashboards/ prometheus/ *.rules.yml
```

**`/metrics` is no longer registered when `metrics.enabled = false`.** It used
to answer `200` with an empty body rendered from an orphan recorder. A deployment with metrics off looked like a working scrape target that never had any series. It now returns `404` with the standard error envelope. That holds when `admin_auth.enabled = true` too, where an unregistered path falls through to the 404 fallback rather than answering `401`. If a scrape job goes red on upgrade, that is the misconfiguration becoming visible. Set `metrics.enabled = true`, or point the job at the new `metrics.bind_addr` listener described below.

**Two new audit metrics** are worth adding while you are here: `orion_audit_events_dropped_total{reason}` and `orion_audit_queue_depth`. Alert on the first existing at all, not on a threshold: any non-zero value is a hole in the audit trail. See [Audit log](./security-and-access.md#audit-log-new-actor-format-new-fields-two-new-settings).

### Give Prometheus its own listener (optional, recommended)

The `/metrics` endpoint is guarded by `admin_auth` along with the rest of the admin plane. Until now every scraper had to hold an admin API key, a credential that can also rewrite workflows and read trace payloads. The setting `metrics.bind_addr` moves the endpoint onto its own listener and removes it from the main one:

```toml
[metrics]
enabled = true
bind_addr = "127.0.0.1:9090"    # or a pod IP, or a private Compose network
```

That listener is plain HTTP (`server.tls` governs the main listener only) and has **no authentication**. The address is the access control, so bind it somewhere only your scrapers can reach. Startup logs a warning if it is not a loopback address, refuses to start if it overlaps `server.host`/`server.port`, and binds it before the main server. A clash or a permission problem is a startup failure rather than a silently missing scrape target. It requires `metrics.enabled = true`; set alone it warns and raises no listener.

It is a move, not a copy: once `bind_addr` is set the main listener returns `404` for `/metrics`. Update the scrape config in the same change. The listener joins the same graceful-shutdown path as the main one, so the last scrape of a node being drained still succeeds.

---

## Related

- [Upgrade to 1.0.0](./index.md): the checklist, and every other break.
- [Upgrades](../../operate/maintain/upgrades.md): the version-independent procedure.
- [`orion-server preflight`](../../reference/cli/orion-server/preflight.md): the scan that finds the stored ones.
- [Releases](./index.md): what changed in each version.
