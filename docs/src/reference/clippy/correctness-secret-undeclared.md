<!-- description: The `correctness.secret_undeclared` advisory rule, level `deny`, scope set: a {"secret": name} that the config given with -c does not declare. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `correctness.secret_undeclared`

A `deny` rule, scope set. A {"secret": name} that the config given with -c does not declare.

## Synopsis

```console
$ orion-server clippy ./definitions
deny[correctness.secret_undeclared] a {"secret": name} that the config given with -c does not declare
```

## Description

With `-c`: a `{"secret": "<name>"}` that the config's `[secrets]` does not declare. The engine refuses to build the workflow and the channel is quarantined at load.

## Caveats

Skipped, with a note, without `-c`.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Shared definitions](../cli/shared-definitions.md): the set the rule reads, and its fragments.
