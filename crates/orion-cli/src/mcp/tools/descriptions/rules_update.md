Update an existing rule by ID. The rule_json contains only the fields to update — unspecified fields remain unchanged.

After updating, call `engine_reload` to activate changes.

Updatable fields: `name`, `channel`, `priority`, `condition`, `description`, `tags`, `tasks`, `continue_on_error`.

See `rules_create` for the full rule JSON format, task structure, and JSONLogic reference.
