<!-- description: The two clippy levels, deny and warn, what each means for the exit code, and how a rule is promoted from one to the other. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Levels

The two levels a rule ships at, and what each does to the exit code. Also how a rule moves between them.

## Description

| Level | Meaning | Effect |
|---|---|---|
| `deny` | The workflow cannot behave as written | exit 1 |
| `warn` | A certain fact the author would want to know; the suggestion is a suggestion | exit 0, or 1 with `--deny-warnings` |

Rules ship at `warn`. A rule is promoted to `deny` only after a release of field use with no false positive. A change of level is a release-note item.

`scope: set` rules read the whole set: the other workflows, the channels, the shared `fragments`. They run only in directory mode. The two rules that need the serving instance's config (`-c`) are skipped, with a note, when none is given.

`lint`'s own findings are re-reported unchanged, so a clippy run is a superset of a lint run. Those findings are `logic.unresolvable`, `logic.escaped_template_key`, `engine.unguarded_validation` and `engine.group_continue_on_error`. Also `engine.advisory`, `closure.channel_call_dynamic`, and the `env.reference` and `secrets.reference` notes. When `lint` reports an *error*, the rules do not run at all. A rule over a document the API would refuse produces a second finding about the same mistake, and a false one.

## Related

- [Advisory checks](./index.md): every rule, with its level and scope.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [`orion-server lint`](../cli/orion-server/lint.md): the gate that runs first, and must pass.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
