<!-- description: The function catalogue endpoint: every task function a workflow may name, with the input-field schema of each one that declares one. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Function endpoints

The endpoint that tells a client which task functions exist, and what each one accepts.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/admin/functions` | The catalogue of every task function a workflow may name, with the input-field schema of each one that declares one (category, type, required flag, description). `source` is `orion` for a handler Orion input-validates at create time and `engine` for a dataflow-rs built-in, which carries no `input_fields`. Used by CLI tools and IDEs for autocompletion and by workflow validators to give field-pathed errors |

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Task functions](../functions/index.md): the same catalogue, written for a reader.
- [Workflow reference](../workflows.md): the `function` block this schema validates.
- [OpenAPI specification](../openapi.md): the generated contract, and where to fetch it.
