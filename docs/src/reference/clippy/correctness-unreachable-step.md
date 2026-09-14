<!-- description: The `correctness.unreachable_step` advisory rule, level `deny`, scope workflow: steps after an unconditional terminal step can never run. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `correctness.unreachable_step`

A `deny` rule, scope workflow. Steps after an unconditional terminal step can never run.

## Synopsis

```console
$ orion-server clippy ./definitions
deny[correctness.unreachable_step] steps after an unconditional terminal step can never run
```

## Description

Everything in document order after a terminal step that is certain to be reached. Certain means no condition on it or on any enclosing group. A terminal *task* halts after it ran, so it must be unconditional. A terminal *group* halts when its span closes, whatever its members did.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Workflows](../workflows.md): the step grammar the rule reads.
