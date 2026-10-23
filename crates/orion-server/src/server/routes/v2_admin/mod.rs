//! V2 Admin API routes (`/api/v2/admin/*`) — `V2-07.1`, `V2-07.2`, `V2-07.3`, `V2-07.4`, `V2-07.5`.
//!
//! Exposes the v2 control-plane surface across focused domain modules (`MOD-03`):
//! - Runtime capability discovery (`/runtime-capabilities`)
//! - [`packages`]: Connector package ingestion, admission & revocation (`/connector-packages`)
//! - [`connections`]: Immutable connection revisions & health probes (`/connections`)
//! - [`deployments`]: Candidate deployment planning, idempotent CAS apply (`202 Accepted`),
//!   convergence polling (`?target=this-node|observed`), and rollback by new revision (`/deployments`)
//! - [`migration`]: Legacy v1 -> v2 migration report & edition transition preflight (`/migration-report`)
//! - [`authoring`]: Provider package authoring scaffold, pack & lint (`/authoring/*`)

mod authoring;
mod connections;
mod deployments;
mod migration;
mod packages;

use std::collections::BTreeMap;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use base64::Engine as _;
use serde_json::{Value, json};

use crate::extensions::deployment_store::DeploymentError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::state::AppState;

pub use authoring::ScaffoldRequest;
pub use connections::{CreateConnectionRevisionRequest, ProbeConnectionRequest};
pub use deployments::{
    ApplyDeploymentRequest, ConvergenceQuery, CreateDeploymentPlanRequest,
    RollbackDeploymentRequest,
};
pub use migration::{
    EditionTransitionPreflightRequest, LegacyChannelItem, LegacyConnectorItem, LegacyWorkflowItem,
    MigrationReportRequest,
};
pub use packages::{AdmitPackageRequest, UploadPackageRequest};

pub fn v2_admin_routes(max_admin_body_size: usize) -> Router<AppState> {
    Router::new()
        .route("/runtime-capabilities", get(get_runtime_capabilities))
        .route(
            "/connector-packages",
            get(packages::list_connector_packages).post(packages::upload_connector_package),
        )
        .route(
            "/connector-packages/{digest}/admit",
            post(packages::admit_connector_package),
        )
        .route(
            "/connector-packages/{digest}/revoke",
            post(packages::revoke_connector_package),
        )
        .route(
            "/connections",
            get(connections::list_connections).post(connections::create_connection_revision),
        )
        .route(
            "/connections/{id}/probe",
            post(connections::probe_connection),
        )
        .route(
            "/deployment-plans",
            post(deployments::create_deployment_plan),
        )
        .route(
            "/deployments",
            get(deployments::get_deployments_summary).post(deployments::apply_deployment),
        )
        .route(
            "/deployments/{revision}",
            get(deployments::get_deployment_revision),
        )
        .route(
            "/deployments/{revision}/convergence",
            get(deployments::get_deployment_convergence),
        )
        .route(
            "/deployments/rollback",
            post(deployments::rollback_deployment),
        )
        .route(
            "/migration-report",
            post(migration::create_migration_report),
        )
        .route(
            "/edition-transition-preflight",
            post(migration::edition_transition_preflight),
        )
        .route("/authoring/scaffold", post(authoring::authoring_scaffold))
        .route("/authoring/lint", post(authoring::authoring_lint))
        .route("/authoring/pack", post(authoring::authoring_pack))
        .layer(DefaultBodyLimit::max(max_admin_body_size))
}

pub(super) fn deployment_err_response(err: DeploymentError) -> (StatusCode, Json<Value>) {
    let status = match err.code.as_str() {
        "PACKAGE_NOT_FOUND"
        | "ADMISSION_NOT_FOUND"
        | "BASE_REVISION_NOT_FOUND"
        | "ROLLBACK_TARGET_NOT_FOUND" => StatusCode::NOT_FOUND,
        "PUBLISHER_VERSION_CONFLICT"
        | "IMMUTABLE_CONTENT_CONFLICT"
        | "IMMUTABLE_CONNECTION_CONFLICT"
        | "IDEMPOTENCY_KEY_CONFLICT"
        | "DEPLOYMENT_HEAD_CONFLICT" => StatusCode::CONFLICT,
        "PROVIDER_NOT_ADMITTED"
        | "PROVIDER_REVOKED"
        | "ADMISSION_INVALIDATED"
        | "BACKFILL_ENTERPRISE_PROVIDER_REQUIRED" => StatusCode::FORBIDDEN,
        _ => StatusCode::BAD_REQUEST,
    };
    (
        status,
        Json(crate::errors::error_envelope_json(err.code, err.message)),
    )
}

pub(super) fn principal_id(ext: Option<Extension<AdminPrincipal>>) -> String {
    ext.map(|Extension(p)| p.key_id)
        .unwrap_or_else(|| crate::server::routes::admin::ANONYMOUS_PRINCIPAL.to_string())
}

pub(super) async fn sync_v2_deployments_to_db(state: &AppState) {
    if let Err(e) = state
        .composition
        .deployments
        .persist_to_db_pool(&state.db_pool)
        .await
    {
        tracing::error!(
            error = %e,
            "Failed to persist v2 deployment store state to database"
        );
    }
}

pub(super) fn decode_files_map(
    raw: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut out = BTreeMap::new();
    for (path, content) in raw {
        let bytes = if let Some(b64) = content.strip_prefix("base64:") {
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| format!("invalid base64 for '{path}': {e}"))?
        } else {
            content.as_bytes().to_vec()
        };
        out.insert(path.clone(), bytes);
    }
    Ok(out)
}

/// `GET /api/v2/admin/runtime-capabilities` (`V2-07.5`).
async fn get_runtime_capabilities(State(state): State<AppState>) -> impl IntoResponse {
    let comp = &state.composition;
    let (head_rev, epoch) = comp.deployments.current_head();
    let local_status = comp.reconciler.local_status();
    let providers: Vec<Value> = comp
        .providers
        .registered_drivers()
        .into_iter()
        .map(|(kind, id)| {
            json!({
                "kind": kind.as_str(),
                "driver_id": id,
            })
        })
        .collect();

    Json(json!({
        "api_version": "v2",
        "edition": "community",
        "schema_profiles": ["orion-json-schema/1"],
        "wasm_worlds": ["orion:connector@2.0.0", "orion:plugin@1.0.0"],
        "wasm_import_profiles": ["core-v2"],
        "channel_drivers": comp.channels.protocol_ids(),
        "provider_backends": providers,
        "admission_policy_id": comp.admission.policy_id(),
        "deployment_head": {
            "revision": head_rev,
            "epoch": epoch,
        },
        "reconciler": {
            "applied_revision": local_status.applied_revision,
            "backoff_attempt": local_status.backoff_attempt,
            "quarantined_entities": local_status.quarantined_entities,
        },
        "native_scheduler": {
            "in_flight": comp.native_scheduler.in_flight(),
            "max_in_flight": comp.native_scheduler.max_in_flight(),
        }
    }))
}
