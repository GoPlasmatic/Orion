<!-- description: The `smtp` connector config: the host and port, credentials, TLS mode, the default sender, and what the reachability probe does. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `smtp` connectors

The `config` fields of a `smtp` connector, which backs transactional email over SMTP.

An SMTP server for [`send_email`](../functions/send_email.md): the
lowest-common-denominator mail transport that self-hosted and enterprise
environments already run. Transport, credentials, TLS mode, and the default
sender live here; the message lives on the task.

```json
{
  "name": "mailer",
  "connector_type": "smtp",
  "config": {
    "type": "smtp",
    "host": "smtp.gmail.com",
    "port": 587,
    "tls": "starttls",
    "auth": { "type": "basic", "username": "env://SMTP_USER", "password": "env://SMTP_PASS" },
    "from": "Orion <noreply@example.in>"
  }
}
```

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `host` | string | yes | — | Server hostname — a host, not a URL |
| `port` | integer | no | `587` | `587` pairs with `starttls`, `465` with `implicit`, `25` with internal relays |
| `tls` | string | no | `"starttls"` | `starttls` \| `implicit` \| `none`. Certificates validate against the platform trust store — install a private CA at the OS level; there is deliberately no skip-verification knob. `none` is for local dev relays and draws a validation warning |
| `auth` | object | no | `{"type": "none"}` | `none` (unauthenticated smarthost) or `basic` with `username`/`password` (secret references accepted). Future mechanisms are new tagged variants |
| `from` | string | yes | — | Default sender; `addr@example.com` or `Name <addr@example.com>` |
| `allow_from_override` | boolean | no | `false` | Let a task supply its own `from` |
| `allow_private_urls` | boolean | no | `false` | Allow private and internal addresses — the usual opt-in for an internal relay |
| `timeout_ms` | integer | no | `10000` | Per-send timeout (connect + protocol exchange) |

`POST /api/v1/admin/connectors/{name}/test` probes connect + EHLO + TLS +
authentication without sending mail, through the same pooled transport real
sends use. There is no `retry` field, and `send_email` never retries on its
own: SMTP has no idempotency key, so a re-driven timeout is a duplicate
email. See [Retries](./reliability.md#retries).

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [Task functions](../functions/index.md): the functions that call through a connector.
- [Operation gates](./operation-gates.md): the `operations` block this type carries.
- [Definition and identity](./identity.md): the row the `config` sits in.
