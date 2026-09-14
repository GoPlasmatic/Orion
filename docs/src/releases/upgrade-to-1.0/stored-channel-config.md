<!-- description: A channel whose stored config no longer parses is refused at every ingress in Orion 1.0 rather than served with a guard missing. -->
<!-- type: migration -->
<!-- last_verified: 2026-09-14 -->

# Stored channel config

Break 3 of eleven in the 0.3.0 → 1.0.0 upgrade.

## Before you start

Read [Upgrade to 1.0.0](./index.md) first: it carries the checklist, the backup step and the `preflight` scan.

**What changed.** A channel's stored `config_json` could fail to parse, or its `validation_logic` fail to compile. Orion used to log a warning and serve the channel anyway, **with that channel's validation, dedup, rate limit, cache and backpressure guards silently disabled**. A channel whose `validation_logic` was broken was, in effect, an unvalidated channel. That is now a refusal, in both single-node and cluster mode.

**How you'll notice.** Quietly, and only on the channel that is broken. The
refusal is scoped to the offending row — the server boots, binds, and serves everything else:

- **At startup, the process boots normally.** Each broken channel is
  *quarantined*: absent from the serving map and from the route table, while
  every healthy channel loads. One bad row no longer takes the server down.
- **The quarantined channel answers `503` at every ingress**, with
  `Channel '<name>' failed to load and is not being served: <reason>`. Because
  it is also absent from the route table, a REST channel's path stops matching
  and falls through to the usual not-found handling.
- **Reload and admin mutations still succeed.** `POST /api/v1/admin/engine/reload`
  and the mutations that trigger a reload — activate, archive, delete, rollout —
  complete normally; the rest of the rebuild is unaffected. In cluster mode the
  epoch watcher keeps advancing, so nodes continue to pick up config changes.
- **`GET /health` reports it.** The endpoint returns HTTP `200` with
  `"status": "degraded"` and `"components": {"channels": "degraded"}`. The
  offending names are listed under `channels.quarantined` — detail that is only
  rendered for a development instance or an authenticated admin caller, so an
  unauthenticated prober learns *that* something is degraded, not *what*.

This is the change worth internalising: the failure is no longer loud. Nothing crashes, no admin call errors, and a channel stops answering. Watch `/health`'s `components.channels`, and alert on it.

Log lines to grep for — one per channel, with the reason:

```
Channel quarantined: it will be refused at every ingress until fixed
Refusing to load channel: config_json does not parse
Refusing to load channel: validation_logic does not compile
Refusing to load channel: rate_limit.key_logic does not compile
Refusing to load channel: auth config cannot be compiled
```

**What to do — before you upgrade.** Only rows with `status = 'active'` are
parsed, and only those surviving your `channel_filter.include` / `exclude` patterns. Start by listing exactly what the server will try to load:

```sql
SELECT channel_id, version, name, config_json
FROM channels
WHERE status = 'active'
ORDER BY name;
```

On PostgreSQL you can sweep for the type mismatches that actually fail. These are fields with a required type and no default:

```sql
SELECT channel_id, version, name, config_json
FROM channels
WHERE status = 'active'
  AND pg_input_is_valid(config_json, 'jsonb')          -- PG16+; else cast and catch
  AND (
       jsonb_typeof(config_json::jsonb #> '{rate_limit,requests_per_second}') NOT IN ('number','null')
    OR jsonb_typeof(config_json::jsonb #> '{cache,ttl_secs}')                 NOT IN ('number','null')
    OR jsonb_typeof(config_json::jsonb #> '{timeout_ms}')                     NOT IN ('number','null')
    OR jsonb_typeof(config_json::jsonb #> '{backpressure,max_concurrent_per_node}') NOT IN ('number','null')
    -- required whenever the parent object is present:
    OR (config_json::jsonb ? 'backpressure'  AND NOT (config_json::jsonb->'backpressure')  ? 'max_concurrent_per_node')
    OR (config_json::jsonb ? 'cache'         AND NOT (config_json::jsonb->'cache')         ? 'enabled')
    OR (config_json::jsonb ? 'deduplication' AND NOT (config_json::jsonb->'deduplication') ? 'header')
    OR (config_json::jsonb ? 'rate_limit'    AND NOT (config_json::jsonb->'rate_limit')    ? 'requests_per_second')
  );
```

On SQLite, `json_valid(config_json) = 0` catches outright malformed JSON:

```sql
SELECT channel_id, version, name FROM channels
WHERE status = 'active' AND json_valid(config_json) = 0;
```

**SQL cannot predict the `validation_logic` case**, which requires compiling the JSONLogic expression. `orion-server preflight` runs the real parser over every stored row and names each offending channel in one report. That is what the queries above approximate. Running the 1.0.0 binary against a restored snapshot or a read replica and confirming it boots exercises the same code path end to end.

> **Unknown fields fail too.** `ChannelConfig` is `deny_unknown_fields` as of
> 1.0, so a stray key in `config_json` — a typo, the removed
> [`backpressure.queue_depth`](./renames-and-removals.md#backpressurequeue_depth-was-removed), or either
> of the two renamed keys — fails the same way a wrong type does. See [Unknown
> keys in a channel config are now
> refused](./api-and-response-shape.md#unknown-keys-in-a-channel-config-are-now-refused). The type sweeps
> above therefore under-report; `preflight` does not.

---

## Related

- [Upgrade to 1.0.0](./index.md): the checklist, and every other break.
- [Upgrades](../../operate/maintain/upgrades.md): the version-independent procedure.
- [`orion-server preflight`](../../reference/cli/orion-server/preflight.md): the scan that finds the stored ones.
- [Releases](./index.md): what changed in each version.
