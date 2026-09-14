<!-- description: The response block of a channel: shaped status, headers, body and cookies from the workflow, and per-status error bodies for guard rejections. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `response`

Shaping a sync channel's reply: status, headers, body, cookies, and per-status error bodies. By default every sync channel answers `200` with the fixed envelope `{id, status, data, errors}`, whatever happened. That contract works between workflows and is awkward for a REST API, which is what this block replaces.

## Synopsis

```json
{ "response": { "mode": "shaped", "allowed_headers": ["location"] } }
```

## Description

The fixed envelope allows no `201` with a `Location`, no `404`, and no content type but JSON. See [Errors & response envelopes](../errors.md) for the envelope itself. `mode: "shaped"` hands the status, headers, body and cookies to the workflow instead.

## Fields

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `response.mode` | string | no | `"envelope"` | `envelope` or `shaped`. |
| `response.allowed_headers` | array of strings | no | default allowlist | Headers the workflow may set. **Replaces** the default list, so a channel can narrow it as well as widen it. Case-insensitive. |
| `response.cookies` | boolean | no | `false` | Whether the workflow may set cookies through `data._orion.response.cookies`. Independent of `allowed_headers` — see [Cookies](#cookies) below. |

A shaped channel's workflow writes a control block to `data._orion.response`. Orion drains it before responding — it is control, not content, and never reaches the caller's body:

```json
{
  "id": "respond", "name": "Respond",
  "function": { "name": "map", "input": { "mappings": [
    { "path": "data._orion.response.status",  "logic": 201 },
    { "path": "data._orion.response.headers", "logic": {
        "Location": { "cat": ["/orders/", { "var": "data.order.id" }] } } },
    { "path": "data._orion.response.body_path", "logic": "data.order" }
  ]}}
}
```

The control block's fields:

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `status` | number | no | `200` | HTTP status. Out-of-range values fall back to `200`. |
| `headers` | object | no | `{}` | Response headers, subject to the allowlist below. A value may be a string, or an **array of strings** to send the header once per element. |
| `cookies` | array of objects | no | `[]` | Cookies to set. Requires `response.cookies` on the channel — see [Cookies](#cookies). |
| `body_path` | string | no | whole document | Field to send instead of the entire data document. A leading `data.` is optional. |
| `raw` | boolean | no | `false` | Send a string field verbatim rather than as a JSON string — how a channel returns CSV, XML, or plain text. |

`Content-Type` is `application/json` unless the workflow sets it.

**Header allowlist.** With no `allowed_headers`, a workflow may set `content-type`, `location`, `cache-control`, `etag`, `last-modified`, `retry-after`, `content-language`, and `link`. The hop-by-hop headers (`connection`, `keep-alive`, `proxy-authenticate`, `proxy-authorization`, `te`, `trailer`, `transfer-encoding`, `upgrade`), `content-length`, and `x-request-id` are refused even when listed. Response framing belongs to the server, and `x-request-id` correlates a response with its stored trace. A dropped header does not fail the request.

**Repeated headers.** The first value for a name replaces whatever the platform set, so a workflow's `content-type` still wins, and every later value is appended beside it. That is what makes an array meaningful.

**Failures are soft.** A shaped channel whose workflow sets no control block, or an unusable one, falls back to the standard envelope rather than erroring.

**Interactions.** A cached shaped response replays its status and headers, not only its body, *unless* it sets a cookie, which is never cached. Profiling (`?profile=1`) appends `_orion.profile` to the envelope only — a shaped body is the workflow's own — though timings still reach the trace and metrics. Shaping applies to the synchronous path only; [`/async`](../data-api.md#asynchronous-processing) answers `202` with a trace id as always.

### Cookies

Reading cookies is configured per channel with `request.cookies_to_metadata`. Writing them is the mirror: a shaped channel sets `response.cookies` to `true`, and its workflow declares them rather than assembling the attribute string by hand.

```json
{ "path": "data._orion.response.cookies", "logic": [
  { "name": "session", "value": { "var": "temp_data.jwt" },
    "path": "/", "http_only": true, "secure": true,
    "same_site": "Lax", "max_age": 2592000 },
  { "name": "oauth_state", "value": "", "path": "/", "max_age": 0 }
] }
```

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | string | yes | Cookie name. Must be an RFC 6265 token — no spaces, `=`, `;` or separators. |
| `value` | string | yes | Cookie value. May be empty, which with `max_age: 0` is how a cookie is cleared. |
| `path` | string | no | `Path` attribute. |
| `domain` | string | no | `Domain` attribute. |
| `max_age` | number | no | `Max-Age` in seconds. `0` expires the cookie immediately. |
| `expires` | string | no | `Expires` attribute, as an HTTP date. |
| `same_site` | string | no | `Strict`, `Lax` or `None`. Case-insensitive, emitted canonically. |
| `http_only` | boolean | no | Adds `HttpOnly`. A `false` emits nothing — the attribute has no negative form. |
| `secure` | boolean | no | Adds `Secure`. |

**Why its own switch, not `allowed_headers`.** That list *replaces* the default one. Gating cookies on it would mean a channel setting a session cookie also has to re-list `content-type` to keep serving JSON. The raw escape hatch still works: list `set-cookie` in `allowed_headers` and write the header directly, with an array for more than one. The declared form is what validates the value and spells the attributes for you.

**A response that sets a cookie is never cached.** The response cache keys on the method, path parameters, query and payload, never on who is calling. A stored `Set-Cookie` would therefore be replayed to every caller repeating that request for the TTL. Orion suppresses the cache write instead. This applies however the cookie was set, including through `allowed_headers`.

**Values are validated, and a refusal is reported.** A `value` carrying `;`, a comma, a quote, a backslash, CR or LF is refused. A workflow interpolating user input into a cookie could otherwise inject further attributes or split the response. `path`, `domain` and `expires` refuse `;`, CR and LF for the same reason. `secure`/`http_only` must be real booleans, because coercing the string `"false"` to `true` would be worse than refusing it.

As everywhere on this path the failure is **soft**: the cookie is dropped and the rest of the response still ships with its declared status. It is not **silent**. Every dropped declaration appends a `{code, message, path}` entry to the response envelope's `errors` and increments [`orion_response_drops_total`](../metrics.md). That covers an invalid cookie, a disallowed header, a header value that is not a string, and cookies declared with the switch off. A shaped channel's body belongs to its workflow, so the entries do not reach the caller there; they reach the **trace**. That is where a `302` that quietly did not set a session cookie is otherwise indistinguishable from a browser having refused it. `orion-server clippy` catches the statically decidable half ([`correctness.response_cookie_type`](../clippy/correctness-response-cookie-type.md)).

### Error bodies

Every ingress guard rejection answers with the platform envelope `{"error": {"code", "message", "request_id"}}`. `response.error_bodies` lets a channel replace those **bytes**: for a migrated API whose deployed clients parse a different shape. **The platform still decides the status.**

```json
{
  "config": {
    "response": {
      "error_bodies": {
        "default": { "body": "{\"errorCode\":\"{status}\",\"message\":\"{message}\"}" },
        "401": { "body": "{\"status\":401,\"error\":\"SESSION_EXPIRED\",\"message\":\"{message}\"}" },
        "429": { "body": "…", "content_type": "application/json" }
      }
    }
  }
}
```

Keys are HTTP statuses (`400`–`599`) plus an optional `"default"`. `error_bodies` is **independent of `mode`**: an `envelope` channel can use it, since the two settings answer different questions and `mode` covers only the success path.

| Placeholder | Value |
|---|---|
| `{status}` / `{code}` | The status and the stable error code |
| `{message}` | The platform's message — already redacted, since it comes from the same chokepoint the envelope uses |
| `{request_id}` | The correlation id the envelope carries |
| `{channel}` | The resolved channel name |
| `{timestamp}` | RFC 3339, milliseconds, UTC |

A placeholder is `{` + a lowercase identifier + `}` and nothing else, so ordinary JSON braces need no escaping. Write `{{` and `}}` for a literal brace pair. An **unknown** placeholder is refused at authoring time rather than shipped as a literal. A misspelled `{mesage}` is a body that would be wrong forever.

Applies to the fourteen ingress guard rejections, on both the sync and `/async` paths, which run the same guards. Those are rate limit (`429`, `503`), auth (`401`, `403`), origin (`403`), validation (`400`), dedup (`409`, `503`) and backpressure (`503`).

**Not shapeable:** `413`, the global rate-limit `429`, CORS preflights, and pre-resolution rejections (`404`, `415`, malformed JSON). The body extractor produces `413` before any channel is known. Post-guard errors (`504`, `500`) are out of scope for now.

> [!WARNING]
> **No cause-selectable bodies.** Keying is by status, never by *why* a request was refused. A uniform `401` is an anti-oracle: the response never reveals whether the header was missing, the key was wrong, the signature was malformed or the timestamp was stale. Keying by cause would rebuild exactly that credential oracle. Status is also the honest key — two rejections already share `RATE_LIMITED` and three share `SERVICE_UNAVAILABLE`.

Three further guarantees:

- **Error-owned headers survive.** `retry-after` on a `429` and `WWW-Authenticate` on a refused token are attached by the error itself and are preserved when the body is replaced.
- **Metrics and traces are unaffected.** Rejection counters fire before the response is built, so an operator loses no visibility when a channel changes its bytes.
- **Soft failure.** A template that no longer renders falls back to the platform envelope rather than 500ing — a cosmetic authoring slip must not become an outage. Templates are capped at 4 KiB so a refusal cannot become an amplification primitive, and there is no JSONLogic: there is no engine at guard time, and evaluating expressions over attacker-influenced input on the cheapest-must-be path would be new attack surface for no gain.

## Related

- [Data API › Shaped responses](../data-api.md#shaped-responses): the shaped path from the caller's side.
- [Errors and response envelopes](../errors.md): the standard envelope a shaped channel replaces.
- [Metrics](../metrics.md): `orion_response_drops_total`.
- [`correctness.response_cookie_type`](../clippy/correctness-response-cookie-type.md): the rule that catches a mistyped cookie declaration.
- [Channel configuration](./index.md): every key, with its page.
