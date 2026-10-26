//! V2 Immutable connection revisions & health probes (`/api/v2/admin/connections*`).

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use orion_api::v2::{
    ConnectionProbeResponse, ConnectionRevisionResponse, ConnectionsListResponse,
    ConnectorErrorClass, CreateConnectionRevisionRequest, CreateConnectionRevisionResponse,
    EffectStatus, ProbeDetails,
};

use super::{V2Error, V2Result};
use crate::errors::{FieldError, OrionError};
use crate::extensions::deployment_store::{
    ConnectionRevisionRow, DeploymentErrorKind, PackageSchema,
};
use crate::extensions::{
    AdmittedRun, AuthorityGrant, BudgetLimits, ConnectionId, ConnectionReference,
    EffectiveConnectionIdentity, ProviderRef, Revision,
};
use crate::server::admin_auth::AdminPrincipal;
use crate::server::extract::OrionJson;
use crate::server::state::AppState;
use orion_api::error::field_codes;

fn connection_row_to_dto(row: &ConnectionRevisionRow) -> ConnectionRevisionResponse {
    ConnectionRevisionResponse {
        connection_id: row.connection_id.to_string(),
        revision: row.revision.get(),
        provider_digest: row.provider_digest.to_string(),
        public_config: row.public_config.clone(),
        secret_locators: row.secret_locators.clone(),
        bindings: row.bindings.clone(),
        configuration_digest: row.configuration_digest.to_string(),
    }
}

/// `rows` as a list, ordered by connection then revision.
fn list_response(mut rows: Vec<std::sync::Arc<ConnectionRevisionRow>>) -> ConnectionsListResponse {
    rows.sort_by(|a, b| {
        (a.connection_id.as_str(), a.revision.get())
            .cmp(&(b.connection_id.as_str(), b.revision.get()))
    });
    let connections: Vec<ConnectionRevisionResponse> =
        rows.iter().map(|r| connection_row_to_dto(r)).collect();
    ConnectionsListResponse {
        count: connections.len(),
        connections,
    }
}

/// The stored revision `revision` of `connection_id`.
fn stored_revision(
    state: &AppState,
    connection_id: &str,
    revision: u64,
) -> V2Result<std::sync::Arc<ConnectionRevisionRow>> {
    Revision::new(revision)
        .and_then(|rev| state.deployments.get_connection(connection_id, rev))
        .ok_or_else(|| {
            V2Error::new(
                DeploymentErrorKind::ConnectionRevisionNotFound,
                format!("connection '{connection_id}' has no revision {revision}"),
            )
        })
}

/// `GET /api/v2/admin/connections`.
#[utoipa::path(
    get,
    path = "/api/v2/admin/connections",
    tag = "Connections",
    summary = "List every stored connection revision",
    description = "Returns every stored revision of every connection, ordered by connection then revision, with its provider digest and configuration digest.",
    responses(
        (status = 200, description = "Connection revisions", body = ConnectionsListResponse),
    )
)]
pub(crate) async fn list_connections(
    State(state): State<AppState>,
) -> Json<ConnectionsListResponse> {
    Json(list_response(state.deployments.list_connections()))
}

/// `GET /api/v2/admin/connections/{id}/revisions`.
#[utoipa::path(
    get,
    path = "/api/v2/admin/connections/{id}/revisions",
    tag = "Connections",
    summary = "List one connection's revisions",
    description = "Returns every stored revision of the connection, oldest first. Empty for a connection with none.",
    params(("id" = String, Path, description = "Connection identifier")),
    responses(
        (status = 200, description = "The connection's revisions", body = ConnectionsListResponse),
    )
)]
pub(crate) async fn list_connection_revisions(
    State(state): State<AppState>,
    Path(connection_id): Path<String>,
) -> Json<ConnectionsListResponse> {
    let rows = state
        .deployments
        .list_connections()
        .into_iter()
        .filter(|c| c.connection_id == connection_id)
        .collect();
    Json(list_response(rows))
}

/// `GET /api/v2/admin/connections/{id}/revisions/{revision}`.
#[utoipa::path(
    get,
    path = "/api/v2/admin/connections/{id}/revisions/{revision}",
    tag = "Connections",
    summary = "Read one connection revision",
    params(
        ("id" = String, Path, description = "Connection identifier"),
        ("revision" = u64, Path, description = "Connection revision"),
    ),
    responses(
        (status = 200, description = "The connection revision", body = ConnectionRevisionResponse),
        (status = 404, description = "No such revision is stored (`CONNECTION_REVISION_NOT_FOUND`)"),
    )
)]
pub(crate) async fn get_connection_revision(
    State(state): State<AppState>,
    Path((connection_id, revision)): Path<(String, u64)>,
) -> V2Result<Json<ConnectionRevisionResponse>> {
    let row = stored_revision(&state, &connection_id, revision)?;
    Ok(Json(connection_row_to_dto(&row)))
}

/// `POST /api/v2/admin/connections/{id}/revisions`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/connections/{id}/revisions",
    tag = "Connections",
    summary = "Create an immutable connection revision",
    description = "Refuses a raw secret in public_config and a malformed secret locator, then judges the configuration as the node will read it — public_config with each secret reference at its locator path, the references unresolved. Over a built-in driver it is typed and checked as the connector it serves (`http`, `db` or `cache`), exactly as `orion-server compile` checks it; over an uploaded provider package it must satisfy the package's declared `configuration_schema`. Over a `workflow` provider package, `bindings` must bind each dependency alias the package declares to a stored connection over a built-in or rust provider (the one the dependency names, when it names one), and nothing else; a deployment member for the connection must then require each bound connection. A refusal names each problem by field in `details`. Then persists an immutable, content-addressed connection revision: the same revision with the same content answers it again, with different content it is a conflict.",
    params(("id" = String, Path, description = "Connection identifier")),
    request_body = CreateConnectionRevisionRequest,
    responses(
        (status = 201, description = "Connection revision created", body = CreateConnectionRevisionResponse),
        (status = 400, description = "Raw secret in public_config, a malformed locator, or a configuration that would not load; `details` names each problem by field"),
        (status = 409, description = "The revision is stored with other content (`IMMUTABLE_CONNECTION_CONFLICT`)"),
    )
)]
pub(crate) async fn create_connection_revision(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(connection_id): Path<String>,
    OrionJson(req): OrionJson<CreateConnectionRevisionRequest>,
) -> V2Result<(StatusCode, Json<CreateConnectionRevisionResponse>)> {
    // The store refuses these too; first here so a raw secret is named as
    // one before the configuration's shape is judged.
    crate::extensions::deployment_store::check_connection_config(
        &req.public_config,
        &req.secret_locators,
    )?;
    let mut problems = config_problems(&state, &req);
    problems.extend(
        crate::extensions::workflow_provider::binding_problems(
            &state.deployments,
            &connection_id,
            &req.provider_digest,
            &req.bindings,
        )
        .into_iter()
        .map(|p| FieldError::new(p.path, p.code, p.message)),
    );
    if !problems.is_empty() {
        return Err(OrionError::Validation {
            code: orion_api::error::codes::VALIDATION_ERROR,
            message: format!(
                "connection '{connection_id}' revision {}: its configuration would not load",
                req.revision
            ),
            details: problems,
        }
        .into());
    }
    let row = state
        .deployments
        .store_connection_revision_with_bindings_in_db(
            &state.db_pool,
            &connection_id,
            req.revision,
            &req.provider_digest,
            req.public_config,
            req.secret_locators,
            req.bindings,
        )
        .await?;
    crate::server::routes::admin::audit_log(
        &state.audit_queue,
        &principal,
        "v2_connection_create",
        "connection",
        &format!("{}@{}", row.connection_id, row.revision),
    );
    Ok((
        StatusCode::CREATED,
        Json(CreateConnectionRevisionResponse {
            connection: connection_row_to_dto(&row),
        }),
    ))
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

/// `POST /api/v2/admin/connections/{id}/revisions/{revision}/probes`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/connections/{id}/revisions/{revision}/probes",
    tag = "Connections",
    summary = "Probe a connection revision's backend",
    description = "Probes the stored revision through the provider it names — nothing in the request can choose another — recorded in the audit log first because it reaches a real backend. Over a built-in driver it connects for real, with the revision's secret references resolved as the node resolves them at load: `org.orion.postgresql`, `org.orion.mysql` and `org.orion.sqlite` run `SELECT 1` through the node's own connector pool (refusing a private address unless `allow_private_urls`); `org.orion.http` resolves the configured URL's host and opens a TCP connection to it (no HTTP request is sent and no credential is presented; the same private-address rule applies); `org.orion.memory-cache` reads a probe key. Over an uploaded provider package it validates the configuration and runs the provider's health check within an admitted run budget. A probe that ran answers `200` whatever it found: `healthy` is the finding, `details.probe` says which check ran and `details.error` why it failed — never a resolved secret.",
    params(
        ("id" = String, Path, description = "Connection identifier"),
        ("revision" = u64, Path, description = "Connection revision"),
    ),
    responses(
        (status = 200, description = "The probe ran; `healthy` says what it found", body = ConnectionProbeResponse),
        (status = 403, description = "The revision's provider package is revoked (`PROVIDER_REVOKED`)"),
        (status = 404, description = "No such revision (`CONNECTION_REVISION_NOT_FOUND`), its package is not stored (`PACKAGE_NOT_FOUND`), or this node has no backend for it (`PROVIDER_NOT_FOUND`)"),
    )
)]
pub(crate) async fn probe_connection(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path((connection_id, revision)): Path<(String, u64)>,
) -> V2Result<Json<ConnectionProbeResponse>> {
    let started = Instant::now();
    let conn_row = stored_revision(&state, &connection_id, revision)?;

    // A probe reaches a production backend with the connection's real
    // credentials: on the record before it runs.
    crate::server::routes::admin::audit_log(
        &state.audit_queue,
        &principal,
        "v2_connection_probe",
        "connection",
        &format!("{}@{}", conn_row.connection_id, conn_row.revision),
    );
    if let Some(driver) = crate::extensions::foundation::builtin_driver(&conn_row.provider_digest) {
        let outcome = probe_builtin(&state, &conn_row, driver).await;
        return Ok(Json(probe_response(
            &conn_row,
            driver.to_string(),
            started,
            outcome,
        )));
    }

    let pkg = state
        .deployments
        .get_package(&conn_row.provider_digest)
        .ok_or_else(|| {
            V2Error::new(
                DeploymentErrorKind::PackageNotFound,
                format!(
                    "connection '{connection_id}' revision {revision} names provider package '{}', which is not stored",
                    conn_row.provider_digest
                ),
            )
        })?;
    if state
        .deployments
        .get_admission(&pkg.package_digest)
        .is_some_and(|a| a.revoked)
    {
        return Err(V2Error::new(
            DeploymentErrorKind::ProviderRevoked,
            format!(
                "provider package '{}' ({}) for connection '{connection_id}' is revoked",
                pkg.provider_id, pkg.package_digest
            ),
        ));
    }

    let kind = pkg.implementation;
    let driver_id = pkg.backend_id().to_string();
    let Some(backend) = state.composition.providers().get(kind, &driver_id) else {
        return Err(V2Error::new(
            DeploymentErrorKind::ProviderNotFound,
            format!(
                "this node registers no {} provider backend '{driver_id}' for connection '{connection_id}'",
                kind.as_str()
            ),
        ));
    };

    if !conn_row.public_config.is_null()
        && conn_row
            .public_config
            .as_object()
            .is_none_or(|m| !m.is_empty())
        && let Err(cfg_err) = backend.validate_config(&conn_row.public_config)
    {
        return Ok(Json(probe_response(
            &conn_row,
            driver_id,
            started,
            ProbeOutcome {
                probe: "configuration",
                result: Err(ProbeFailure::from(&cfg_err)),
            },
        )));
    }

    let mut allowed_operations = BTreeMap::new();
    allowed_operations.insert(
        connection_id.clone(),
        ["probe".to_string()].into_iter().collect(),
    );
    // The connection's own dependencies, each at its latest revision: what a
    // provider's health check may reach through them.
    let grant = AuthorityGrant {
        allowed_operations,
        declared_dependencies: probe_dependencies(&state, &conn_row),
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
        connection_id: ConnectionId::from(&connection_id),
        revision: conn_row.revision,
        provider: ProviderRef {
            publisher: pkg.publisher.clone(),
            provider_id: driver_id.as_str().into(),
            version: pkg.version.clone(),
            digest: conn_row.provider_digest.clone(),
        },
    };
    let eff = EffectiveConnectionIdentity {
        provider_digest: conn_row.provider_digest.clone(),
        connection_id: ConnectionId::from(&connection_id),
        revision: conn_row.revision,
        secret_hash: "sha256:probe".to_string(),
    };
    let attempt = run_ctx
        .start_invocation(conn_ref, eff, "probe", None, 1)
        .and_then(|inv| inv.next_attempt())
        .map_err(|e| {
            V2Error::new(
                DeploymentErrorKind::ProbeFailed,
                format!("the probe of connection '{connection_id}' could not start: {e:?}"),
            )
        })?;

    let outcome = ProbeOutcome {
        probe: "provider health check",
        result: backend
            .health_check(&attempt)
            .await
            .map_err(|err| ProbeFailure::from(&err)),
    };
    Ok(Json(probe_response(&conn_row, driver_id, started, outcome)))
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
        let Some(provider) = store.provider_ref(&row.provider_digest) else {
            continue;
        };
        out.insert(
            dependency_key(&conn.connection_id, alias),
            (
                row.reference(provider),
                dependencies
                    .get(alias)
                    .map(|d| d.operations.clone())
                    .unwrap_or_default(),
            ),
        );
    }
    out
}

/// What a probe did, and why it failed when it did.
struct ProbeOutcome {
    probe: &'static str,
    result: Result<(), ProbeFailure>,
}

/// Why a probe failed: the message, and — when a provider answered — its
/// normalised class and what it observed of the effect.
struct ProbeFailure {
    message: String,
    class: Option<ConnectorErrorClass>,
    effect_status: Option<EffectStatus>,
}

impl From<String> for ProbeFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            class: None,
            effect_status: None,
        }
    }
}

impl From<&crate::extensions::ConnectorError> for ProbeFailure {
    fn from(err: &crate::extensions::ConnectorError) -> Self {
        Self {
            message: err.to_string(),
            class: Some(err.class),
            effect_status: Some(err.effect_status),
        }
    }
}

/// The answer to a probe: `healthy`, with `details.probe` naming the check
/// and `details.error` why it failed.
fn probe_response(
    row: &ConnectionRevisionRow,
    driver_id: String,
    started: Instant,
    outcome: ProbeOutcome,
) -> ConnectionProbeResponse {
    let (healthy, failure) = match outcome.result {
        Ok(()) => (true, None),
        Err(failure) => (false, Some(failure)),
    };
    ConnectionProbeResponse {
        connection_id: row.connection_id.to_string(),
        revision: row.revision.get(),
        driver_id,
        healthy,
        latency_ms: started.elapsed().as_millis() as u64,
        details: ProbeDetails {
            probe: outcome.probe.to_string(),
            error: failure.as_ref().map(|f| f.message.clone()),
            error_class: failure.as_ref().and_then(|f| f.class),
            effect_status: failure.as_ref().and_then(|f| f.effect_status),
        },
    }
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
    let config_stage = |result: Result<(), String>| ProbeOutcome {
        probe: "configuration",
        result: result.map_err(ProbeFailure::from),
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
            result: probe_db(state, name, db).await.map_err(ProbeFailure::from),
        },
        ConnectorConfig::Cache(cache) => ProbeOutcome {
            probe: "cache read of a probe key",
            result: probe_cache(state, name, cache)
                .await
                .map_err(ProbeFailure::from),
        },
        ConnectorConfig::Http(http) => ProbeOutcome {
            probe: "TCP connect to the URL's host",
            result: probe_http(name, http).await.map_err(ProbeFailure::from),
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
