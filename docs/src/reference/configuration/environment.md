<!-- description: The environment setting: any value starting with prod turns three warnings into startup errors, and ORION_ENVIRONMENT is the override. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Deployment environment

One setting changes how strictly everything else is validated.

## Synopsis

```toml
environment = "development"
```

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `environment` | `"development"` | `ORION_ENVIRONMENT` | Set to `"production"` before exposing an instance to anything you care about. |

Any value starting with `prod` (case-insensitive) is a production environment, which turns three warnings into startup errors:

- **Admin auth must be enabled.** `admin_auth.enabled = false` becomes a fatal config error instead of a log line nobody reads.
- **CORS may not be `["*"]`.** The wildcard is rejected; list explicit origins.
- **A cluster may not migrate at boot.** `cluster.enabled = true` with `storage.auto_migrate = true` is refused. See [`auto_migrate` in a cluster](./storage.md).

That is the whole mechanism — it does not change any other default. Everything else on this page is still yours to set, and the [Production Checklist](../../operate/production-checklist.md) is the list worth walking.

The variable is `ORION_ENVIRONMENT`, derived from the field name like every other override. `ORION_ENV` was the pre-1.0 alias and is now refused at startup rather than silently ignored.

## Related

- [Production checklist](../../operate/production-checklist.md): what a production instance must set.
- [Secure an instance](../../operate/run/security.md): the checks production turns into errors.
- [Server settings](./server.md): the settings `verbose_errors` and the API docs key off this one.
- [Server configuration](./index.md): every section, by what you are configuring.
