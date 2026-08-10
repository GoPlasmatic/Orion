Update an existing workflow by ID. The workflow_json contains only the fields to update — unspecified fields remain unchanged.

After updating, call `engine_reload` to activate changes.

Updatable fields: `name`, `priority`, `condition`, `description`, `tags`, `tasks`, `continue_on_error`.

See `workflows_create` for the full workflow JSON format, task structure, and JSONLogic reference.
