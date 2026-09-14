<!-- description: The `style.terminal_on_last_step` advisory rule, level `warn`, scope workflow: terminal: true on the last top-level step is a no-op. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `style.terminal_on_last_step`

A `warn` rule, scope workflow. Terminal: true on the last top-level step is a no-op.

## Synopsis

```console
$ orion-server clippy ./definitions
warn[style.terminal_on_last_step] terminal: true on the last top-level step is a no-op
```

## Description

`terminal: true` on the last top-level step. Nothing follows it.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Workflows](../workflows.md): the step grammar the rule reads.
