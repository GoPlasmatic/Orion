<!-- description: The `correctness.payload_var` advisory rule, level `deny`, scope workflow: a read of `payload` — which is not in the data context — is always null. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `correctness.payload_var`

A `deny` rule, scope workflow. A read of `payload` — which is not in the data context — is always null.

## Synopsis

```console
$ orion-server clippy ./definitions
deny[correctness.payload_var] a read of `payload` — which is not in the data context — is always null
```

## Description

`{"var": "payload…"}` in an expression the engine evaluates.

## Caveats

Silent inside the element-scoped arguments of `map`, `filter`, `reduce`, `all`, `some`, `none`, `group_by`, `distinct`, `sort`, `try`, `switch`, `match`.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Workflows](../workflows.md): the step grammar the rule reads.
