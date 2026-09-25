use axum::extract::{Path, State};
use axum::{Extension, Json};
use serde_json::Value;

use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::routes::openapi::DataEnvelope;
use crate::server::routes::response_helpers::data_response;
use crate::server::state::AppState;

use super::audit_log;

// ============================================================
// Response-cache invalidation
// ============================================================

/// The operator's `cache_invalidate`: bump one namespace in every
/// response-cache store, so each channel declaring it misses on its next
/// request. For the change a workflow does not make — a board flipped by hand,
/// a row fixed in the database.
#[utoipa::path(
    post,
    path = "/api/v1/admin/cache/namespaces/{namespace}/invalidate",
    tag = "Cache",
    params(("namespace" = String, Path, description = "A namespace channels declare in \
        `cache.namespaces`")),
    responses(
        (status = 200, description = "Namespace invalidated. Entries stored under the old \
            version are no longer served, on every node sharing the store.",
            body = DataEnvelope<orion_api::CacheInvalidatedResponse>),
        (status = 400, description = "Not a valid namespace name"),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn invalidate_namespace(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(namespace): Path<String>,
) -> Result<Json<Value>, OrionError> {
    crate::channel::cache_namespace::check_name(&namespace).map_err(OrionError::validation)?;
    let targets = state
        .channel_loader
        .response_cache_targets(&state.connector_registry, &state.caches.cache_pool)
        .await;
    crate::channel::cache_namespace::invalidate(
        &targets,
        std::slice::from_ref(&namespace),
        "admin",
    )
    .await?;
    audit_log(
        &state.audit_queue,
        &principal,
        "invalidate",
        "cache_namespace",
        &namespace,
    );
    Ok(data_response(orion_api::CacheInvalidatedResponse {
        namespace,
        stores: targets.len() as u64,
    }))
}
