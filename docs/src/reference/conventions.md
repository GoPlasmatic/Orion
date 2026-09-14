<!-- description: How to read Orion reference pages: the one page skeleton, field-table columns and legend, version markers, freshness stamps, and where rationale lives. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Reference conventions

Reference pages describe the release this site documents. A running instance remains authoritative for its function catalogue, its OpenAPI document and its reported version. This page is how to read the others: the skeleton, the tables, the markers, and what is contract.

## Synopsis

Every reference page carries these sections, in this order, and omits the ones that do not apply. A section is never renamed.

| Section | Holds |
|---|---|
| Synopsis | The signature, the shape, the grammar, or the full default configuration |
| Description | What the thing does, in present-tense declaratives, with no instruction |
| Parameters | A table of fields, options or properties, with fixed columns |
| Returns | The result, output, response, or exit code |
| Errors | Every error the thing can raise, with the condition |
| Examples | One complete example per common use, each with a one-line title |
| Caveats | The surprising behaviours, as bullets |
| Compatibility | What changed and when, inline with the version number |
| Related | The concept page that explains it and the guide that uses it |

## Parameters

A field table has fixed columns in a fixed order: name, type, required, default, description. A description is one sentence. Enumerated values are each listed with one line.

| Value in the `Required` column | Meaning |
|---|---|
| `yes` | The field must be present |
| `no` | The field may be omitted |
| `conditional` | The description states when it is required |
| `one of …` | Exactly one of the listed fields must be present |
| a protocol or mode name, such as `rest` or `hmac` | Required exactly when that protocol or mode applies |

A field typed **JSONLogic** takes an expression evaluated against the data context. A plain JSON literal is valid JSONLogic and evaluates to itself, folded once when the engine is built. Only a field that reads the message is evaluated per request. The fields that are not JSONLogic are named on the function's page. They are target selectors such as `connector`, validated enums and security switches such as `crypto.op` and `http_call.method`, and the document-shaped fields listed under [Connector fields](./expressions.md#connector-fields-expressions-and-documents).

An em dash in the `Default` column means there is no default, or the cell does not apply. Examples use exact wire names such as `workflow_id`, even where prose says "workflow ID". A value in angle brackets, such as `<trace-id>`, is a placeholder you replace.

## Errors

Branch on the HTTP status for the broad outcome and on `error.code` for program logic. The human-readable `message` can change and must not be parsed. [Errors and response envelopes](./errors.md) holds the complete registry.

| Situation | Typical status and code | Correction |
|---|---|---|
| A definition has an invalid or missing field | `400 VALIDATION_ERROR` | Correct the field path in `details` and validate again |
| The requested entity does not exist | `404 NOT_FOUND` | Check the id and the instance |
| An id, name, route or package version conflicts | `409 CONFLICT` | Inspect the existing resource or create a new version |
| A channel or a dependency cannot serve | `503 SERVICE_UNAVAILABLE` | Check health, quarantine, connector state and backpressure |

## Caveats

- Field tables and endpoint descriptions are normative. A paragraph explaining why a contract has a shape adds no client requirement; the deeper reasoning is in [Design notes](../concepts/design-notes.md).
- The CLI and the Console wrap the admin API. Where the three differ in wording, the API page is the contract.
- The tables on [Task functions](./functions/index.md), [Metrics](./metrics.md), [Expression language](./expressions.md), [Errors](./errors.md) and [Server configuration](./configuration/index.md) are asserted against the code by the test suite. A disagreement fails the build, so those tables are the code's own statement.

## Compatibility

Unmarked material applies to Orion 1.0 and later. A `**Since:** Orion x.y` marker means the feature needs that release or a newer one. The per-version [upgrade guides](../releases/index.md) are the inventory of behaviour changes between releases; a page never restates them.

A page also carries one of two freshness stamps in its footer. **Last verified** names the date a person ran the page against the release it documents. **Generated from** names the version and date a generated page was produced from.

| Feature | Since | Reference |
|---|---:|---|
| Rooted regression-test `expect` paths | 1.2 | [Root every `expect` path](../guides/author/testing.md#root-every-expect-path) |
| Nested task groups and `terminal` steps | 1.2 | [Task groups](./workflows.md#task-groups) |
| Complete runtime function discovery | 1.2 | [Inspecting schemas at runtime](./functions/runtime-discovery.md) |

Consult the release's configuration reference when running an older binary. A newer setting is rejected as unknown rather than ignored.

## Related

- [Reference](./index.md): choose the contract by task.
- [Versioning and support policy](../releases/versioning-policy.md): what a release number promises.
- [OpenAPI specification](./openapi.md): the machine-readable API contract.
