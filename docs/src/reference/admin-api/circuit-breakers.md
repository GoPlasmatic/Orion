<!-- description: The circuit-breaker endpoints: list the breaker states this node holds per channel and connector, and reset one breaker on every node. -->
<!-- type: reference -->
<!-- last_verified: 2026-10-25 -->

# Circuit breaker endpoints

Reading and resetting the breakers that guard each `(channel, connector)` pairing.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v2/admin/connectors/circuit-breakers` | List this node's circuit breaker states |
| POST | `/api/v2/admin/connectors/circuit-breakers/{key}` | Reset one circuit breaker. The key is `channel:connector` |

```bash
curl -s http://localhost:8080/api/v2/admin/connectors/circuit-breakers | jq '.data'
curl -s -X POST http://localhost:8080/api/v2/admin/connectors/circuit-breakers/orders:orders-db
```

**Breakers are node-local.** The list answers this node's breakers only, and says so with `"scope": "node"` and its `instance_id`. Another replica may hold a different state for the same key.

**A reset reaches every node.** In cluster mode the reset fans out, so one call clears the key on every replica. The breaker you want to clear is often open on a node other than the one that answered. For that reason a key this node does not hold is a `404` only on a single node. Every reset is audited as `reset` / `circuit_breaker`.

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [Retries and circuit breakers](../connectors/reliability.md): when a breaker opens, and the settings behind it.
- [Engine settings](../configuration/engine.md#circuit-breaker): where the breaker settings live.
