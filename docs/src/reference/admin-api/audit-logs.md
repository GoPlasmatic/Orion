<!-- description: The audit log endpoint: its filters, time range and paging, what a row records, and the change-context header that groups a multi-request operation. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Audit log endpoints

Querying the row every admin mutation writes, and what that row holds.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/admin/audit-logs` | List audit log entries, newest first. Filters (AND-combined, exact match): `?action=`, `?resource_type=`, `?resource_id=`, `?principal=`; time range: `?start_time=` (inclusive) and `?end_time=` (exclusive), RFC 3339; paging: `?offset=`, `?limit=` (clamped to 1–1000, default 50). An unknown parameter returns `400` |

**What a row records.** Every admin mutation writes one, including
`POST /workflows/{id}/test`, which runs the workflow's tasks against live connectors and so is a side-effecting operation, not a dry run.

- `principal` — the actor. `key-<16 hex>` for an authenticated caller, or
  `anonymous` when `admin_auth.enabled = false`. The id is derived as
  `SHA-256("orion:audit:key-id:v1" ‖ SHA-256(key))`, truncated to 8 bytes: it
  is stable for a given key (the same value whether the key is configured in
  plaintext or `sha256:` form), distinct for keys that share a prefix, and
  cannot be reversed to the key. Hold the config and you can recompute it to
  map a row back to a key you issued; nobody else can go in either direction.
- `details` — a JSON object with the request context: `request_id` (the same
  value as the `x-request-id` header and `error.request_id`), `client_ip`,
  `user_agent`, and `change_context` when the request carried an
  `X-Orion-Change-Context` header — free-form, truncated at 256 bytes;
  promotion tooling stamps `package=<name>@<version>` on every call of an
  apply so the trail groups the whole operation. Both attacker-controlled inputs are truncated before storage
  (256 bytes for `user_agent`, 200 for a supplied `x-request-id`). Fields that
  are unavailable are omitted rather than recorded empty.

  `client_ip` follows the `rate_limit.trusted_proxies` policy, which applies
  whether or not `rate_limit.enabled` is set, so a forged `X-Forwarded-For`
  cannot dictate it. The flip side is that with the **default empty list**, forwarded headers are ignored entirely and the recorded address is the direct peer. Behind an ingress or load balancer, that is the proxy's address on every row. List your proxies in `rate_limit.trusted_proxies` to record
  the real client.

Rows are written asynchronously, so admin responses never wait on the INSERT. The queue is bounded (`audit.max_pending`) and drained at shutdown (`audit.drain_timeout_secs`), so a mutation accepted moments before `SIGTERM` is still recorded. Anything that does not make it is counted in `orion_audit_events_dropped_total`.

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Audit logs](../../operate/run/audit-logs.md): every action and resource type a row can carry.
- [Audit log settings](../configuration/audit.md): retention, cleanup and the write queue.
- [Export and promotion](./export-and-promotion.md): the header that groups a promotion's rows.
