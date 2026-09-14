<!-- description: How http_call builds a request: the four header layers in precedence order, and the three query-parameter layers that all survive. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Request layering

The order header and query-parameter layers apply in when `http_call` builds a request.

## Header precedence

When `http_call` builds a request, header layers apply in order. Later layers override earlier ones:

| Priority | Source |
|----------|--------|
| 1 (lowest) | Connector `headers` |
| 2 | Connector `auth` |
| 3 | Default `content-type: application/json` (only when the request has a body) |
| 4 (highest) | Task-level `headers` in the `http_call` input |

Task headers always win. A workflow may override `content-type`, `authorization`, or any header the connector sets.

## Query-parameter precedence

Some APIs authenticate with credentials **in the query string**: legacy SMS and telecom gateways, older payment and lookup APIs. `query_params` is their home:

```json
{
  "type": "http",
  "url": "https://gw.example.com/api.aspx",
  "query_params": {
    "uid": "env://SMS_UID",
    "pwd": "env://SMS_PWD"
  }
}
```

Parameters are applied in this order, and all three layers survive:

| Order | Source |
|-------|--------|
| 1 | The connector `url`'s own query string |
| 2 | A query string on the task's `path` |
| 3 | Connector `query_params` |

**Do not put credentials in the connector `url` instead.** It works, and it fails two ways. Export masks a query value whose *name* looks secret, and `pwd` is masked where `pass` is not. Re-import then refuses the masked literal, so the connector cannot be promoted between instances. The resolved URL is also interpolated into every timeout and failure message. That reaches traces, the DLQ, server logs, OTel spans, the trace read API and the admin connector probe's response body. A caller can reach the trace read API with its own `x-trace-token`, so this is not an admin-only exposure.

`query_params` avoids all of that because the values are **never merged into the URL**. They are applied at the request builder, so the SSRF-validated URL and every error message stay credential-free. They cannot ride a cross-host redirect, the same rule headers and auth already follow. They are percent-encoded, so a secret containing `&`, `=` or a space works where URL interpolation would silently corrupt it.

Two behaviours to know:

- **Order is sorted, not authored.** Parameter order is observable on the wire and matters to signature-based gateways, so the map is stored sorted rather than in an order that would vary per call.
- **A name already present in the connector `url`'s query is refused at authoring time**: the request would otherwise carry it twice with an undefined tie-break.

Values mask on admin reads like header values do, and a `env://`/`vault://` reference survives export → import intact. Use references rather than literals for anything secret: a masked literal cannot be re-imported.

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [`http`](./http.md): the `headers` and `query_params` fields these layers read.
- [`http_call`](../functions/http_call.md): the task-level headers that win.
- [Authentication](./authentication.md): the layer between connector headers and the default content type.
