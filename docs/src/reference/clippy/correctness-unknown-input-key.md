<!-- description: The `correctness.unknown_input_key` advisory rule, level `deny`, scope workflow: a task input key the function does not declare, which is silently ignored. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `correctness.unknown_input_key`

A `deny` rule, scope workflow. A task input key the function does not declare, which is silently ignored.

## Synopsis

```console
$ orion-server clippy ./definitions
deny[correctness.unknown_input_key] a task input key the function does not declare, which is silently ignored
```

## Description

A key in a task's `function.input` that the named function does not declare. Orion's own connector handlers read freeform JSON rather than a struct: `db_read`, `db_write`, `data_query`, `data_write`, `cache_read`, `cache_write`, `mongo_read` and `channel_call`. An unknown key is therefore not refused at create, not reported at load, and does nothing at run time. `"limit": 10` written beside a `db_read` `query` reads as enforced and enforces nothing. The query envelope already applies this rule one level down, where an unknown key is an error. Its reason: "a key that cannot apply is a filter, a projection or a limit silently not applying". Aliases count as declared, and a close miss is offered as a suggestion.

## Caveats

Silent when the function declares no field table, which is an engine built-in. Silent too when it declares `deny_unknown`, where the key is already refused at create with a located error.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Workflows](../workflows.md): the step grammar the rule reads.
