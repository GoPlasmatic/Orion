<!-- description: The mistakes clippy deliberately does not report, and the reason each one cannot be reported without sometimes being wrong. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# What is not a rule, and why

Candidates that were turned down, each with the reason it cannot be reported without sometimes being wrong.

## Description

Recorded so a future addition has to argue against the reason rather than rediscover it. Each of these is a real class of mistake; none can be reported without sometimes being wrong.

| Candidate | Why it is not certain |
|---|---|
| A read of a `data.*` path nothing earlier writes | a `continue_on_error` predecessor, a connector response shape, or a value that arrives another way are all legitimate — `POST /workflows/validate` still offers this as an advisory, with that caveat |
| A misspelled operator in a mapping (`{"uppr": […]}`) | an object literal in a mapping is a legitimate way to write an object; distance to an operator name is a guess |
| A URL, a `dev-`/`staging-` prefix, an e-mail address in a definition | patterns over strings |
| A string that looks like a key, or a literal in a key-material field | heuristic; and the registry-exact form fires on every test fixture with a throwaway HS256 key, which the docs allow |
| A workflow condition that *reads* `data.*` | superseded by `workflow_never_matches`, which evaluates the condition instead of guessing from its reads (`{"!": {"var": "data.x"}}` reads `data` and always matches) |
| Near-duplicate step runs or strings; repeated scalars | which leaves "should" be parameters, and which repeated `500`s are one threshold, are judgements |
| Mutually exclusive branches without `terminal`; adjacent `map`s; a group of one; an explicit `"condition": true` | readability opinions |
| A `loop` with no break | "max near the cap" is a threshold guess; a bounded sweep with no break is a valid design |
| A number compared to a string | JSONLogic coerces; the comparison works |
| A connector task's `output` overwritten unread | the call still happened — a `data_write` wrote to the database |
| A `channel_call` cycle with a conditional edge | bounded recursion with a base case is a legal pattern; the depth cap exists for it |
| A cron `payload` key the bound workflow never reads | the payload reaches the workflow through `parse_json` into a target the rule would have to guess at, and a workflow may branch on a key only some occurrences carry — the same uncertain-reads problem the first row records |
| A cron schedule that fires faster than its work takes | knowable only at run time, and visible where it is knowable: `skipped_singleton` occurrences and `orion_cron_schedule_lag_seconds` |
| Anything about a [plugin](../plugin-manifest.md) function beyond what its manifest declares | a plugin's writes are already proven structurally through `output`, exactly as `crypto`'s are, and its reads arrive only through declared fields — there is no second proof source to add; a plugin function no manifest covers is unverifiable, and an unverifiable fact is not a certain one |
| A template key that names a [tensor operator](../expressions.md#tensors-tensor) (`{"shape": …}` in a `map` mapping) | whether the author meant data or a call is not provable: `{"shape": {"var": "data.dims"}}` is a call on a set being written today and a literal on one stored before 1.8. `lint` reports the constant objects that do not evaluate as a call (`logic.tensor_operator_key`), `preflight` the dynamic ones too, both as advisories with the `$` escape as the remedy |
| A `?` placeholder count that differs from `params` | on SQLite a missing value binds as `NULL` and the statement runs, and MySQL's behaviour is not established from source here; `sql check` asks the database instead |
| A `$n` statement that skips a number while the count matches | Orion's value-shaped fallback declares the skipped parameter's type, and the statement runs |
| A per-item `loop` slot read before this iteration writes it | the slot may be written by an earlier iteration on purpose — a running total is the common case |
| A cron channel's `timeout_ms` above `cron.shutdown_timeout_secs` | a long job on a node that is not shutting down is fine; only a shutdown cuts it short, which is not a property of the definition |

## Related

- [Advisory checks](./index.md): every rule, with its level and scope.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [`orion-server lint`](../cli/orion-server/lint.md): the gate that runs first, and must pass.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
