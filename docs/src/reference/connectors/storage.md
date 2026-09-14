<!-- description: The `storage` connector config: the bucket, region, endpoint and credentials for presigned URLs and object metadata, with no data path. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `storage` connectors

The `config` fields of a `storage` connector, which backs S3-compatible object storage, presign and metadata only.

S3-compatible object storage for [`storage_presign`](../functions/storage_presign.md) and [`storage_head`](../functions/storage_head.md), with a deliberately **zero-data-path** surface. Presigning is local SigV4 arithmetic over the connector's credentials, and `storage_head` is one bounded metadata request. Object bytes never move through the runtime. Works against any S3-compatible
store: AWS, Linode/Akamai, Cloudflare R2, Backblaze B2, Wasabi, and
self-hosted Garage / SeaweedFS / RustFS (usually with `force_path_style`).

```json
{
  "name": "media",
  "connector_type": "storage",
  "config": {
    "type": "storage",
    "endpoint": "https://ap-south-1.linodeobjects.com",
    "region": "ap-south-1",
    "bucket": "media-bucket",
    "access_key": "env://S3_ACCESS_KEY",
    "secret_key": "env://S3_SECRET_KEY"
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `provider` | string | no | `"s3"` | Signing scheme. `s3` covers every S3-compatible store; GCS/Azure later are new values |
| `endpoint` | string | yes | — | Base URL, for example `https://s3.us-east-1.amazonaws.com` |
| `region` | string | yes | — | SigV4 signing region |
| `bucket` | string | yes | — | The bucket this connector reaches — deliberately connector-owned: a second bucket is a second connector |
| `access_key` | string | yes | — | Access key id (masked on reads — use `env://` references) |
| `secret_key` | string | yes | — | Secret key; literal or `env://VAR` |
| `session_token` | string | no | — | STS temporary-credential token, signed as `X-Amz-Security-Token` |
| `force_path_style` | boolean | no | `false` | Path-style addressing (`endpoint/bucket/key`) — most self-hosted stores want `true` |
| `allow_private_urls` | boolean | no | `false` | Allow a private/internal endpoint for `storage_head`'s network call |
| `timeout_ms` | integer | no | `10000` | `storage_head` timeout; presigning makes no network call |
| `operations` | object | no | all allowed | `presign_get` / `presign_put` / `head` — `presign_put: false` makes a media connector read-only |

`POST /api/v1/admin/connectors/{name}/test` performs one signed HEAD of the
bucket. There is no retry field: presigning is local computation, and
`storage_head` follows the estate rule that only `http` connectors retry.

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [Task functions](../functions/index.md): the functions that call through a connector.
- [Operation gates](./operation-gates.md): the `operations` block this type carries.
- [Definition and identity](./identity.md): the row the `config` sits in.
