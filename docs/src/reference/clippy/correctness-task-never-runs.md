<!-- description: The `correctness.task_never_runs` advisory rule, level `warn`, scope workflow: a step's condition folds to a constant false, so the step never runs. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `correctness.task_never_runs`

A `warn` rule, scope workflow. A step's condition folds to a constant false, so the step never runs.

## Synopsis

```console
$ orion-server clippy ./definitions
warn[correctness.task_never_runs] a step's condition folds to a constant false, so the step never runs
```

## Description

The compiler folded a step's condition to `false`/`null`.

## Caveats

A warning: `"condition": false` is a way to switch a step off.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Workflows](../workflows.md): the step grammar the rule reads.
