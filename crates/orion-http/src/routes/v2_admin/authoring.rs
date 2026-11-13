//! V2 Provider package authoring scaffold, pack & lint routes (`/api/v2/admin/authoring/*`).

use std::collections::BTreeMap;

use axum::Json;
use base64::Engine as _;
use orion_api::v2::{
    LintResponse, PackResponse, PackageFilesRequest, ScaffoldRequest, ScaffoldResponse,
};

use super::{V2Error, V2Result, decode_files_map};
use crate::extensions::ImplementationKind;
use crate::extensions::authoring::{
    lint_provider_package, pack_provider_package, scaffold_provider_package,
};
use crate::extensions::deployment_store::DeploymentErrorKind;
use crate::extract::OrionJson;

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
        (status = 400, description = "A body that does not parse, such as an implementation kind that is not `rust`, `workflow` or `wasm` (`VALIDATION_ERROR`)"),
    )
)]
pub(crate) async fn authoring_scaffold(
    OrionJson(req): OrionJson<ScaffoldRequest>,
) -> V2Result<Json<ScaffoldResponse>> {
    let pkg = scaffold_provider_package(&req.publisher, &req.provider_id, &req.version, req.kind);
    let package_digest = pack_provider_package(&pkg.files)
        .map_err(|err| {
            V2Error::new(
                DeploymentErrorKind::PackValidationFailed,
                format!("the files do not pack: {}", err.code),
            )
        })?
        .package_digest
        .into_string();
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
    Ok(Json(ScaffoldResponse {
        publisher: pkg.publisher,
        provider_id: pkg.provider_id,
        version: pkg.version,
        implementation: pkg.implementation,
        package_digest,
        files: files_utf8,
    }))
}

/// `POST /api/v2/admin/authoring/pack`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/authoring/pack",
    tag = "Providers",
    summary = "Pack a provider package into a deterministic content-addressed bundle",
    description = "Validates the package files, computes canonical SHA-256 digests and `inventory.json`, and returns the packed bundle ready for upload.",
    request_body = PackageFilesRequest,
    responses(
        (status = 200, description = "Deterministically packed provider package", body = PackResponse),
        (status = 400, description = "A file is not valid `base64:` (`INVALID_PACKAGE_ENCODING`), or the files do not pack (`PACK_VALIDATION_FAILED`)"),
    )
)]
pub(crate) async fn authoring_pack(
    OrionJson(req): OrionJson<PackageFilesRequest>,
) -> V2Result<Json<PackResponse>> {
    let mut raw_files = req.files;
    let files = decode_files_map(&raw_files)?;
    let packed = pack_provider_package(&files).map_err(|err| {
        V2Error::new(
            DeploymentErrorKind::PackValidationFailed,
            format!("the files do not pack: {}", err.code),
        )
    })?;
    let inventory = String::from_utf8(packed.inventory_bytes).map_err(|e| {
        crate::errors::OrionError::internal_from("packing a provider package's inventory", e)
    })?;
    raw_files.insert("inventory.json".to_string(), inventory);
    Ok(Json(PackResponse {
        package_digest: packed.package_digest.to_string(),
        files: raw_files,
    }))
}

/// `POST /api/v2/admin/authoring/lint`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/authoring/lint",
    tag = "Providers",
    summary = "Lint a provider package manifest and schema bundle",
    description = "Validates `connector.json`, `distribution.json`, and implementation files, returning the lint report (`valid` and `errors`).",
    request_body = PackageFilesRequest,
    responses(
        (status = 200, description = "Lint report (`valid` indicates whether all checks passed)", body = LintResponse),
        (status = 400, description = "A file is not valid `base64:` (`INVALID_PACKAGE_ENCODING`), or the request body did not parse (`VALIDATION_ERROR`)"),
    )
)]
pub(crate) async fn authoring_lint(
    OrionJson(req): OrionJson<PackageFilesRequest>,
) -> V2Result<Json<LintResponse>> {
    let files = decode_files_map(&req.files)?;
    let report = lint_provider_package(&files);
    // The report names what `connector.json` declares, empty when it could
    // not be read.
    let declared = |s: String| (!s.is_empty()).then_some(s);
    Ok(Json(LintResponse {
        valid: report.valid,
        implementation: ImplementationKind::parse(&report.implementation),
        provider_id: declared(report.provider_id),
        version: declared(report.version),
        errors: report.errors,
    }))
}
