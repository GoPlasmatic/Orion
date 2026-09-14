<!-- description: The `correctness.metadata_var_undeclared` advisory rule, level `deny`, scope workflow: a read of `metadata.vars.<name>` that the config given with -c does not. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `correctness.metadata_var_undeclared`

A `deny` rule, scope workflow. A read of `metadata.vars.<name>` that the config given with -c does not declare.

## Synopsis

```console
$ orion-server clippy ./definitions
deny[correctness.metadata_var_undeclared] a read of `metadata.vars.<name>` that the config given with -c does not declare
```

## Description

With `-c`: a `metadata.vars.<name>` read that the config's `[vars]` does not declare. `vars` is stamped at ingress and cannot be caller-supplied, so the read is always `null`.

## Caveats

Skipped, with a note, without `-c`.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Workflows](../workflows.md): the step grammar the rule reads.
