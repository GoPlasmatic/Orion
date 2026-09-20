<!-- description: The `perf.redundant_step_condition` advisory rule, level `warn`, scope workflow: consecutive steps repeat one condition that none of them can change; a task. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# `perf.redundant_step_condition`

A `warn` rule, scope workflow. Consecutive steps repeat one condition that none of them can change; a task group evaluates it once.

## Synopsis

```console
$ orion-server clippy ./definitions
warn[perf.redundant_step_condition] consecutive steps repeat one condition that none of them can change; a task group evaluates it once
```

## Description

Consecutive steps in one list with a byte-identical condition that none of them can change. No step in the run writes a path the condition reads, and the condition has no computed, scoped or nondeterministic part. A task group would evaluate it once.

The rule is silent when the condition reads `metadata.progress`. The engine overwrites that path after every task, so each step in the run changes what the next one's condition sees.

## Fix

`clippy --fix` folds the run into one task group:

```json
{ "id": "when_claim", "condition": { "==": [{ "var": "data.input.kind" }, "claim"] },
  "tasks": [ { "id": "claim", … }, { "id": "read", "terminal": true, … } ] }
```

The group is named `when_` and the first member's id. It carries the condition as the first member wrote it, so a `$from` stays a reference. Each member keeps everything but its condition, its own `terminal` included. The group takes no `terminal`, so the workflow halts where it did. Member ids do not change, so traces and metric labels read the same.

The fix is refused, and reported, when the steps are not written in the file, such as those from a fragment or an `$each`. It is also refused when the group id is taken or the group would nest too deep. So is a step whose condition is not written on it.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Workflows](../workflows.md): the step grammar the rule reads.
