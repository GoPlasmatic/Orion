//! V2 Provider package authoring scaffold, pack & lint routes (`/api/v2/admin/authoring/*`).

use std::collections::BTreeMap;

use axum::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};

use super::decode_files_map;
use super::packages::UploadPackageRequest;
use crate::extensions::ImplementationKind;
use crate::extensions::authoring::{
    lint_provider_package, pack_provider_package, scaffold_provider_package,
};

#[derive(Debug, Deserialize)]
pub struct ScaffoldRequest {
    pub publisher: String,
    pub provider_id: String,
    pub version: String,
    pub kind: ImplementationKind,
}

/// `POST /api/v2/admin/authoring/scaffold` (`V2-07.2`).
pub(super) async fn authoring_scaffold(Json(req): Json<ScaffoldRequest>) -> impl IntoResponse {
    let pkg = scaffold_provider_package(&req.publisher, &req.provider_id, &req.version, req.kind);
    let package_digest = pack_provider_package(&pkg.files)
        .map(|inv| inv.package_digest)
        .unwrap_or_default();
    let files_utf8: BTreeMap<String, String> = pkg
        .files
        .into_iter()
        .map(|(k, v)| {
            let s = String::from_utf8(v.clone()).unwrap_or_else(|_| {
                format!(
                    "base64:{}",
                    base64::engine::general_purpose::STANDARD.encode(&v)
                )
            });
            (k, s)
        })
        .collect();
    (
        StatusCode::OK,
        Json(json!({
            "publisher": pkg.publisher,
            "provider_id": pkg.provider_id,
            "version": pkg.version,
            "implementation": pkg.implementation.as_str(),
            "package_digest": package_digest,
            "files": files_utf8,
        })),
    )
}

/// `POST /api/v2/admin/authoring/pack` (`V2-07.2`).
pub(super) async fn authoring_pack(Json(req): Json<UploadPackageRequest>) -> impl IntoResponse {
    let files = match decode_files_map(&req.files) {
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
    match pack_provider_package(&files) {
        Ok(packed) => (
            StatusCode::OK,
            Json(json!({
                "data": {
                    "package_digest": packed.package_digest,
                    "inventory": serde_json::from_slice::<Value>(&packed.inventory_bytes).unwrap_or(Value::Null),
                }
            })),
        ),
        Err(err) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": {
                    "code": "PACK_VALIDATION_FAILED",
                    "message": err,
                }
            })),
        ),
    }
}

/// `POST /api/v2/admin/authoring/lint` (`V2-07.2`).
pub(super) async fn authoring_lint(Json(req): Json<UploadPackageRequest>) -> impl IntoResponse {
    let files = match decode_files_map(&req.files) {
        Ok(f) => f,
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "valid": false, "errors": [msg] })),
            );
        }
    };
    let report = lint_provider_package(&files);
    let status = if report.valid {
        StatusCode::OK
    } else {
        StatusCode::UNPROCESSABLE_ENTITY
    };
    (
        status,
        Json(serde_json::to_value(report).unwrap_or(Value::Null)),
    )
}
