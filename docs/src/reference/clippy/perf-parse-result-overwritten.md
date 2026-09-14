<!-- description: The `perf.parse_result_overwritten` advisory rule, level `warn`, scope workflow: a parse/publish target is overwritten by a later unconditional task before. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `perf.parse_result_overwritten`

A `warn` rule, scope workflow. A parse/publish target is overwritten by a later unconditional task before anything reads it.

## Synopsis

```console
$ orion-server clippy ./definitions
warn[perf.parse_result_overwritten] a parse/publish target is overwritten by a later unconditional task before anything reads it
```

## Description

A `parse_json`/`parse_xml`/`publish_json`/`publish_xml` target that a later unconditional task writes (or a path above it) before anything reads it.

## Caveats

Silent when any step between has a computed or scoped read, reads the target, or is a connector or `channel_call` task. Silent when the overwriter is conditional, and when the workflow has a `loop`.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Workflows](../workflows.md): the step grammar the rule reads.
