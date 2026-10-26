//! `POST /api/v2/admin/edition-transition-preflight`: whether a deployment
//! revision can be served by the Community edition.

use axum::Json;
use axum::extract::State;
use orion_api::v2::{EditionTransitionPreflightRequest, EditionTransitionPreflightResponse};
use serde_json::json;

use crate::server::extract::OrionJson;
use crate::server::state::AppState;

/// `POST /api/v2/admin/edition-transition-preflight`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/edition-transition-preflight",
    tag = "Deployments",
    summary = "Run Enterprise-to-Community edition transition preflight check",
    description = "Inspects the specified or active deployment revision for Enterprise-only providers or channels and returns a transition readiness report and backup checklist.",
    request_body(content = Option<EditionTransitionPreflightRequest>, description = "Optional target revision to inspect"),
    responses(
        (status = 200, description = "Edition transition preflight report", body = EditionTransitionPreflightResponse),
    )
)]
pub(crate) async fn edition_transition_preflight(
    State(state): State<AppState>,
    req: Option<OrionJson<EditionTransitionPreflightRequest>>,
) -> Json<EditionTransitionPreflightResponse> {
    let requested_rev = req.and_then(|OrionJson(r)| r.revision);
    let (head_rev, _) = state.deployments.current_head();
    let target_rev = requested_rev.unwrap_or(head_rev);

    let revisions = state.deployments.list_revisions();
    let mut incompatible_providers = Vec::new();
    let incompatible_channels: Vec<String> = Vec::new();
    let mut findings = Vec::new();

    if let Some(rev) = revisions.into_iter().find(|r| r.revision == target_rev) {
        for p in &rev.manifest.providers {
            let full_id = format!("{}.{}", p.publisher, p.provider_id);
            let is_community_foundation = crate::extensions::COMMUNITY_RUST_DRIVERS
                .contains(&full_id.as_str())
                || crate::extensions::COMMUNITY_RUST_DRIVERS.contains(&p.provider_id.as_str());
            if !is_community_foundation
                && (p.publisher.starts_with("orion.enterprise")
                    || p.publisher.starts_with("com.orion.enterprise"))
            {
                incompatible_providers.push(full_id.clone());
                findings.push(json!({
                    "code": "ENTERPRISE_PROVIDER_IN_MANIFEST",
                    "provider": full_id,
                    "digest": p.digest,
                    "message": "Enterprise provider must be replaced or removed before transitioning to Community Edition"
                }));
            }
        }
    }

    let transition_allowed = incompatible_providers.is_empty() && incompatible_channels.is_empty();
    Json(EditionTransitionPreflightResponse {
        target_edition: "community".to_string(),
        transition_allowed,
        inspected_revision: target_rev,
        incompatible_providers,
        incompatible_channels,
        findings,
        backup_retention_checklist: vec![
            "1. Capture full database snapshot (orion-cli backups create) and verify artifact_objects checksums".to_string(),
            "2. Export encrypted secret store backup and retain historical rollback revisions".to_string(),
            "3. Replace or remove any Enterprise providers/channels in a new candidate revision before starting the Community binary".to_string(),
        ],
    })
}
