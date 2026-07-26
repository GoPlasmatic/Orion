use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde_json::{Value, json};

use crate::connector::mask_connector;
use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::extract::OrionJson;
use crate::server::routes::response_helpers::{
    created_response, data_response, paginated_response,
};
use crate::server::state::AppState;
use crate::storage::repositories::connectors::{
    ConnectorFilter, CreateConnectorRequest, UpdateConnectorRequest,
};

use super::audit_log;

// ============================================================
// Connectors CRUD
// ============================================================

/// Reload the connector registry after a mutation, then advance the config
/// epoch so other nodes resync too (connector edits previously propagated to
/// no other node at all).
async fn reload_connectors(state: &AppState) -> Result<(), OrionError> {
    state
        .connector_registry
        .reload(state.connector_repo.as_ref())
        .await?;
    super::bump_config_epoch(state).await
}

/// Evict cached connection pools for a connector whose config may have changed.
async fn evict_connector_pools(state: &AppState, connector_name: &str) {
    state.sql_pool_cache.evict(connector_name).await;
    state.cache_pool.evict_pool(connector_name).await;
    state.mongo_pool_cache.evict(connector_name).await;
    tracing::debug!(
        connector = connector_name,
        "Evicted cached connection pools"
    );
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/connectors",
    tag = "Connectors",
    params(ConnectorFilter),
    responses(
        (status = 200, description = "Paginated list of connectors"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_connectors(
    State(state): State<AppState>,
    Query(filter): Query<ConnectorFilter>,
) -> Result<Json<Value>, OrionError> {
    let result = state.connector_repo.list_paginated(&filter).await?;
    let masked: Vec<_> = result.data.iter().map(mask_connector).collect();
    Ok(paginated_response(
        masked,
        result.total,
        result.limit,
        result.offset,
    ))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/connectors",
    tag = "Connectors",
    request_body = CreateConnectorRequest,
    responses(
        (status = 201, description = "Connector created"),
        (status = 409, description = "Connector name conflict"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn create_connector(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    OrionJson(req): OrionJson<CreateConnectorRequest>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    crate::validation::validate_create_connector(&req)?;
    let connector = state.connector_repo.create(&req).await?;
    audit_log(
        &state.audit_log_repo,
        &principal,
        "create",
        "connector",
        &connector.id,
    );
    reload_connectors(&state).await?;
    let masked = mask_connector(&connector);
    Ok(created_response(masked))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/connectors/{id}",
    tag = "Connectors",
    params(("id" = String, Path, description = "Connector ID")),
    responses(
        (status = 200, description = "Connector details"),
        (status = 404, description = "Connector not found"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn get_connector(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let connector = state.connector_repo.get_by_id(&id).await?;
    let masked = mask_connector(&connector);
    Ok(data_response(masked))
}

#[utoipa::path(
    put,
    path = "/api/v1/admin/connectors/{id}",
    tag = "Connectors",
    params(("id" = String, Path, description = "Connector ID")),
    request_body = UpdateConnectorRequest,
    responses(
        (status = 200, description = "Connector updated"),
        (status = 404, description = "Connector not found"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn update_connector(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    OrionJson(req): OrionJson<UpdateConnectorRequest>,
) -> Result<Json<Value>, OrionError> {
    crate::validation::validate_update_connector(&req)?;
    let connector = state.connector_repo.update(&id, &req).await?;
    evict_connector_pools(&state, &connector.name).await;
    audit_log(
        &state.audit_log_repo,
        &principal,
        "update",
        "connector",
        &id,
    );
    reload_connectors(&state).await?;
    let masked = mask_connector(&connector);
    Ok(data_response(masked))
}

#[utoipa::path(
    delete,
    path = "/api/v1/admin/connectors/{id}",
    tag = "Connectors",
    params(("id" = String, Path, description = "Connector ID")),
    responses(
        (status = 204, description = "Connector deleted"),
        (status = 404, description = "Connector not found"),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn delete_connector(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
) -> Result<StatusCode, OrionError> {
    // Fetch connector name before deletion so we can evict cached pools.
    let connector = state.connector_repo.get_by_id(&id).await?;
    state.connector_repo.delete(&id).await?;
    evict_connector_pools(&state, &connector.name).await;
    audit_log(
        &state.audit_log_repo,
        &principal,
        "delete",
        "connector",
        &id,
    );
    reload_connectors(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ============================================================
// Connector Bulk Import (B6)
// ============================================================

#[utoipa::path(
    post,
    path = "/api/v1/admin/connectors/import",
    tag = "Connectors",
    request_body = Vec<CreateConnectorRequest>,
    params(super::workflows::ImportQuery),
    responses(
        (status = 200, description = "Import results with counts (or would-be results when ?dry_run=true). \
            Dry-run validates each item's shape and values only — it does NOT read the database, so it \
            cannot detect name conflicts. Connectors whose names already exist are reported as would_create \
            and will surface as Conflict on the real (non-dry-run) import."),
    )
)]
#[tracing::instrument(skip(state, items, principal), fields(count = items.len()))]
pub(crate) async fn import_connectors(
    State(state): State<AppState>,
    Query(query): Query<super::workflows::ImportQuery>,
    principal: Option<Extension<AdminPrincipal>>,
    OrionJson(items): OrionJson<Vec<Value>>,
) -> Result<Json<Value>, OrionError> {
    if query.dry_run {
        // Dry-run is pure validation: no DB reads, so name conflicts are not
        // detected here (they surface as Conflict on the real import).
        let mut would_create = 0u64;
        let mut would_fail = 0u64;
        let mut errors = Vec::new();
        for (i, item) in items.into_iter().enumerate() {
            // Deserialize per-item so a single shape/enum typo becomes one
            // would_fail entry instead of aborting the whole batch.
            let c = match serde_json::from_value::<CreateConnectorRequest>(item) {
                Ok(c) => c,
                Err(e) => {
                    would_fail += 1;
                    errors.push(json!({"index": i, "error": e.to_string()}));
                    continue;
                }
            };
            match crate::validation::validate_create_connector(&c) {
                Ok(()) => would_create += 1,
                Err(e) => {
                    would_fail += 1;
                    errors.push(json!({"index": i, "error": e.to_string()}));
                }
            }
        }
        return Ok(Json(json!({
            "dry_run": true,
            "would_create": would_create,
            "would_fail": would_fail,
            "imported": 0,
            "failed": would_fail,
            "errors": errors,
        })));
    }
    let mut imported = 0u64;
    let mut failed = 0u64;
    let mut errors = Vec::new();
    for (i, item) in items.into_iter().enumerate() {
        let c = match serde_json::from_value::<CreateConnectorRequest>(item) {
            Ok(c) => c,
            Err(e) => {
                failed += 1;
                errors.push(json!({"index": i, "error": e.to_string()}));
                continue;
            }
        };
        if let Err(e) = crate::validation::validate_create_connector(&c) {
            failed += 1;
            errors.push(json!({"index": i, "error": e.to_string()}));
            continue;
        }
        match state.connector_repo.create(&c).await {
            Ok(_) => imported += 1,
            Err(e) => {
                failed += 1;
                errors.push(json!({"index": i, "error": e.to_string()}));
            }
        }
    }
    audit_log(
        &state.audit_log_repo,
        &principal,
        "import",
        "connector",
        &format!("{imported} imported"),
    );
    // Reload registry once at the end if anything succeeded.
    if imported > 0 {
        reload_connectors(&state).await?;
    }
    Ok(Json(json!({
        "imported": imported,
        "failed": failed,
        "errors": errors,
    })))
}

// ============================================================
// Circuit Breakers
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v1/admin/connectors/circuit-breakers",
    tag = "Connectors",
    responses(
        (status = 200, description = "Circuit breaker states"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_circuit_breakers(
    State(state): State<AppState>,
) -> Result<Json<Value>, OrionError> {
    let states = state.connector_registry.circuit_breaker_states().await;
    Ok(Json(json!({
        "enabled": state.connector_registry.circuit_breaker_enabled(),
        "breakers": states,
    })))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/connectors/circuit-breakers/{key}",
    tag = "Connectors",
    params(("key" = String, Path, description = "Circuit breaker key (channel:connector)")),
    responses(
        (status = 200, description = "Circuit breaker reset"),
        (status = 404, description = "Circuit breaker not found"),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn reset_circuit_breaker(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(key): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let found = state.connector_registry.reset_circuit_breaker(&key).await;
    if found {
        audit_log(
            &state.audit_log_repo,
            &principal,
            "reset",
            "circuit_breaker",
            &key,
        );
        // Breakers are node-local (D3); fan the reset out over the epoch bus
        // so one API call resets the same key on every node.
        if state.cluster.enabled {
            let breaker_epoch = state.cluster.repo.request_breaker_reset(&key).await?;
            state
                .cluster
                .last_seen_breaker_epoch
                .fetch_max(breaker_epoch, std::sync::atomic::Ordering::AcqRel);
        }
        Ok(Json(json!({ "reset": true, "key": key })))
    } else {
        Err(OrionError::NotFound(format!(
            "Circuit breaker '{key}' not found"
        )))
    }
}
