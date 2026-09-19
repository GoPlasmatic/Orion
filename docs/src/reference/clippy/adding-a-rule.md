<!-- description: What a new clippy rule must ship with: the Rule impl, the registry entry, the fires and quiet fixtures, a page here, and a run over a real estate. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Adding a rule

What a new rule must ship with before it is merged. The tests refuse one missing any of it.

## Description

A rule is a struct implementing `Rule` under `crates/orion-server/src/definitions/clippy/rules/`, registered in `rules::ALL`. It ships with a `tests/fixtures/clippy/<id>/fires/` set it must fire on, and a `quiet/` set on which **no rule** may fire. The exclusions are written down. The tests refuse a rule missing either fixture set, and a rule this page does not list. They also refuse any rule that fires on `examples/` or the e2e fixtures. Before it ships, it is run over a real estate and every diagnostic is read.

A rule may offer a fix by attaching a `Fix` to its diagnostic, but only when the rewrite is exactly what its proof licenses. A new kind of fix is a `Fix` variant with a fold on the source tree and on the compiled form. `apply` checks the two against each other through the compiler. It ships with a `tests/fixtures/clippy-fix/<case>/{before,after}/` pair, which `clippy_fix_test` runs and runs again.

## Related

- [Advisory checks](./index.md): every rule, with its level and scope.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [`orion-server lint`](../cli/orion-server/lint.md): the gate that runs first, and must pass.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
