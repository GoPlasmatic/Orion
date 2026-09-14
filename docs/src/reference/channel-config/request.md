<!-- description: The request block of a channel: the auto and payload body modes, what each does to data and metadata, and the cookies a workflow may read. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `request`

`request` controls how the HTTP request body becomes `data` and `metadata`, on HTTP ingresses only. Kafka parses the whole payload as `data` and builds metadata separately. `channel_call` inherits the parent's metadata with `data` from the task input, so neither is affected.


## Fields

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `body_mode` | string | no | `"auto"` | `auto` detects the Orion envelope; `payload` takes the parsed body verbatim. |
| `cookies_to_metadata` | array of strings | no | — | Named request cookies copied to `metadata.cookies.*`. Absent exposes nothing. |

Under `auto`, **an object carrying a top-level `data` or `metadata` key is the envelope**: that key becomes the payload and every sibling field is discarded. Anything else (an array, a scalar, an object without those keys) is the payload as it stands, and an empty body is `{}`.

That rule keys on a field *name*. A request model that owns the name `data` — the standard FCM/push payload shape, among others — is read as an envelope. It loses its siblings silently, with a normal `200`. `payload` mode is the opt-out:

```json
{
  "config": {
    "request": { "body_mode": "payload" }
  }
}
```

The two modes differ for **exactly one input shape**: a top-level object carrying `data` or `metadata`. Everything else already took the payload path in both.

Three consequences worth knowing before switching a channel:

- **A caller cannot supply `metadata` at all** in `payload` mode — the metadata object is server-stamped keys only (`channel`, `http_method`, and `params`/`query`/`headers` where applicable). Under `auto`, a caller-supplied `metadata.params` or `metadata.query` survives when the server has none of its own to stamp, so this is a small security win as well as a trade-off.
- **Downstream, consistently:** `validation_logic` sees the whole body under `data`, and `cache.cache_key_fields` paths resolve against it. HMAC signing is unaffected — it always signed the raw bytes.
- **`orion-cli send` needs `--raw` for a payload-mode channel.** The CLI wraps its argument in `{"data": …}` by default, which a payload-mode channel then delivers as `data = {"data": …}`. `orion-cli send my-channel --raw -d '…'` sends the payload verbatim. Because such a channel accepts no caller metadata, `--raw` and `--metadata` are refused together rather than one being dropped.

> [!WARNING]
> Flipping a **live** channel from `auto` to `payload` changes its wire contract for any caller currently sending a legitimate `{"data": …}` envelope — that envelope becomes the payload, so the workflow starts reading `data.data.*`. It is a config change with the blast radius of a code change.

### Reading request cookies

The `Cookie` header is masked to `"******"` before request metadata is built, along with `authorization`, `proxy-authorization` and `x-api-key`. The metadata map is persisted verbatim into `traces.result_json` and `trace_dlq.metadata_json`, so a plaintext value there is a plaintext credential at rest.

Not every cookie is a credential, though. `cookies_to_metadata` names the ones a workflow may read:

```json
{
  "config": {
    "request": { "cookies_to_metadata": ["browser_uuid"] }
  }
}
```

and then, in any task or in `validation_logic`:

```json
{ "var": "metadata.cookies.browser_uuid" }
```

A listed-but-absent cookie is not present: never `null`, never an error. The raw `Cookie` header stays masked: this allowlist is additive and never unmasks it. `metadata.cookies` is platform-reserved, stamped from the allowlist and stripped otherwise, so a caller cannot supply it in an envelope.

**Scope it to opaque identifiers a workflow matches against its own stored state**: a browser-pinning id, a first-party visitor id, a bucket cookie. For a session token, JWT or CSRF token use [`auth.mode: "jwt"`](./auth.md) with `source: {"cookie": …}` instead. There the token is consumed at verification rather than copied into the context.

> [!WARNING]
> **Allowlisted values land in `traces.result_json` and `trace_dlq.metadata_json` unmasked.** The read side is covered — `GET /admin/traces/{id}` strips all of `context.metadata`, but the row on disk is not. Note also that `tracing.mode = "off"` suppresses only *sync* persistence: on an `/async` channel the row is still written before the `202`, so turning tracing off is **not** a complete mitigation there. `trace_queue.retention_hours` is the ageing-out control.

Two further limits worth knowing:

- **A cookie-varying channel must not enable `cache`.** `compute_cache_key` hashes method, params, query and payload — never headers, so a cached response would replay one caller's `Set-Cookie` to the next.
- **`rate_limit.key_logic` still cannot see cookies.** Its context is `{client_ip, channel, headers}`, and `cookie` is not among the readable headers. Per-cookie rate limiting stays out of reach. It cannot see the authenticated principal either, because it runs before authentication — [`principal_rate_limit`](./principal_rate_limit.md) is the block that can.

`channel_call` propagates metadata verbatim, so an allowlisted cookie reaches sub-channels — the same way verified claims do.

## Related

- [Data API › Request body](../data-api.md#request-body): the envelope the `auto` mode detects.
- [`orion-cli send`](../cli/orion-cli/send.md): the `--raw` flag a payload-mode channel needs.
- [Traces and async processing](../../operate/run/traces.md): where allowlisted cookies land on disk.
- [`auth`](./auth.md): the `jwt` mode with a cookie source, for session tokens.
- [Channel configuration](./index.md): every key, with its page.
