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
use orion_api::{Digest, ErrorEnvelope, codes};

// Content identities are checked types, not opaque strings.
let digest = Digest::parse(
    "sha256:0000000000000000000000000000000000000000000000000000000000000000",
)?;
assert_eq!(digest.as_str().len(), 71);

// Errors carry a stable, machine-readable code — branch on the constant.
let failed = r#"{"error": {"code": "NOT_FOUND", "message": "no such deployment"}}"#;
let err: ErrorEnvelope = serde_json::from_str(failed)?;
assert_eq!(err.error.code, codes::NOT_FOUND);
# Ok::<(), Box<dyn std::error::Error>>(())
```

## What is here

| Module | Holds |
|---|---|
| `digest` | `Digest`, the checked `sha256:<64 lowercase hex>` content digest |
| `dto` | one struct per response body the operational admin API serves |
| `engine` | engine status, load issues, capabilities and cache-invalidation answers |
| `enums` | the closed value sets (status, protocol, …) and their string constants |
| `envelope` | the `{"data": …}` and paginated envelopes the operational admin 2xx responses carry |
| `error` | the `{"error": {code, message, details[], request_id}}` envelope, the stable `codes` registry, and the `field_codes` vocabulary |
| `time` | the wire timestamp helpers |
| `v2` | the strict, versioned `/api/v2/admin` request and response contract |

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

**This crate shares the workspace version.** Every crate in the Orion repository
is released together at one version, so `orion-api X.Y.Z` describes the wire
format of `orion-server X.Y.Z`. The operational DTOs are tolerant (every field
defaults, unknown fields are ignored); the v2 admin contract (`v2`) is strict,
and skew is negotiated through `v2::API_VERSION`.

The `utoipa` feature adds `ToSchema` derives so the server can publish these
exact types in its OpenAPI document. Clients leave it off.

## Licence

Apache-2.0. See [LICENSE](https://github.com/GoPlasmatic/Orion/blob/main/LICENSE).
