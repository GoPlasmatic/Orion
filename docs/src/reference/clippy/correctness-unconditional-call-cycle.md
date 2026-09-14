<!-- description: The `correctness.unconditional_call_cycle` advisory rule, level `deny`, scope set: channel_call edges that are all unconditional form a cycle, so every. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `correctness.unconditional_call_cycle`

A `deny` rule, scope set. Channel_call edges that are all unconditional form a cycle, so every request into it fails at the depth limit.

## Synopsis

```console
$ orion-server clippy ./definitions
deny[correctness.unconditional_call_cycle] channel_call edges that are all unconditional form a cycle, so every request into it fails at the depth limit
```

## Description

Static `channel_call` edges, joined to the set's channel → workflow binding, restricted to edges whose calling task and workflow are unconditional. Any conditional edge, a computed `channel`, or a target outside the set keeps the rule silent. Bounded recursion with a base case is a legal pattern.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Shared definitions](../cli/shared-definitions.md): the set the rule reads, and its fragments.
