<!-- description: The `correctness.model_timeout_clamped` advisory rule, level `warn`, scope workflow: a `model_infer` `timeout_ms` above the model's ceiling. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# `correctness.model_timeout_clamped`

A `warn` rule, scope workflow. A literal `model_infer` `timeout_ms` above the ceiling the config gives the model.

## Synopsis

```console
$ orion-server clippy ./definitions
warn[correctness.model_timeout_clamped] a literal `model_infer` `timeout_ms` above the ceiling the config gives the model
```

## Description

A `model_infer` whose literal `timeout_ms` is above the ceiling the serving config gives the model. The handler runs under the shorter of the two and reports nothing, so the deadline the author wrote is not the one that runs. The ceiling is `[models] max_timeout_ms`, lowered for a literal model id by its `[[models.overrides]]` row. With a computed model id the host ceiling is the bound, since an override can only lower it.

The rule needs the serving config and is skipped with a note without `-c`.

## Caveats

Silent without `-c`, and when that config has `[models] enabled = false`. Silent when `timeout_ms` is computed, and when it is not a positive integer: the handler refuses that at run time, which is a different finding.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Workflows](../workflows.md): the step grammar the rule reads.
