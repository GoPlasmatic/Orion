//! V2 Admin API routes (`/api/v2/admin/*`) — `V2-07.1`, `V2-07.2`, `V2-07.3`, `V2-07.4`, `V2-07.5`.
//!
//! Exposes the v2 control-plane surface across focused domain modules:
//! - Runtime capability discovery (`/runtime-capabilities`)
//! - `packages`: Connector package ingestion, admission & revocation (`/connector-packages`)
//! - `connections`: Immutable connection revisions & health probes (`/connections`)
//! - `deployments`: Candidate deployment planning, idempotent CAS apply (`202 Accepted`),
//!   convergence polling (`?target=this-node|observed`), and rollback by new revision (`/deployments`)
//! - `edition`: Enterprise-to-Community edition transition preflight (`/edition-transition-preflight`)
//! - `authoring`: Provider package authoring scaffold, pack & lint (`/authoring/*`)

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
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use base64::Engine as _;
use orion_api::v2::RuntimeCapabilitiesResponse;
use serde_json::{Value, json};

use crate::extensions::deployment_store::DeploymentError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::state::AppState;

pub use deployments::ConvergenceQuery;
pub use orion_api::v2::{
    AdmitPackageRequest, ApplyDeploymentRequest, CreateConnectionRevisionRequest,
    CreateDeploymentPlanRequest, EditionTransitionPreflightRequest, ProbeConnectionRequest,
    RollbackDeploymentRequest, ScaffoldRequest, UploadPackageRequest,
};

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
            "/edition-transition-preflight",
            post(edition::edition_transition_preflight),
        )
        .route("/authoring/scaffold", post(authoring::authoring_scaffold))
        .route("/authoring/lint", post(authoring::authoring_lint))
        .route("/authoring/pack", post(authoring::authoring_pack))
        .merge(definitions::routes().layer(DefaultBodyLimit::max(definition_body_size)))
        .layer(DefaultBodyLimit::max(max_admin_body_size))
}

pub(super) fn deployment_err_response(err: DeploymentError) -> (StatusCode, Json<Value>) {
    let status = match err.code.as_str() {
        "PACKAGE_NOT_FOUND"
        | "ADMISSION_NOT_FOUND"
        | "BASE_REVISION_NOT_FOUND"
        | "ROLLBACK_TARGET_NOT_FOUND"
        | "CONNECTION_REVISION_NOT_FOUND" => StatusCode::NOT_FOUND,
        "PUBLISHER_VERSION_CONFLICT"
        | "IMMUTABLE_CONTENT_CONFLICT"
        | "IMMUTABLE_CONNECTION_CONFLICT"
        | "IDEMPOTENCY_KEY_CONFLICT"
        | "DEPLOYMENT_HEAD_CONFLICT"
        | "REVIEWED_DIGEST_MISMATCH" => StatusCode::CONFLICT,
        "PROVIDER_NOT_ADMITTED" | "PROVIDER_REVOKED" | "ADMISSION_INVALIDATED" => {
            StatusCode::FORBIDDEN
        }
        "DB_TRANSACTION_ERROR" | "DB_WRITE_ERROR" | "DB_COMMIT_ERROR" | "DB_READ_ERROR" => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        "CORRUPT_PERSISTED_STATE" => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    (
        status,
        Json(crate::errors::error_envelope_json(err.code, err.message)),
    )
}

pub(super) fn principal_id(ext: &Option<Extension<AdminPrincipal>>) -> String {
    ext.as_ref()
        .map(|Extension(p)| p.key_id.clone())
        .unwrap_or_else(|| crate::server::routes::admin::ANONYMOUS_PRINCIPAL.to_string())
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

/// `GET /api/v2/admin/runtime-capabilities`.
#[utoipa::path(
    get,
    path = "/api/v2/admin/runtime-capabilities",
    tag = "Deployments",
    summary = "Discover runtime capabilities and edition boundaries",
    description = "Returns the server's active v2 edition (`community` or `enterprise`), registered channel drivers, provider backends, the plugin Wasm world, deployment head revision, and reconciler status.",
    responses(
        (status = 200, description = "Runtime capabilities report", body = RuntimeCapabilitiesResponse),
    )
)]
pub(crate) async fn get_runtime_capabilities(State(state): State<AppState>) -> impl IntoResponse {
    let comp = &state.composition;
    let (head_rev, epoch) = comp.deployments.current_head();
    let local_status = comp.reconciler.local_status();
    let policy_id = comp.admission.policy_id().to_string();
    let edition = comp.admission.edition().as_str();
    let drivers = comp.providers.registered_drivers();
    let mut rust_providers = Vec::new();
    let mut workflow_providers = Vec::new();
    let mut providers = Vec::new();
    for (kind, id) in drivers {
        match kind {
            crate::extensions::ImplementationKind::Rust => rust_providers.push(id.clone()),
            crate::extensions::ImplementationKind::Workflow => workflow_providers.push(id.clone()),
            crate::extensions::ImplementationKind::Wasm => {}
        }
        // A built-in driver's digest is what a connection revision names
        // as its provider, so it is reported where a client looks for it.
        let mut backend = json!({
            "kind": kind.as_str(),
            "driver_id": id,
        });
        if let Some(digest) = crate::extensions::foundation::builtin_provider_digest(&id) {
            backend["package_digest"] = json!(digest);
        }
        providers.push(backend);
    }

    Json(RuntimeCapabilitiesResponse {
        api_version: "v2".to_string(),
        edition: edition.to_string(),
        admission_policy_id: policy_id,
        registered_channel_drivers: comp.channels.protocol_ids(),
        schema_profiles: vec![crate::extensions::PROFILE_ID.to_string()],
        wasm_worlds: vec![crate::plugin::manifest::ABI.to_string()],
        provider_backends: providers,
        registered_rust_providers: rust_providers,
        registered_workflow_providers: workflow_providers,
        deployment_head: json!({
            "revision": head_rev,
            "epoch": epoch,
        }),
        reconciler: json!({
            "applied_revision": local_status.applied_revision,
            "backoff_attempt": local_status.backoff_attempt,
            "quarantined_entities": local_status.quarantined_entities,
        }),
        native_scheduler: json!({
            "in_flight": comp.native_scheduler.in_flight(),
            "max_in_flight": comp.native_scheduler.max_in_flight(),
        }),
        active_deployment_revision: local_status.applied_revision,
        node_status: serde_json::to_value(&local_status).unwrap_or(Value::Null),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn v2_admin_capabilities_and_dtos_match_wire_contract() {
        let comp = crate::extensions::CompositionRoot::community();
        let (head_rev, epoch) = comp.deployments.current_head();
        let local_status = comp.reconciler.local_status();
        let policy_id = comp.admission.policy_id();
        let payload = json!({
            "api_version": "v2",
            "edition": "community",
            "schema_profiles": [crate::extensions::PROFILE_ID],
            "wasm_worlds": [crate::plugin::manifest::ABI],
            "channel_drivers": comp.channels.protocol_ids(),
            "provider_backends": [],
            "admission_policy_id": policy_id,
            "deployment_head": { "revision": head_rev, "epoch": epoch },
            "reconciler": {
                "applied_revision": local_status.applied_revision,
                "backoff_attempt": local_status.backoff_attempt,
                "quarantined_entities": local_status.quarantined_entities,
            },
            "native_scheduler": {
                "in_flight": comp.native_scheduler.in_flight(),
                "max_in_flight": comp.native_scheduler.max_in_flight(),
            }
        });

        let dto: orion_api::v2::RuntimeCapabilitiesResponse =
            serde_json::from_value(payload).expect("deserialize RuntimeCapabilitiesResponse");
        assert_eq!(dto.api_version, "v2");
        assert_eq!(dto.edition, "community");
        assert_eq!(dto.wasm_worlds, vec!["orion:plugin@1.0.0"]);
        assert_eq!(
            dto.registered_channel_drivers,
            vec!["cron", "http", "rest", "source"]
        );
    }

    #[tokio::test]
    async fn cli_and_server_contract_end_to_end_against_running_server() {
        use axum::body::{Body, to_bytes};
        use axum::http::{Method, Request, StatusCode};
        use orion_api::v2::*;
        use orion_client::paths;
        use std::str::FromStr;
        use tower::ServiceExt as _;

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
        let assembled = crate::bootstrap::assemble(crate::bootstrap::AssembleParams {
            config: std::sync::Arc::new(config),
            pool: db_pool,
            metrics_handle,
        })
        .await
        .expect("assemble test server");
        let app = axum::Router::new()
            .nest(
                "/api/v2/admin",
                super::v2_admin_routes(10 * 1024 * 1024, 10 * 1024 * 1024),
            )
            .with_state(assembled.state);

        async fn send_get<Resp: serde::de::DeserializeOwned>(
            app: &axum::Router,
            path: &str,
        ) -> (StatusCode, Resp) {
            let req = Request::builder()
                .method(Method::GET)
                .uri(path)
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            let parsed: Resp = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                panic!(
                    "failed to decode GET {path} (status {status}): {e}; body={}",
                    String::from_utf8_lossy(&bytes)
                )
            });
            (status, parsed)
        }

        async fn send_post<Req: serde::Serialize, Resp: serde::de::DeserializeOwned>(
            app: &axum::Router,
            path: &str,
            body: &Req,
        ) -> (StatusCode, Resp) {
            let payload = serde_json::to_vec(body).unwrap();
            let req = Request::builder()
                .method(Method::POST)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(payload))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            let parsed: Resp = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                panic!(
                    "failed to decode POST {path} (status {status}): {e}; body={}",
                    String::from_utf8_lossy(&bytes)
                )
            });
            (status, parsed)
        }

        // 1. Capabilities
        let (status, cap): (_, RuntimeCapabilitiesResponse) =
            send_get(&app, paths::V2_RUNTIME_CAPABILITIES).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cap.api_version, "v2");
        assert_eq!(cap.edition, "community");

        // 2. Scaffold (sending `kind`, and also verifying `implementation` alias works)
        let scaffold_req = ScaffoldRequest {
            publisher: "org.orion".to_string(),
            provider_id: "org.orion.memory-cache".to_string(),
            version: "2.0.0".to_string(),
            kind: "rust".to_string(),
        };
        let (status, scaffold_resp): (_, ScaffoldResponse) =
            send_post(&app, paths::V2_AUTHORING_SCAFFOLD, &scaffold_req).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!scaffold_resp.files.is_empty());
        assert_eq!(scaffold_resp.implementation, "rust");

        // 3. Lint
        let bundle_req = UploadPackageRequest {
            files: scaffold_resp.files.clone(),
            ..Default::default()
        };
        let (status, lint_resp): (_, LintResponse) =
            send_post(&app, paths::V2_AUTHORING_LINT, &bundle_req).await;
        assert_eq!(status, StatusCode::OK);
        assert!(lint_resp.valid);

        // 4. Pack
        let (status, pack_resp): (_, PackResponse) =
            send_post(&app, paths::V2_AUTHORING_PACK, &bundle_req).await;
        assert_eq!(status, StatusCode::OK);
        assert!(pack_resp.package_digest.starts_with("sha256:"));
        assert!(pack_resp.files.contains_key("inventory.json"));

        // 5. Upload & Admit
        let upload_req = UploadPackageRequest {
            files: pack_resp.files.clone(),
            ..Default::default()
        };
        let (status, upload_resp): (_, UploadPackageResponse) =
            send_post(&app, paths::V2_CONNECTOR_PACKAGES, &upload_req).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(upload_resp.digest, pack_resp.package_digest);

        let (status, admit_resp): (_, AdmitPackageResponse) = send_post(
            &app,
            &paths::v2_connector_package_admit(&upload_resp.digest),
            &AdmitPackageRequest::default(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(admit_resp.admission.package_digest, upload_resp.digest);

        let (status, pkg_list): (_, ConnectorPackagesListResponse) =
            send_get(&app, paths::V2_CONNECTOR_PACKAGES).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(pkg_list.packages.len(), 1);
        assert!(pkg_list.packages[0].admitted);

        // 6. Connections Create, List & Probe
        let conn_req = CreateConnectionRevisionRequest {
            connection_id: "cache-main".to_string(),
            revision: 1,
            provider_digest: upload_resp.digest.clone(),
            public_config: json!({"max_entries": 128}),
            secret_locators: Default::default(),
        };
        let (status, conn_resp): (_, CreateConnectionRevisionResponse) =
            send_post(&app, paths::V2_CONNECTIONS, &conn_req).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(conn_resp.connection.revision, 1);

        let (status, probe_resp): (_, ConnectionProbeResponse) = send_post(
            &app,
            &paths::v2_connection_probe("cache-main"),
            &ProbeConnectionRequest {
                revision: Some(1),
                driver_id: None,
                kind: None,
            },
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(probe_resp.healthy);
        assert_eq!(probe_resp.status, "healthy");

        // 7. Deployment Plan & Apply (including reviewed_manifest_digest validation)
        let manifest_val = json!({
            "schema_version": 2,
            "providers": [{
                "publisher": "org.orion",
                "provider_id": "org.orion.memory-cache",
                "version": "2.0.0",
                "digest": upload_resp.digest,
            }],
            "members": [{
                "entity": {
                    "kind": "connection",
                    "id": "cache-main",
                    "revision": 1,
                },
                "definition_digest": conn_resp.connection.configuration_digest,
                "requires": [],
                "rollouts": [],
            }],
        });
        let (status, plan_resp): (_, DeploymentPlanResponse) = send_post(
            &app,
            paths::V2_DEPLOYMENT_PLANS,
            &CreateDeploymentPlanRequest {
                base_revision: 0,
                manifest: manifest_val.clone(),
                reason: Some("initial plan".to_string()),
            },
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(plan_resp.reviewed_manifest_digest.starts_with("sha256:"));

        // Mismatched reviewed_manifest_digest must be rejected with 409
        let (bad_status, bad_body): (_, Value) = send_post(
            &app,
            paths::V2_DEPLOYMENTS,
            &ApplyDeploymentRequest {
                idempotency_key: "idem-bad".to_string(),
                base_revision: 0,
                manifest: manifest_val.clone(),
                reason: "cli apply".to_string(),
                reviewed_manifest_digest: Some(
                    "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                        .to_string(),
                ),
            },
        )
        .await;
        assert_eq!(bad_status, StatusCode::CONFLICT);
        assert_eq!(bad_body["error"]["code"], "REVIEWED_DIGEST_MISMATCH");

        // Matching reviewed_manifest_digest succeeds
        let (status, apply_resp): (_, ApplyDeploymentResponse) = send_post(
            &app,
            paths::V2_DEPLOYMENTS,
            &ApplyDeploymentRequest {
                idempotency_key: "idem-apply-1".to_string(),
                base_revision: 0,
                manifest: manifest_val.clone(),
                reason: "cli apply".to_string(),
                reviewed_manifest_digest: Some(plan_resp.reviewed_manifest_digest.clone()),
            },
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(apply_resp.committed_revision, 1);
        assert_eq!(apply_resp.receipt.committed_revision, 1);

        // 8. Convergence polling & Get revision
        let (status, conv): (_, ConvergenceStatusResponse) = send_get(
            &app,
            &paths::v2_deployment_convergence(1, Some("this-node")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(conv.converged);
        assert_eq!(conv.target_revision, 1);

        let (status, got_rev): (_, DeploymentRevisionResponse) =
            send_get(&app, &paths::v2_deployment(1)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(got_rev.revision, 1);
        assert_eq!(got_rev.manifest_digest, plan_resp.reviewed_manifest_digest);

        // 9. Rollback
        let (status, rb_resp): (_, RollbackDeploymentResponse) = send_post(
            &app,
            paths::V2_DEPLOYMENTS_ROLLBACK,
            &RollbackDeploymentRequest {
                idempotency_key: "idem-rb-1".to_string(),
                expected_head_revision: 1,
                target_revision: 1,
                reason: "rollback test".to_string(),
            },
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(rb_resp.committed_revision, 2);
    }
}
