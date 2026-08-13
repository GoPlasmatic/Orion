# orion-api

The wire contract for [Orion](https://github.com/GoPlasmatic/Orion), a
declarative services runtime: every JSON shape its HTTP API serves that a
client is expected to read back.

`orion-server` serializes these types and clients deserialize them, so a client
cannot drift from the server's wire shapes. If you are writing a Rust client
against an Orion instance, depend on this crate and on
[`orion-client`](https://crates.io/crates/orion-client) — the HTTP transport
built over it — rather than re-deriving the shapes yourself.

```rust
use orion_api::{ErrorEnvelope, WorkflowResponse, codes};

let body = r#"{"data": {"workflow_id": "orders", "status": "active"}}"#;
let env: orion_api::DataEnvelope<WorkflowResponse> = serde_json::from_str(body)?;
assert_eq!(env.data.workflow_id, "orders");

// Errors carry a stable, machine-readable code — branch on the constant.
let failed = r#"{"error": {"code": "NOT_FOUND", "message": "no such workflow"}}"#;
let err: ErrorEnvelope = serde_json::from_str(failed)?;
assert_eq!(err.error.code, codes::NOT_FOUND);
# Ok::<(), serde_json::Error>(())
```

## What is here

| Module | Holds |
|---|---|
| `dto` | one struct per response body the admin API serves |
| `enums` | the closed value sets (status, protocol, …) and their string constants |
| `envelope` | the `{"data": …}` and paginated envelopes admin 2xx responses carry |
| `error` | the `{"error": {code, message, details[], request_id}}` envelope, the stable `codes` registry, and the `field_codes` vocabulary |
| `import` | the bulk-import report and its vocabulary |

## Reading responses across versions

Response deserialization is deliberately tolerant: every `dto` field defaults,
unknown fields are ignored, and fields holding a growing vocabulary — `status`,
`state`, `channel_type`, `mode` — are `String` rather than an enum. A client
one release away from the server keeps parsing. Compare those strings against
the `enums` constants instead of deserializing into the enum, and an
unrecognised value degrades gracefully rather than failing the response.

The `enums` types themselves are strict on purpose: they serve the request
side, where an unrecognised value is a caller error worth a `400`.

## Versioning

**This crate's version is independent of the Orion server's.** It is published
as a rider alongside server and CLI releases, so `orion-api 1.2.0` does not
imply and is not implied by any particular `orion-server` version. Its *Rust*
API is semver'd on its own version; the *wire format* it describes is covered
by the server's `/api/v1/` contract, which holds for the life of the server's
1.x line.

The `utoipa` feature adds `ToSchema` derives so the server can publish these
exact types in its OpenAPI document. Clients leave it off.

## Licence

Apache-2.0. See [LICENSE](https://github.com/GoPlasmatic/Orion/blob/main/LICENSE).
