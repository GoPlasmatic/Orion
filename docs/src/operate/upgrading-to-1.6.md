<!-- description: Upgrading Orion 1.5.x to 1.6.0 — plugins off by default, the cron scheduler on, nine migrations, a stricter db_read and validate-config, and a new MSRV rule. -->
# Upgrading to 1.6.0

This page is for operators upgrading an existing Orion deployment from
**1.5.x** to **1.6.0**. It covers only what *changes behaviour*. The two new
capabilities — [plugins](../concepts/plugins.md), custom task functions in a
WebAssembly sandbox, and [cron channels](../concepts/channels.md), a workflow
that runs on a schedule instead of on a request — are described in full in the
[CHANGELOG](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/CHANGELOG.md).

**1.6.0 is a minor release and behaves like one.** No config key was renamed
or removed, no API path moved, and no metric was renamed. Nine things can
reach you. Four of them can change what an existing deployment already does —
§4, §5, §6 and §7 — and the rest are additive.

The version-independent procedure — back up, preflight, validate config,
migrate, roll — is on [Upgrades](./upgrades.md).

---

## Before you start

| # | Check | Applies to you if |
|---|-------|-------------------|
| 1 | [Run the migrations as a deploy step](#1-nine-migrations-expand-only) | You run a cluster with `auto_migrate = false` — everyone else gets them at startup |
| 2 | [Nothing — plugins are off](#2-plugins-are-off-by-default) | Every deployment; read it to know what turning them on will mean |
| 3 | [Decide whether this node runs schedules](#3-cron-is-on-by-default-and-that-is-deliberate) | Every deployment, and especially a cluster: every node must agree |
| 4 | [Check your `db_read` statements are reads](#4-db_read-refuses-a-statement-that-is-not-a-read) | You use `db_read` — a statement that writes now fails |
| 5 | [Re-run `validate-config`](#5-validate-config-checks-the-storage-url-scheme-everywhere) | You run single-node — a check that was skipped now runs |
| 6 | [Move trace reads to the header](#6-the-token-query-parameter-is-deprecated) | Anything reads a trace with `?token=` |
| 7 | [Re-check numeric columns](#7-sql-connectors-decode-on-the-real-driver) | You read PostgreSQL `numeric`, binary or previously-failing column types |
| 8 | [Allow for a ~6 MB larger binary and image](#8-the-binary-carries-wasmtime) | You pin image sizes, or build from source on a constrained host |
| 9 | [Note the MSRV policy](#9-the-msrv-now-tracks-wasmtimes) | You build from source |

`orion-server preflight` gained two things for this release. It now reads the
active plugins, so a stored workflow calling a plugin function is checked
against the plugin's manifest rather than reported as naming an unknown
function; and its report now has two sections, of which only the first gates
the exit code — the engine's *advisories* are reported with their own `check`
id so a pipeline can grandfather one without silencing the rest. A clean run
on 1.5.x is a clean run on 1.6.0.

---

## 1. Nine migrations, expand-only

**What changed.** Three new migrations per backend: `plugins` (two new tables,
`plugins` and `plugin_artifacts`), `plugin_signatures` (one nullable column on
`plugins`), and `cron_scheduling` (three new tables — `cron_schedule_state`,
`cron_occurrences` and `cron_singletons`). They add schema and touch nothing
that exists, so a 1.5.x binary keeps working against a migrated database and a
rollback needs no schema work.

**What to do.** Nothing, unless `storage.auto_migrate = false`: then
`orion-server migrate` is the deploy step, as it always is in
[cluster mode](./cluster.md). On MySQL, the artifact table's `bytes` column is
a `LONGBLOB`; if you later enable plugins, make sure the server's
`max_allowed_packet` exceeds `plugins.max_component_bytes` (16 MiB by
default) or an upload will fail at write.

```bash
orion-server migrate --dry-run -c config.toml   # names all nine by backend
```

The plugin and cron tables stay empty until a plugin or a cron channel is
activated.

## 2. Plugins are off by default

**What changed.** The plugin sandbox exists in every binary, and
`plugins.enabled` defaults to `false`. With it off, no Wasmtime engine is
constructed, no epoch ticker runs, `POST /api/v1/admin/plugins` answers `400`,
and a plugin row that reaches this node's database — through a cluster
peer's activation, or an import — becomes a `disabled` load issue that
quarantines the workflows naming its functions, never an abort.

**What to do.** Nothing. When you turn it on, read the
[production checklist row](./production-checklist.md): the pooling allocator
reserves `max_live_instances × max_memory_bytes` of virtual address space at
startup (16 GiB by default), and `[plugins.trust]` is where signing keys go.

## 3. `[cron]` is on by default, and that is deliberate

**What changed.** The new `[cron]` section defaults to `enabled = true` with a
one-second poll. On an instance with no cron channels that costs one indexed
query per second and nothing else — the reconciler short-circuits when there
is nothing scheduled.

Turning it off is a real choice with a visible consequence:

```toml
[cron]
enabled = false
```

An **active** cron channel on a node with the scheduler off is
**quarantined**: refused at load, listed under `channels.quarantined` on
`/health`, and reported as `components.cron: degraded`. Activating one is
refused outright. That is on purpose — a stored, active schedule that silently
never fires is the one failure an operator has no way to notice.

Drafts, imports, exports and reads are unaffected, so an instance with the
scheduler off is still a place to author and promote schedules.

**What to do.** Nothing to run. Decide whether this deployment should run
schedules, and make every node in a cluster agree — a mixed cluster
quarantines the channel on the nodes that have it off and runs it on the rest,
which works, but is not what anyone meant. See
[Scheduled Channels](../reference/configuration.md#scheduled-channels-cron)
for every setting.

Terminal occurrences age out with traces, on the
`trace_queue.retention_hours` schedule — one retention decision, not two, so
you never read an occurrence whose trace is gone. Occurrences still `pending`
are never deleted, however old: that is a backlog, not history.

## 4. `db_read` refuses a statement that is not a read

**What changed.** `db_read` gated on the connector's `read` operation and then
handed the statement to the driver, which executes whatever it is given.
`DELETE FROM audit_log RETURNING id` therefore ran on PostgreSQL and SQLite,
and a bare `DELETE` / `UPDATE` / `INSERT` ran on all three — with
`raw_write: false` and `delete: false` set on the connector. The gates
advertised more than they enforced.

A `db_read` statement must now open with `SELECT`, `WITH`, `VALUES` or
`TABLE`. `EXPLAIN` is not admitted (`EXPLAIN ANALYZE DELETE …` executes the
delete) and neither is `PRAGMA`, which writes on SQLite. A `WITH` carrying a
data-modifying CTE is refused by shape. Comments and quoted strings are
stripped before the check, so `WHERE note = 'delete me'` is an ordinary read
and `SELECT … FOR UPDATE` is an ordinary locking read.

**What to do.** Grep your stored workflows for `db_read` tasks whose
`statement` is not a read, and move them to `db_write` (or `data_write`) —
they will now fail at first traffic instead of quietly writing.

```bash
orion-cli workflows export --status active \
  | jq -r '.. | objects | select(.name? == "db_read") | .input.statement'
```

The served field description was wrong on the same line and is now fixed: it
gave PostgreSQL's `$1, $2` as the placeholder spelling for all three backends,
where SQLite and MySQL use `?`. If your tooling reads
`GET /api/v1/admin/functions`, it was being told the wrong thing.

## 5. `validate-config` checks the storage URL scheme everywhere

**What changed.** The check that `storage.url` carries a supported scheme sat
inside a condition that also required cluster mode, so `&&` short-circuited it
away for every single-node deployment. `validate-config` exited `0` on a URL
the server then died at boot for.

The check now belongs to `[storage]` and runs unconditionally.

**What to do.** Run `orion-server validate-config -c config.toml` before
rolling. A config that validated on 1.5.x and can never boot will now say so
at the point you can still fix it.

Related, and additive: config values may now be `env://NAME` references, so
`[storage] url = "env://ORION_STATE_DB_URL"` resolves at load and keeps its
type. `${VAR}` text substitution still works as before. An unset variable is a
hard error naming both the variable and the field.

## 6. The `?token=` query parameter is deprecated

**What changed.** A trace read returns the submission's full result, and the
capability that authorises it can travel either in the `x-trace-token` header
or in the URL. The query parameter leaks: it reaches browser history, reverse
proxy and CDN access logs, analytics, and the `Referer` of whatever the page
loads next. The header reaches none of them.

Removing it would break a documented surface on a 1.x server, so it is
deprecated rather than removed: still accepted, answered with
`Deprecation: true`, and counted by `orion_trace_token_query_reads_total`.
Trace responses on both lanes now also answer `Cache-Control: no-store` —
nothing previously stopped a shared cache from storing the body.

**What to do.** Move callers to the header, and alert on the counter reaching
zero — that is what makes the eventual removal safe to schedule.

```bash
curl -H "x-trace-token: $TOKEN" "$ORION/api/v1/data/traces/$TRACE_ID"
```

## 7. SQL connectors decode on the real driver

**What changed.** Connector queries ran on `sqlx::AnyPool`, whose type layer
has nine variants and errors on anything it cannot spell. Ten PostgreSQL types
— `uuid`, `numeric`, `timestamptz`, `timestamp`, `date`, `json`, `jsonb`,
arrays, enums and `inet` — failed the task with a 500 before Orion's decoder
ran, and the failure was per row: a query passed every test against an empty
table and failed the first time production had data. `db_read`, `data_query`
and `data_write`'s `returning` share the decoder and all three inherited it.
Four more arms (including MySQL's `BOOLEAN`/`TINYINT(1)`) were keyed on type
names sqlx does not agree with, and answered 400 on every row.

Connector pools now dispatch to the concrete PostgreSQL, MySQL or SQLite
driver, the way Orion's own database has since 1.0.

**What to do.** Two representation choices are worth checking if you read
these columns:

- **`numeric`** now decodes to a JSON number by default. Arbitrary precision
  has no JSON equivalent and JSONLogic computes in `f64`, so set
  `numeric_as: "string"` on a money column to keep every digit.
- **Binary columns** (`bytea`, `blob`, `varbinary`) had their *shape* decided
  by the value: text when the bytes happened to be valid UTF-8, lowercase hex
  when they were not, with nothing distinguishing the two. `binary_as` now
  names the shape explicitly. A workflow that hex-decodes such a column should
  declare it.

## 8. The binary carries Wasmtime

**What changed.** Wasmtime and Cranelift are compiled into every target,
adding roughly 6 MB to the release binary and to the container images. They
are inert until `plugins.enabled = true`.

**What to do.** Nothing, unless a size budget is pinned somewhere. `cargo
deny check` now allows `Apache-2.0 WITH LLVM-exception`, which Wasmtime and
Cranelift carry.

## 9. The MSRV now tracks Wasmtime's

**What changed.** The minimum supported Rust version is still **1.98**, but
the rule behind it changed: it now also follows
[Wasmtime's policy](https://docs.wasmtime.dev/stability-release.html) (stable
minus two), so a Wasmtime upgrade in a future minor may move it.
[Support & Compatibility](../reference/support.md#rust-toolchain-msrv) states
the policy.

**What to do.** Nothing for the released binaries and images. Building from
source, keep the toolchain at the `rust-version` the checked-out release
declares.

---

## New surface, all additive

Nothing here is removed and no existing field changes meaning.

| Added | Notes |
|---|---|
| `protocol: "cron"` | A fourth `ChannelProtocol`. Older clients that pattern-match on the enum should already tolerate unknown values; `orion-api` deserialization is tolerant by design |
| `mode: "cron"` on traces | An open string, like `kafka`. Filter with `?mode=cron` |
| `GET/POST /api/v1/admin/cron/...` | The occurrence ledger. See [Cron occurrences](../reference/admin-api.md#cron-occurrences) |
| `POST /api/v1/admin/channels/{id}/trigger` | A manual run of a cron channel, through the same claim and singleton |
| `/api/v1/admin/plugins` | The plugin entity: upload, validate, activate, archive, export |
| `orion-cli cron`, `orion-cli channels trigger`, `orion-cli plugins` | The CLI side of all three |
| `components.cron` on `/health` | Present only when the node has something to say about schedules |
| Seven `orion_cron_*` metrics | [Scheduling](../reference/metrics.md#scheduling) |
| `"count": true` on `data_query` | A total for a paginated endpoint, on all five backends |
| `last_insert_id` from `db_write` | On MySQL and SQLite, for an `INSERT`/`REPLACE`, as `data_write` already reported |
| `halt_on` on a task | Ends the workflow when that task failed — the outcome axis to `terminal`'s position axis |
| `config.oauth2_login` on a channel | Inbound OAuth2 / OIDC sign-in: the redirect, the state cookie, PKCE and the code exchange, with the grant at `metadata.oauth` |
| `retry_safety` on `GET /api/v1/admin/functions` | Whether a task function is safe to run twice |

## If you deploy in a cluster

Every node runs its own cron reconciler and workers. Coordination is entirely
through the three cron tables: occurrence identity is
`(channel_id, scheduled_for)`, claims are leased with the database clock, and
a `forbid` singleton is a row that one occurrence holds at a time. There is no
leader to elect and nothing new to configure — but every node must agree on
`cron.enabled` (§3) and on `plugins.enabled` (§2).

---

## Related

- [Plugins](../concepts/plugins.md), [Build a Plugin](../build/plugins.md) and
  the [Plugins reference](../reference/plugins.md).
- [Run work on a schedule](../guides/scheduled-workflows.md) and the
  [cron transport](../reference/channel-config.md#cron-transport).
- [Cluster Mode & High Availability](./cluster.md).
- [Upgrades](./upgrades.md): the version-independent procedure.
