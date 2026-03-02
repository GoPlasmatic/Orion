Import rules from a JSON array string. Each element must be a complete rule definition (see `rules_create` for format).

Use `dry_run=true` to preview what would be imported without making changes.

After importing, call `engine_reload` to activate the imported rules.
