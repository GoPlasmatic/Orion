<!-- description: The `correctness.workflow_never_matches` advisory rule, level `deny`, scope workflow: the workflow-level condition is false for every request, so the workflow. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `correctness.workflow_never_matches`

A `deny` rule, scope workflow. The workflow-level condition is false for every request, so the workflow never runs.

## Synopsis

```console
$ orion-server clippy ./definitions
deny[correctness.workflow_never_matches] the workflow-level condition is false for every request, so the workflow never runs
```

## Description

A workflow's `condition` is evaluated before any task runs, against a context where `data` and `temp_data` are `{}`. A condition that reads only those has one possible result, and the rule asks the engine for it.

## Caveats

Silent on any `metadata` read, any computed `val`, or any read inside an element-scoped operator. Silent too on `now`/`random`/`secret`, a loop-counter read, and any result but exactly `false`/`null`. `{"!": {"var": "data.flag"}}` is true on empty data and does **not** fire.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Workflows](../workflows.md): the step grammar the rule reads.
