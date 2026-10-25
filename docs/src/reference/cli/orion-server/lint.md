<!-- description: orion-server lint validates one workflow or a whole definition set with the server's checks, resolves cross-references, and reports stable [check] ids. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-20 -->

# `orion-server lint`

Statically validates a workflow JSON file with the same checks the server runs when the definition is uploaded. Exits non-zero with field-pathed errors, so it can gate CI. Needs no config, database, or server.

## Synopsis

```bash
orion-server lint <workflow.json | dir> [--deny-warnings]
                  [--requires-channel NAME]... [--requires-connector NAME]...
```

## Description

**A plugin function is checked against its manifest, or reported unverifiable.** A task naming `acme.codec.parse` is validated against the `acme.codec` manifest when the set (or a `--plugin-dir`) carries one. Required fields, kinds, `template_at` and `resolvable` are checked exactly as the admin API checks them against the active plugin. A function of a plugin the manifest *does* cover but does not declare is a `[closure.plugin]` error. A function of a plugin no manifest here accounts for is a `[plugin.unverifiable]` note. It is neither valid nor invalid offline, and the server decides when the definition arrives. Each manifest in the set is listed as a `[plugin.manifest]` inventory note. The note carries its digest, or the fact that no component sits beside it.

**A directory is linted as a set.** Every channel, workflow and connector under it is validated, *and* the references between them are resolved. Those are a `channel_call` target, and a task's connector and its type. They are also a channel's `workflow_id`, and duplicate ids, names and routes. Those are the errors a per-file lint cannot see, because the file that would disprove them is one it never opens.

The reference rules are the same ones a deployment is checked against. A set that lints clean here is one whose references a deployment accepts.

Entities are found by shape, recursively: an object with `tasks` is a workflow, `connector_type` a connector, `channel_type` or `protocol` a channel. Anything else is reported as skipped rather than silently ignored, and a directory yielding no definitions is an error.

By default every reference must resolve inside the set. Use `--requires-channel` / `--requires-connector` for a set that genuinely depends on something deployed elsewhere.

A set with a [`package` document](../shared-definitions.md#the-package-document) whose `requires.orion` excludes this binary stops before any finding, with one line naming the range and this version. Each finding carries a stable `[check]` id, so a pipeline can grandfather one rule without silencing the rest. `[env.unresolved]` is an error rather than an advisory. It fires when a workflow field that resolves no secret reference contains one, which the admin API refuses on the same terms. `note:` findings are exit-neutral inventory, not defects. `[env.reference]` lists each environment variable the set references through `env://`. `[secrets.reference]` lists each name it reads with `{"secret": …}`, which the serving instance's `[secrets]` section must declare. Both name the files that reference them, and neither the exit code nor `--deny-warnings` counts them. `[sql.read_only]` is an error: a literal `db_read` statement that is not a read, which the handler refuses on every run. It names the `.sql` file when the statement came from one. `[env.embedded_reference]` is a warning about a connector string with `env://` inside a longer value, such as `"Bearer env://API_KEY"`. A reference must be the whole value, so that text is sent literally. `[connector.db_driver]` is an error: a `db` connector in the set does not name its [`driver`](../../connectors/db.md#the-driver-field). A connection string behind a secret reference shows no scheme, so a set states the backend on every `db` connector. `[cron.slots_mismatch]` is a warning: two cron channels share a `concurrency.key` but declare different `slots`. Each run is admitted against its own channel's bound, which is coherent but rarely intended.

Advisory findings print on stderr and do not fail the command unless `--deny-warnings` is set. There are three. The first two are Orion's own:

- `[logic.unresolvable]` — JSONLogic in a connector field that folds `{"var": …}` and nothing else, so the expression is stored or sent verbatim.
- `[logic.escaped_template_key]` — a `$`-prefixed key in a position the engine evaluates as a template. One `$` is stripped from every such key, so a `{"$set": …}` update document composed in a `map` task is emitted as `{"set": …}` and the write replaces the document instead of updating it. Nothing fails at any gate; the fix is to double the prefix (`$$set`), and the doubled spelling is not reported.

The third is the engine's, reported here because `Engine::build` does not refuse it. A workflow carrying it loads, serves, and does less than its author wrote:

- `[engine.unguarded_validation]` — a [`validation`](../../functions/validation.md) whose failure changes nothing: a failed rule records `400`, the engine warns on `4xx` and carries on, and every task after it is unguarded. Silenced by [`"on_rejection": "halt"`](../../workflows.md#halting-on-a-rejection) on the task, or by a condition on what follows. Collecting failures and carrying on is a legitimate shape, so this says the assertion is not acting as a gate — not that it is wrong.

A fourth id, `[engine.advisory]`, is the catch-all. An engine newer than this Orion may report an advisory it has no specific id for. It is printed with the engine's own message rather than dropped. Seeing one means the server binary is older than the definitions it is checking.

## Options

| Flag | Description |
|------|-------------|
| `--deny-warnings` | Exit non-zero on advisory findings too, not only errors. |
| `--requires-channel` | Channel name that may be referenced without being in the set. Repeatable, directory mode only. |
| `--requires-connector` | Connector name that may be referenced without being in the set. Repeatable, directory mode only. |
| `--definitions` | Directory holding the set's shared `constants`, `errors` and `fragments`. Implicit when linting a directory. |
| `--plugin-dir` | Directory of plugin manifests (`plugin.toml`) beyond the set's own tree, so a workflow naming a plugin function is checked against the manifest's field table. Repeatable. A `plugin.toml` inside the linted directory is found without it. |

## Examples

```
$ orion-server lint ./definitions
note: definitions/request.json is not a channel, workflow or connector — skipped
error: [closure.connector] workflow 'auth-login': connector 'app-db' is neither in the set nor declared on the boundary
warning: [closure.channel_call_dynamic] workflow 'route': resolves channel_call targets dynamically — closure checking cannot cover those calls
./definitions: 0 connector(s), 62 workflow(s), 62 channel(s) — 1 error(s), 1 warning(s)
```

```bash
orion-server lint examples/packages/high-value-order/workflow.json
```

## Related

- [Test a workflow offline](../../../guides/author/testing.md): lint, dry-run and test in a CI gate.
- [Advisory checks (`clippy`)](../../clippy/index.md): the rules that run after lint passes.
- [Shared definitions](../shared-definitions.md): the `$from` and `use` forms a set may use.
- [Errors and response envelopes](../../errors.md): the field codes a finding names.
- [`orion-server` commands](./index.md): every `orion-server` subcommand.
