<!-- description: The contracts every admin endpoint shares, whatever resource it addresses: the one API key header and the success and error response envelopes. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Admin API conventions

What holds for every admin endpoint whatever resource it addresses.

| Page | Holds |
|---|---|
| [Authentication](./authentication.md) | the one header the server reads, the key forms, rotation, and the failure backoff. |
| [Response envelopes](./envelopes.md) | the success and error bodies, paging and sorting per endpoint, and the common errors. |

## Related

- [Admin API](./index.md): every admin resource, and the contracts they share.
- [OpenAPI specification](../openapi.md): the generated contract, and where to fetch it.
- [Errors and response envelopes](../errors.md): every code these endpoints return.
- [Deployment endpoints](./deployments.md): how definitions reach a node.
