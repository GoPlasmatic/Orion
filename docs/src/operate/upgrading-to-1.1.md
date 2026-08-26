<!-- description: Upgrading Orion 1.0.x to 1.1.0 — only what changes behaviour, with the new JWT auth, SMTP and storage connectors and managed OAuth2 covered elsewhere. -->
# Upgrading to 1.1.0

This page is for operators upgrading an existing Orion deployment from
**1.0.x** to **1.1.0**. It covers only what *changes behaviour*. The new
capabilities — JWT channel auth, the SMTP and storage connectors, managed
OAuth2, the MongoDB write surface, the `crypto` and `random` primitives — are
in the
[CHANGELOG](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/CHANGELOG.md),
and their configuration keys are in the
[Config Reference](../reference/configuration.md).

**1.1.0 is a minor release and behaves like one.** No config key was renamed,
no API path moved, no metric was renamed, and every stored workflow, channel
and connector keeps validating. Nothing here requires a rewrite. Five changes
can reach you, and **one of them can start refusing live traffic on a channel
that appeared to be working** — that is the row to read first.

The version-independent procedure — back up, preflight, validate config,
migrate, roll — is on [Upgrades](./upgrades.md).

---

## Before you start

| # | Check | Applies to you if |
|---|-------|-------------------|
| 1 | [Fix any `rate_limit.key_logic` that reads an undeclared header](#1-a-rate-limit-key-that-resolves-to-nothing-is-now-refused) | Any channel sets `rate_limit.key_logic` — **this one turns a silently-unenforced limit into a 429 on every request** |
| 2 | [Re-point alerts and workflow logic that match `TASK_ERROR`](#2-errorscode-names-the-real-failure-instead-of-task_error) | You alert on, or branch on, `errors[].code` from a `continue_on_error` channel |
| 3 | [Nothing — but know your CORS surface widened](#3-the-default-cors-config-now-allows-authorization-on-a-preflight) | You run the shipped default `allowed_origins = ["*"]` |
| 4 | [Stop reading header values back from a connector](#4-connector-reads-mask-every-header-value) | You read connector config through the admin API or CLI and expect header values in the clear |
| 5 | [Re-point anything parsing connector URLs or upstream bodies out of error text](#5-error-messages-no-longer-quote-connector-urls-or-upstream-bodies) | You scrape error messages or trace rows for the failing endpoint or the upstream response |

**`orion-server preflight` does not cover this release.** Its checks are the
0.3 → 1.0 breaks; it has no 1.1 rules, so a clean preflight run says nothing
about the rows above. Run it anyway if you are also crossing 1.0 — it is
read-only and cheap — but use the detection command in each section below for
1.1. Row 1's detection is the one worth doing *before* the rollout.

---

## 1. A rate-limit key that resolves to nothing is now refused

**What changed.** `rate_limit.key_logic` could only read eight hard-coded
request headers, and referencing any other header was not an error. A missing
path resolves to `null` in datalogic, and the guard serialized that into the
key — so the bucket became the literal string `"null"` for **every** caller on
the channel. An intended per-device or per-partner quota silently became one
shared channel-wide bucket, with no log, no warning and no metric. A single
typo — `deviceid` for `device-id` — was enough.

A key that resolves to `null` or an empty string is now refused with
`429 RATE_LIMITED`, exactly as a key that fails to evaluate already was: a
request whose key cannot be computed is rejected rather than counted in the
wrong bucket, which is what
[the channel reference](../reference/channel-config.md) has always said
happens.

**How you'll notice.** A channel with a typo'd header name previously appeared
to work and will now refuse every request with `429`. Orion warns at channel
load, naming the channel and the headers:

```text
rate_limit.key_logic reads headers that are not in the key context;
every request will be refused 429 until they are added to rate_limit.key_headers
```

The new `orion_rate_limit_key_unavailable_total{channel}` counter separates
this from ordinary over-limit rejections.

**What to do.** Find the affected channels *before* you roll:

```bash
orion-cli channels export --output json \
  | jq -r '(if type == "array" then . else .data end)[]
           | select(.config.rate_limit.key_logic != null)
           | "\(.name)\t\(.config.rate_limit.key_logic | tostring)"'
```

Any header named there that is not one of the eight always-present names —
`authorization`, `x-api-key`, `x-forwarded-for`, `x-real-ip`, `user-agent`,
`content-type`, `origin`, `x-tenant-id` — must now be declared:

```json
{ "rate_limit": {
    "requests_per_second": 100,
    "key_headers": ["device-id"],
    "key_logic": { "var": "headers.device-id" } } }
```

`key_headers` is **merged with** the built-in eight rather than replacing
them, so no existing `key_logic` changes meaning, and names match
case-insensitively. Editing it rebuilds the limiter rather than carrying
per-key state across a re-dimensioning.

That configuration was never enforcing the limit it declared — it was
admitting unbounded traffic against a control that read as active — so the
refusal surfaces a defect rather than creating one. If a channel turns out not
to need a keyed limit at all, remove `key_logic` and the limit applies
per-caller-identity as it does everywhere else.

> **Also authoring-time now:** `rate_limit.requests_per_second: 0` is refused
> at create and update. It used to be accepted and floored to `1` by the
> limiter, so asking for "admit nothing" quietly got one request per second.
> Stored channels are unaffected until something rewrites them, so this bites
> on your next edit, not on the upgrade.

---

## 2. `errors[].code` names the real failure instead of `TASK_ERROR`

**What changed.** A failed task used to report a flat `TASK_ERROR` in the
response envelope's `errors[]`. It now reports the code that describes the
failure. This is **wire-visible**: a client or alert matching
`errors[].code == "TASK_ERROR"` stops matching.

- A connection that could not be established — `IO_ERROR`
- A slow one — `TIMEOUT_ERROR`
- A request rejected before any socket opened (SSRF protection, a closed
  operation gate) — `FUNCTION_ERROR`
- A request shed by an open circuit breaker — the connector's own service
  kind, `circuit_open`, lower-case and verbatim
- `TASK_ERROR` remains the fallback for an engine-owned error with no more
  specific classification, so it does not disappear from the vocabulary

**How you'll notice.** Only where the failure reaches `errors[]` at all — that
is, on a channel with `continue_on_error: true`. With the default
`continue_on_error: false` the request still fails with the top-level error
envelope and its own code, which is unchanged. An alert counting
`TASK_ERROR` will quietly go to zero rather than firing.

**What to do.** Match on the specific code, or on the set, rather than on
`TASK_ERROR` alone:

```json
{ "condition": { "in": [ { "var": "metadata._orion_errors.0.code" },
                         ["TIMEOUT_ERROR", "IO_ERROR", "circuit_open"] ] } }
```

The same codes now also reach workflow logic through the new
`metadata._orion_errors`, so a workflow can branch on *why* a step failed
rather than only that it did. The full vocabulary is in
[Workflows → Branching on a failure](../reference/workflows.md#branching-on-a-failure).

> **If you alert on circuit breakers**, note that a shed request still returns
> HTTP `200` under `continue_on_error: true`, so the status code remains the
> wrong signal — alert on
> `orion_circuit_breaker_rejections_total{connector, channel}`. What changed is
> that the `errors[]` entry is now distinguishable rather than an anonymous
> `TASK_ERROR`.

---

## 3. The default CORS config now allows `Authorization` on a preflight

**What changed.** `allowed_origins = ["*"]` — the shipped default — took a
permissive branch that emitted a literal `Access-Control-Allow-Headers: *`.
Per the Fetch Standard, `Authorization` is a *CORS non-wildcard request-header
name*: `*` never covers it, and it must be listed explicitly. So on a default
install a browser calling the admin API with a bearer token failed preflight,
while a named-origin config worked, because that branch listed
`AUTHORIZATION` by name.

Orion now sends explicit allow-headers and expose-headers lists on **both**
branches, never `*`.

**How you'll notice.** You probably won't, except that a browser call that
used to fail preflight now succeeds. This is a **strictly widening** change on
the default config: it authorizes everything `*` did, plus the header `*`
silently withheld.

**What to do.** Nothing is required. If a wide-open default was not what you
wanted, 1.1.0 also adds `additional_allowed_headers`, `expose_headers`,
`allow_credentials` and `max_age_secs` under `[cors]` — see the
[Config Reference](../reference/configuration.md). `max_age_secs` is capped at
`86400`, because browsers clamp it anyway.

---

## 4. Connector reads mask every header value

**What changed.** The masking allowlist was flat and keyed by leaf name, so a
header whose name collided with a structural key was served **readable** —
`headers: {"from": "x"}` among them, despite header values being documented as
all masked. `username`, `method`, `host`, `port`, `url`, `type`, `region`,
`bucket`, `topic`, `resource` and `audience` would all have collided the same
way.

Every descendant of `headers`, `query_params` and `extra_params` now masks by
container rather than by name.

**How you'll notice.** A connector read through the admin API or
`orion-cli connectors get` that used to show one of those header values now
shows the mask.

**What to do.** Nothing, unless you had tooling reading a header value back out
of a connector — which was never the supported path. **Export and import are
unaffected**: `env://` and `vault://` references still survive a read, so
promotion round-trips exactly as before.

---

## 5. Error messages no longer quote connector URLs or upstream bodies

**What changed.** Every error `http_call` minted named the endpoint it failed
to reach, and the non-2xx arm additionally copied up to `max_response_size` —
10 MB by default — of the *upstream* response body into the message, none of
it passing through the redaction helper. The same applied to `storage_head`
and the Elasticsearch send path.

URLs are now redacted, and an upstream error body becomes a 512-byte preview
marked `… (truncated)`.

**How you'll notice.** Error text and trace rows carry less. A 10 MB upstream
error body no longer becomes a 10 MB trace row, which is the other half of the
change.

**What to do.** Re-point anything that scraped the failing endpoint or the full
upstream response out of error text; read the trace's task record instead.

> Redaction is name-keyed, so `?pwd=` masks and `?pass=` does not — it is a
> backstop, not the control. Keep credentials out of connector URLs entirely,
> using `auth` or the new `query_params`.

---

## The migration

One additive migration, on all three backends: a `connector_oauth_state` table
holding the access token, expiry and refresh token for connectors using the new
managed [`oauth2` auth](../reference/connectors.md), encrypted at rest exactly
like `config_json`. Nothing existing is altered, and the `connectors` table is
never mutated.

It applies at boot in a single-node deployment, or as `orion-server migrate` in
a cluster, per the [standard procedure](./upgrades.md#the-order-that-works).
Deployments not using `oauth2` simply carry an empty table.

## Related

- [Upgrades](./upgrades.md) — the version-independent procedure
- [Upgrading to 1.0.0](./upgrading-to-1.0.md) — if you are crossing 1.0 as well
- [Support & Compatibility](../reference/support.md) — what a version number
  promises, and the supported-version window
- [CHANGELOG](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/CHANGELOG.md)
  — what was added, as opposed to what changed behaviour
