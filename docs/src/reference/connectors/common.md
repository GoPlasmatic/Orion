<!-- description: The blocks every connector carries whatever its type: identity, secret references, secret locators, authentication, request layering, gates and retries. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Shared connector blocks

What every connector carries whatever its type, and the rules that apply to all four.

| Page | Holds |
|---|---|
| [Definition and identity](./identity.md) | the name, type and config, and the connection revision compile makes of them. |
| [Secrets by reference](./secrets.md) | `env://` and `vault://`, the reserved `ORION_` prefix, and when a reference resolves. |
| [Secret masking](./masking.md) | why a connection revision holds no secret value. |
| [Authentication](./authentication.md) | `bearer`, `basic`, `apikey`, and the managed `oauth2` grant. |
| [Request layering](./request-layering.md) | the header and query-parameter layers, in the order they apply. |
| [Operation gates](./operation-gates.md) | the `operations` block, per type, and what each gate blocks. |
| [Retries and circuit breakers](./reliability.md) | the `http` retry loop, its deadline, and the per-channel breaker. |

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [Task functions](../functions/index.md): the functions that call through a connector.
- [Deployment endpoints](../admin-api/deployments.md#connections): the connection revision a connector becomes.
- [The three types](./types.md): the three types, and the `config` each takes.
