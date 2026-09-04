<!-- description: How to read Orion reference pages: field-table conventions, version applicability, stable error contracts and where implementation rationale lives. -->
# Using the Reference

**Page type:** Reference · **Audience:** Developers looking up an exact contract

Orion reference pages describe the release documented by this site. A running
instance remains authoritative for its function catalog, OpenAPI document, and
reported version.

## Field-table conventions

Reference entries use the same sequence wherever the fields apply:

1. purpose and exact wire name,
2. syntax, endpoint, or enclosing object,
3. fields with type, required state, default, and constraints,
4. version applicability,
5. complete minimal example,
6. result shape,
7. likely errors and corrective action,
8. related concept and task guides.

In a **Required** column, `yes` means the field must be present, `no` means it
may be omitted, and `conditional` means the description states when it is
required. An em dash means there is no default or the cell does not apply.

Examples use exact API field names such as `workflow_id`, even when prose uses
“workflow ID.” Values in angle brackets, such as `<trace-id>`, are placeholders
and must be replaced.

## Version applicability

Unmarked reference material applies to Orion 1.0 and later. Material marked
`**Since:** Orion x.y` requires that release or a newer one. The per-version
[upgrade guides](../operate/upgrades.md) remain the authoritative inventory of
behavior changes between releases.

Important post-1.0 authoring features include:

| Feature | Available since | Reference |
|---|---:|---|
| Rooted regression-test `expect` paths | 1.2 | [Test Workflows Offline](../build/testing.md#every-expect-path-names-its-root) |
| Nested task groups and `terminal` steps | 1.2 | [Workflow JSON Schema](./workflows.md#task-groups) |
| Complete runtime function discovery | 1.2 | [Function Reference](./functions.md#inspecting-schemas-at-runtime) |

Consult the release's configuration reference when running an older binary;
new settings may be rejected as unknown rather than ignored.

## Errors are part of the contract

Use HTTP status for the broad outcome and `error.code` for program logic. The
human-readable `message` can change and must not be parsed. Every task guide
should link likely failures to [Errors & Response Envelopes](./errors.md).

| Situation | Typical status/code | Correction |
|---|---|---|
| Definition has an invalid or missing field | `400 VALIDATION_ERROR` | Correct the field path in `details` and validate again |
| Requested entity does not exist | `404 NOT_FOUND` | Check the ID and instance |
| ID, name, route, or package version conflicts | `409 CONFLICT` | Inspect the existing resource or create a new version |
| Channel or dependency cannot serve | `503 SERVICE_UNAVAILABLE` | Check health, quarantine, connector state, and backpressure |

The complete registry is in [Errors & Response Envelopes](./errors.md).

## Contract versus rationale

Field tables and endpoint descriptions are normative. Paragraphs labelled
**Rationale** explain why a contract has that shape but do not add hidden client
requirements. Deeper implementation decisions belong in [Design
Notes](./design-notes.md).

## Related

- [Reference Index](./index.md) — choose the contract by task.
- [Support & Compatibility](./support.md) — release and upgrade guarantees.
- [OpenAPI Specification](./openapi.md) — machine-readable API contract.
