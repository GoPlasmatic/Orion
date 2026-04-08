use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde_json::{Value, json};

use crate::connector::mask_connector;
use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
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

/// Reload the connector registry after a mutation.
async fn reload_connectors(state: &AppState) -> Result<(), OrionError> {
    state
        .connector_registry
        .reload(state.connector_repo.as_ref())
        .await?;
    Ok(())
}

/// Evict cached connection pools for a connector whose config may have changed.
#[allow(unused_variables)]
async fn evict_connector_pools(state: &AppState, connector_name: &str) {
    #[cfg(feature = "connectors-sql")]
    state.sql_pool_cache.evict(connector_name).await;
    #[cfg(feature = "connectors-redis")]
    state.cache_pool.evict_pool(connector_name).await;
    #[cfg(feature = "connectors-mongodb")]
    state.mongo_pool_cache.evict(connector_name).await;
    tracing::debug!(connector = connector_name, "Evicted cached connection pools");
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
    Json(req): Json<CreateConnectorRequest>,
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
    Json(req): Json<UpdateConnectorRequest>,
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
        Ok(Json(json!({ "reset": true, "key": key })))
    } else {
        Err(OrionError::NotFound(format!(
            "Circuit breaker '{}' not found",
            key
        )))
    }
}
