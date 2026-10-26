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

use crate::errors::OrionError;
use crate::extensions::deployment_store::{DeploymentError, DeploymentErrorKind};
use crate::server::admin_auth::AdminPrincipal;
use crate::server::state::AppState;

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
                request_id: crate::request_context::request_id(),
            },
        };
        (status, Json(body)).into_response()
    }
}

pub(super) fn principal_id(ext: &Option<Extension<AdminPrincipal>>) -> String {
    ext.as_ref()
        .map(|Extension(p)| p.key_id.clone())
        .unwrap_or_else(|| crate::server::routes::admin::ANONYMOUS_PRINCIPAL.to_string())
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use serde_json::{Value, json};

    /// The v2 admin router behind `orion_client::V2Admin`: every typed
    /// client call, sent through the real routes.
    struct RouterClient(axum::Router);

    impl orion_client::V2Admin for RouterClient {
        type Error = orion_client::ClientError;

        async fn send(
            &self,
            method: orion_client::Method,
            path: &str,
            body: Option<Value>,
        ) -> Result<Value, Self::Error> {
            use axum::body::{Body, to_bytes};
            use tower::ServiceExt as _;
            let mut req = axum::http::Request::builder()
                .method(method.as_str())
                .uri(path);
            let body = match body {
                Some(body) => {
                    req = req.header("content-type", "application/json");
                    Body::from(serde_json::to_vec(&body)?)
                }
                None => Body::empty(),
            };
            let resp = self
                .0
                .clone()
                .oneshot(req.body(body).unwrap())
                .await
                .unwrap();
            let status = resp.status();
            let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            if !status.is_success() {
                return Err(orion_client::ClientError::from_response(
                    orion_client::StatusCode::from_u16(status.as_u16()).unwrap(),
                    &bytes,
                ));
            }
            Ok(serde_json::from_slice(&bytes)?)
        }
    }

    /// Every typed client call the CLI makes, against the real router:
    /// upload, admit, connect, probe, plan, apply, read back, roll back,
    /// revoke.
    #[tokio::test]
    async fn cli_and_server_contract_end_to_end_against_running_server() {
        use orion_api::v2::*;
        use orion_client::V2Admin as _;
        use std::str::FromStr;

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:").unwrap())
            .await
            .unwrap();
        crate::storage::migrator_for(crate::storage::DbBackend::Sqlite)
            .run(&pool)
            .await
            .unwrap();
        let db_pool = crate::storage::DbPool::Sqlite(pool);

        let metrics_handle = crate::metrics::init_metrics_with_instance(None);
        let mut config = crate::config::AppConfig::default();
        config.cluster.instance_id = "node-cli-test".to_string();
        let assembled = crate::bootstrap::assemble(
            crate::bootstrap::AssembleParams {
                config: std::sync::Arc::new(config),
                pool: db_pool,
                metrics_handle,
            },
            crate::extensions::CompositionRoot::community(),
        )
        .await
        .expect("assemble test server");
        let client = RouterClient(
            axum::Router::new()
                .nest(
                    "/api/v2/admin",
                    super::v2_admin_routes(10 * 1024 * 1024, 10 * 1024 * 1024),
                )
                .with_state(assembled.state),
        );

        // 1. Capabilities
        let cap = client.runtime_capabilities().await.unwrap();
        assert_eq!(cap.api_version, orion_api::v2::API_VERSION);
        assert_eq!(client.server_api_version().await.unwrap(), Some(1));
        assert_eq!(cap.edition, EditionTier::Community);
        assert_eq!(cap.wasm_worlds, vec!["orion:plugin@1.0.0"]);
        assert_eq!(cap.channel_drivers, vec!["cron", "http", "rest"]);
        assert_eq!(cap.node_status.node_id, "node-cli-test");
        assert!(
            cap.provider_backends
                .iter()
                .any(|b| b.driver_id == "org.orion.sqlite" && b.package_digest.is_some()),
            "{cap:?}"
        );

        // 2. Scaffold, lint, pack
        let scaffold = client
            .authoring_scaffold(&ScaffoldRequest {
                publisher: "org.orion".to_string(),
                provider_id: "org.orion.memory-cache".to_string(),
                version: "2.0.0".to_string(),
                kind: ImplementationKind::Rust,
            })
            .await
            .unwrap();
        assert!(!scaffold.files.is_empty());
        assert_eq!(scaffold.implementation, ImplementationKind::Rust);
        let bundle = UploadPackageRequest {
            files: scaffold.files.clone(),
            ..Default::default()
        };
        assert!(client.authoring_lint(&bundle).await.unwrap().valid);
        let packed = client.authoring_pack(&bundle).await.unwrap();
        assert!(packed.package_digest.starts_with("sha256:"));
        assert!(packed.files.contains_key("inventory.json"));

        // 3. Upload, read back, admit
        let uploaded = client
            .upload_connector_package(&UploadPackageRequest {
                files: packed.files.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(uploaded.package.digest, packed.package_digest);
        let digest = uploaded.package.digest.clone();
        let read = client.get_connector_package(&digest).await.unwrap();
        assert_eq!(read.package.digest, digest);
        assert!(read.admission.is_none(), "not admitted yet");
        let admitted = client
            .admit_connector_package(&digest, &AdmitPackageRequest::default())
            .await
            .unwrap();
        assert_eq!(admitted.admission.package_digest, digest);
        let listed = client.list_connector_packages().await.unwrap();
        assert_eq!(listed.packages.len(), 1);
        assert!(listed.packages[0].admitted);
        assert!(
            client
                .get_connector_package(&digest)
                .await
                .unwrap()
                .package
                .admitted
        );

        // 4. A connection revision, read back and probed
        let created = client
            .create_connection_revision(
                "cache-main",
                &CreateConnectionRevisionRequest {
                    revision: 1,
                    provider_digest: digest.clone(),
                    public_config: json!({"endpoint": "https://cache.example.com"}),
                    secret_locators: Default::default(),
                    bindings: Default::default(),
                },
            )
            .await
            .unwrap();
        assert_eq!(created.connection.revision, 1);
        let revision = client
            .get_connection_revision("cache-main", 1)
            .await
            .unwrap();
        assert_eq!(
            revision.configuration_digest,
            created.connection.configuration_digest
        );
        let revisions = client
            .list_connection_revisions("cache-main")
            .await
            .unwrap();
        assert_eq!(revisions.count, 1);
        let probe = client.probe_connection("cache-main", 1).await.unwrap();
        assert!(probe.healthy, "{probe:?}");
        assert!(probe.details.error.is_none(), "{probe:?}");
        let missing = client.probe_connection("cache-main", 2).await.unwrap_err();
        assert_eq!(missing.status().map(|s| s.as_u16()), Some(404));

        // 5. Plan, then apply with the reviewed digest
        let manifest = DeploymentManifest {
            providers: vec![ManifestProvider {
                publisher: "org.orion".to_string(),
                provider_id: "org.orion.memory-cache".to_string(),
                version: "2.0.0".to_string(),
                digest: digest.clone(),
            }],
            ..DeploymentManifest::of_members(vec![ManifestMember {
                entity: EntityRef {
                    kind: EntityKind::Connection,
                    id: "cache-main".to_string(),
                    revision: 1,
                },
                definition_digest: created.connection.configuration_digest.clone(),
                owning_package: None,
                requires: Vec::new(),
                rollouts: Vec::new(),
            }])
        };
        let plan = client
            .plan_deployment(&CreateDeploymentPlanRequest {
                base_revision: 0,
                manifest: manifest.clone(),
            })
            .await
            .unwrap();
        assert!(plan.reviewed_manifest_digest.starts_with("sha256:"));
        assert_eq!(
            plan.added_entities,
            vec![EntityRef {
                kind: EntityKind::Connection,
                id: "cache-main".into(),
                revision: 1,
            }]
        );
        assert!(plan.updated_entities.is_empty() && plan.removed_entities.is_empty());

        let apply = |key: &str, reviewed: &str| {
            CreateDeploymentRequest::Apply(ApplyDeploymentRequest {
                idempotency_key: key.to_string(),
                base_revision: 0,
                manifest: manifest.clone(),
                reason: "cli apply".to_string(),
                reviewed_manifest_digest: reviewed.to_string(),
            })
        };
        let mismatch = client
            .create_deployment(&apply(
                "idem-bad",
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            ))
            .await
            .unwrap_err();
        match mismatch {
            orion_client::ClientError::Api { status, error } => {
                assert_eq!(status.as_u16(), 409);
                assert_eq!(error.code, "REVIEWED_DIGEST_MISMATCH");
            }
            other => panic!("{other:?}"),
        }
        let applied = client
            .create_deployment(&apply("idem-apply-1", &plan.reviewed_manifest_digest))
            .await
            .unwrap();
        assert_eq!(applied.committed_revision, 1);
        assert_eq!(applied.receipt.committed_revision, 1);

        // 6. Convergence, and the revision read back as a page and in full
        let conv = client
            .deployment_convergence(1, Some("this-node"))
            .await
            .unwrap();
        assert!(conv.converged);
        assert_eq!(conv.target_revision, 1);
        let page = client.list_deployments(None, None).await.unwrap();
        assert_eq!(page.head_revision, 1);
        assert_eq!(page.data.len(), 1);
        assert_eq!(page.data[0].member_count, 1);
        assert!(page.next_cursor.is_none());
        let got = client.get_deployment(1).await.unwrap();
        assert_eq!(got.summary.revision, 1);
        assert_eq!(got.summary.manifest_digest, plan.reviewed_manifest_digest);
        assert_eq!(got.manifest, manifest);

        // 7. Rollback: a new revision deploying revision 1 again
        let rolled = client
            .create_deployment(&CreateDeploymentRequest::Rollback(
                RollbackDeploymentRequest {
                    idempotency_key: "idem-rb-1".to_string(),
                    base_revision: 1,
                    rollback_to: 1,
                    reason: "rollback test".to_string(),
                },
            ))
            .await
            .unwrap();
        assert_eq!(rolled.committed_revision, 2);
        assert!(
            !rolled.warnings.is_empty(),
            "a rollback says what it does not undo"
        );
        let first = client.list_deployments(Some(1), None).await.unwrap();
        assert_eq!(first.data[0].revision, 2);
        let cursor = first.next_cursor.expect("an older page follows");
        let second = client
            .list_deployments(Some(1), Some(&cursor))
            .await
            .unwrap();
        assert_eq!(second.data[0].revision, 1);
        assert!(second.next_cursor.is_none());

        // 8. Revoke
        let revoked = client
            .revoke_connector_package(&digest, &RevokePackageRequest::default())
            .await
            .unwrap();
        assert_eq!(revoked.package_digest, digest);
        let read = client.get_connector_package(&digest).await.unwrap();
        assert!(read.package.revoked);
    }
}
