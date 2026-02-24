# CLI

[← Back to README](../README.md)

`orion-cli` is a full-featured management tool for Orion — like `kubectl` for your rules engine.

```bash
# List rules in a formatted table
orion-cli rules list --output table

# Filter by channel and tag
orion-cli rules list --channel orders --tag alerts

# Diff local rules file against the live server
orion-cli rules diff -f rules.json

# Import rules with dry-run to preview changes
orion-cli rules import -f rules.json --dry-run

# Test a rule with trace output
orion-cli rules test <id> -d '{"data": {"total": 25000}}' --trace

# Send data to a channel
orion-cli send orders -d '{"data": {"total": 25000}}'

# Send async and wait for result
orion-cli send orders -d '{"data": {"total": 25000}}' --async --wait

# Check engine status
orion-cli engine status

# Generate shell completions
orion-cli completions zsh
```

All commands support `--output table|json|yaml` for flexible output formatting.

## Full Command Reference

| Command | Subcommands |
|---------|-------------|
| `rules` | `list`, `get`, `create`, `update`, `delete`, `activate`, `pause`, `archive`, `test`, `export`, `import`, `diff` |
| `connectors` | `list`, `get`, `create`, `update`, `delete`, `enable`, `disable` |
| `send` | *(direct)* — supports `--async`, `--wait`, `--batch`, `--metadata` |
| `jobs` | `get`, `wait` |
| `engine` | `status`, `reload` |
| `metrics` | *(direct)* — supports `--raw` |
| `health` | *(direct)* |
| `config` | `set-server`, `show`, `set` |
| `completions` | bash, zsh, fish, powershell, elvish |

## Global Flags

`--server <url>`, `--output <format>`, `--quiet`, `--verbose`, `--no-color`, `--yes`

## CLI Config File

`~/.orion/config.toml`
