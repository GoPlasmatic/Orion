# orion-client

The HTTP client for [Orion](https://github.com/GoPlasmatic/Orion)'s admin and
data APIs — the same transport `orion-cli` and the server's `orion-server
package` promotion CLI both drive, so there is exactly one implementation of
the wire protocol in the workspace.

Response types come from [`orion-api`](https://crates.io/crates/orion-api);
this crate is the transport over them.

```rust
use orion_client::{OrionClient, paths};
use orion_api::WorkflowResponse;

# async fn example() -> Result<(), orion_client::ClientError> {
let client = OrionClient::new("http://localhost:8080")?
    .with_api_key("my-admin-key".to_string(), None);

// `*_data` unwraps the `{"data": …}` envelope every admin 2xx carries.
let workflow: WorkflowResponse = client.get_data(&paths::workflow("orders")).await?;
println!("{} is {}", workflow.workflow_id, workflow.status);
# Ok(())
# }
```

## What it gives you

- **`OrionClient`** — auth (API key or bearer, custom header), an optional
  `X-Orion-Change-Context` stamp for audit trails, and a configurable timeout.
- **`ClientError`** — a typed error that keeps the server's error envelope
  structured, so you branch on `status` and `code` rather than matching prose.
- **`paths`** — every endpoint path built in one place, instead of format
  strings scattered across call sites.

Two families of verbs, matching the two things a caller can want:

| Verbs | Return |
|---|---|
| `get` / `post` / `put` / `patch` / `delete` | the **full response body**, for passthrough tools that print responses verbatim |
| `get_data` / `post_data` / … | the payload with the **`{"data": …}` envelope unwrapped**, tolerating the bare pre-1.0 shape |

Presentation stays out of scope: hints, colours and message wording belong to
the calling binary. This crate reports *what happened*; the caller decides how
to say it.

## Versioning

**This crate shares the workspace version.** Every crate in the Orion repository
is released together at one version, so `orion-client X.Y.Z` is the client for
`orion-server X.Y.Z`. The v2 admin contract is strict, and skew is negotiated
rather than tolerated: the server reports `orion_api::v2::API_VERSION`, and
`check_api_version` refuses a server speaking another.

## Licence

Apache-2.0. See [LICENSE](https://github.com/GoPlasmatic/Orion/blob/main/LICENSE).
