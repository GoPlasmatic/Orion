use axum::extract::{Path, State};
use axum::{Extension, Json};

use crate::admin_auth::AdminPrincipal;
use crate::errors::OrionError;
use crate::routes::response_helpers::data_response;
use crate::runtime::state::AppState;
use orion_api::Data;

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
    path = "/api/v2/admin/cache/namespaces/{namespace}/invalidate",
    tag = "Cache",
    params(("namespace" = String, Path, description = "A namespace channels declare in \
        `cache.namespaces`")),
    responses(
        (status = 200, description = "Namespace invalidated. Entries stored under the old \
            version are no longer served, on every node sharing the store.",
            body = Data<orion_api::CacheInvalidatedResponse>),
        (status = 400, description = "Not a valid namespace name"),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn invalidate_namespace(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(namespace): Path<String>,
) -> Result<Json<Data<orion_api::CacheInvalidatedResponse>>, OrionError> {
    crate::channel::cache_namespace::check_name(&namespace).map_err(OrionError::validation)?;
    let stores = state
        .channel_loader
        .invalidate_namespaces(
            &state.caches.cache_pool,
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
        stores: stores as u64,
    }))
}
