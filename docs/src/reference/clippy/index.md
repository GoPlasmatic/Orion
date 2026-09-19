<!-- description: The advisory rules `orion-server clippy` runs beyond `lint`: the twenty-one rules, their levels and scopes, and where each one's certainty comes from. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-19 -->

# Advisory checks (`clippy`)

The rules `orion-server clippy` runs beyond `lint`. Each has its own page with the proof it rests on and where it stays silent.

`orion-server clippy` runs `lint`'s gate over a definition set, then a fixed set of rules. They cover things `lint` accepts but an author would want to know. A workflow whose condition can never match, steps that can never run, a call cycle that always fails. Work the engine does for nothing, and things the set says three times that it could say once.

It has **no configuration and no suppression**. That is what sets the bar. An author cannot silence a wrong rule, so every rule here fires only when its finding is certain. Each one states the proof it rests on and the shapes on which it stays silent. `orion-server clippy --explain <rule>` prints both. Silence is never wrong; a wrong warning is.

The command, its flags and exit codes are on the [CLI reference](../cli/orion-server/clippy.md).

| Rule | Level | Scope | Summary |
|---|---|---|---|
| [`correctness.workflow_never_matches`](./correctness-workflow-never-matches.md) | deny | workflow | the workflow-level condition is false for every request, so the workflow never runs |
| [`correctness.task_never_runs`](./correctness-task-never-runs.md) | warn | workflow | a step's condition folds to a constant false, so the step never runs |
| [`correctness.unreachable_step`](./correctness-unreachable-step.md) | deny | workflow | steps after an unconditional terminal step can never run |
| [`correctness.unconditional_call_cycle`](./correctness-unconditional-call-cycle.md) | deny | set | channel_call edges that are all unconditional form a cycle, so every request into it fails at the depth limit |
| [`correctness.payload_var`](./correctness-payload-var.md) | deny | workflow | a read of `payload` — which is not in the data context — is always null |
| [`correctness.mapping_overwritten`](./correctness-mapping-overwritten.md) | warn | workflow | two mappings in one map write the same path with nothing reading it in between |
| [`correctness.mapping_always_null`](./correctness-mapping-always-null.md) | deny | workflow | a `map` mapping whose `logic` is always null, so it never writes |
| [`correctness.metadata_var_undeclared`](./correctness-metadata-var-undeclared.md) | deny | workflow | a read of `metadata.vars.<name>` that the config given with -c does not declare |
| [`correctness.secret_undeclared`](./correctness-secret-undeclared.md) | deny | set | a {"secret": name} that the config given with -c does not declare |
| [`correctness.response_cookie_type`](./correctness-response-cookie-type.md) | warn | workflow | a response cookie attribute is a literal of the wrong type, so the cookie is always dropped |
| [`correctness.unknown_input_key`](./correctness-unknown-input-key.md) | deny | workflow | a task input key the function does not declare, which is silently ignored |
| [`correctness.unordered_page`](./correctness-unordered-page.md) | deny | workflow | a read that skips rows without ordering them, so the page it skips is undefined |
| [`correctness.sql_bind_count`](./correctness-sql-bind-count.md) | deny | workflow | a PostgreSQL statement's `$n` placeholders and its literal `params` differ in number |
| [`correctness.model_timeout_clamped`](./correctness-model-timeout-clamped.md) | warn | workflow | a literal `model_infer` `timeout_ms` above the ceiling the config gives the model |
| [`perf.parse_result_overwritten`](./perf-parse-result-overwritten.md) | warn | workflow | a parse/publish target is overwritten by a later unconditional task before anything reads it |
| [`perf.redundant_step_condition`](./perf-redundant-step-condition.md) | warn | workflow | consecutive steps repeat one condition that none of them can change; a task group evaluates it once |
| [`perf.group_condition_repeated`](./perf-group-condition-repeated.md) | warn | workflow | a group member repeats the group's own condition, which was already true on entry |
| [`duplication.fragment_available`](./duplication-fragment-available.md) | warn | set | a run of steps is exactly what an existing fragment expands to; a `use` would say it once |
| [`duplication.repeated_task_sequence`](./duplication-repeated-task-sequence.md) | warn | set | the same run of two or more steps appears three or more times across the set |
| [`duplication.repeated_value`](./duplication-repeated-value.md) | warn | set | the same object literal appears three or more times across the set |
| [`style.terminal_on_last_step`](./style-terminal-on-last-step.md) | warn | workflow | terminal: true on the last top-level step is a no-op |

| Page | Holds |
|---|---|
| [Levels](./levels.md) | `deny` and `warn`, what each does to the exit code, and how a rule is promoted. |
| [Where certainty comes from](./certainty.md) | the proof sources a rule may rest on, and nothing else. |
| [What is not a rule, and why](./not-a-rule.md) | the candidates turned down, each with its reason. |
| [Adding a rule](./adding-a-rule.md) | what a new rule must ship with, and the tests that check it. |

## Related

- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Definition style (`fmt`)](../fmt.md): the formatter the same set passes through.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Reference](../index.md): every reference page, by what you are looking up.
