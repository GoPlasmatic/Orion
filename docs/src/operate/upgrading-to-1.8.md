<!-- description: Upgrading Orion 1.7.x to 1.8.0: one expand-only migration, the tensor operators are live, seven names now collide with ordinary keys, and the ops budget is off. -->
# Upgrading to 1.8.0

This page is for operators upgrading an existing Orion deployment from
**1.7.x** to **1.8.0**. It covers only what *changes behaviour*. The new
capabilities — the [tensor operators](../reference/expressions.md#tensors-tensor)
and the [evaluation budget](../reference/configuration.md#engine) — are
described in full in the
[CHANGELOG](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/CHANGELOG.md).

**1.8.0 is a minor release and behaves like one.** No config key was renamed
or removed, no API path moved, and no metric was renamed. Three things can
reach you. One adds schema — §1 — one can change what an existing deployment
already does — §2 — and the third is off until you turn it on.

The version-independent procedure — back up, preflight, validate config,
migrate, roll — is on [Upgrades](./upgrades.md).

---

## Before you start

| # | Check | Applies to you if |
|---|-------|-------------------|
| 1 | [Run the migration as a deploy step](#1-one-migration-expand-only) | You run a cluster with `auto_migrate = false` — everyone else gets it at startup |
| 2 | [Run `preflight` and escape the keys it lists](#2-twenty-operator-names-are-now-live) | A stored workflow emits a literal object keyed `shape`, `full`, `cast`, `pad`, `crop`, `concat` or `stack` from a `map` mapping or a template field |
| 3 | [Nothing — the budget is off](#3-engineops_budget-is-new-and-off) | Every deployment; read it before turning it on |

---

## 1. One migration, expand-only

**What changed.** One new migration per backend, `models`: the `models` table
that holds a model registration — its manifest, its artifact reference, and
the node-written admission verdict — plus two indexes and the single-draft and
active-immutability triggers every versioned entity has. It adds schema and
touches nothing that exists, so a 1.7.x binary keeps working against a
migrated database and a rollback needs no schema work.

The migration runs whether or not you intend to use models: it is applied at
startup like every other one, and
[`models.enabled`](../reference/configuration.md#models) — which is off by
default — governs only whether the node serves them, never whether the table
exists.

**What to do.** Nothing, unless `storage.auto_migrate = false`: then
`orion-server migrate` is the deploy step, as it always is in
[cluster mode](./cluster.md).

## 2. Twenty operator names are now live

1.8 enables dataflow-rs's `tensor` feature, so every engine Orion builds
evaluates twenty more operators: `tensor`, `zeros`, `full`, `scatter`,
`rle_expand`, `one_hot`, `stack`, `concat`, `unstack`, `reshape`, `transpose`,
`pad`, `crop`, `cast`, `normalize`, `argmax`, `gather`, `to_list`, `shape`,
`dtype`.

The rule that makes this a review item is the one templating has always had:
in a **template position** — a `map` mapping's `logic`, or a custom function's
field the registry marks as an expression — a **single-key object whose key is
a live operator is a call, not data**. Before 1.8, `shape` was not an
operator, so this mapping emitted an object:

```json
{ "path": "data.board", "logic": { "shape": [6, 7] } }
```

From 1.8 it calls `shape([6, 7])`, which is not a valid call, so the task
fails with an evaluation error. The fix is the `$` key escape every engine
already carries — one `$` is stripped from every template key — so the literal
is spelled:

```json
{ "path": "data.board", "logic": { "$shape": [6, 7] } }
```

Multi-key objects (`{"shape": "queue", "type": "channel"}`) are unaffected.
Conditions are unaffected: they compile strictly and always did, so a
tensor-named key there raised `Invalid operator` before 1.8 and could not have
been serving.

**Finding them.** Run the scan against the database before rolling the new
binary; it is read-only and reports the stored estate:

```bash
orion-server preflight -c config.toml
```

Every affected key is listed under the advisory id `logic.tensor_operator_key`
with its workflow, its path and the rewrite. Advisories never change the exit
code, so add this to a deploy gate deliberately if you want it to. `preflight`
lists both the constant objects (`{"shape": [6, 7]}`) and the dynamic ones
(`{"shape": {"var": "data.dims"}}`): on a stored estate written before 1.8,
neither can have meant a call.

For definition sets in a repository, `orion-server lint <dir>` reports the
same id — but only for the constant objects that do not evaluate as a call.
On a set being written today, `{"shape": {"var": …}}` is a call an author
meant, and `{"zeros": [[2], "i64"]}` is a working one; `lint` stays quiet on
both so that `--deny-warnings` does not refuse the feature it now supports.

**Then** prefix each listed key and `PUT` the workflow (or re-run the
pipeline that promotes it). The escaped spelling is not reported, so a second
`preflight` is the proof the estate is clean.

## 3. `engine.ops_budget` is new, and off

The `budget` feature is enabled too, which adds one setting:

```toml
[engine]
# ops_budget = 0
```

`0`, the default, installs no ceiling and changes nothing. Set it when
expressions come from someone other than the operator — a tenant's rules, a
competitor's model adapters — and read the caveat on the
[configuration page](../reference/configuration.md#engine) first: a custom
function's template field that crosses the ceiling fails its task with
`BUDGET_EXCEEDED`, a built-in `map` mapping fails its task with status `500`,
but a **condition** that crosses it fails closed to `false` and is only
logged, so a ceiling low enough to trip an ordinary condition reads as "no
workflow matched". Size it from the heaviest legitimate expression in the
estate.

The counter behind the setting runs whether or not a ceiling is set. The cost
is one add-and-compare per dispatched node.

---

## Related

- [Expressions › Tensors](../reference/expressions.md#tensors-tensor) — the
  operator table with an example per operator.
- [CLI › `lint`](../reference/cli.md#lint) and [CLI › `preflight`](../reference/cli.md#preflight)
  — the advisory ids and what each command reports.
- [Configuration › Engine](../reference/configuration.md#engine) —
  `engine.ops_budget`.
- [Models](../concepts/models.md) — the release's headline capability, and the
  reason the tensor operators exist. It changes nothing about an existing
  deployment: `models.enabled` is off by default, and a node with it off
  behaves exactly as 1.7.x did.
