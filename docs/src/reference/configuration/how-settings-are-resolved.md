<!-- description: The three layers an Orion setting resolves through: struct defaults, the config file with ${VAR} and env:// references, then ORION_SECTION__KEY variables. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# How settings are resolved

Three layers, in increasing precedence:

## Description

One more `ORION_*` name is read but is **not** a config setting. `ORION_ADMIN_TOKEN` is the bearer token the [`orion-server package`](../cli/orion-server/package.md) subcommands send when calling a target instance's admin API. It carries no `__`, so the startup scan leaves it alone, like the `ORION_SERVER_URL` / `ORION_API_KEY` pair `orion-cli` reads.

1. **Struct defaults**: everything on this page.
2. **The config file**, passed with `-c`. Values may reference process environment variables two ways:

   - `${VAR}` (required — startup fails if unset) or `${VAR:-default}` (optional), substituted **before** parsing, so it can build a value out of parts. `$$` escapes a literal `$`. The same substitution runs against connector `config_json` blobs at startup, so secrets can stay out of the database.
   - `env://VAR_NAME` as a whole value, resolved **after** parsing — the spelling a connector's `connection_string` already uses. `[storage] url = "env://ORION_STATE_DB_URL"` names its source the way a connector does, instead of leaving the file either silent about `[storage]` or repeating a credential.

   Both are strict: an unset variable is a startup error naming the variable, and `orion-server validate-config` reports it the same way. `vault://` and the reserved cloud schemes are **not** available in the config file. Resolving them is a network call, and the config is what tells the process how to make one. Declare those under [`[secrets]`](./vars-and-secrets.md), which resolves at startup through every scheme.

   `[vars]` and `[secrets]` are skipped by the `env://` pass because each owns its own reference semantics. A var must be a literal, because nothing resolves one on its way into metadata. A secret must be a reference.
3. **Environment variables**, named `ORION_SECTION__KEY` with a double underscore between levels — `ORION_SERVER__PORT`, `ORION_ENGINE__CIRCUIT_BREAKER__ENABLED`. These win over the file. Every setting's variable is in the tables below; list-valued settings take a comma-separated string.

The two substitution syntaxes reach different surfaces, because connectors and channels live in the database, not in this file. [Environment variables](../environment-variables.md) is the one table of which resolves where.

Run `orion-server validate-config` to see the merged result without starting. It prints the full effective config, every section serialized from the same structs the server runs on, as TOML; `--format json` and `--format summary` also exist. Secrets are masked with the same policy as the connector API. Values under secret-looking keys are replaced with `******`, and passwords embedded in URL-shaped values such as `storage.url` are struck out in place. Configuration is validated at startup too, and an invalid value stops the boot rather than being silently ignored.

### Misspellings are startup errors, not silent no-ops

A key the config file does not have fails to parse and names itself: `[server] wrokers = 4` stops the boot. The environment is held to the same standard. Orion scans the process environment at startup and refuses any variable that follows the override grammar without being one of the documented overrides. The error names the offender and the nearest real key.

```
Error: Configuration error: these ORION_* environment variables are not Orion
settings and would be silently ignored:
  ORION_SERVER__PORTT (did you mean ORION_SERVER__PORT?)
```

**What the scan looks at is the `__`, not the `ORION_`.** Every override is `ORION_` + the field path with a double underscore between levels. A name without one, such as `ORION_PORT` or `ORION_SERVER_URL`, cannot be a misspelling of a setting and is left alone. That matters because the prefix is not Orion's to claim. Kubernetes hands every pod a Docker-style block named after each Service in the namespace unless the PodSpec sets `enableServiceLinks: false`. A Service called `orion` therefore puts `ORION_SERVICE_HOST`, `ORION_PORT` and `ORION_PORT_8080_TCP_ADDR` into every container, written by the kubelet rather than by any manifest you can edit. `orion-cli` reads `ORION_SERVER_URL` and `ORION_API_KEY`, and a shell that exports those for the CLI passes them to a server started from the same shell. None of those can collide, and none needs a workaround. (The shipped Helm chart sets `enableServiceLinks: false` regardless — nothing here reads the link variables.)

The price is the single-underscore near-miss: `ORION_SERVER_PORT` is exactly the link a Service named `orion-server` would produce, so it is ignored rather than reported. Type the separator and the guard has your back.

Three exemptions cover names that *do* carry the separator, or would:

- **Names the config file references.** A file containing `url = "${ORION_DB_URL}"` makes `ORION_DB_URL` legitimate, even though no setting is named after it. The same `${VAR}` substitution also runs over connector `config_json` blobs — those live in the database and cannot be enumerated while the config is loading, so name them under `ORION_SECRET_*`.
- **The reserved `ORION_SECRET_*` namespace**, which Orion never interprets as configuration. Use it for values you reference yourself and cannot declare up front — an `env://ORION_SECRET_DB_PASSWORD` connector secret, for instance, since connectors live in the database rather than the config.
- **`ORION_ENVIRONMENT`**, the one setting that lives at the top level of the config and so has no separator of its own. It is checked by proximity instead: `ORION_ENVIRONMEN` is refused with a suggestion, because a silently ignored `environment` would leave the instance in `development` with the production checks downgraded to warnings.

Every setting resolves through three layers, in increasing precedence: the struct defaults, the config file passed with `-c`, and `ORION_SECTION__KEY` environment variables. The file may reference the environment two ways, and a misspelled key or variable is a startup error. The defaults on every configuration page are checked against `crates/orion-server/src/config/*.rs` by an integration test.

## Related

- [Environment variables](../environment-variables.md): every way the process environment reaches a value.
- [`orion-server validate-config`](../cli/orion-server/validate-config.md): printing the merged result.
- [Install Orion](../../get-started/install.md): the first config file.
- [Server configuration](./index.md): every section, by what you are configuring.
