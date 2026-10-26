<!-- description: Why a connection revision holds no secret value: references move to secret locators, and a literal credential is refused before it is stored. -->
<!-- type: reference -->
<!-- last_verified: 2026-10-25 -->

# Secret masking

Why a connection read has nothing to mask.

A connection revision stores no secret value. Its `secret_locators` map a dotted config path to a reference such as `env://ORDERS_DB_URL` or `vault://secret/data/db#url`. The node resolves the reference when the connection is used. A read of the revision therefore gives back the references, which are pointers, not secrets.

`public_config` refuses anything that looks like a credential, before it is stored:

- a secret-shaped key, such as `password`, `token` or `secret`, with a literal value;
- an `Authorization` value;
- credentials in a URL.

`orion-cli compile` applies the same check to a definition set, so a literal credential fails at compile rather than at deploy. Author connectors with `env://` or `vault://` references.

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [Secrets by reference](./secrets.md): the references a locator holds.
- [Deployment endpoints](../admin-api/deployments.md#connections): connection revisions and their locators.
