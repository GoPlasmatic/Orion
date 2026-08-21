List audit log entries from the Orion server. Audit logs record admin actions such as creating, updating, or deleting workflows, channels, connectors, and backups.

Each entry includes the principal (who performed the action), the action type, the resource type and ID, a `details` object, and a timestamp. Results are ordered newest first. Supports pagination with limit and offset.

Filters are applied in the database and combine with AND. `action`, `resource_type`, `resource_id` and `principal` match **exactly** — there is no prefix or substring matching, so a filter is only as good as the vocabulary behind it; `start_time` (inclusive) and `end_time` (exclusive) bound `created_at`.

An unrecognised filter name is rejected with a 400 rather than answered with unfiltered rows, so a mistyped query can never come back looking like a clean answer.
