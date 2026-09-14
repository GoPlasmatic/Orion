<!-- description: The origin_allow_list key of a channel: a server-side Origin header check on the HTTP ingresses, and how it differs from the platform CORS layer. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `origin_allow_list`

`origin_allow_list` restricts which `Origin` values a channel accepts, server-side.

## Synopsis

```json
{ "origin_allow_list": ["https://app.example.com", "https://admin.example.com"] }
```

## Fields

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `origin_allow_list` | array of strings | no | no check | Accepted `Origin` values. `"*"` allows any origin. |

Rules:

- A request whose `Origin` header is present and unlisted is refused `403` before the workflow runs.
- A request with no `Origin` header is not checked at all.
- Omitting the key checks nothing. `origin_allow_list` is the only accepted spelling.
- The check applies to the HTTP ingresses only — a Kafka record and a `channel_call` have no origin to check.

**This is not CORS.** It performs no handshake, sets no `Access-Control-Allow-Origin`, and takes no part in a preflight. The browser handshake is the platform [`[cors]`](../configuration/cors.md) layer's job. The division of labor:

- **`[cors]` governs the browser handshake.** It short-circuits a genuine preflight from an unlisted origin, but a non-preflighted cross-origin request still runs server-side. The layer omits the response header and the browser discards the answer. Non-browser clients are unaffected entirely.
- **`origin_allow_list` is the server-side check.** It runs on every request that reaches the handler, browser or not, and stops the workflow from executing.

Neither is authentication: `Origin` is client-supplied, and any non-browser caller can set or omit it. For access control that holds against a hostile client, use [`auth`](./auth.md).

## Related

- [CORS settings](../configuration/cors.md): the browser handshake, which this is not.
- [Secure an instance](../../operate/run/security.md): origin checks in context.
- [`auth`](./auth.md): access control that holds against a hostile client.
- [Channel configuration](./index.md): every key, with its page.
