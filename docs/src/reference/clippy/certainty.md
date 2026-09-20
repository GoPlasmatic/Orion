<!-- description: The six proof sources a clippy rule may rest on — engine evaluation, ingress facts, engine semantics, the registry, structural identity, the config. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# Where certainty comes from

The proof sources a rule may rest on. Nothing else is admitted, which is what keeps the rule set free of guesses.

## Description

Every rule's proof is one of these, and nothing else is admitted — no patterns over strings, no "usually a mistake", no near-matches:

- **The engine's own evaluation.** The datalogic compiler reports when it
  folded a condition to a constant; the evaluator, with Orion's operators
  registered, is run on exactly the context the serving engine will have.
- **Ingress facts established in the code.** When a workflow is selected,
  `data` and `temp_data` are empty — the request body is the *payload*,
  which only `parse_json` brings into `data`; `metadata.vars` is stamped
  from `[vars]` and cannot be caller-supplied; `payload` is not in the
  context at all.
- **Engine and handler semantics read from source.** A terminal task halts
  after it ran; a terminal group halts when its span closes; `channel_call`
  fails at `max_channel_call_depth`; `map` applies mappings in order and
  skips a null result; every task overwrites `metadata.progress`;
  `model_infer` takes the shorter deadline; the SQL binder refuses a bind of
  the wrong length.
- **The function registry.** Which functions write only their target;
  which input fields the engine evaluates.
- **Structural identity.** Byte-identical after ids and names are
  stripped.
- **The config you passed with `-c`.** The `[vars]` and `[secrets]` the
  serving instance declares, and its `[models]` ceilings.

## Related

- [Advisory checks](./index.md): every rule, with its level and scope.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [`orion-server lint`](../cli/orion-server/lint.md): the gate that runs first, and must pass.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
