//! V2 Connector package ingestion, admission & revocation routes (`/api/v2/admin/connector-packages*`).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Extension, Json};
use orion_api::v2::{
    AdmitPackageRequest, AdmitPackageResponse, ConnectorPackageSummary,
    ConnectorPackagesListResponse, ProviderAdmissionDto, RevokePackageRequest,
    RevokePackageResponse, UploadPackageRequest, UploadPackageResponse,
};
use serde_json::{Value, json};

use super::{decode_files_map, deployment_err_response};
use crate::extensions::authoring::pack_provider_package;
use crate::extensions::deployment_store::{ArchiveEntryInput, ArtifactState, ProviderPackageRow};
use crate::server::admin_auth::AdminPrincipal;
use crate::server::state::AppState;

fn package_row_to_summary(
    row: &ProviderPackageRow,
    admitted: bool,
    revoked: bool,
    artifact_state: &str,
) -> ConnectorPackageSummary {
    let tier = row
        .distribution_json
        .get("tier")
        .and_then(Value::as_str)
        .unwrap_or("community")
        .to_string();
    ConnectorPackageSummary {
        publisher: row.publisher.clone(),
        provider_id: row.provider_id.clone(),
        version: row.version.clone(),
        digest: row.package_digest.clone(),
        package_digest: row.package_digest.clone(),
        implementation: row.implementation.as_str().to_string(),
        tier,
        admitted,
        revoked,
        artifact_state: artifact_state.to_string(),
        manifest: row.manifest_json.clone(),
        distribution: row.distribution_json.clone(),
        inventory: row.inventory_json.clone(),
    }
}

/// `GET /api/v2/admin/connector-packages`.
#[utoipa::path(
    get,
    path = "/api/v2/admin/connector-packages",
    tag = "Providers",
    summary = "List uploaded and admitted connector provider packages",
    description = "Returns all ingested provider packages alongside their admission and revocation state.",
    responses(
        (status = 200, description = "Connector provider packages", body = ConnectorPackagesListResponse),
    )
)]
pub(crate) async fn list_connector_packages(State(state): State<AppState>) -> impl IntoResponse {
    let raw_packages = state.composition.deployments.list_packages();
    let raw_admissions = state.composition.deployments.list_admissions();
    let admissions: Vec<ProviderAdmissionDto> = raw_admissions
        .iter()
        .map(|a| ProviderAdmissionDto {
            package_digest: a.package_digest.clone(),
            policy_id: a.policy_id.clone(),
            admission_fingerprint: a.admission_fingerprint.clone(),
            revoked: a.revoked,
        })
        .collect();
    let packages: Vec<ConnectorPackageSummary> = raw_packages
        .iter()
        .map(|p| {
            let adm = raw_admissions
                .iter()
                .find(|a| a.package_digest == p.package_digest);
            let admitted = adm.is_some_and(|a| !a.revoked);
            let revoked = adm.is_some_and(|a| a.revoked);
            let artifact_state = match state
                .composition
                .deployments
                .get_artifact(&p.package_digest)
                .map(|a| a.state)
            {
                Some(ArtifactState::Draining) => "draining",
                Some(ArtifactState::Deleted) => "deleted",
                _ => "active",
            };
            package_row_to_summary(p, admitted, revoked, artifact_state)
        })
        .collect();
    Json(ConnectorPackagesListResponse {
        count: packages.len(),
        packages,
        admissions,
    })
}

/// `POST /api/v2/admin/connector-packages`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/connector-packages",
    tag = "Providers",
    summary = "Upload a verified connector provider package",
    description = "Verifies archive bounds, digests, and publisher/version immutability, then stores the package in the deployment store.",
    request_body = UploadPackageRequest,
    responses(
        (status = 201, description = "Package uploaded", body = UploadPackageResponse),
        (status = 400, description = "Invalid package encoding or manifest"),
        (status = 409, description = "Publisher/version or immutable content conflict"),
    )
)]
pub(crate) async fn upload_connector_package(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Json(req): Json<UploadPackageRequest>,
) -> impl IntoResponse {
    let raw_files = req.merged_files();
    let mut files = match decode_files_map(&raw_files) {
        Ok(f) => f,
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(crate::errors::error_envelope_json(
                    "INVALID_PACKAGE_ENCODING",
                    msg,
                )),
            );
        }
    };
    if !files.contains_key("inventory.json")
        && let Ok(packed) = pack_provider_package(&files)
    {
        files.insert("inventory.json".to_string(), packed.inventory_bytes);
    }
    let entries: Vec<ArchiveEntryInput> = files
        .into_iter()
        .map(|(path, bytes)| ArchiveEntryInput {
            path,
            compressed_size: bytes.len(),
            bytes,
            is_symlink: false,
        })
        .collect();

    let row = match state
        .composition
        .deployments
        .ingest_verified_archive_in_db(&state.db_pool, &entries)
        .await
    {
        Ok(row) => row,
        Err(e) => return deployment_err_response(e),
    };

    let mut admission_dto = None;
    if req.admit {
        match state
            .composition
            .deployments
            .admit_package_in_db(
                &state.db_pool,
                &row.package_digest,
                state.composition.admission.as_ref(),
                req.lock.as_ref(),
            )
            .await
        {
            Ok(adm) => {
                admission_dto = Some(ProviderAdmissionDto {
                    package_digest: adm.package_digest,
                    policy_id: adm.policy_id,
                    admission_fingerprint: adm.admission_fingerprint,
                    revoked: adm.revoked,
                });
            }
            Err(e) => return deployment_err_response(e),
        }
    }

    crate::server::routes::admin::audit_log(
        &state.audit_queue,
        &principal,
        "v2_package_upload",
        "connector_package",
        &row.package_digest,
    );

    let summary = package_row_to_summary(&row, admission_dto.is_some(), false, "active");
    let resp = UploadPackageResponse {
        status: "uploaded".to_string(),
        digest: row.package_digest.clone(),
        package: summary,
        admission: admission_dto,
    };
    (
        StatusCode::CREATED,
        Json(serde_json::to_value(resp).unwrap_or(Value::Null)),
    )
}

/// `POST /api/v2/admin/connector-packages/{digest}/admit`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/connector-packages/{digest}/admit",
    tag = "Providers",
    summary = "Admit an uploaded connector provider package",
    description = "Evaluates the active edition admission policy against the uploaded package and records a durable admission fingerprint.",
    params(
        ("digest" = String, Path, description = "Package SHA-256 digest (`sha256:<64-hex>`)")
    ),
    request_body(content = Option<AdmitPackageRequest>, description = "Optional lockfile verification payload"),
    responses(
        (status = 200, description = "Package admitted", body = AdmitPackageResponse),
        (status = 403, description = "Rejected by admission policy"),
        (status = 404, description = "Package not found"),
    )
)]
pub(crate) async fn admit_connector_package(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(digest): Path<String>,
    req: Option<Json<AdmitPackageRequest>>,
) -> impl IntoResponse {
    let lock = req.and_then(|Json(r)| r.lock);
    match state
        .composition
        .deployments
        .admit_package_in_db(
            &state.db_pool,
            &digest,
            state.composition.admission.as_ref(),
            lock.as_ref(),
        )
        .await
    {
        Ok(adm) => {
            crate::server::routes::admin::audit_log(
                &state.audit_queue,
                &principal,
                "v2_package_admit",
                "connector_package",
                &digest,
            );
            let resp = AdmitPackageResponse {
                status: "admitted".to_string(),
                admission: ProviderAdmissionDto {
                    package_digest: adm.package_digest,
                    policy_id: adm.policy_id,
                    admission_fingerprint: adm.admission_fingerprint,
                    revoked: adm.revoked,
                },
            };
            (
                StatusCode::OK,
                Json(serde_json::to_value(resp).unwrap_or(Value::Null)),
            )
        }
        Err(e) => deployment_err_response(e),
    }
}

/// `POST /api/v2/admin/connector-packages/{digest}/revoke`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/connector-packages/{digest}/revoke",
    tag = "Providers",
    summary = "Revoke an admitted connector provider package",
    description = "Marks the package digest as revoked in the deployment store and runtime admission policy, blocking new invocations and deployments.",
    params(
        ("digest" = String, Path, description = "Package SHA-256 digest (`sha256:<64-hex>`)")
    ),
    request_body(content = Option<RevokePackageRequest>, description = "Optional revocation reason"),
    responses(
        (status = 200, description = "Package revoked", body = RevokePackageResponse),
        (status = 404, description = "Admission not found"),
    )
)]
pub(crate) async fn revoke_connector_package(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(digest): Path<String>,
    req: Option<Json<RevokePackageRequest>>,
) -> impl IntoResponse {
    let reason = req.and_then(|Json(r)| r.reason);
    match state
        .composition
        .deployments
        .revoke_package_in_db(&state.db_pool, &digest)
        .await
    {
        Ok(()) => {
            state.composition.admission.revoke_digest(&digest);
            crate::server::routes::admin::audit_log(
                &state.audit_queue,
                &principal,
                "v2_package_revoke",
                "connector_package",
                &digest,
            );
            let resp = RevokePackageResponse {
                status: "revoked".to_string(),
                package_digest: digest,
                revoked: true,
                reason,
            };
            (
                StatusCode::OK,
                Json(serde_json::to_value(resp).unwrap_or_else(|_| json!({"revoked": true}))),
            )
        }
        Err(e) => deployment_err_response(e),
    }
}
