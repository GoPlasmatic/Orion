<!-- description: What breaks moving from Orion 1.x to 2.0: the fresh database, the /api/v2 routes, strict definitions, cursor-paged lists and changed defaults. -->
<!-- type: migration -->
<!-- last_verified: 2026-10-28 -->

# Upgrade to 2.0.0

Orion 2.0 is a major release. It changes the HTTP routes, the state database, the shape of every list, several defaults and the extension API. Nothing here migrates in place: you stand up Orion 2 beside the 1.x instance, deploy your definitions to it, and move the clients.

Read [Move from Orion 1.x](../operate/maintain/upgrades.md#move-from-orion-1x) for the procedure. This page is the list of what changed, so you know what to look for in your own clients, dashboards and automation.

## Before you start

- **The server and the CLI must be the same release.** `orion-cli` reads the server's `api_version` and refuses one it does not speak. Upgrade both together.
- **There is no in-place upgrade.** Orion 2 refuses a 1.x state database at startup. Traces, audit logs, the trace DLQ and the cron ledger stay in the 1.x database.
- **Your definitions must exist as files.** Orion 2 reads workflows, channels and connections only from a definition set that `orion-cli compile` turns into a deployment. If yours exist only in the 1.x database, there is no import tool: recreate them as files while the 1.x instance still runs.

## What changed

| Area | Orion 1.x | Orion 2.0 |
|---|---|---|
| Routes | `/api/v1/*` | `/api/v2/*`. `/api/v1/` answers `404`. |
| State | Entity tables in the state database | Content-addressed definitions and immutable deployment revisions; a fresh database |
| Deploying | `POST`/`PUT` per entity | `orion-cli compile`, then `orion-cli deployments apply -d`; a rollback is a new revision |
| Definitions | v3 spellings tolerated in places | Parsed strictly: `halt_on`, a task's `continue_on_error`, the function name `validate` and undefined keys are refused at upload and at load |
| Lists | `{data, total, limit, offset}`, `?offset=` and `?sort_by=` | `{data, limit, next_cursor?, total?}`; see [Lists page by cursor](#lists-page-by-cursor) |
| Timestamps | Zone-less (`2026-01-01T00:00:00`) | RFC 3339 UTC with a `Z`. The zone-less form still parses on input. |
| Default bind | `0.0.0.0:8080` | `127.0.0.1:8080` |
| `environment` | Any string; only `prod*` meant production | Must be `development`, `staging`, `test` or `production` (short forms accepted); any other name is a startup error |
| CLI/server versions | Tolerant | Strict `api_version` match |

### Lists page by cursor

Every list answers one envelope, `{ "data": [...], "limit": 50, "next_cursor": "…", "total": 137 }`:

- `next_cursor` is present when more rows follow. Pass it back as `?cursor=`, unmodified.
- `total` appears only with `?include_total=true`. The count is a full scan on PostgreSQL and MySQL, so it is opt-in.
- `?offset=` and `?sort_by=` / `?sort_order=` are gone. Lists are newest-first; filters narrow them.
- `/connections`, `/connections/{id}/revisions` and `/connector-packages` page too (they returned every row before). `/connector-packages` no longer carries an `admissions` array; each package already has `admitted` and `revoked`.

In `orion-cli`, `--offset`, `--sort-by` and `--sort-order` are removed; every `list` takes `--limit`, `--cursor` and `--include-total`.

### Responses that changed shape

| Endpoint | Change |
|---|---|
| Trace DLQ entry | `payload_json` and `metadata_json` (JSON inside a string) are now `payload` and `metadata`, real JSON values |
| `POST /backups` | Answers `201`; the list field `modified_at` is now `created_at` |
| Connection and package lists | Paged, as above |
| Channel definitions | `protocol` and `channel_type` must be exactly lowercase (`rest`, `sync`); `REST` and `Sync` are refused |

### Defaults that tightened

- **Bind address.** The server listens on `127.0.0.1` unless `server.host` says otherwise. The container image and the Helm chart set `0.0.0.0`, so a containerised deployment is unchanged. A bare-metal deployment behind a proxy on another host must set `server.host`.
- **Environment names.** Set `ORION_ENVIRONMENT` to a recognised name. An unrecognised one (`live`, `prd`, `eu-west`) used to behave as not-production and silently skip the production checks; it now stops the server at startup.
- **Read-only admin keys** may also call the side-effect-free `POST` routes: `deployment-plans`, `authoring/scaffold`, `authoring/lint`, `authoring/pack` and `edition-transition-preflight`.

### Observability

- `orion_http_request_duration_seconds` carries `status_class` (`2xx`, `4xx`, …) in place of `status`. The exact code remains on `orion_http_requests_total`. Update any dashboard or alert that selected on the old label.
- CLI errors honour `--output json|yaml`: the server's `code`, `request_id` and field errors arrive as structured output on stderr.
- `~/.orion/config.toml`'s `default_output` is now applied.

### Database

The schema starts over with one baseline migration per backend. On MySQL, timestamps now keep microseconds and the JSON and message columns are `mediumtext` (they were `text`, which truncated at 64 KB). Trace, DLQ and audit ids are time-ordered UUIDs.

### Extension authors and Rust consumers

The `orion-extension-api` data types (`ChannelDriverCapabilities`, `ChannelProtocolFields`, `PackageAdmissionRequest`, `AdmissionVerdict`, `AdmissionRejection`, `ConnectorError` and the source batch types) are `#[non_exhaustive]` and built through constructors and builders. `ChannelDriver::protocol_id` and `PackageVerifier::policy_id` return `&str`. `ProviderBackend::literal_input_keys` returns `Vec<String>`. The bootstrap entry points return a typed `BootstrapError`. A plugin manifest's `abi` is accepted when it names the same major version and is no newer than the host's.

## Related

- [Move from Orion 1.x](../operate/maintain/upgrades.md#move-from-orion-1x): the procedure, step by step.
- [Versioning and support policy](./versioning-policy.md): what 2.x guarantees.
- [Envelopes](../reference/admin-api/envelopes.md): the list contract.
