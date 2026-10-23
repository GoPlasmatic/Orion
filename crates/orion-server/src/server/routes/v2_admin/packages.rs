//! V2 Connector package ingestion, admission & revocation routes (`/api/v2/admin/connector-packages*`).

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{decode_files_map, deployment_err_response, sync_v2_deployments_to_db};
use crate::extensions::authoring::pack_provider_package;
use crate::extensions::deployment_store::ArchiveEntryInput;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::state::AppState;

#[derive(Debug, Deserialize)]
pub struct UploadPackageRequest {
    /// Map of relative path -> UTF-8 or `base64:<...>` file content.
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct AdmitPackageRequest {
    pub lock: Option<Value>,
}

/// `GET /api/v2/admin/connector-packages` (`V2-07.1`).
pub(super) async fn list_connector_packages(State(state): State<AppState>) -> impl IntoResponse {
    let packages = state.composition.deployments.list_packages();
    let admissions = state.composition.deployments.list_admissions();
    Json(json!({
        "packages": packages,
        "admissions": admissions,
    }))
}

/// `POST /api/v2/admin/connector-packages` (`V2-07.1`).
pub(super) async fn upload_connector_package(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Json(req): Json<UploadPackageRequest>,
) -> impl IntoResponse {
    let mut files = match decode_files_map(&req.files) {
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

    match state
        .composition
        .deployments
        .ingest_verified_archive(&entries)
    {
        Ok(row) => {
            sync_v2_deployments_to_db(&state).await;
            crate::server::routes::admin::audit_log(
                &state.audit_queue,
                &principal,
                "v2_package_upload",
                "connector_package",
                &row.package_digest,
            );
            (StatusCode::CREATED, Json(json!({ "package": row })))
        }
        Err(e) => deployment_err_response(e),
    }
}

/// `POST /api/v2/admin/connector-packages/{digest}/admit` (`V2-07.1`).
pub(super) async fn admit_connector_package(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(digest): Path<String>,
    req: Option<Json<AdmitPackageRequest>>,
) -> impl IntoResponse {
    let lock = req.and_then(|Json(r)| r.lock);
    match state.composition.deployments.admit_package(
        &digest,
        state.composition.admission.as_ref(),
        lock.as_ref(),
    ) {
        Ok(adm) => {
            sync_v2_deployments_to_db(&state).await;
            crate::server::routes::admin::audit_log(
                &state.audit_queue,
                &principal,
                "v2_package_admit",
                "connector_package",
                &digest,
            );
            (StatusCode::OK, Json(json!({ "admission": adm })))
        }
        Err(e) => deployment_err_response(e),
    }
}

/// `POST /api/v2/admin/connector-packages/{digest}/revoke` (`V2-07.1`).
pub(super) async fn revoke_connector_package(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(digest): Path<String>,
) -> impl IntoResponse {
    match state.composition.deployments.revoke_package(&digest) {
        Ok(()) => {
            sync_v2_deployments_to_db(&state).await;
            crate::server::routes::admin::audit_log(
                &state.audit_queue,
                &principal,
                "v2_package_revoke",
                "connector_package",
                &digest,
            );
            (
                StatusCode::OK,
                Json(json!({ "package_digest": digest, "revoked": true })),
            )
        }
        Err(e) => deployment_err_response(e),
    }
}
