<!-- description: orion-cli test runs a directory of *.case.json workflow regression cases offline, with shared definitions and plugin components. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-25 -->

# `orion-cli test`

Runs a directory of offline workflow test cases. Each `*.case.json` file names a workflow, an input, optional request metadata and connector stubs, and what it expects. Prints a per-case diff and exits non-zero on any failure.

## Synopsis

```bash
orion-cli test <path>
```

## Description

With `--definitions`, a set whose [`package` document](../shared-definitions.md#the-package-document) declares a `requires.orion` range excluding this binary stops before any case runs. One line names the range and this version.

A case expects output values (`expect`), task-error codes (`expect_errors`), connector calls (`expect_calls`) and executed task ids (`expect_tasks`). See [Test a workflow offline](../../../guides/author/testing.md) for the case format.

A case runs the way a node runs a message whose channel does not trace task details. It records no execution trace, and no per-write capture unless an `expect` path is rooted at `audit_trail`. A long looping workflow costs about what it costs on a node.

## Options

| Argument | Description |
|----------|-------------|
| `path` | A directory of `*.case.json` files, or a single case file. Paths inside a case resolve relative to the case file. |
| `--definitions` | Directory holding the set's shared definitions, resolved before each case's workflow is validated and run. |
| `--plugin-dir` | Directory of plugin manifests and their components, loaded and compiled once for the whole suite. Plugin functions run for real, never stubbed; a case naming one whose component is absent fails as `PLUGIN_ARTIFACT_UNAVAILABLE`, never as a silently passing stub. Repeatable. |

## Examples

```bash
orion-cli test examples/workflow-tests
```

## Related

- [Test a workflow offline](../../../guides/author/testing.md): the case format, with every `expect` root.
- [`orion-cli dry-run`](./dry-run.md): one execution, printed in full.
- [CI/CD with packages](../../../guides/patterns/ci-cd.md): the suite as a pipeline stage.
- [`orion-cli` commands](./index.md): every `orion-cli` subcommand.
