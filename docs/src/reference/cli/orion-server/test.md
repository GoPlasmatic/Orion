<!-- description: orion-server test runs a directory of *.case.json workflow regression cases offline, with shared definitions, plugin components and model artifacts. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-server test`

Runs a directory of offline workflow test cases. Each `*.case.json` file names a workflow, an input, optional request metadata and connector stubs, and what it expects. Prints a per-case diff and exits non-zero on any failure.

## Synopsis

```bash
orion-server test <path>
```

## Description

A case expects output values (`expect`), task-error codes (`expect_errors`), connector calls (`expect_calls`) and executed task ids (`expect_tasks`). See [Test a workflow offline](../../../guides/author/testing.md) for the case format.

## Options

| Argument | Description |
|----------|-------------|
| `path` | A directory of `*.case.json` files, or a single case file. Paths inside a case resolve relative to the case file. |
| `--definitions` | Directory holding the set's shared definitions, resolved before each case's workflow is validated and run. |
| `--plugin-dir` | Directory of plugin manifests and their components, loaded and compiled once for the whole suite. Plugin functions run for real, never stubbed; a case naming one whose component is absent fails as `PLUGIN_ARTIFACT_UNAVAILABLE`, never as a silently passing stub. Repeatable. |
| `--model-dir` | Directory of model manifests and their artifacts, loaded once for the whole suite — each model is loaded into the runtime once, however many cases call it. `model_infer` runs the model for real, as `dry-run` does with the flag; a case naming a model the directory does not hold fails as `MODEL_ARTIFACT_UNAVAILABLE`. Without the flag a case's `stubs` answer the function by name. Repeatable. |

## Examples

```bash
orion-server test examples/workflow-tests
```

## Related

- [Test a workflow offline](../../../guides/author/testing.md): the case format, with every `expect` root.
- [`orion-server dry-run`](./dry-run.md): one execution, printed in full.
- [CI/CD with packages](../../../guides/patterns/ci-cd.md): the suite as a pipeline stage.
- [`orion-server` commands](./index.md): every `orion-server` subcommand.
