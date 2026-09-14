<!-- description: The blocks every connector carries whatever its type: identity, secret references, masking, authentication, request layering, gates and retries. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Shared connector blocks

What every connector row carries whatever its type, and the rules that apply to all seven.

| Page | Holds |
|---|---|
| [Definition and identity](./identity.md) | the name, config, enabled flag and tags, and the two masked copies a read returns. |
| [Secrets by reference](./secrets.md) | `env://` and `vault://`, the reserved `ORION_` prefix, and when a reference resolves. |
| [Secret masking](./masking.md) | the allowlist a read masks by, and what survives export then import. |
| [Authentication](./authentication.md) | `bearer`, `basic`, `apikey`, and the managed `oauth2` grant. |
| [Request layering](./request-layering.md) | the header and query-parameter layers, in the order they apply. |
| [Operation gates](./operation-gates.md) | the `operations` block, per type, and what each gate blocks. |
| [Retries and circuit breakers](./reliability.md) | the `http` retry loop, its deadline, and the per-channel breaker. |

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [Task functions](../functions/index.md): the functions that call through a connector.
- [Admin API — Connectors](../admin-api/connectors.md): the endpoints, and the reachability probe.
- [The seven types](./types.md): the seven types, and the `config` each takes.
