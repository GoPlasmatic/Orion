<!-- description: How connector reads and exports mask by allowlist, which values pass through unmasked, and why a literal secret cannot be re-imported. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Secret masking

What a connector read gives back readable, what it replaces, and what that means for export then import.

Connector API reads mask by **allowlist**. A field comes back readable only when its key is on the known-safe list. Every other value — including a secret stored under a key the list never anticipated — returns as `"******"`. Unanticipated secrets fail closed.

- `env://` and `vault://` references pass through unmasked. They are pointers, not secrets, and masking them would break export → import.
- `connection_string` and all header values are always masked.
- Readable URL-shaped values are redacted in band: userinfo and query parameters are stripped.

Exports apply the same masking, so a literal secret does not survive export → import. Author connectors with `env://` references. See [Secrets in an exported bundle](../admin-api/export-and-promotion.md#secrets-in-an-exported-bundle).

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [Secrets by reference](./secrets.md): the references that pass through unmasked.
- [Promote a package](../../operate/maintain/promotion.md): moving connectors between instances.
- [Admin API — Connectors](../admin-api/connectors.md): the endpoints, and the reachability probe.
