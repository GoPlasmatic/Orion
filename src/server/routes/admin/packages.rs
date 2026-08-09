//! Package receipts (K14): the single package-aware point of the admin API.
//!
//! Packaging itself lives in the `orion-server package` CLI, which stages and
//! activates artifacts through the per-kind endpoints. What the server keeps
//! is one receipt per applied package version, because the promotion rule —
//! *an applied package version is immutable; only a staged one may change;
//! any content change rides a version bump* — cannot be enforced without the
//! target remembering what was applied. The enforcement itself is in
//! [`crate::storage::repositories::packages`]; these handlers add validation,
//! audit, and the wire shapes.

use axum::extract::{Path, State};
use axum::{Extension, Json};
use serde_json::Value;

use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::extract::{OrionJson, OrionQuery};
use crate::server::routes::openapi::{DataEnvelope, PaginatedEnvelope};
use crate::server::routes::response_helpers::{data_response, paginated_response};
use crate::server::state::AppState;
use crate::storage::models::{PackageReceiptResponse, PackageState};
use crate::storage::repositories::helpers::{VersionFilter, clamp_pagination};
use crate::storage::repositories::packages::PutPackageReceiptRequest;

use super::audit_log;

/// `GET /api/v1/admin/packages/{name}` — one package's receipts.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct PackageDetail {
    name: String,
    /// The newest `applied` receipt — what this deployment currently runs.
    /// `null` while every receipt is still `staged`.
    current: Option<PackageReceiptResponse>,
    /// Every receipt for this package, newest first.
    versions: Vec<PackageReceiptResponse>,
}

/// Shared caps for the receipt key fields. The MySQL column widths
/// (`migrations/mysql/013_package_receipts.sql`) are sized to these, so the
/// route layer must refuse anything longer before it reaches the driver.
const MAX_NAME_LEN: usize = 128;
const MAX_VERSION_LEN: usize = 64;
const MAX_HASH_LEN: usize = 128;

/// A receipt key: non-empty, bounded, and drawn from a charset that stays
/// unambiguous in URLs, shell commands and audit rows.
fn validate_key_field(
    field: &str,
    value: &str,
    max_len: usize,
    extra: &[char],
) -> Result<(), OrionError> {
    if value.trim().is_empty() {
        return Err(OrionError::validation(format!("{field} must not be empty")));
    }
    if value.len() > max_len {
        return Err(OrionError::validation(format!(
            "{field} must be at most {max_len} characters, got {}",
            value.len()
        )));
    }
    if let Some(bad) = value
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') || extra.contains(c)))
    {
        return Err(OrionError::validation(format!(
            "{field} contains unsupported character '{bad}' — use letters, digits, \
             '.', '_' and '-'"
        )));
    }
    Ok(())
}

fn validate_put(name: &str, req: &PutPackageReceiptRequest) -> Result<(), OrionError> {
    validate_key_field("package name", name, MAX_NAME_LEN, &[])?;
    validate_key_field("version", &req.version, MAX_VERSION_LEN, &[])?;
    // `sha256:…` is the expected spelling, so ':' is allowed here and only here.
    validate_key_field("content_hash", &req.content_hash, MAX_HASH_LEN, &[':'])?;
    Ok(())
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/packages",
    tag = "Packages",
    params(VersionFilter),
    responses(
        (status = 200, description = "Paginated receipt rows, ordered by package name, \
            newest first within a package.", body = PaginatedEnvelope<PackageReceiptResponse>),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_packages(
    State(state): State<AppState>,
    OrionQuery(filter): OrionQuery<VersionFilter>,
) -> Result<Json<Value>, OrionError> {
    let (limit, offset) = clamp_pagination(filter.limit, filter.offset);
    let result = state.repos.packages.list(limit, offset).await?;
    let rows: Vec<PackageReceiptResponse> = result
        .data
        .iter()
        .map(PackageReceiptResponse::from)
        .collect();
    Ok(paginated_response(
        rows,
        result.total,
        result.limit,
        result.offset,
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/packages/{name}",
    tag = "Packages",
    params(("name" = String, Path, description = "Package name")),
    responses(
        (status = 200, description = "The package's receipts, with `current` naming the \
            newest applied version.", body = DataEnvelope<PackageDetail>),
        (status = 404, description = "No receipts recorded for this package"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn get_package(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let rows = state.repos.packages.get_by_name(&name).await?;
    // Rows arrive newest-first, so the first applied receipt is the current
    // one — re-applying an older version touches its updated_at, which is
    // exactly what moves `current` back to it (the rollback path).
    let current = rows
        .iter()
        .find(|r| r.state == PackageState::Applied.as_str())
        .map(PackageReceiptResponse::from);
    Ok(data_response(PackageDetail {
        name,
        current,
        versions: rows.iter().map(PackageReceiptResponse::from).collect(),
    }))
}

#[utoipa::path(
    put,
    path = "/api/v1/admin/packages/{name}",
    tag = "Packages",
    params(("name" = String, Path, description = "Package name")),
    request_body = PutPackageReceiptRequest,
    responses(
        (status = 200, description = "The receipt as stored", body = DataEnvelope<PackageReceiptResponse>),
        (status = 400, description = "Invalid name, version, content hash, or state"),
        (status = 409, description = "The version is already applied with different \
            content (an applied package version is immutable — bump the package \
            version), already applied and asked to go back to staged, or was written \
            by a concurrent request."),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn put_package(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(name): Path<String>,
    OrionJson(req): OrionJson<PutPackageReceiptRequest>,
) -> Result<Json<Value>, OrionError> {
    validate_put(&name, &req)?;
    let who = principal
        .as_ref()
        .map(|e| e.0.key_id.as_str())
        .unwrap_or(super::ANONYMOUS_PRINCIPAL);
    let receipt = state.repos.packages.put(&name, &req, who).await?;
    // Receipts never touch the engine — no reload, like every draft path.
    audit_log(
        &state.audit_queue,
        &principal,
        &format!("package_{}", req.state),
        "package",
        &format!("{name}@{}", req.version),
    );
    Ok(data_response(PackageReceiptResponse::from(&receipt)))
}
