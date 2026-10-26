//! V2 Immutable connection revisions & health probes (`/api/v2/admin/connections*`).

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use orion_api::v2::{
    ConnectionProbeResponse, ConnectionRevisionResponse, ConnectionsListResponse,
    CreateConnectionRevisionRequest, CreateConnectionRevisionResponse, ProbeConnectionRequest,
};
use serde_json::{Value, json};

use super::deployment_err_response;
use crate::errors::{FieldError, OrionError};
use crate::extensions::deployment_store::{ConnectionRevisionRow, PackageSchema};
use crate::extensions::{
    AdmittedRun, AuthorityGrant, BudgetLimits, ConnectionReference, EffectiveConnectionIdentity,
    ImplementationKind, ProviderReference,
};
use crate::server::admin_auth::AdminPrincipal;
use crate::server::state::AppState;
use orion_api::error::field_codes;

fn connection_row_to_dto(row: &ConnectionRevisionRow) -> ConnectionRevisionResponse {
    ConnectionRevisionResponse {
        connection_id: row.connection_id.clone(),
        revision: row.revision,
        provider_digest: row.provider_digest.clone(),
        provider_ref: json!({
            "package_digest": row.provider_digest,
        }),
        config: row.public_config.clone(),
        public_config: row.public_config.clone(),
        secret_locators: row.secret_locators.clone(),
        bindings: row.bindings.clone(),
        config_fingerprint: row.configuration_digest.clone(),
        configuration_digest: row.configuration_digest.clone(),
    }
}

/// `GET /api/v2/admin/connections`.
#[utoipa::path(
    get,
    path = "/api/v2/admin/connections",
    tag = "Connections",
    summary = "List immutable connection revisions",
    description = "Returns all stored connection revisions alongside their provider digests and configuration fingerprints.",
    responses(
        (status = 200, description = "Connection revisions", body = ConnectionsListResponse),
    )
)]
pub(crate) async fn list_connections(State(state): State<AppState>) -> impl IntoResponse {
    let raw = state.deployments.list_connections();
    let connections: Vec<ConnectionRevisionResponse> =
        raw.iter().map(connection_row_to_dto).collect();
    Json(ConnectionsListResponse {
        count: connections.len(),
        connections,
    })
}

/// `POST /api/v2/admin/connections`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/connections",
    tag = "Connections",
    summary = "Create an immutable connection revision",
    description = "Refuses a raw secret in public_config and a malformed secret locator, then judges the configuration as the node will read it — public_config with each secret reference at its locator path, the references unresolved. Over a built-in driver it is typed and checked as the connector it serves (`http`, `db` or `cache`), exactly as `orion-server compile` checks it; over an uploaded provider package it must satisfy the package's declared `configuration_schema`. Over a `workflow` provider package, `bindings` must bind each dependency alias the package declares to a stored connection over a built-in or rust provider (the one the dependency names, when it names one), and nothing else; a deployment member for the connection must then require each bound connection. A refusal names each problem by field in `details`. Then persists an immutable, content-addressed connection revision.",
    request_body = CreateConnectionRevisionRequest,
    responses(
        (status = 201, description = "Connection revision created", body = CreateConnectionRevisionResponse),
        (status = 400, description = "Raw secret in public_config, a malformed locator, or a configuration that would not load; `details` names each problem by field"),
        (status = 409, description = "Immutable connection revision conflict"),
    )
)]
pub(crate) async fn create_connection_revision(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Json(req): Json<CreateConnectionRevisionRequest>,
) -> Response {
    // The store refuses these too; first here so a raw secret is named as
    // one before the configuration's shape is judged.
    if let Err(e) = crate::extensions::deployment_store::check_connection_config(
        &req.public_config,
        &req.secret_locators,
    ) {
        return deployment_err_response(e).into_response();
    }
    let mut problems = config_problems(&state, &req);
    problems.extend(
        crate::extensions::workflow_provider::binding_problems(
            &state.deployments,
            &req.connection_id,
            &req.provider_digest,
            &req.bindings,
        )
        .into_iter()
        .map(|p| FieldError::new(p.path, p.code, p.message)),
    );
    if !problems.is_empty() {
        return OrionError::Validation {
            code: orion_api::error::codes::VALIDATION_ERROR,
            message: format!(
                "connection '{}' revision {}: its configuration would not load",
                req.connection_id, req.revision
            ),
            details: problems,
        }
        .into_response();
    }
    match state
        .deployments
        .store_connection_revision_with_bindings_in_db(
            &state.db_pool,
            &req.connection_id,
            req.revision,
            &req.provider_digest,
            req.public_config,
            req.secret_locators,
            req.bindings,
        )
        .await
    {
        Ok(row) => {
            crate::server::routes::admin::audit_log(
                &state.audit_queue,
                &principal,
                "v2_connection_create",
                "connection",
                &format!("{}@{}", row.connection_id, row.revision),
            );
            let resp = CreateConnectionRevisionResponse {
                status: "created".to_string(),
                connection: connection_row_to_dto(&row),
            };
            (StatusCode::CREATED, Json(resp)).into_response()
        }
        Err(e) => deployment_err_response(e).into_response(),
    }
}

/// What the connection's configuration would not load with, by field: over a
/// built-in driver, [`crate::validation::builtin_connection_problems`] (the
/// check `compile` makes); over an uploaded package, its declared
/// `configuration_schema`. A provider digest naming neither is judged when a
/// manifest names the connection.
fn config_problems(state: &AppState, req: &CreateConnectionRevisionRequest) -> Vec<FieldError> {
    let rooted = |path: &str| field_root(path, &req.secret_locators);
    if let Some(problems) = crate::validation::builtin_connection_problems(
        &req.provider_digest,
        &req.public_config,
        &req.secret_locators,
    ) {
        return problems
            .into_iter()
            .map(|mut problem| {
                problem.path = rooted(&problem.path);
                problem
            })
            .collect();
    }
    let config = crate::extensions::deployment_store::place_locators(
        &req.public_config,
        &req.secret_locators,
    );
    match state.deployments.check_package_instance(
        &req.provider_digest,
        PackageSchema::Configuration,
        &config,
    ) {
        Some(Err(finding)) => {
            let field = finding.instance_field();
            let code = match finding.location.as_str() {
                "required" => field_codes::REQUIRED,
                "type" => field_codes::TYPE_MISMATCH,
                _ => field_codes::INVALID,
            };
            vec![FieldError::new(
                rooted(&field),
                code,
                format!(
                    "the configuration does not satisfy the provider package's \
                     configuration_schema: {} refused {}",
                    finding.location,
                    if field.is_empty() {
                        "the configuration".to_string()
                    } else {
                        format!("'{field}'")
                    }
                ),
            )]
        }
        _ => Vec::new(),
    }
}

/// A configuration path as a request field: under `secret_locators` when a
/// locator supplies it, under `public_config` otherwise.
fn field_root(path: &str, locators: &BTreeMap<String, String>) -> String {
    let supplied_by_locator = locators.keys().any(|key| {
        path == key
            || path
                .strip_prefix(key.as_str())
                .is_some_and(|rest| rest.starts_with('.'))
    });
    match (supplied_by_locator, path.is_empty()) {
        (true, _) => format!("secret_locators.{path}"),
        (false, true) => "public_config".to_string(),
        (false, false) => format!("public_config.{path}"),
    }
}

/// `POST /api/v2/admin/connections/{id}/probe`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/connections/{id}/probe",
    tag = "Connections",
    summary = "Execute an audited health probe against a connection",
    description = "Probes the connection's latest revision (or `revision`), recorded in the audit log first because it reaches a real backend. Over a built-in driver it connects for real, with the revision's secret references resolved as the node resolves them at load: `org.orion.postgresql`, `org.orion.mysql` and `org.orion.sqlite` run `SELECT 1` through the node's own connector pool (refusing a private address unless `allow_private_urls`); `org.orion.http` resolves the configured URL's host and opens a TCP connection to it (no HTTP request is sent and no credential is presented; the same private-address rule applies); `org.orion.memory-cache` reads a probe key. `details.probe` says which check ran and `details.error` why it failed — never a resolved secret. Over an uploaded provider package it validates the configuration and runs the provider's health check within an admitted run budget.",
    params(
        ("id" = String, Path, description = "Connection identifier")
    ),
    request_body(content = Option<ProbeConnectionRequest>, description = "Optional target revision or driver override"),
    responses(
        (status = 200, description = "Connection healthy", body = ConnectionProbeResponse),
        (status = 400, description = "Unsupported implementation kind"),
        (status = 403, description = "Provider package revoked"),
        (status = 404, description = "Connection or provider not found"),
        (status = 503, description = "Connection unhealthy", body = ConnectionProbeResponse),
    )
)]
pub(crate) async fn probe_connection(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(connection_id): Path<String>,
    req: Option<Json<ProbeConnectionRequest>>,
) -> impl IntoResponse {
    let req = req.map(|Json(r)| r).unwrap_or_default();
    let started = Instant::now();

    let stored_conn = state
        .deployments
        .list_connections()
        .into_iter()
        .filter(|c| c.connection_id == connection_id)
        .filter(|c| req.revision.is_none_or(|rev| c.revision == rev))
        .max_by_key(|c| c.revision);

    if let Some(conn_row) = &stored_conn {
        // A probe reaches a production backend with the connection's real
        // credentials: on the record before it runs.
        crate::server::routes::admin::audit_log(
            &state.audit_queue,
            &principal,
            "v2_connection_probe",
            "connection",
            &format!("{}@{}", conn_row.connection_id, conn_row.revision),
        );
        if let Some(driver) =
            crate::extensions::foundation::builtin_driver(&conn_row.provider_digest)
        {
            let outcome = probe_builtin(&state, conn_row, driver).await;
            return probe_response(
                conn_row.connection_id.clone(),
                conn_row.revision,
                driver.to_string(),
                started,
                outcome,
            );
        }
    }

    let stored_pkg = stored_conn.as_ref().and_then(|conn_row| {
        state
            .deployments
            .list_packages()
            .into_iter()
            .find(|p| p.package_digest == conn_row.provider_digest)
    });

    if let Some(ref pkg) = stored_pkg {
        let is_revoked = state
            .deployments
            .list_admissions()
            .into_iter()
            .any(|a| a.package_digest == pkg.package_digest && a.revoked);
        if is_revoked {
            return (
                StatusCode::FORBIDDEN,
                Json(crate::errors::error_envelope_json(
                    "PROVIDER_REVOKED",
                    format!(
                        "Provider package '{}' ({}) for connection '{connection_id}' is revoked",
                        pkg.provider_id, pkg.package_digest
                    ),
                )),
            );
        }
    }

    let kind_str = req
        .kind
        .as_deref()
        .or_else(|| stored_pkg.as_ref().map(|p| p.implementation.as_str()))
        .unwrap_or("rust");
    let Some(kind) = ImplementationKind::parse(kind_str) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(crate::errors::error_envelope_json(
                "INVALID_IMPLEMENTATION_KIND",
                format!(
                    "unsupported implementation kind '{kind_str}'; expected 'rust', 'workflow', or 'wasm'"
                ),
            )),
        );
    };

    let driver_id = req
        .driver_id
        .clone()
        .or_else(|| stored_pkg.as_ref().map(|p| p.backend_id().to_string()))
        .unwrap_or_else(|| connection_id.clone());

    let Some(backend) = state.composition.providers().get(kind, &driver_id) else {
        let code = if stored_conn.is_none() && req.driver_id.is_none() {
            "CONNECTION_NOT_FOUND"
        } else {
            "PROVIDER_NOT_FOUND"
        };
        return (
            StatusCode::NOT_FOUND,
            Json(crate::errors::error_envelope_json(
                code,
                format!(
                    "connection '{connection_id}' / provider ({}, '{driver_id}') is not registered",
                    kind.as_str()
                ),
            )),
        );
    };

    if let Some(ref conn_row) = stored_conn
        && !conn_row.public_config.is_null()
        && conn_row
            .public_config
            .as_object()
            .is_none_or(|m| !m.is_empty())
        && let Err(cfg_err) = backend.validate_config(&conn_row.public_config)
    {
        let resp = ConnectionProbeResponse {
            connection_id,
            revision: conn_row.revision,
            driver_id,
            healthy: false,
            status: "unhealthy".to_string(),
            latency_ms: started.elapsed().as_millis() as u64,
            details: json!({ "error": cfg_err }),
        };
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::to_value(resp).unwrap_or(Value::Null)),
        );
    }

    let resolved_revision = stored_conn
        .as_ref()
        .map(|c| c.revision)
        .or(req.revision)
        .unwrap_or(1);
    let resolved_digest = stored_conn
        .as_ref()
        .map(|c| c.provider_digest.clone())
        .unwrap_or_else(|| {
            "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string()
        });
    let resolved_version = stored_pkg
        .as_ref()
        .map(|p| p.version.clone())
        .unwrap_or_else(|| "2.0.0".to_string());

    let mut allowed_operations = BTreeMap::new();
    allowed_operations.insert(
        connection_id.clone(),
        ["probe".to_string()].into_iter().collect(),
    );
    // The connection's own dependencies, each at its latest revision: what a
    // provider's health check may reach through them.
    let declared_dependencies = stored_conn
        .as_ref()
        .map(|conn| probe_dependencies(&state, conn))
        .unwrap_or_default();
    let grant = AuthorityGrant {
        allowed_operations,
        declared_dependencies,
        allowed_channels: BTreeSet::new(),
    };
    let admitted = AdmittedRun::admit(
        format!("probe-{connection_id}"),
        1,
        Instant::now() + Duration::from_secs(5),
        BudgetLimits::default(),
        grant,
    );
    let run_ctx = admitted.context();
    let conn_ref = ConnectionReference {
        connection_id: connection_id.clone(),
        revision: resolved_revision,
        provider: ProviderReference {
            provider_id: driver_id.clone(),
            version: resolved_version,
            package_digest: resolved_digest.clone(),
        },
    };
    let eff = EffectiveConnectionIdentity {
        provider_digest: resolved_digest,
        connection_id: connection_id.clone(),
        revision: resolved_revision,
        secret_hash: "sha256:probe".to_string(),
    };
    let Ok(inv) = run_ctx.start_invocation(conn_ref, eff, "probe", None, 1) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(crate::errors::error_envelope_json(
                "PROBE_INIT_FAILED",
                "Failed to initialize connection health probe",
            )),
        );
    };
    let Ok(attempt) = inv.next_attempt() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(crate::errors::error_envelope_json(
                "PROBE_ATTEMPT_FAILED",
                "Failed to start connection health probe attempt",
            )),
        );
    };

    match backend.health_check(&attempt).await {
        Ok(()) => {
            let resp = ConnectionProbeResponse {
                connection_id,
                revision: resolved_revision,
                driver_id,
                healthy: true,
                status: "healthy".to_string(),
                latency_ms: started.elapsed().as_millis() as u64,
                details: json!({ "status": "ok" }),
            };
            (
                StatusCode::OK,
                Json(serde_json::to_value(resp).unwrap_or(Value::Null)),
            )
        }
        Err(err) => {
            let resp = ConnectionProbeResponse {
                connection_id,
                revision: resolved_revision,
                driver_id,
                healthy: false,
                status: "unhealthy".to_string(),
                latency_ms: started.elapsed().as_millis() as u64,
                details: json!({ "error": err }),
            };
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::to_value(resp).unwrap_or(Value::Null)),
            )
        }
    }
}

/// The dependencies `conn`'s bindings name, keyed as a run's authority keys
/// them, each bound connection at its latest stored revision with the
/// operations the package declares for the alias. A binding to a connection
/// that is not stored, or to a provider that is not, is left out.
fn probe_dependencies(
    state: &AppState,
    conn: &ConnectionRevisionRow,
) -> BTreeMap<String, (ConnectionReference, BTreeSet<String>)> {
    use crate::extensions::workflow_provider::{declared_dependencies, dependency_key};
    let store = &state.deployments;
    let Some(package) = store.get_package(&conn.provider_digest) else {
        return BTreeMap::new();
    };
    let dependencies = declared_dependencies(&package.manifest_json);
    let connections = store.list_connections();
    let mut out = BTreeMap::new();
    for (alias, target) in &conn.bindings {
        let Some(row) = connections
            .iter()
            .filter(|c| &c.connection_id == target)
            .max_by_key(|c| c.revision)
        else {
            continue;
        };
        let provider = match crate::extensions::foundation::builtin_driver(&row.provider_digest) {
            Some(driver) => ProviderReference {
                provider_id: driver.to_string(),
                version: crate::version::ORION_VERSION.to_string(),
                package_digest: row.provider_digest.clone(),
            },
            None => match store.get_package(&row.provider_digest) {
                Some(pkg) => ProviderReference {
                    provider_id: pkg.provider_id,
                    version: pkg.version,
                    package_digest: pkg.package_digest,
                },
                None => continue,
            },
        };
        out.insert(
            dependency_key(&conn.connection_id, alias),
            (
                ConnectionReference {
                    connection_id: target.clone(),
                    revision: row.revision,
                    provider,
                },
                dependencies
                    .get(alias)
                    .map(|d| d.operations.clone())
                    .unwrap_or_default(),
            ),
        );
    }
    out
}

/// What a built-in probe did, and why it failed when it did.
struct ProbeOutcome {
    probe: &'static str,
    result: Result<(), String>,
}

/// The answer to a probe: `200` with `healthy: true`, or `503` with
/// `details.error`.
fn probe_response(
    connection_id: String,
    revision: u64,
    driver_id: String,
    started: Instant,
    outcome: ProbeOutcome,
) -> (StatusCode, Json<Value>) {
    let healthy = outcome.result.is_ok();
    let mut details = json!({ "probe": outcome.probe });
    match outcome.result {
        Ok(()) => details["status"] = json!("ok"),
        Err(error) => details["error"] = json!(error),
    }
    let resp = ConnectionProbeResponse {
        connection_id,
        revision,
        driver_id,
        healthy,
        status: if healthy { "healthy" } else { "unhealthy" }.to_string(),
        latency_ms: started.elapsed().as_millis() as u64,
        details,
    };
    let status = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(serde_json::to_value(resp).unwrap_or(Value::Null)),
    )
}

/// Probe a connection over a built-in driver: its configuration resolved
/// exactly as the registry resolves it at load (`[vars]`, secret references,
/// the typed parse, the endpoint scheme), then the driver's own check of the
/// backend.
///
/// Nothing a reference resolved to reaches the answer: the shape is checked
/// before resolution, so a parse failure after it can only be about a
/// resolved value, and is reported without it (the detail is logged).
async fn probe_builtin(
    state: &AppState,
    row: &ConnectionRevisionRow,
    driver: &'static str,
) -> ProbeOutcome {
    use crate::connector::{ConnectorConfig, ConnectorType};
    let config_stage = |result| ProbeOutcome {
        probe: "configuration",
        result,
    };
    let Some((connector_type, config)) =
        crate::extensions::foundation::builtin_connector(driver, row.config_with_locators())
    else {
        return config_stage(Err(format!("'{driver}' serves no built-in connector")));
    };
    let Some(typed) = ConnectorType::from_stored(connector_type) else {
        return config_stage(Err(format!("unknown connector type '{connector_type}'")));
    };
    if let Err(e) = ConnectorConfig::parse_variant(
        typed,
        &config,
        &crate::connector::secrets::UnresolvedReferences,
    ) {
        return config_stage(Err(e));
    }
    let name = row.connection_id.as_str();
    let resolved = match crate::connector::resolve_connector_config(
        name,
        &format!("{}@{}", row.connection_id, row.revision),
        connector_type,
        &config.to_string(),
        state.vars.as_deref(),
        crate::connector::secrets::default_resolvers(),
    )
    .await
    {
        Ok(resolved) => resolved,
        // The stored shape parsed above, so a parse failure here is about a
        // resolved value; the resolver logged it.
        Err(issue) if issue.stage == "deserialize" => {
            return config_stage(Err(
                "the configuration parses as stored but not with its references resolved; \
                 the parse error is in the server log"
                    .to_string(),
            ));
        }
        Err(issue) => return config_stage(Err(format!("{}: {}", issue.stage, issue.reason))),
    };
    match &resolved {
        ConnectorConfig::Db(db) => ProbeOutcome {
            probe: "SELECT 1",
            result: probe_db(state, name, db).await,
        },
        ConnectorConfig::Cache(cache) => ProbeOutcome {
            probe: "cache read of a probe key",
            result: probe_cache(state, name, cache).await,
        },
        ConnectorConfig::Http(http) => ProbeOutcome {
            probe: "TCP connect to the URL's host",
            result: probe_http(name, http).await,
        },
    }
}

/// The cheapest statement every supported SQL backend accepts, through the
/// pool the node's `db_*`/`data_*` functions use — which refuses a
/// private/internal address before dialling unless `allow_private_urls`.
async fn probe_db(
    state: &AppState,
    name: &str,
    db: &crate::connector::DbConnectorConfig,
) -> Result<(), String> {
    let pool = state
        .caches
        .sql_pool_cache
        .get_pool(name, db)
        .await
        .map_err(|e| match e {
            // A driver's connect error names hosts, roles and databases.
            OrionError::Internal { .. } => {
                tracing::warn!(connection = %name, error = %e, "connection probe could not connect");
                "could not connect to the database; the driver error is in the server log"
                    .to_string()
            }
            other => other.client_message(),
        })?;
    pool.ping().await.map_err(|e| {
        tracing::warn!(connection = %name, error = %e, "connection probe query failed");
        if matches!(e, sqlx::Error::Database(_)) {
            "connected, but the probe query failed; the driver error is in the server log"
                .to_string()
        } else {
            "the probe could not complete against the database, which may be unreachable; \
             the driver error is in the server log"
                .to_string()
        }
    })
}

/// A read, never a write: the probe leaves nothing in a store a workflow
/// shares.
async fn probe_cache(
    state: &AppState,
    name: &str,
    cache: &crate::connector::CacheConnectorConfig,
) -> Result<(), String> {
    let backend = state
        .caches
        .cache_pool
        .get_backend(
            crate::connector::cache_backend::CachePurpose::Workflow,
            name,
            cache,
        )
        .await
        .map_err(|e| e.client_message())?;
    backend
        .get("__orion_connectivity_probe__")
        .await
        .map(|_| ())
        .map_err(|e| {
            tracing::warn!(connection = %name, error = %e, "connection probe read failed");
            "the probe read failed; the backend error is in the server log".to_string()
        })
}

/// Resolve the configured URL's host and open a TCP connection to it. No
/// HTTP request is sent: a request would be a call against a production API
/// with its credentials, with a status that says little about whether the
/// connection's real calls will work. A private/internal address is refused
/// unless `allow_private_urls`, as the node refuses it on every request.
async fn probe_http(
    name: &str,
    http: &crate::connector::HttpConnectorConfig,
) -> Result<(), String> {
    if http.url.is_empty() {
        return Err(
            "the connection has no url (its tasks supply whole URLs), so there is no host \
             to connect to"
                .to_string(),
        );
    }
    let url = url::Url::parse(&http.url).map_err(|e| format!("the url does not parse: {e}"))?;
    let authority_host = url
        .host_str()
        .ok_or_else(|| "the url has no host".to_string())?;
    // `[::1]` in a URL, `::1` to a socket.
    let host = authority_host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "the url has no port".to_string())?;
    if !http.allow_private_urls {
        crate::validation::ssrf::validate_hostport_not_private(authority_host, port)
            .await
            .map_err(|e| {
                format!("{e}; set allow_private_urls on the connection if that is intended")
            })?;
    }
    match tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await
    {
        Ok(Ok(_stream)) => Ok(()),
        Ok(Err(e)) => {
            tracing::warn!(connection = %name, error = %e, "connection probe could not connect");
            Err(format!("could not connect to {host}:{port}: {}", e.kind()))
        }
        Err(_) => Err(format!("connecting to {host}:{port} timed out after 5s")),
    }
}
