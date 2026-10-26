//! V2 Provider package authoring scaffold, pack & lint routes (`/api/v2/admin/authoring/*`).

use std::collections::BTreeMap;

use axum::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use base64::Engine as _;
use orion_api::v2::{
    LintResponse, PackResponse, ScaffoldRequest, ScaffoldResponse, UploadPackageRequest,
};
use serde_json::{Value, json};

use super::decode_files_map;
use crate::extensions::ImplementationKind;
use crate::extensions::authoring::{
    lint_provider_package, pack_provider_package, scaffold_provider_package,
};

fn parse_implementation_kind(kind_str: &str) -> Result<ImplementationKind, String> {
    let name = kind_str.trim().to_ascii_lowercase();
    ImplementationKind::parse(&name).ok_or_else(|| {
        format!("unsupported implementation kind '{name}'; expected 'rust', 'workflow', or 'wasm'")
    })
}

/// `POST /api/v2/admin/authoring/scaffold`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/authoring/scaffold",
    tag = "Providers",
    summary = "Generate a deterministic v2 provider package scaffold",
    description = "Generates starter manifest, distribution, and implementation files for a `rust`, `workflow`, or `wasm` provider package.",
    request_body = ScaffoldRequest,
    responses(
        (status = 200, description = "Generated provider package scaffold", body = ScaffoldResponse),
        (status = 400, description = "Unsupported implementation kind"),
    )
)]
pub(crate) async fn authoring_scaffold(Json(req): Json<ScaffoldRequest>) -> impl IntoResponse {
    let kind = match parse_implementation_kind(&req.kind) {
        Ok(k) => k,
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(crate::errors::error_envelope_json(
                    "INVALID_IMPLEMENTATION_KIND",
                    msg,
                )),
            );
        }
    };
    let pkg = scaffold_provider_package(&req.publisher, &req.provider_id, &req.version, kind);
    let package_digest = pack_provider_package(&pkg.files)
        .map(|inv| inv.package_digest.into_string())
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
    let resp = ScaffoldResponse {
        publisher: pkg.publisher,
        provider_id: pkg.provider_id,
        version: pkg.version,
        implementation: pkg.implementation.as_str().to_string(),
        package_digest,
        files: files_utf8,
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(resp).unwrap_or(Value::Null)),
    )
}

/// `POST /api/v2/admin/authoring/pack`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/authoring/pack",
    tag = "Providers",
    summary = "Pack a provider package into a deterministic content-addressed bundle",
    description = "Validates the package files, computes canonical SHA-256 digests and `inventory.json`, and returns the packed bundle ready for upload.",
    request_body = UploadPackageRequest,
    responses(
        (status = 200, description = "Deterministically packed provider package", body = PackResponse),
        (status = 400, description = "Invalid file encoding"),
        (status = 422, description = "Package validation failed"),
    )
)]
pub(crate) async fn authoring_pack(Json(req): Json<UploadPackageRequest>) -> impl IntoResponse {
    let mut raw_files = req.merged_files();
    let files = match decode_files_map(&raw_files) {
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
        Ok(packed) => {
            let inventory_json =
                serde_json::from_slice::<Value>(&packed.inventory_bytes).unwrap_or(Value::Null);
            if let Ok(inv_str) = String::from_utf8(packed.inventory_bytes) {
                raw_files.insert("inventory.json".to_string(), inv_str);
            }
            let resp = PackResponse {
                package_digest: packed.package_digest.to_string(),
                inventory: inventory_json.clone(),
                files: raw_files,
            };
            let mut val = serde_json::to_value(&resp).unwrap_or(Value::Null);
            if let Some(obj) = val.as_object_mut() {
                obj.insert(
                    "data".to_string(),
                    json!({
                        "package_digest": packed.package_digest,
                        "inventory": inventory_json,
                    }),
                );
            }
            (StatusCode::OK, Json(val))
        }
        Err(err) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(crate::errors::error_envelope_json(
                "PACK_VALIDATION_FAILED",
                err.code,
            )),
        ),
    }
}

/// `POST /api/v2/admin/authoring/lint`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/authoring/lint",
    tag = "Providers",
    summary = "Lint a provider package manifest and schema bundle",
    description = "Validates `manifest.json`, `distribution.json`, and implementation files, returning lint errors, warnings, and the canonical package digest.",
    request_body = UploadPackageRequest,
    responses(
        (status = 200, description = "Package is valid", body = LintResponse),
        (status = 400, description = "Invalid file encoding", body = LintResponse),
        (status = 422, description = "Package has lint errors", body = LintResponse),
    )
)]
pub(crate) async fn authoring_lint(Json(req): Json<UploadPackageRequest>) -> impl IntoResponse {
    let raw_files = req.merged_files();
    let files = match decode_files_map(&raw_files) {
        Ok(f) => f,
        Err(msg) => {
            let resp = LintResponse {
                valid: false,
                errors: vec![msg],
                ..LintResponse::default()
            };
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::to_value(resp).unwrap_or(Value::Null)),
            );
        }
    };
    let report = lint_provider_package(&files);
    let status = if report.valid {
        StatusCode::OK
    } else {
        StatusCode::UNPROCESSABLE_ENTITY
    };
    let dto: LintResponse = serde_json::to_value(&report)
        .and_then(serde_json::from_value)
        .unwrap_or_else(|_| LintResponse {
            valid: report.valid,
            ..LintResponse::default()
        });
    (
        status,
        Json(serde_json::to_value(dto).unwrap_or(Value::Null)),
    )
}
