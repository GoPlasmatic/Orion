//! V2 Admin API routes (`/api/v2/admin/*`).
//!
//! Resources, not verbs, across focused domain modules:
//! - Runtime capability discovery (`/runtime-capabilities`)
//! - `packages`: connector packages, their admission (`PUT …/admission`) and
//!   revocations (`POST …/revocations`) (`/connector-packages`)
//! - `connections`: immutable connection revisions and their probes
//!   (`/connections/{id}/revisions`)
//! - `deployments`: candidate plans (`/deployment-plans`), and deployments —
//!   an apply or a rollback, both an idempotent CAS commit answered `202` —
//!   with their convergence (`/deployments`)
//! - `edition`: Enterprise-to-Community edition transition preflight (`/edition-transition-preflight`)
//! - `authoring`: Provider package authoring scaffold, pack & lint (`/authoring/*`)
//!
//! Every refusal is a code from the closed registry `orion_api::V2Code`,
//! answered with the status its kind names, or one of the general
//! `orion_api::codes` an `OrionError` answers.

pub(crate) mod authoring;
pub(crate) mod connections;
pub(crate) mod definitions;
pub(crate) mod deployments;
pub(crate) mod edition;
pub(crate) mod packages;
pub(crate) mod plugin_definition;

use std::collections::BTreeMap;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use base64::Engine as _;
use orion_api::v2::{
    DeploymentHead, NativeSchedulerStatus, NodeStatus, ProviderBackendInfo, QuarantinedEntity,
    RuntimeCapabilitiesResponse,
};
use orion_api::{ErrorBody, ErrorEnvelope, ErrorKind};

use crate::admin_auth::AdminPrincipal;
use crate::errors::OrionError;
use crate::extensions::deployment_store::{DeploymentError, DeploymentErrorKind};
use crate::runtime::state::AppState;

pub use deployments::{ConvergenceQuery, DeploymentsQuery};

/// The v2 admin API. `definition_body_size` bounds `POST /definitions`,
/// whose plugin definitions carry a component; everything else is bounded by
/// `max_admin_body_size`.
pub fn v2_admin_routes(
    max_admin_body_size: usize,
    definition_body_size: usize,
) -> Router<AppState> {
    Router::new()
        .route("/runtime-capabilities", get(get_runtime_capabilities))
        .route(
            "/connector-packages",
            get(packages::list_connector_packages).post(packages::upload_connector_package),
        )
        .route(
            "/connector-packages/{digest}",
            get(packages::get_connector_package),
        )
        .route(
            "/connector-packages/{digest}/admission",
            put(packages::admit_connector_package),
        )
        .route(
            "/connector-packages/{digest}/revocations",
            post(packages::revoke_connector_package),
        )
        .route("/connections", get(connections::list_connections))
        .route(
            "/connections/{id}/revisions",
            get(connections::list_connection_revisions)
                .post(connections::create_connection_revision),
        )
        .route(
            "/connections/{id}/revisions/{revision}",
            get(connections::get_connection_revision),
        )
        .route(
            "/connections/{id}/revisions/{revision}/probes",
            post(connections::probe_connection),
        )
        .route(
            "/deployment-plans",
            post(deployments::create_deployment_plan),
        )
        .route(
            "/deployments",
            get(deployments::list_deployments).post(deployments::create_deployment),
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
            "/edition-transition-preflight",
            post(edition::edition_transition_preflight),
        )
        .route("/authoring/scaffold", post(authoring::authoring_scaffold))
        .route("/authoring/lint", post(authoring::authoring_lint))
        .route("/authoring/pack", post(authoring::authoring_pack))
        .merge(definitions::routes().layer(DefaultBodyLimit::max(definition_body_size)))
        .layer(DefaultBodyLimit::max(max_admin_body_size))
}

/// What a v2 handler refuses with: a registered store refusal, or one of the
/// general refusals every endpoint shares (a body that does not parse, a
/// definition that would not serve, by field).
#[derive(Debug)]
pub(crate) enum V2Error {
    Store(DeploymentError),
    Orion(OrionError),
}

pub(crate) type V2Result<T> = Result<T, V2Error>;

impl From<DeploymentError> for V2Error {
    fn from(e: DeploymentError) -> Self {
        Self::Store(e)
    }
}

impl From<OrionError> for V2Error {
    fn from(e: OrionError) -> Self {
        Self::Orion(e)
    }
}

impl From<serde_json::Error> for V2Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Orion(OrionError::from(e))
    }
}

impl V2Error {
    /// A registered refusal with its message.
    pub(crate) fn new(code: DeploymentErrorKind, message: impl Into<String>) -> Self {
        Self::Store(DeploymentError::new(code, message))
    }
}

impl IntoResponse for V2Error {
    fn into_response(self) -> Response {
        let err = match self {
            Self::Orion(e) => return e.into_response(),
            Self::Store(e) => e,
        };
        let kind = err.class();
        // A fault on the server's side names its internals (a driver error,
        // a row it could not decode): logged, and answered without them, as
        // every other 5xx is.
        let message = match kind {
            ErrorKind::Internal => {
                tracing::error!(code = err.code(), error = %err.message, "v2 admin request failed");
                "an internal error occurred; the detail is in the server log".to_string()
            }
            ErrorKind::Unavailable => {
                tracing::warn!(code = err.code(), error = %err.message, "v2 admin request could not reach the state database");
                "the state database could not be read or written; the detail is in the server log"
                    .to_string()
            }
            _ => err.message,
        };
        let status =
            StatusCode::from_u16(kind.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = ErrorEnvelope {
            error: ErrorBody {
                code: err.kind.as_str().to_string(),
                message,
                details: Vec::new(),
                request_id: orion_common::request_context::request_id(),
            },
        };
        (status, Json(body)).into_response()
    }
}

pub(super) fn principal_id(ext: &Option<Extension<AdminPrincipal>>) -> String {
    ext.as_ref()
        .map(|Extension(p)| p.key_id.clone())
        .unwrap_or_else(|| crate::routes::admin::ANONYMOUS_PRINCIPAL.to_string())
}

/// The files of an upload, each UTF-8 text or `base64:<…>`.
pub(super) fn decode_files_map(
    raw: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, Vec<u8>>, V2Error> {
    let mut out = BTreeMap::new();
    for (path, content) in raw {
        let bytes = if let Some(b64) = content.strip_prefix("base64:") {
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| {
                    V2Error::new(
                        DeploymentErrorKind::InvalidPackageEncoding,
                        format!("invalid base64 for '{path}': {e}"),
                    )
                })?
        } else {
            content.as_bytes().to_vec()
        };
        out.insert(path.clone(), bytes);
    }
    Ok(out)
}

/// `GET /api/v2/admin/runtime-capabilities`.
#[utoipa::path(
    get,
    path = "/api/v2/admin/runtime-capabilities",
    tag = "Deployments",
    summary = "Discover runtime capabilities and edition boundaries",
    description = "Returns the v2 contract revision this server speaks (`api_version` — a client that speaks another must stop), its edition (`community` or `enterprise`), registered channel drivers and provider backends (each built-in driver with the digest a connection revision names), the plugin Wasm world, the deployment head, and this node's reconciler status.",
    responses(
        (status = 200, description = "Runtime capabilities report", body = RuntimeCapabilitiesResponse),
    )
)]
pub(crate) async fn get_runtime_capabilities(
    State(state): State<AppState>,
) -> Json<RuntimeCapabilitiesResponse> {
    let comp = &state.composition;
    let (revision, epoch) = state.deployments.current_head();
    let provider_backends = comp
        .providers()
        .registered_drivers()
        .into_iter()
        .map(|(kind, driver_id)| ProviderBackendInfo {
            kind,
            // A built-in driver's digest is what a connection revision
            // names as its provider, so it is reported where a client
            // looks for it.
            package_digest: crate::extensions::foundation::builtin_provider_digest(&driver_id)
                .map(|d| d.to_string()),
            driver_id,
        })
        .collect();
    Json(RuntimeCapabilitiesResponse {
        api_version: orion_api::v2::API_VERSION,
        edition: comp.edition(),
        admission_policy_id: comp.admission().policy_id().to_string(),
        channel_drivers: comp.channels().protocol_ids(),
        schema_profiles: vec![crate::extensions::PROFILE_ID.to_string()],
        wasm_worlds: vec![crate::plugin::manifest::ABI.to_string()],
        provider_backends,
        deployment_head: DeploymentHead { revision, epoch },
        native_scheduler: NativeSchedulerStatus {
            in_flight: state.native_scheduler.in_flight() as u64,
            max_in_flight: state.native_scheduler.max_in_flight() as u64,
        },
        node_status: node_status_dto(&state.reconciler.local_status()),
    })
}

/// A node's reconciler status as the wire carries it.
pub(crate) fn node_status_dto(
    status: &crate::extensions::reconciler::RuntimeNodeStatus,
) -> NodeStatus {
    NodeStatus {
        node_id: status.identity.node_id.clone(),
        boot_id: status.identity.boot_id.clone(),
        desired_revision: status.desired_revision,
        applied_revision: status.applied_revision,
        applied_manifest_digest: status.applied_manifest_digest.clone(),
        configuration_degraded: status.configuration_degraded,
        quarantined_entities: status
            .quarantined_entities
            .iter()
            .map(|q| QuarantinedEntity {
                entity: entity_ref_dto(&q.entity),
                reason: q.reason.clone(),
            })
            .collect(),
        last_error: status.last_error.clone(),
        backoff_attempt: status.backoff_attempt,
        backoff_delay_ms: status.backoff_delay_ms,
        security_lease_expires_at_ms: status.security_lease_expires_at_ms,
        retained_revisions: status.retained_revisions.iter().copied().collect(),
    }
}

/// A member identity as the wire carries it.
pub(crate) fn entity_ref_dto(
    entity: &crate::extensions::deployment_store::EntityRef,
) -> orion_api::v2::EntityRef {
    orion_api::v2::EntityRef {
        kind: entity.kind,
        id: entity.id.clone(),
        revision: entity.revision.get(),
    }
}
