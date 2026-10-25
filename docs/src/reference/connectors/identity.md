<!-- description: A connector's name, type and config as a definition set holds them, and the immutable connection revision that compile turns them into. -->
<!-- type: reference -->
<!-- last_verified: 2026-10-25 -->

# Definition and identity

The fields every connector carries in a definition set, and what it becomes in a deployment.

You write a connector as a file in a definition set:

```json
{
  "name": "payments-api",
  "connector_type": "http",
  "config": {
    "type": "http",
    "url": "https://api.stripe.com/v1",
    "auth": { "type": "bearer", "token": "env://STRIPE_API_KEY" }
  }
}
```

- **`name`**: required. Letters, digits, `.`, `_` and `-`. Workflows reference the connector by this name, and it becomes the connection id.
- **`connector_type`**: selects the built-in driver the connection runs on.
- **`config`**: the per-type object documented below. Its `type` field selects the shape.
- **`enabled`**: optional, and only `true` is accepted. Every connection of a deployment loads, so leave a connector out of the set to turn it off.
- **`tags`**: accepted, and not carried. A connection has no tags, and `compile` prints a note.

[`orion-server compile`](../cli/orion-server/compile.md#deployment-directories) turns the connector into a connection revision. Every secret reference in `config` moves to `secret_locators` at its dotted path, and the rest becomes `public_config`. A literal credential is refused. The revision is immutable: a changed config is a new revision ([Connections](../admin-api/deployments.md#connections)).

> [!NOTE]
> Connector configs ignore unknown top-level fields. The `operations`, `retry`, and `dialect` blocks are the exception: each refuses unknown keys, as its section states. A misspelled control would otherwise read as protection while providing none.

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [Deployment endpoints](../admin-api/deployments.md#connections): connection revisions and their secret locators.
- [Secrets by reference](./secrets.md): the `env://` and `vault://` schemes a config field may hold.
- [Secret masking](./masking.md): why a connection holds no secret value.
