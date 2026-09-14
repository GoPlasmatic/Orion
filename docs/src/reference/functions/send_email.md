<!-- description: The send_email task function: send transactional email through an SMTP connector with JSONLogic recipients, subject and body, and no automatic retry. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `send_email`

Sends transactional email through an [SMTP connector](../connectors/smtp.md).
Transport, credentials, TLS mode, and the default sender live on the
connector; the message lives here.

## Synopsis

```json
{
  "name": "send_email",
  "input": {
    "connector": "mailer",
    "to": {
      "var": "data.email"
    },
    "cc": "…",
    "bcc": "…",
    "subject": {
      "cat": [
        "Order ",
        {
          "var": "data.order_id"
        },
        " confirmed"
      ]
    },
    "text": {
      "cat": [
        "Your OTP is ",
        {
          "var": "temp_data.otp"
        }
      ]
    },
    "html": "…",
    "from": "…",
    "reply_to": "…",
    "headers": {},
    "output": "temp_data.mail_result"
  }
}
```

## Description

`send_email` is a connector function. It names a [connector](../connectors/index.md) for its credentials and endpoint. Orion validates its `input` when the workflow is saved, and the call runs through the connector's circuit breaker.

**Retry safety:** `unsafe_write`. See [Retry safety](./retry-safety.md) for what the answer costs.

**No automatic retries.** A timeout after the message body is transmitted is indistinguishable from an accepted message. SMTP has no idempotency key, so a retry would be a duplicate email. The circuit breaker still applies.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `connector` | string | yes | — | Name of the SMTP connector |
| `to` | string \| array | yes | — | Recipients; each is `addr@example.com` or `Name <addr@example.com>` |
| `cc` / `bcc` | string \| array | no | — | Same address forms |
| `subject` | string | yes | — | UTF-8 subject |
| `text` | string | one of `text`/`html` | — | Plain-text body |
| `html` | string | one of `text`/`html` | — | HTML body; with `text` too, the message is `multipart/alternative` |
| `from` | string | no | connector `from` | Honored only when the connector sets `allow_from_override` |
| `reply_to` | string | no | — | The `Reply-To` address |
| `headers` | object | no | — | Extra headers (string values). Structured names (`From`, `To`, `Subject`, `Content-Type`, …) are rejected — this is for `List-Unsubscribe`, `Auto-Submitted`, correlation IDs |
| `output` | string \| JSONLogic | no | `"data"` | Where `{ "message_id", "response" }` is stored — the generated Message-ID (for correlation/threading) and the server's acceptance line |

A wrong address fails at workflow create when static (naming the field and
index) and at send time when resolved from the message. Rejected recipients
fail the task with the server's reply — no partial-success reporting.

Every field above except `connector`, `headers` and `output` is JSONLogic. A body or a subject can therefore be composed in the task rather than in a `map` task ahead of it:

## Examples

```json
{
  "name": "send_email",
  "input": {
    "connector": "mailer",
    "to": { "var": "data.email" },
    "subject": { "cat": ["Order ", { "var": "data.order_id" }, " confirmed"] },
    "text": { "cat": ["Your OTP is ", { "var": "temp_data.otp" }] },
    "output": "temp_data.mail_result"
  }
}
```

## Related

- [Connectors](../../concepts/connectors.md): why credentials and endpoints live on a connector.
- [Connect a database or API](../../guides/author/connectors.md): creating the connector this function names.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Connector types](../connectors/index.md): the connector fields, retries and circuit breakers behind the call.
