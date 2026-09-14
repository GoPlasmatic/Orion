<!-- description: orion-server preflight scans the stored channels and workflows for anything the 1.0 rules refuse, and lists engine advisories, without changing a row. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `orion-server preflight`

Scans stored channels and workflows for anything the 1.0 rules refuse. That is configs that no longer parse, tasks the validator rejects, and `data_query`/`data_write` tasks with no `schema`. It is read-only, and config-file problems are `validate-config`'s job; this reads what only the database knows.

## Synopsis

```bash
orion-server [-c <config.toml>] preflight
```

## Description

The report has two sections and only the first gates. **Breaks** are numbered by their [checklist row](../../../releases/upgrade-to-1.0/index.md) and make the command exit non-zero, so `orion-server preflight || exit 1` is a deploy gate. **Advisories** carry the same ids `lint` uses: `[engine.unguarded_validation]`, `[engine.group_continue_on_error]`, `[logic.tensor_operator_key]`. They name a stored workflow that serves correctly and says less than its author meant it to, and they never change the exit code. For the tensor key, `preflight` reports more than `lint` does. On a stored estate a `{"shape": {"var": …}}` written before 1.8 cannot have meant a call. The dynamic objects are therefore listed here as well as the constant ones.

Stored workflows are checked against the functions the estate serves: the built-ins plus every function the **active** plugins declare, read from the same database. A workflow calling a function of an archived plugin is therefore a break, because it would not activate. One calling an active plugin's function is not.

## Examples

```bash
orion-server -c config.toml preflight
```

## Related

- [Upgrade to 1.0](../../../releases/upgrade-to-1.0/index.md): the checklist rows the breaks are numbered by.
- [Upgrade to 1.8](../../../releases/upgrade-to-1.8.md): the tensor-key advisory this command reports.
- [Upgrade an instance](../../../operate/maintain/upgrades.md): where preflight sits in the procedure.
- [`orion-server` commands](./index.md): every `orion-server` subcommand.
