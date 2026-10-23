//! V2 Immutable connection revisions & health probes (`/api/v2/admin/connections*`).

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use axum::Json;
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
    Json(req): Json<CreateConnectionRevisionRequest>,
) -> impl IntoResponse {
    match state.composition.deployments.store_connection_revision(
        &req.connection_id,
        req.revision,
        &req.provider_digest,
        req.public_config,
        req.secret_locator,
    ) {
        Ok(row) => {
            sync_v2_deployments_to_db(&state).await;
            (StatusCode::CREATED, Json(json!({ "connection": row })))
        }
        Err(e) => deployment_err_response(e),
    }
}

/// `POST /api/v2/admin/connections/{id}/probe` (`V2-07.1`).
pub(super) async fn probe_connection(
    State(state): State<AppState>,
    Path(connection_id): Path<String>,
    req: Option<Json<ProbeConnectionRequest>>,
) -> impl IntoResponse {
    let req = req.map(|Json(r)| r).unwrap_or_default();
    let kind = match req.kind.as_deref().unwrap_or("rust") {
        "workflow" => ImplementationKind::Workflow,
        "wasm" => ImplementationKind::Wasm,
        _ => ImplementationKind::Rust,
    };
    let driver_id = req.driver_id.as_deref().unwrap_or(&connection_id);
    let Some(backend) = state.composition.providers.get(kind, driver_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(crate::errors::error_envelope_json(
                "PROVIDER_NOT_FOUND",
                format!(
                    "provider ({}, '{driver_id}') is not registered",
                    kind.as_str()
                ),
            )),
        );
    };

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
        revision: 1,
        provider: ProviderReference {
            provider_id: driver_id.to_string(),
            version: "2.0.0".to_string(),
            package_digest:
                "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .to_string(),
        },
    };
    let eff = EffectiveConnectionIdentity {
        provider_digest: conn_ref.provider.package_digest.clone(),
        connection_id: connection_id.clone(),
        revision: 1,
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
                "driver_id": driver_id,
                "healthy": true,
            })),
        ),
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "connection_id": connection_id,
                "driver_id": driver_id,
                "healthy": false,
                "error": err,
            })),
        ),
    }
}
