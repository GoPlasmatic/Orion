//! V2 Connector package ingestion, admission & revocation routes (`/api/v2/admin/connector-packages*`).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use orion_api::v2::{
    AdmitPackageRequest, AdmitPackageResponse, ConnectorPackageResponse, ConnectorPackageSummary,
    ConnectorPackagesListResponse, EditionTier, PackageArtifactState, ProviderAdmissionDto,
    RevokePackageRequest, RevokePackageResponse, UploadPackageRequest, UploadPackageResponse,
};
use serde_json::Value;

use super::{V2Error, V2Result, decode_files_map};
use crate::extensions::Digest;
use crate::extensions::authoring::pack_provider_package;
use crate::extensions::deployment_store::{
    ArchiveEntryInput, ArtifactState, DeploymentErrorKind, ProviderAdmissionRow, ProviderPackageRow,
};
use crate::runtime::state::AppState;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::extract::OrionJson;

fn admission_dto(adm: &ProviderAdmissionRow) -> ProviderAdmissionDto {
    ProviderAdmissionDto {
        package_digest: adm.package_digest.to_string(),
        policy_id: adm.policy_id.clone(),
        admission_fingerprint: adm.admission_fingerprint.clone(),
        revoked: adm.revoked,
    }
}

fn artifact_state_name(state: Option<&ArtifactState>) -> PackageArtifactState {
    match state {
        Some(ArtifactState::Draining) => PackageArtifactState::Draining,
        Some(ArtifactState::Deleted) => PackageArtifactState::Deleted,
        Some(ArtifactState::Active) | None => PackageArtifactState::Active,
    }
}

/// A workflow package's hermetic-rule problems as the refusal they are:
/// `400 VALIDATION_ERROR` naming each by file and path.
fn refusal(what: &str, problems: Vec<crate::extensions::PackageProblem>) -> V2Error {
    V2Error::Orion(crate::runtime::workflow_providers::refusal(what, problems))
}

fn package_row_to_summary(
    row: &ProviderPackageRow,
    admitted: bool,
    revoked: bool,
    artifact_state: PackageArtifactState,
) -> ConnectorPackageSummary {
    // Admission refuses a tier that is not one, so a stored package's is.
    let tier = row
        .distribution_json
        .get("tier")
        .and_then(Value::as_str)
        .and_then(EditionTier::parse)
        .unwrap_or(EditionTier::Community);
    ConnectorPackageSummary {
        publisher: row.publisher.clone(),
        provider_id: row.provider_id.to_string(),
        version: row.version.clone(),
        digest: row.package_digest.to_string(),
        implementation: row.implementation,
        tier,
        admitted,
        revoked,
        artifact_state,
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
pub(crate) async fn list_connector_packages(
    State(state): State<AppState>,
) -> Json<ConnectorPackagesListResponse> {
    let raw_packages = state.deployments.list_packages();
    let raw_admissions = state.deployments.list_admissions();
    let digests: Vec<String> = raw_packages
        .iter()
        .map(|p| p.package_digest.to_string())
        .collect();
    // One read for every package's artifact state; a failed read answers
    // each as active, as a package with no artifact row does.
    let states = state
        .deployments
        .artifact_states(&state.db_pool, &digests)
        .await
        .unwrap_or_default();
    let admissions: Vec<ProviderAdmissionDto> =
        raw_admissions.iter().map(|a| admission_dto(a)).collect();
    let packages: Vec<ConnectorPackageSummary> = raw_packages
        .iter()
        .map(|p| {
            let adm = raw_admissions
                .iter()
                .find(|a| a.package_digest == p.package_digest);
            let admitted = adm.is_some_and(|a| !a.revoked);
            let revoked = adm.is_some_and(|a| a.revoked);
            let artifact_state = artifact_state_name(states.get(p.package_digest.as_str()));
            package_row_to_summary(p, admitted, revoked, artifact_state)
        })
        .collect();
    Json(ConnectorPackagesListResponse {
        count: packages.len(),
        packages,
        admissions,
    })
}

/// `GET /api/v2/admin/connector-packages/{digest}`.
#[utoipa::path(
    get,
    path = "/api/v2/admin/connector-packages/{digest}",
    tag = "Providers",
    summary = "Read one connector provider package",
    description = "The package stored under the digest, with its manifest, distribution and inventory, and the admission recorded for it.",
    params(
        ("digest" = String, Path, description = "Package SHA-256 digest (`sha256:<64-hex>`)")
    ),
    responses(
        (status = 200, description = "The package and its admission", body = ConnectorPackageResponse),
        (status = 404, description = "No package is stored under the digest (`PACKAGE_NOT_FOUND`)"),
    )
)]
pub(crate) async fn get_connector_package(
    State(state): State<AppState>,
    Path(digest): Path<String>,
) -> V2Result<Json<ConnectorPackageResponse>> {
    let package = state.deployments.get_package(&digest).ok_or_else(|| {
        V2Error::new(
            DeploymentErrorKind::PackageNotFound,
            format!("no connector package is stored as '{digest}'"),
        )
    })?;
    let admission = state.deployments.get_admission(&digest);
    let states = state
        .deployments
        .artifact_states(&state.db_pool, std::slice::from_ref(&digest))
        .await
        .unwrap_or_default();
    let summary = package_row_to_summary(
        &package,
        admission.as_ref().is_some_and(|a| !a.revoked),
        admission.as_ref().is_some_and(|a| a.revoked),
        artifact_state_name(states.get(digest.as_str())),
    );
    Ok(Json(ConnectorPackageResponse {
        package: summary,
        admission: admission.as_deref().map(admission_dto),
    }))
}

/// `POST /api/v2/admin/connector-packages`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/connector-packages",
    tag = "Providers",
    summary = "Upload a verified connector provider package",
    description = "Verifies archive bounds, digests, and publisher/version immutability, then stores the package in the deployment store. A `workflow` package is also held to the hermetic rules — one entry workflow per operation, only the dataflow built-ins, `crypto`, `jwt_sign`, `jwt_verify`, plugin functions and `connector_call` on its declared dependencies, and no secret node or reference — and refused with `details` naming each problem by file and path.",
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
    OrionJson(req): OrionJson<UploadPackageRequest>,
) -> V2Result<(StatusCode, Json<UploadPackageResponse>)> {
    let mut files = decode_files_map(&req.files)?;
    // A workflow package is held to the hermetic rules before anything is
    // stored.
    let problems = crate::runtime::workflow_providers::upload_problems(&state, &files);
    if !problems.is_empty() {
        return Err(refusal("package refused", problems));
    }
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

    let row = state
        .deployments
        .ingest_verified_archive_in_db(&state.db_pool, &entries)
        .await?;

    let mut admission = None;
    if req.admit {
        let problems =
            crate::runtime::workflow_providers::admission_problems(&state, &row.package_digest)
                .await;
        if !problems.is_empty() {
            return Err(refusal("admission refused", problems));
        }
        let adm = state
            .deployments
            .admit_package_in_db(
                &state.db_pool,
                &row.package_digest,
                state.composition.admission(),
                req.lock.as_ref(),
            )
            .await?;
        admission = Some(admission_dto(&adm));
        crate::runtime::workflow_providers::sync(&state).await;
    }

    crate::server::routes::admin::audit_log(
        &state.audit_queue,
        &principal,
        "v2_package_upload",
        "connector_package",
        &row.package_digest,
    );

    let summary = package_row_to_summary(
        &row,
        admission.is_some(),
        false,
        PackageArtifactState::Active,
    );
    Ok((
        StatusCode::CREATED,
        Json(UploadPackageResponse {
            package: summary,
            admission,
        }),
    ))
}

/// `PUT /api/v2/admin/connector-packages/{digest}/admission`.
#[utoipa::path(
    put,
    path = "/api/v2/admin/connector-packages/{digest}/admission",
    tag = "Providers",
    summary = "Admit an uploaded connector provider package",
    description = "Evaluates the active edition admission policy against the uploaded package and records its admission, with a durable fingerprint. Putting the admission again re-evaluates it. A `workflow` package is checked against the hermetic rules again first, and once admitted is registered as a provider backend under its digest.",
    params(
        ("digest" = String, Path, description = "Package SHA-256 digest (`sha256:<64-hex>`)")
    ),
    request_body(content = Option<AdmitPackageRequest>, description = "Optional lockfile verification payload"),
    responses(
        (status = 200, description = "Package admitted", body = AdmitPackageResponse),
        (status = 400, description = "A workflow package that is not hermetic (`VALIDATION_ERROR`, by file), or a lock that does not verify"),
        (status = 403, description = "Rejected by the admission policy"),
        (status = 404, description = "No package is stored under the digest (`PACKAGE_NOT_FOUND`)"),
    )
)]
pub(crate) async fn admit_connector_package(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(digest): Path<String>,
    req: Option<OrionJson<AdmitPackageRequest>>,
) -> V2Result<Json<AdmitPackageResponse>> {
    let lock = req.and_then(|OrionJson(r)| r.lock);
    let problems = crate::runtime::workflow_providers::admission_problems(&state, &digest).await;
    if !problems.is_empty() {
        return Err(refusal("admission refused", problems));
    }
    let adm = state
        .deployments
        .admit_package_in_db(
            &state.db_pool,
            &digest,
            state.composition.admission(),
            lock.as_ref(),
        )
        .await?;
    crate::runtime::workflow_providers::sync(&state).await;
    crate::server::routes::admin::audit_log(
        &state.audit_queue,
        &principal,
        "v2_package_admit",
        "connector_package",
        &digest,
    );
    Ok(Json(AdmitPackageResponse {
        admission: admission_dto(&adm),
    }))
}

/// `POST /api/v2/admin/connector-packages/{digest}/revocations`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/connector-packages/{digest}/revocations",
    tag = "Providers",
    summary = "Revoke an admitted connector provider package",
    description = "Records a revocation of the package's admission in the deployment store and the runtime admission ledger, blocking new invocations and deployments. A `workflow` package's backend is unregistered.",
    params(
        ("digest" = String, Path, description = "Package SHA-256 digest (`sha256:<64-hex>`)")
    ),
    request_body(content = Option<RevokePackageRequest>, description = "Optional revocation reason"),
    responses(
        (status = 201, description = "Package revoked", body = RevokePackageResponse),
        (status = 404, description = "The package was never admitted (`ADMISSION_NOT_FOUND`)"),
    )
)]
pub(crate) async fn revoke_connector_package(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(digest): Path<String>,
    req: Option<OrionJson<RevokePackageRequest>>,
) -> V2Result<(StatusCode, Json<RevokePackageResponse>)> {
    let reason = req.and_then(|OrionJson(r)| r.reason);
    state
        .deployments
        .revoke_package_in_db(&state.db_pool, &digest)
        .await?;
    // The store revoked a package stored under `digest`, so it is one.
    if let Ok(revoked) = Digest::parse(&digest) {
        state
            .composition
            .admission()
            .ledger()
            .revoke_digest(&revoked);
    }
    crate::runtime::workflow_providers::sync(&state).await;
    crate::server::routes::admin::audit_log(
        &state.audit_queue,
        &principal,
        "v2_package_revoke",
        "connector_package",
        &digest,
    );
    Ok((
        StatusCode::CREATED,
        Json(RevokePackageResponse {
            package_digest: digest,
            reason,
        }),
    ))
}
