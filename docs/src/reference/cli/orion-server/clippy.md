<!-- description: orion-server clippy runs the advisory rules beyond lint, only where certain, with --fix, --list, --explain, JSON output and the -c config. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# `orion-server clippy`

Advisory checks beyond `lint`, said only when certain: the `cargo clippy` to `lint`'s `cargo check`. The rules, each with the proof it rests on and when it stays silent, are on [Advisory checks](../../clippy/index.md). No configuration, no suppression.

## Synopsis

```bash
orion-server clippy <dir | file> [--deny-warnings] [--format text|json] [--fix [--check]]
                    [--definitions DIR] [--requires-channel NAME]... [--requires-connector NAME]...
orion-server clippy --list
orion-server clippy --explain <rule>
```

## Description

A set whose [`package` document](../shared-definitions.md#the-package-document) declares a `requires.orion` range excluding this binary stops before any rule runs. One line names the range and this version.

Needs no database or server. It takes the serving config with `-c` for the three rules that read `[vars]`, `[secrets]` and the `[models]` ceilings.



`lint` runs first. Its findings are re-reported, and when it reports an *error* the rules do not run — the summary says `fix those first`. Diagnostics go to stderr in `lint`'s line format. They carry a `file:line:col:` prefix wherever the source file has the same coordinates as the compiled form (no `use`, no `$from`). The one-line summary goes to stdout.

## Options

| Flag | Description |
|------|-------------|
| `<dir>` / `<file>` | A directory is checked as a set (every rule); a single file as a set of one — the set-scoped rules have nothing to compare it with. |
| `--deny-warnings` | Exit non-zero on warnings too. |
| `--format json` | One JSON object per diagnostic on stdout — `level`, `rule`, `entity`, `file`, `path`, `line`, `column`, `message`, `remedy`, `via`, `fixable` — and nothing else, for editors and pipelines. |
| `--fix` | Apply the fixes the rules can prove to the source files, then report what remains. See [Fixes](#fixes). |
| `--check` | With `--fix`: print a diff of each file that would change, write nothing, and exit `1` when anything would. |
| `--list` | Every rule with its level, scope and summary. |
| `--explain RULE` | One rule's rationale, its proof and when it is silent. |
| `--definitions`, `--requires-*`, `--plugin-dir`, `--model-dir` | As `lint` takes them. A plugin function's `template_at` fields are analysed as the server evaluates them once its manifest is in the set; a model manifest is what the `lint` gate checks literal `model_infer` references against. |
| `-c FILE` (global) | The serving instance's config. Only a config you name counts: the defaults say nothing about `[vars]`, `[secrets]` or `[models]`. |

## Returns

| Exit code | Meaning |
|---|---|
| `0` | No error. Warnings may have been printed. |
| `1` | A `lint` error, a `deny`-level rule, or a warning under `--deny-warnings`, counted after any `--fix`. With `--fix --check`, a file that would change. |
| `2` | The path is not a set, or a usage error. |

## Fixes

`--fix` applies a rewrite only where a rule proved it exact, which today is [`perf.redundant_step_condition`](../../clippy/perf-redundant-step-condition.md#fix). Each edit is checked before it is written. The edited file is compiled again and must produce exactly the folded workflow. The set is then linted with the edit in place. A file that fails either check is left alone. A written file takes the house style, as `orion-server fmt` writes it. Each edit is reported on stderr:

```
fixed definitions/claims.json: folded `claim`, `read` into group `when_claim`
note: [perf.redundant_step_condition] definitions/pay.json: not fixed — steps `pay.check`, `pay.charge` are not written in this file — …
```

Only steps written in the file are edited. A run a fragment or an `$each` produced is reported and left, because the edit would belong in the fragment and change every caller. A refused fix leaves its warning, which still fails `--deny-warnings`. In CI, `clippy --fix --check` fails a change that `--fix` would rewrite and prints the diff.

## Examples

```
$ orion-server clippy ./definitions
definitions/workflows/auth-login.json: warning: [perf.redundant_step_condition] workflow 'Auth - login' at tasks[15].tasks[0].condition: 2 consecutive steps (`send_otp` and `when_unverified`) repeat this condition, and none of them writes what it reads; it is evaluated 2 times for one answer
        fix: wrap them in a task group carrying the condition once: { "id": …, "condition": …, "tasks": [ … ] }
note: [correctness.metadata_var_undeclared] skipped — needs the serving config (-c <config.toml>)
./definitions: 59 workflow(s), 62 channel(s), 9 connector(s) — 0 error(s), 1 warning(s) from 17 rule(s)
```

```bash
orion-server -c config.toml clippy ./definitions --deny-warnings
```

## Related

- [Advisory checks (`clippy`)](../../clippy/index.md): every rule, with its proof and when it stays silent.
- [`orion-server lint`](./lint.md): the gate that runs first.
- [Test a workflow offline](../../../guides/author/testing.md): where the advisory checks sit in a pipeline.
- [`orion-server` commands](./index.md): every `orion-server` subcommand.
