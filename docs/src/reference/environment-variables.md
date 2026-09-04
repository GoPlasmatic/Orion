<!-- description: The five ways Orion reads the environment — ORION_* overrides, ${VAR} substitution, env://, vault:// and var:// references, and declared vars and secrets. -->
# Environment Variables

Orion reads the process environment five ways, and which one applies is decided by *where the value sits*, not by what the value is. The same value written into a config file, a connector and a workflow is not read the same way in all three.

| Mechanism | Syntax | Where you write it | Read |
|---|---|---|---|
| Setting override | `ORION_SECTION__KEY` | the environment itself | once, at startup |
| Text substitution | `${VAR}`, `${VAR:-default}`, `$$` | the config file, a connector `config` *(deprecated)* | before the text is parsed |
| Secret reference | `env://VAR`, `vault://…` | selected parsed string fields | at every load and reload |
| Var reference | `var://name` | a stored connector or channel config | at every load and reload |
| Declared value | `[vars]` / `[secrets]` | the config file, read by name from a workflow | once, at startup |

The first belongs to [Configuration Reference](./configuration.md): every setting has an override, and that page names them all. The rest are this page's subject. They are not interchangeable — `${VAR}` rewrites *text* before it is parsed, `env://` rewrites a *parsed value* after, which is why both reach a connector and only one reaches a config file.

The last is different in kind: it does not rewrite anything. The operator declares a name in the config file, and a workflow reads that name. Which is why it is the one to reach for when a *workflow* needs a value that varies by environment. See [Values a workflow reads by name](#values-a-workflow-reads-by-name).

`var://name` is the same declaration reaching a *stored config* instead of a message. `[vars]` is one section listing everything that differs per instance; `var://` is how a connector or a channel — neither of which the config file can see — reads one. See [Parameterising a stored config](#parameterising-a-stored-config).

## `${VAR}` in a connector is deprecated

`${VAR}` reaches a stored connector `config` for historical reasons: it predates `env://` and the two overlap there and nowhere else. **Use `env://VAR` instead.** A connector whose stored config still carries a placeholder logs a deprecation warning at load and keeps resolving; it will stop resolving in a future release.

The two are not equivalent, and the differences all favour `env://`:

- **`${VAR}` is invisible to masking.** The policy that lets a reference survive an API read recognises `env://VAR` as a *name* and passes it through, while masking a literal as `"******"`. `${VAR}` is neither — it is an ordinary string, so it exports as itself while the credential beside it is masked.
- **`${VAR}` substitutes text, not a value.** It runs before the JSON is parsed, so a variable holding `", "admin": true` edits the document's *structure*. `env://` replaces one already-parsed string with one string.
- **`${VAR}` has no offline reading.** `lint`, `clippy` and `dry-run` have no process environment standing in for the deployment's, so a placeholder is opaque to them; `env://` is inventoried by the reference scan.

Rewriting is mechanical — `"${DB_URL}"` becomes `"env://DB_URL"` — with one caveat: `${VAR:-default}` has no `env://` equivalent, because a defaulted credential is a credential that fails open. Give the variable a value in every environment, or move the fallback into the connector as a literal non-secret.

## Where a reference resolves

`env://` is not a template pass over your JSON. Each reference is resolved by the code that reads one particular field, so it works where a resolver is wired in and nowhere else:

| Surface | `${VAR}` | `env://VAR`, `vault://…` | `var://name` |
|---|---|---|---|
| Config file passed with `-c` | yes | `[secrets]` values only | no — it *is* the declaration |
| Connector `config` — any string field, at any depth | yes, deprecated | yes | yes |
| Channel config — any field except `*_logic` | no | no¹ | yes |
| Channel `auth` — `keys`, `secret`, `secrets`, `jwt_keys[].key` | no | yes | yes |
| Workflow task `crypto` — `key` | no | yes | no² |
| Workflow task `jwt_sign` — `key` | no | yes | no² |
| Workflow task `jwt_verify` — `keys[].key`, `issuer`, `audience` | no | yes | no² |
| Everything else in a workflow | no | no | no² |

¹ Only the four `auth` fields on the row below resolve a secret reference; the rest of a channel config takes `var://` for the non-secret values it needs.
² A workflow reads a declared value *by name* instead — `{"var": "metadata.vars.name"}` for a var, `{"secret": "name"}` for a secret. A stored config has no message to read from, which is why it needs a reference form and a workflow does not.

A workflow carrying one anywhere else is **refused**: `POST /workflows`, `POST /workflows/validate` and `orion-server lint` all report `UNRESOLVED_SECRET_REF` naming the field, because `env://` at the head of a string has no reading in which it is data. Before that check existed the reference was simply a string — a task with `"path": "env://API_BASE"` requested a URL spelled `env://API_BASE` and failed with whatever the backend made of it.

> [!WARNING]
> `${VAR}` has no such guard, because `${…}` is ordinary text in a query or a template. A workflow embedding `${TENANT}` sends those nine characters to the backend. Nothing substitutes them.

The three workflow entries are exceptions with a reason: a signing key has nowhere else to live. Everything else that varies between environments belongs in a **connector**, or — since 1.3 — in `[secrets]`, which those same three fields also read. The workflow names the connector or the secret, the value lives outside the definition, and the workflow document is then byte-identical in dev, staging and production, which is what makes a package promotable at all.

`vault://<api-path>#<field>` resolves wherever `env://` does, reading HashiCorp Vault when `VAULT_ADDR` and `VAULT_TOKEN` are set. The cloud schemes `aws-sm://`, `gcp-sm://` and `azure-kv://` are reserved: a reference using one is refused rather than handed to the backend as a literal credential. The forms are in [Connector Types › Secrets by reference](./connectors.md#secrets-by-reference).

## Parameterising a stored config

A connector and a channel live in the **database**, not in the config file, so `${VAR}` never reaches them, and promoting a package from staging to production would otherwise carry staging's TTLs, rate limits and hostnames with it. `var://name` is how one stored config reads a value the instance declares:

```toml
[vars]
cache_ttl = 300
rate_limit_rps = 50
idp_issuer = "https://login.${ENVIRONMENT}.example.com"
```

```json
{
  "cache":      { "enabled": true, "ttl_secs": "var://cache_ttl" },
  "rate_limit": { "requests_per_second": "var://rate_limit_rps", "burst": 10 }
}
```

Three things follow from `var://` resolving *before* the config is typed:

- **A var keeps its type.** `"ttl_secs": "var://cache_ttl"` becomes the number `300`, not the string `"300"`, which matters because the fields worth varying per instance are mostly numbers, and a string where a number belongs simply fails to parse. This is the one respect in which `var://` differs from `env://`, whose values are always strings.
- **A reference is the whole string.** `"var://cache_ttl"` resolves; `"ttl is var://cache_ttl"` is that text.
- **An undeclared name refuses the row.** The connector or channel does not load, and the error names the reference and lists what `[vars]` does declare. Passing `var://cache_ttl` through as its own text is how a TTL silently stops being a number.

**A channel's `*_logic` fields are skipped**: `validation_logic`, `authorization_logic`, and the rate-limit and cache `key_logic`. Those are JSONLogic evaluated per *message*, so a literal `"var://x"` inside one is a string the author wrote to compare against, not a value to substitute. Read a var there the way a workflow does, with `{"var": "metadata.vars.x"}`.

> [!NOTE]
> `var://` is for values that are **not** secret — a TTL, a limit, a hostname, a topic prefix. Vars are stamped into every message's metadata and appear in traces, which is the point. A credential belongs in `[secrets]` or on a connector; see [Which section a value belongs in](#values-a-workflow-reads-by-name).

## Values a workflow reads by name

A reference resolves in five workflow fields and a channel's `auth` block. Everything else a workflow needs from the environment it reads *by name*, from one of two config sections the operator declares:

```toml
[vars]
kafka_topic_prefix = "${KAFKA_TOPIC_PREFIX:-dev}"

[secrets]
partner_hmac = "env://PARTNER_HMAC_KEY"
```

```json
{ "topic": { "cat": [{ "var": "metadata.vars.kafka_topic_prefix" }, "order.placed"] } }
```

```json
{ "op": "hmac", "key": { "secret": "partner_hmac" }, "data": { "var": "data.body" } }
```

The two are the same declaration model with opposite trace contracts, and that is the whole distinction:

| | `[vars]` | `[secrets]` |
|---|---|---|
| A workflow reads it as | `{"var": "metadata.vars.<name>"}` | `{"secret": "<name>"}` |
| Where it lives at runtime | stamped into every message's `metadata` | held by the engine, never in a message |
| In traces | **yes**, deliberately | **no**, structurally |
| Values must be | literals | `env://` / `vault://` references |
| A workflow expression reads it | yes | yes |
| A channel guard reads it | yes | yes |
| A stored connector or channel config reads it | yes, as `var://<name>` | no — put the credential on a connector, or in `auth` |

**Vars are recorded on purpose.** An operator asking "which topic did this run publish to?" is asking to see them, and a deployment constant hidden from the trace makes that question unanswerable. They are stamped at every ingress — HTTP and Kafka alike — over whatever the caller sent, so an envelope-mode request cannot name its own topic prefix, and they keep the type they were written as.

**Secrets are unrecordable rather than redacted.** The store belongs to the engine, not to the message, so a secret cannot appear in a trace snapshot, a `map` mapping clone or a response body — there is nothing to strip. The engine enforces the two ways that could go wrong: a workflow that reads a secret where the result would be recorded (a `map` mapping, a `log` field) is refused when the engine is built, as is one naming a secret the instance does not declare. Both surface as a quarantined channel naming the reason.

Each section refuses the other's value shape, because either mistake is silent. A literal in `[secrets]` is a key in the deployment's file tree; a reference in `[vars]` reaches the workflow as the characters `env://PARTNER_HMAC_KEY`, because nothing resolves one on the way into metadata.

> [!NOTE]
> `{"secret": …}` resolves in a **channel guard** too — `validation_logic`, `authorization_logic`, and the rate-limit and cache `key_logic`, because secrets are start-time config, resolved before any channel loads, and the guard engine is built over the same store the workflow engines get. Channel authentication additionally takes `env://` in its four key-bearing fields.
>
> A stored *config value* is the one place a secret does not reach: `var://` resolves there and `{"secret": …}` does not. That is deliberate — a config value is not evaluated, so there is nothing to hold the result, and a credential a backend needs belongs on the connector that dials it.

Offline, there is no config file to read either section from. `orion-server dry-run --secrets` and a `*.case.json` `secrets` block supply stand-in values; `metadata.vars` is written directly into the case's `metadata`. See [Test Workflows Offline](../build/testing.md).

## Author every credential as a reference

```json
{ "name": "orders-db", "config": { "type": "db", "connection_string": "env://ORDERS_DB_URL" } }
```

```json
{ "config": { "auth": { "mode": "hmac", "secret": "env://STRIPE_WEBHOOK_SECRET" } } }
```

The stored row holds a variable name, so a database dump is not a leak and one document deploys everywhere. A **literal** credential is worse than untidy: API reads mask it as `"******"`, and an import carrying `"******"` is refused, so a connector authored with a literal cannot be promoted between instances at all. The table of what survives an export is in [Promote Between Environments](../operate/promotion.md#secrets-survive-the-trip--if-authored-as-references).

For the three workflow fields there is a second reason. Connector configs can be encrypted at rest with `storage.connector_encryption_key`; **workflow documents cannot**. A literal key in a `crypto` or `jwt_sign` task sits in the `workflows` table in clear, and every version of that workflow keeps it.

Two habits close the gap that references alone leave open:

- **Keep credentials out of URLs.** Use a connector's [`query_params`](./connectors.md#query-parameter-precedence) rather than embedding a resolved value in the endpoint. A URL reaches error messages, logs, spans and trace rows; the redaction that masks it there matches conventional parameter names and is a backstop, not the control.
- **Encrypt what is left.** `connector_encryption_key = "env://ORION_SECRET_CONNECTOR_KEY"` puts an AES-256-GCM envelope around stored connector configs, so a dump carries neither the credential nor a readable config. See [Secure an Instance](../operate/security.md#keep-credentials-out-of-the-database).

## Rotate without downtime

Every credential Orion checks on the way in accepts more than one value, so rotation is two deploys instead of a flag day. Add the new variable, deploy, cut clients over, then remove the old one:

| Credential | The list | Syntax per entry |
|---|---|---|
| Channel `api_key` | `auth.keys` — any match authorizes | `env://` |
| Channel `hmac` | `auth.secret` plus `auth.secrets` — each tried in constant time | `env://` |
| Admin API | `admin_auth.api_keys` | `${VAR}` in the config file, or the whole list through `ORION_ADMIN_AUTH__API_KEYS` |

A `vault://` reference is re-read on every reload, so a renewed token applies without a restart. An `env://` value is read from the process environment, which a running process cannot change — rotating one means restarting or redeploying the server.

## Naming the variables

Name them anything, with one restriction: Orion refuses to start on an `ORION_*` variable that follows the override grammar without being one of its settings, so a typo costs a boot rather than a week of a silently ignored setting. A secret that must live in that namespace needs the reserved prefix — `env://ORION_SECRET_STRIPE_API_KEY`, which Orion never reads as configuration. The exemptions, and why the scan looks at the `__` rather than the `ORION_`, are in [Configuration Reference › Misspellings are startup errors](./configuration.md#misspellings-are-startup-errors-not-silent-no-ops).

## What an unset variable does

| Where the reference sits | If the variable is not set |
|---|---|
| Config file, `${VAR}` | Startup fails, naming the file and the position. |
| Config file, `${VAR:-default}` | The default is used. An empty default is legal. |
| Config file, `[secrets]` | **Startup fails**, naming the entry and never its value. The alternative is an instance that runs and fails at the remote system with nothing pointing back here. |
| Connector `config` | That connector is **skipped at load**. The server still starts and every other connector serves; `/health` reports `components.connectors: degraded`, and the connector's row in `GET /api/v1/admin/connectors` carries `load_status: "failed"` with a `load_error`. Every task using it fails. |
| Channel `auth` | The channel is **quarantined** — refused at every ingress rather than served with the guard missing. `/health` reports `components.channels: degraded`. |
| Workflow `crypto` / `jwt_sign` / `jwt_verify` | The task fails when it runs, with a validation error naming the field. Nothing catches it earlier: the value is read at execution. |
| A workflow field that resolves nothing | Refused at create, update and `lint` with `UNRESOLVED_SECRET_REF` — the variable is never consulted, because the reference would never have been resolved. |
| A workflow naming an undeclared `{"secret": …}` | The engine refuses the workflow, and the channel is **quarantined** rather than served with the key resolving to `null`. |
| A reserved scheme | Refused wherever it is resolved, so an unresolvable reference never reaches the remote system as its own text. |

Creating or updating a connector does **not** resolve its references — `POST /connectors` checks the config's shape, not this host's environment, so a connector authored on a laptop is accepted there and reveals the missing variable at load. `POST /connectors/validate` reports an unresolvable reference as a *warning* for the same reason: a CI runner holds no production secrets and must still be able to check a bundle. Neither `/health` line is a 503; both are the degraded-but-serving state described in [Troubleshooting](../operate/troubleshooting.md#health-says-degraded-but-returns-http-200).

## Inventory what a set needs

`orion-server lint ./definitions` ends with one `note:` per secret the set references, naming each one and the files that mention it: `[env.reference]` for each variable an `env://` reference needs in the environment, and `[secrets.reference]` for each name a `{"secret": …}` needs in the serving instance's `[secrets]` section. Both are exit-neutral — neither the exit code nor `--deny-warnings` counts a note, because the machine running `lint` is not the machine that will serve the set, so its environment and its config say nothing about whether the value will be present where it matters.

The scan is textual: it walks every string in every definition, so it reports what a set *mentions*. A reference in a workflow field that resolves nothing no longer hides among them — `lint` fails the set with `[env.unresolved]` before the inventory is worth reading, but a connector or channel field is still inventoried without any claim that the variable will be set where it matters.

## Supplying them

With Compose, the container takes them as ordinary environment entries alongside the `ORION_*` overrides:

```yaml
environment:
  ORION_STORAGE__URL: "postgres://user:pass@db:5432/orion"
  ORDERS_DB_URL: "${ORDERS_DB_URL:?set this}"
  STRIPE_WEBHOOK_SECRET: "${STRIPE_WEBHOOK_SECRET:?set this}"
```

With the Helm chart, `extraEnv` renders verbatim into the container's `env:`, so it carries connector and channel secrets as well as `ORION_*` overrides, and `extraEnvFrom` maps whole Secrets:

```yaml
extraEnv:
  - name: ORDERS_DB_URL
    valueFrom:
      secretKeyRef: { name: orion-connectors, key: orders-db-url }
extraEnvFrom:
  - secretRef: { name: orion-connector-secrets }
```

`orion-server validate-config` prints the merged configuration without starting, which checks the config-file half: secret-looking keys come back `******` and passwords inside URL-shaped values are struck out in place, so the output is safe to paste into an issue. It says nothing about connectors and channels — those live in the database and are resolved at load, so `/health` and the connector list are what report them.

## Related

- [Configuration Reference › Vars and Secrets](./configuration.md#vars-and-secrets): the two declaration sections, and every other setting with its default and `ORION_*` override.
- [Expressions › Secrets](./expressions.md#secrets): the `secret` operator's rules, and what the engine refuses.
- [Connector Types › Secrets by reference](./connectors.md#secrets-by-reference): the reference forms, the Vault path grammar, and the masking policy that decides what survives a read.
- [Channel Configuration › Authentication](./channel-config.md#authentication): the `auth` fields that take references, and what a failure looks like from outside.
- [Secure an Instance](../operate/security.md#keep-credentials-out-of-the-database): the practices above in their operational context.
- [CI/CD with Packages](../guides/ci-cd.md#secrets-never-travel): why one artifact reaches staging and production unchanged.
