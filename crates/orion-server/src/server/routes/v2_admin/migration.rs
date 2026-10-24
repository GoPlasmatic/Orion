//! V2 Legacy v1 -> v2 migration report & edition transition preflight routes (`/api/v2/admin/migration-report`, `/api/v2/admin/edition-transition-preflight`).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use orion_api::v2::{
    EditionTransitionPreflightRequest, EditionTransitionPreflightResponse, MigrationReportRequest,
    MigrationReportResponse,
};
use serde_json::{Value, json};

use crate::extensions::deployment_store::ImmutableDeploymentStore;
use crate::extensions::{EditionTier, check_connector_availability, map_legacy_connector};
use crate::server::state::AppState;

/// `POST /api/v2/admin/migration-report`.
#[utoipa::path(
    post,
    path = "/api/v2/admin/migration-report",
    tag = "Deployments",
    summary = "Generate a legacy v1 -> v2 conversion and migration report",
    description = "Inspects legacy v1 connectors, workflows, and channels, maps them to v2 provider packages and connection revisions, and synthesizes a candidate bootstrap deployment manifest if compatible.",
    request_body = MigrationReportRequest,
    responses(
        (status = 200, description = "Legacy v1 -> v2 migration conversion report", body = MigrationReportResponse),
    )
)]
pub(crate) async fn create_migration_report(
    State(state): State<AppState>,
    Json(req): Json<MigrationReportRequest>,
) -> impl IntoResponse {
    let mut findings = Vec::new();
    let mut resolved_connectors = Vec::new();

    for c in &req.connectors {
        match map_legacy_connector(&c.name, &c.connector_type, &c.config).and_then(|res| {
            check_connector_availability(&state.composition, &c.name, &res).map(|()| res)
        }) {
            Ok(res) => resolved_connectors.push(json!({
                "name": c.name,
                "provider_id": res.provider_id,
                "implementation": res.implementation.as_str(),
                "tier": res.tier.as_str(),
            })),
            Err(finding) => {
                if let Ok(val) = serde_json::to_value(finding) {
                    findings.push(val);
                }
            }
        }
    }

    let enriched_conns: Vec<(String, Value)> = req
        .connectors
        .iter()
        .map(|c| {
            let mut v = c.config.clone();
            if let Some(obj) = v.as_object_mut() {
                obj.entry("connector_type".to_string())
                    .or_insert_with(|| Value::String(c.connector_type.clone()));
            }
            (c.name.clone(), v)
        })
        .collect();
    let conn_tuples: Vec<(&str, &Value)> = enriched_conns
        .iter()
        .map(|(n, cfg)| (n.as_str(), cfg))
        .collect();
    let wf_tuples: Vec<(&str, u64, &Value)> = req
        .workflows
        .iter()
        .map(|w| (w.id.as_str(), w.version, &w.definition))
        .collect();
    let ch_tuples: Vec<(&str, u64, &str, u64)> = req
        .channels
        .iter()
        .map(|ch| {
            (
                ch.id.as_str(),
                ch.version,
                ch.workflow_id.as_str(),
                ch.workflow_version,
            )
        })
        .collect();

    let bootstrap_manifest = if findings.is_empty() {
        ImmutableDeploymentStore::build_legacy_bootstrap_manifest(
            EditionTier::Community,
            &conn_tuples,
            &wf_tuples,
            &ch_tuples,
        )
        .ok()
    } else {
        None
    };

    let bootstrap_manifest_digest = bootstrap_manifest
        .as_ref()
        .and_then(|m| m.canonicalize_and_verify().ok().map(|(_, d)| d));
    let bootstrap_manifest_val = bootstrap_manifest.and_then(|m| serde_json::to_value(m).ok());
    let compatible = findings.is_empty() && bootstrap_manifest_val.is_some();

    let resp = MigrationReportResponse {
        edition: "community".to_string(),
        compatible,
        can_cutover: compatible,
        converted_connections: resolved_connectors.len(),
        resolved_connectors,
        findings: findings.clone(),
        blocking_findings: findings,
        bootstrap_manifest: bootstrap_manifest_val,
        bootstrap_manifest_digest,
    };
    (StatusCode::OK, Json(resp))
}

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
    req: Option<Json<EditionTransitionPreflightRequest>>,
) -> impl IntoResponse {
    let requested_rev = req.and_then(|Json(r)| r.revision);
    let (head_rev, _) = state.composition.deployments.current_head();
    let target_rev = requested_rev.unwrap_or(head_rev);

    let revisions = state.composition.deployments.list_revisions();
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
    let dto = EditionTransitionPreflightResponse {
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
    };
    let mut val = serde_json::to_value(&dto).unwrap_or(Value::Null);
    if let Some(obj) = val.as_object_mut() {
        obj.insert(
            "data".to_string(),
            serde_json::to_value(&dto).unwrap_or(Value::Null),
        );
    }
    (StatusCode::OK, Json(val))
}
