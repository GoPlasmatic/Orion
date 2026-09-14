<!-- description: The http_call task function: call an external API through an HTTP connector with a JSONLogic path, headers and body, retries and a circuit breaker. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `http_call`

Makes an HTTP request through an HTTP connector, with retry and circuit-breaker
support. The connector supplies the base URL and auth.

## Synopsis

```json
{
  "name": "http_call",
  "input": {
    "connector": "partner-api",
    "method": "POST",
    "path": {
      "cat": [
        "/orders/",
        {
          "var": "data.order_id"
        }
      ]
    },
    "headers": {},
    "body": {
      "var": "data.order"
    },
    "body_format": "json",
    "output": "data.result",
    "response_format": "json",
    "timeout_ms": 30000
  }
}
```

## Description

`http_call` is a connector function. It names a [connector](../connectors/index.md) for its credentials and endpoint. Orion validates its `input` when the workflow is saved, and the call runs through the connector's circuit breaker.

**Retry safety:** `depends_on` `method`. See [Retry safety](./retry-safety.md) for what the answer costs.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string \| JSONLogic | yes | — | Name of the HTTP connector. A computed name is not yet supported |
| `method` | string | no | `"GET"` | `GET` \| `POST` \| `PUT` \| `PATCH` \| `DELETE`. The one field here that is not JSONLogic |
| `path` | string \| JSONLogic | no | — | Path appended to the connector's base URL. Accepts the pre-1.0 name `path_logic` |
| `headers` | object | no | `{}` | Extra request headers. Each **value** is JSONLogic |
| `body` | any \| JSONLogic | no | — | Request body. Accepts the pre-1.0 name `body_logic` |
| `body_format` | string \| JSONLogic | no | `"json"` | How the body becomes request bytes: `json`, `form`, or `text`; see [Body formats](#body-formats) |
| `output` | string \| JSONLogic | no | — | Dotted path where the response body is written; omit to discard it. Accepts the pre-1.0 name `response_path` |
| `response_format` | string \| JSONLogic | no | `"json"` | How the response is captured at `output`: `json` (parsed) or `text` (a plain string) |
| `timeout_ms` | number \| JSONLogic | no | `30000` | Per-request timeout in milliseconds |

Every field above except `method` is JSONLogic. It may be written as a plain literal, which is what it evaluates to, or as an expression over the message. A literal is folded once when the engine is built and costs nothing per request; only a field that reads the message pays. `headers` is the one that changes what is expressible. A value can be computed, so a bearer token or a correlation id no longer has to be injected by the service layer.

### Body formats

`json` (the default) serializes the body as JSON. `form` URL-encodes an object's entries as `application/x-www-form-urlencoded` pairs, which is what OAuth 2.0 token endpoints and form-style APIs require. Scalars encode directly, and arrays of scalars become repeated keys (`to=a&to=b`). `null` entries are skipped, so one body shape with conditionally null entries expresses optional parameters. Nested values are rejected; a bracket path like `"metadata[order_id]"` is an ordinary key. `text` sends a string body verbatim, which with an explicit `content-type` header covers XML, CSV, or any other textual payload. Each format stamps its own `content-type` (`application/json`, `application/x-www-form-urlencoded`, `text/plain; charset=utf-8`). A `content-type` set in `headers` or on the connector replaces the stamp: it changes the label, never the bytes.

### Response formats

`json` (the default) parses the response and fails if it
is not valid JSON. `text` captures the body as a plain string, for gateways that answer `text/plain`, leaving the size cap and the non-2xx error path unchanged. Unknown values on either axis are rejected when the workflow is created, **when both are written as literals**. A literal `body` is shape-checked against a literal `body_format` at the same time. A computed body or format gets the same check per request instead.

## Examples

```json
{
  "name": "http_call",
  "input": {
    "connector": "partner-api",
    "method": "POST",
    "path": { "cat": ["/orders/", { "var": "data.order_id" }] },
    "headers": {
      "Authorization": { "cat": ["Bearer ", { "secret": "partner_token" }] },
      "X-Correlation-Id": { "var": "metadata.request_id" }
    },
    "body": { "var": "data.order" },
    "output": "data.result"
  }
}
```

```json
{
  "name": "http_call",
  "input": {
    "connector": "payment-api",
    "method": "POST",
    "path": "/charge",
    "body": { "var": "data.payment" },
    "output": "data.charge_result",
    "timeout_ms": 5000
  }
}
```

```json
{
  "name": "http_call",
  "input": {
    "connector": "webex-oauth",
    "method": "POST",
    "path": "/v1/access_token",
    "body_format": "form",
    "body_logic": {
      "grant_type": "refresh_token",
      "refresh_token": { "var": "temp_data.refresh_token" }
    },
    "output": "temp_data.token_response"
  }
}
```

## Related

- [Connectors](../../concepts/connectors.md): why credentials and endpoints live on a connector.
- [Connect a database or API](../../guides/author/connectors.md): creating the connector this function names.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Connector types](../connectors/index.md): the connector fields, retries and circuit breakers behind the call.
