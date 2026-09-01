<!-- description: The advisory checks `orion-server clippy` runs beyond `lint`, each with the proof it rests on and where it stays silent — no configuration, no suppression. -->
# Advisory Checks (`clippy`)

`orion-server clippy` runs `lint`'s gate over a definition set and then a
fixed set of rules about things `lint` accepts but an author would want to
know: a workflow whose condition can never match, steps that can never run,
a call cycle that always fails, work the engine does for nothing, and
things the set says three times that it could say once.

It has **no configuration and no suppression**. That is what sets the bar:
an author cannot silence a wrong rule, so every rule here fires only when
its finding is certain, and each one states the proof it rests on and the
shapes on which it stays silent. `orion-server clippy --explain <rule>`
prints both. Silence is never wrong; a wrong warning is.

The command, its flags and exit codes are on the [CLI reference](./cli.md#clippy).

## Levels

| Level | Meaning | Effect |
|---|---|---|
| `deny` | The workflow cannot behave as written | exit 1 |
| `warn` | A certain fact the author would want to know; the suggestion is a suggestion | exit 0, or 1 with `--deny-warnings` |

Rules ship at `warn` and are promoted to `deny` only after a release of
field use without a false positive. A change of level is a release-note
item.

## The rules

| Rule | Level | Scope | Summary |
|---|---|---|---|
| `correctness.workflow_never_matches` | deny | workflow | the workflow-level condition is false for every request, so the workflow never runs |
| `correctness.task_never_runs` | warn | workflow | a step's condition folds to a constant false, so the step never runs |
| `correctness.unreachable_step` | deny | workflow | steps after an unconditional terminal step can never run |
| `correctness.unconditional_call_cycle` | deny | set | channel_call edges that are all unconditional form a cycle, so every request into it fails at the depth limit |
| `correctness.payload_var` | deny | workflow | a read of `payload` — which is not in the data context — is always null |
| `correctness.mapping_overwritten` | warn | workflow | two mappings in one map write the same path with nothing reading it in between |
| `correctness.metadata_var_undeclared` | deny | workflow | a read of `metadata.vars.<name>` that the config given with -c does not declare |
| `correctness.secret_undeclared` | deny | set | a {"secret": name} that the config given with -c does not declare |
| `perf.parse_result_overwritten` | warn | workflow | a parse/publish target is overwritten by a later unconditional task before anything reads it |
| `perf.redundant_step_condition` | warn | workflow | consecutive steps repeat one condition that none of them can change; a task group evaluates it once |
| `perf.group_condition_repeated` | warn | workflow | a group member repeats the group's own condition, which was already true on entry |
| `duplication.fragment_available` | warn | set | a run of steps is exactly what an existing fragment expands to; a `use` would say it once |
| `duplication.repeated_task_sequence` | warn | set | the same run of two or more steps appears three or more times across the set |
| `duplication.repeated_value` | warn | set | the same object literal appears three or more times across the set |
| `style.terminal_on_last_step` | warn | workflow | terminal: true on the last top-level step is a no-op |

`scope: set` rules read the whole set — the other workflows, the channels,
the shared `fragments` — and run only in directory mode. The two rules
that need the serving instance's config (`-c`) are skipped with a note
when none is given.

`lint`'s own findings are re-reported unchanged (`logic.unresolvable`,
`logic.escaped_template_key`, `closure.channel_call_dynamic`, the
`env.reference` and `secrets.reference` notes), so a clippy run is a superset of a lint run;
when `lint` reports an *error*, the rules do not run at all — a rule over
a document the API would refuse produces a second finding about the same
mistake, and a false one.

## Where certainty comes from

Every rule's proof is one of these, and nothing else is admitted — no
patterns over strings, no "usually a mistake", no near-matches:

- **The engine's own evaluation.** The datalogic compiler reports when it
  folded a condition to a constant; the evaluator, with Orion's operators
  registered, is run on exactly the context the serving engine will have.
- **Ingress facts established in the code.** When a workflow is selected,
  `data` and `temp_data` are empty — the request body is the *payload*,
  which only `parse_json` brings into `data`; `metadata.vars` is stamped
  from `[vars]` and cannot be caller-supplied; `payload` is not in the
  context at all.
- **Engine semantics read from source.** A terminal task halts after it
  ran; a terminal group halts when its span closes; `channel_call` fails at
  `max_channel_call_depth`; `map` applies mappings in order.
- **The function registry.** Which functions write only their target;
  which input fields the engine evaluates.
- **Structural identity.** Byte-identical after ids and names are
  stripped.
- **The config you passed with `-c`.** The `[vars]` and `[secrets]` the
  serving instance declares.

## What is not a rule, and why

Recorded so a future addition has to argue against the reason rather than
rediscover it. Each of these is a real class of mistake; none can be
reported without sometimes being wrong.

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

## Each rule

### `correctness.workflow_never_matches`

A workflow's `condition` is evaluated before any task runs, against a
context where `data` and `temp_data` are `{}`. A condition that reads only
those has one possible result, and the rule asks the engine for it. Silent
on any `metadata` read, any computed `val`, any read inside an
element-scoped operator, `now`/`random`/`secret`, a loop-counter read, or
a result that is anything but exactly `false`/`null`. `{"!": {"var":
"data.flag"}}` is true on empty data and does **not** fire.

### `correctness.task_never_runs`

The compiler folded a step's condition to `false`/`null`. A warning:
`"condition": false` is a way to switch a step off.

### `correctness.unreachable_step`

Everything in document order after a terminal step that is certain to be
reached — no condition on it or on any enclosing group. A terminal *task*
halts after it ran, so it must be unconditional; a terminal *group* halts
when its span closes whatever its members did.

### `correctness.unconditional_call_cycle`

Static `channel_call` edges, joined to the set's channel → workflow
binding, restricted to edges whose calling task and workflow are
unconditional. Any conditional edge, a computed `channel`, or a target
outside the set keeps the rule silent — bounded recursion with a base case
is a legal pattern.

### `correctness.payload_var`

`{"var": "payload…"}` in an expression the engine evaluates. Silent inside
the element-scoped arguments of `map`, `filter`, `reduce`, `all`, `some`,
`none`, `group_by`, `distinct`, `sort`, `try`, `switch`, `match`.

### `correctness.mapping_overwritten`

Two mappings of one `map` write the same `path`, and neither the mappings
between them nor the second one reads it (or anything inside or above it).
`data.x = data.x + 1` is a pattern and stays silent.

### `correctness.metadata_var_undeclared`

With `-c`: a `metadata.vars.<name>` read that the config's `[vars]` does
not declare. `vars` is stamped at ingress and cannot be caller-supplied,
so the read is always `null`. Skipped, with a note, without `-c`.

### `correctness.secret_undeclared`

With `-c`: a `{"secret": "<name>"}` that the config's `[secrets]` does not
declare. The engine refuses to build the workflow and the channel is
quarantined at load. Skipped, with a note, without `-c`.

### `perf.parse_result_overwritten`

A `parse_json`/`parse_xml`/`publish_json`/`publish_xml` target that a
later unconditional task writes (or a path above it) before anything reads
it. Silent when any step between has a computed or scoped read, reads the
target, or is a connector or `channel_call` task; when the overwriter is
conditional; when the workflow has a `loop`.

### `perf.redundant_step_condition`

Consecutive steps in one list with a byte-identical condition that none
of them can change — no step in the run writes a path the condition reads,
and the condition has no computed, scoped or nondeterministic part. A task
group would evaluate it once. Suggestion only.

### `perf.group_condition_repeated`

A group member repeating its group's condition when no earlier member
writes what it reads: it was already true on entry.

### `duplication.fragment_available`

A run of steps that is exactly an existing fragment's `tasks`, ids aside,
with `$param` holes bound consistently. The message prints the `use` step
that replaces them; it is not applied, because the expanded ids change.

### `duplication.repeated_task_sequence`

A run of ≥ 2 steps, ids and names stripped, in ≥ 3 places; the longest
such run at those places. The message states the fact and where; whether
it should be a fragment is the author's call.

### `duplication.repeated_value`

An object with ≥ 2 keys, byte-identical in ≥ 3 places. Silent for
structure (steps, `function` headers, mapping and rule entries, operator
nodes, roots), for the input of an engine built-in, for a `use` step's
`with` block, for anything containing a `$from`, and for an object every
occurrence of which sits inside a larger reported one.

### `style.terminal_on_last_step`

`terminal: true` on the last top-level step. Nothing follows it.

## Adding a rule

A rule is a struct implementing `Rule` under
`crates/orion-server/src/definitions/clippy/rules/`, registered in
`rules::ALL`, with a `tests/fixtures/clippy/<id>/fires/` set it must fire
on and a `quiet/` set on which **no rule** may fire — the exclusions
written down. The tests refuse a rule missing either, a rule this page
does not list, and any rule that fires on `examples/` or the e2e fixtures.
Before it ships, it is run over a real estate and every diagnostic is read.

## Related

- [CLI Reference — `clippy`](./cli.md#clippy)
- [Test Workflows Offline](../build/testing.md)
- [Definition Style (`fmt`)](./fmt.md)
