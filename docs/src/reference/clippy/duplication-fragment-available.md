<!-- description: The `duplication.fragment_available` advisory rule, level `warn`, scope set: a run of steps is exactly what an existing fragment expands to; a `use` would say. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `duplication.fragment_available`

A `warn` rule, scope set. A run of steps is exactly what an existing fragment expands to; a `use` would say it once.

## Synopsis

```console
$ orion-server clippy ./definitions
warn[duplication.fragment_available] a run of steps is exactly what an existing fragment expands to; a `use` would say it once
```

## Description

A run of steps that is exactly an existing fragment's `tasks`, ids aside, with `$param` holes bound consistently. The message prints the `use` step that replaces them. It is not applied, because the expanded ids change.

Silent for a value fragment. Silent for a task fragment whose steps use a fragment, splice one with `$use`, repeat, or interpolate a parameter with `{{name}}`. Its expansion is not what it says, so it does not compare step for step. Silent for a run that holds a `use`, a `$use` or an `$each` step.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Shared definitions](../cli/shared-definitions.md): the set the rule reads, and its fragments.
