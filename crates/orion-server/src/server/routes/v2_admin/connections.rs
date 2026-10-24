//! V2 Immutable connection revisions & health probes (`/api/v2/admin/connections*`).

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use axum::{Extension, Json};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{deployment_err_response, sync_v2_deployments_to_db};
use crate::extensions::{
    AdmittedRun, AuthorityGrant, BudgetLimits, ConnectionReference, EffectiveConnectionIdentity,
    ImplementationKind, ProviderReference,
};
use crate::server::admin_auth::AdminPrincipal;
use crate::server::state::AppState;

#[derive(Debug, Deserialize)]
pub struct CreateConnectionRevisionRequest {
    pub connection_id: String,
    pub revision: u64,
    pub provider_digest: String,
    #[serde(default)]
    pub public_config: Value,
    pub secret_locator: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ProbeConnectionRequest {
    pub driver_id: Option<String>,
    pub kind: Option<String>,
}

/// `GET /api/v2/admin/connections` (`V2-07.1`).
pub(super) async fn list_connections(State(state): State<AppState>) -> impl IntoResponse {
    let connections = state.composition.deployments.list_connections();
    Json(json!({ "connections": connections }))
}

/// `POST /api/v2/admin/connections` (`V2-07.1`).
pub(super) async fn create_connection_revision(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Json(req): Json<CreateConnectionRevisionRequest>,
) -> impl IntoResponse {
    let snapshot = state.composition.deployments.take_snapshot();
    match state.composition.deployments.store_connection_revision(
        &req.connection_id,
        req.revision,
        &req.provider_digest,
        req.public_config,
        req.secret_locator,
    ) {
        Ok(row) => {
            if let Err(err_resp) = sync_v2_deployments_to_db(&state, snapshot).await {
                return err_resp;
            }
            crate::server::routes::admin::audit_log(
                &state.audit_queue,
                &principal,
                "v2_connection_create",
                "connection",
                &format!("{}@{}", row.connection_id, row.revision),
            );
            (StatusCode::CREATED, Json(json!({ "connection": row })))
        }
        Err(e) => deployment_err_response(e),
    }
}

/// `POST /api/v2/admin/connections/{id}/probe` (`V2-07.1`, `TD-30`).
pub(super) async fn probe_connection(
    State(state): State<AppState>,
    Path(connection_id): Path<String>,
    req: Option<Json<ProbeConnectionRequest>>,
) -> impl IntoResponse {
    let req = req.map(|Json(r)| r).unwrap_or_default();

    let stored_conn = state
        .composition
        .deployments
        .list_connections()
        .into_iter()
        .filter(|c| c.connection_id == connection_id)
        .max_by_key(|c| c.revision);

    let stored_pkg = stored_conn.as_ref().and_then(|conn_row| {
        state
            .composition
            .deployments
            .list_packages()
            .into_iter()
            .find(|p| p.package_digest == conn_row.provider_digest)
    });

    if let Some(ref pkg) = stored_pkg {
        let is_revoked = state
            .composition
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
    let kind = match kind_str {
        "workflow" => ImplementationKind::Workflow,
        "wasm" => ImplementationKind::Wasm,
        _ => ImplementationKind::Rust,
    };

    let driver_id = req
        .driver_id
        .clone()
        .or_else(|| stored_pkg.as_ref().map(|p| p.provider_id.clone()))
        .unwrap_or_else(|| connection_id.clone());

    let Some(backend) = state.composition.providers.get(kind, &driver_id) else {
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
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "connection_id": connection_id,
                "revision": conn_row.revision,
                "driver_id": driver_id,
                "healthy": false,
                "error": cfg_err,
            })),
        );
    }

    let resolved_revision = stored_conn.as_ref().map(|c| c.revision).unwrap_or(1);
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
    let grant = AuthorityGrant {
        allowed_operations,
        declared_dependencies: BTreeMap::new(),
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
        Ok(()) => (
            StatusCode::OK,
            Json(json!({
                "connection_id": connection_id,
                "revision": resolved_revision,
                "driver_id": driver_id,
                "healthy": true,
            })),
        ),
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "connection_id": connection_id,
                "revision": resolved_revision,
                "driver_id": driver_id,
                "healthy": false,
                "error": err,
            })),
        ),
    }
}
