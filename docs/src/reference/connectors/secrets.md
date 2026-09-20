<!-- description: Holding a credential by reference: the env:// and vault:// schemes, the reserved ORION_ prefix, and what an unset variable does at load. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# Secrets by reference

How a connector config names a credential instead of holding one, and when the reference is resolved.

Any string field in `config` may hold `env://VAR_NAME` instead of a literal value. Orion resolves the reference each time the connector loads, so credentials never need to be sent to the API or stored in the database. A create or update checks the config's shape, not this host's environment. An unset variable surfaces at the load that follows, where the connector is skipped and its row reports `load_status: "failed"`. [Environment Variables](../environment-variables.md#what-an-unset-variable-does) has the full table.

Name the variables anything you like, with one restriction. Orion [refuses to start](../configuration/how-settings-are-resolved.md#misspellings-are-startup-errors-not-silent-no-ops) on an `ORION_*` variable that is not one of its own settings. A secret that must live in that namespace needs the reserved prefix: `env://ORION_SECRET_STRIPE_API_KEY`.

`vault://<api-path>#<field>` reads from HashiCorp Vault when `VAULT_ADDR` and `VAULT_TOKEN` are set in the server's environment. The schemes `aws-sm://`, `gcp-sm://`, and `azure-kv://` are reserved. A reference using a reserved scheme without a live resolver is refused — it is never handed to the backend as a literal credential.

<details><summary>Vault reference form</summary>

The api-path is exactly what follows `/v1/` in Vault's HTTP API. A KV v2 secret therefore reads as `vault://secret/data/db#password` — the `data/` segment is KV v2's, not Orion's. Field lookup understands both KV shapes: v2's nested `data.data.<field>` first, then v1's flat `data.<field>`. `VAULT_ADDR` and `VAULT_TOKEN` are re-read on every load, so a renewed token applies at the next reload without a restart.

</details>

## References in fields that are not strings

A reference may stand in any field of a connector's `config`, not only a string one. An `http` connector whose peer and its address class differ per deployment reads both from the environment:

```json
{ "url": "env://PEER_API_URL", "allow_private_urls": "env://PEER_API_PRIVATE" }
```

An `env://` or `vault://` reference always resolves to text. In a boolean field the text must be `true` or `false`, in any case. Anything else is a load issue that names the field and never the value, because a mis-pointed reference may hold a credential. A `var://` reference keeps the type its `[vars]` entry was declared with, so it can fill a number too. Create, update, `validate` and `lint` accept a reference in any field, because they run where the deployment's values are not.

An endpoint that arrives by reference is judged when it resolves. An `http` URL that resolves to anything but `http://` or `https://` is an `endpoint` load issue naming the scheme. The private-address check still runs per request, because it needs DNS and must see every redirect.

## A reference is the whole value

`env://API_KEY` is a reference only when it is the entire string. Inside a longer string it is text, sent exactly as written:

```json
"headers": { "Authorization": "Bearer env://API_KEY" }
```

That header carries the characters `env://API_KEY`, not the key. `lint`, `package lint` and `POST /connectors/validate` warn about it with the check id `env.embedded_reference`. For a bearer token, use the [`auth`](./authentication.md) block, whose `token` takes a reference:

```json
"auth": { "type": "bearer", "token": "env://API_KEY" }
```

## Related

- [Connector types](./index.md): every type, and the shared blocks all of them carry.
- [Environment variables](../environment-variables.md): every scheme, and what an unset variable does.
- [Secret masking](./masking.md): why a literal secret does not survive export then import.
- [Definition and identity](./identity.md): the config field a reference lives in.
