use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde_json::{Value, json};

use crate::connector::mask_connector;
use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::extract::{OrionJson, OrionQuery};
use crate::server::routes::openapi::{
    CircuitBreakerReset, CircuitBreakerStates, ConnectorListItem, DataEnvelope, ImportResult,
    PaginatedEnvelope,
};
use crate::server::routes::response_helpers::{
    created_response, data_response, paginated_response,
};
use crate::server::state::AppState;
use crate::storage::models::ConnectorResponse;
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
    state.cluster.bump_config_epoch().await
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

/// Names of active workflows whose tasks reference `connector_name`.
///
/// F18: workflows address connectors by *name*, and nothing tied the two
/// together — renaming a connector left every referencing workflow pointing at
/// a name that no longer resolves, which is a runtime 500 per request with no
/// error at rename time and no load issue (that list covers connectors that
/// failed to load, not dangling references to them).
async fn active_workflows_using(
    state: &AppState,
    connector_name: &str,
) -> Result<Vec<String>, OrionError> {
    let mut users = Vec::new();
    for workflow in state.workflow_repo.list_active().await? {
        let Ok(tasks) = serde_json::from_str::<serde_json::Value>(&workflow.tasks_json) else {
            continue;
        };
        if super::workflows::connector_refs(&tasks)
            .iter()
            .any(|t| t.connector == connector_name)
        {
            users.push(workflow.workflow_id);
        }
    }
    users.sort();
    users.dedup();
    Ok(users)
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/connectors",
    tag = "Connectors",
    params(ConnectorFilter),
    responses(
        (status = 200, description = "Paginated list of connectors. Each row carries \
            `load_status`: `loaded` when the connector is live in the registry, \
            `failed` (with `load_error`) when it is enabled but could not be loaded, \
            and `disabled` when it is not enabled. A `failed` connector is absent at \
            request time, so every workflow using it returns a 500.", body = PaginatedEnvelope<ConnectorListItem>),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_connectors(
    State(state): State<AppState>,
    OrionQuery(filter): OrionQuery<ConnectorFilter>,
) -> Result<Json<Value>, OrionError> {
    let result = state.connector_repo.list_paginated(&filter).await?;
    // F16: a connector that failed to load is simply missing from the
    // registry, so a list that reports only the stored rows shows a healthy
    // fleet while requests using it 500. Join the two views here.
    let issues = state.connector_registry.load_issues().await;
    let rows: Vec<Value> = result
        .data
        .iter()
        .map(|connector| {
            let mut row = serde_json::to_value(mask_connector(connector))
                .unwrap_or_else(|_| json!({"id": connector.id}));
            let issue = issues.iter().find(|i| i.connector == connector.name);
            if let Some(obj) = row.as_object_mut() {
                let status = match (connector.enabled, issue) {
                    (false, _) => "disabled",
                    (true, Some(_)) => "failed",
                    (true, None) => "loaded",
                };
                obj.insert("load_status".to_string(), json!(status));
                if let Some(issue) = issue {
                    obj.insert("load_error".to_string(), json!(issue.reason));
                    obj.insert("load_error_stage".to_string(), json!(issue.stage));
                }
            }
            row
        })
        .collect();
    Ok(paginated_response(
        rows,
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
        (status = 201, description = "Connector created", body = DataEnvelope<ConnectorResponse>),
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
        &state.audit_queue,
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
        (status = 200, description = "Connector details", body = DataEnvelope<ConnectorResponse>),
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
        (status = 200, description = "Connector updated", body = DataEnvelope<ConnectorResponse>),
        (status = 404, description = "Connector not found"),
        (status = 400, description = "Rename refused: an active workflow references the old name"),
    )
)]
#[tracing::instrument(skip(state, req, principal))]
pub(crate) async fn update_connector(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
    OrionJson(mut req): OrionJson<UpdateConnectorRequest>,
) -> Result<Json<Value>, OrionError> {
    crate::validation::validate_update_connector(&req)?;
    // F18: read the stored row unconditionally. It used to be fetched only for
    // a config-bearing update, which is why the pre-update *name* was not in
    // hand when it mattered — see the rename guard and the eviction below.
    let stored = state.connector_repo.get_by_id(&id).await?;
    let renamed_from = req
        .name
        .as_deref()
        .filter(|new| *new != stored.name)
        .map(|_| stored.name.clone());

    // F18: workflows bind connectors by name. A rename that leaves an active
    // workflow pointing at the old name turns every one of its requests into a
    // 500, discovered in production rather than here. Archive or repoint them
    // first — the same shape as R5/F52 gating activation on the reference.
    if renamed_from.is_some() {
        let users = active_workflows_using(&state, &stored.name).await?;
        if !users.is_empty() {
            return Err(OrionError::validation(format!(
                "Cannot rename connector '{}': active workflow(s) {} reference it by \
                 name and would fail at their next request. Repoint or archive them \
                 first.",
                stored.name,
                users
                    .iter()
                    .map(|w| format!("'{w}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }

    if let Some(ref mut config) = req.config {
        // F34: `update` replaces config_json wholesale, so a GET → edit → PUT
        // round-trip would otherwise persist the "******" the reader was shown
        // as the real credential. Restore every field the caller sent back
        // unchanged in masked form, and reject any mask that has no stored
        // counterpart rather than writing it.
        if let Ok(stored_config) = serde_json::from_str::<Value>(&stored.config_json) {
            crate::connector::unmask_config(config, &stored_config);
        }
        crate::validation::reject_masked_values(config)?;

        // R4: validate against the type the connector will have — the one in
        // the request, or the stored one for a config-only update. Runs after
        // unmasking so validation sees the values that will be persisted.
        let effective_type = match req.connector_type {
            Some(ct) => ct,
            None => serde_json::from_value(Value::String(stored.connector_type.clone())).map_err(
                |_| {
                    OrionError::Internal(format!(
                        "Stored connector '{id}' has unknown type '{}'",
                        stored.connector_type
                    ))
                },
            )?,
        };
        crate::validation::validate_connector_config(effective_type, config)?;
    }
    let connector = state.connector_repo.update(&id, &req).await?;
    // F18: evict under both names. The cache is keyed by connector name, so
    // evicting only the post-update one left the old key holding live TCP
    // connections against the remote database's `max_connections` until the LRU
    // happened to reclaim it.
    evict_connector_pools(&state, &connector.name).await;
    if let Some(old_name) = renamed_from {
        evict_connector_pools(&state, &old_name).await;
    }
    audit_log(&state.audit_queue, &principal, "update", "connector", &id);
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
    audit_log(&state.audit_queue, &principal, "delete", "connector", &id);
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
    params(super::ImportQuery),
    responses(
        (status = 200, description = "Import results with counts (or would-be results when ?dry_run=true). \
            Each item is handled independently: a malformed or conflicting item becomes one entry in \
            `errors` and the rest of the batch still applies. Dry-run additionally probes for name \
            conflicts against stored rows and duplicates within the batch, without writing.", body = DataEnvelope<ImportResult>),
    )
)]
#[tracing::instrument(skip(state, items, principal), fields(count = items.len()))]
pub(crate) async fn import_connectors(
    State(state): State<AppState>,
    OrionQuery(query): OrionQuery<super::ImportQuery>,
    principal: Option<Extension<AdminPrincipal>>,
    OrionJson(items): OrionJson<Vec<Value>>,
) -> Result<Json<Value>, OrionError> {
    super::check_import_batch_size(items.len())?;
    let repo = state.connector_repo.clone();
    let probe = state.connector_repo.clone();
    let (imported, failed, errors) =
        super::import_items::<CreateConnectorRequest, _, _, _, _, _, _>(
            items,
            query.dry_run,
            super::ImportOps {
                validate: crate::validation::validate_create_connector,
                // `name` carries the unique constraint here, not `id`.
                conflict_key: |c: &CreateConnectorRequest| Some(c.name.clone()),
                exists: |name: String| {
                    let repo = probe.clone();
                    async move { repo.exists_by_name(&name).await }
                },
                create: |c: CreateConnectorRequest| {
                    let repo = repo.clone();
                    async move { repo.create(&c).await.map(|_| ()) }
                },
            },
        )
        .await;
    if query.dry_run {
        return Ok(super::dry_run_response(imported, failed, errors));
    }
    audit_log(
        &state.audit_queue,
        &principal,
        "import",
        "connector",
        &format!("{imported} imported"),
    );
    // Reload registry once at the end if anything succeeded.
    if imported > 0 {
        reload_connectors(&state).await?;
    }
    Ok(super::import_response(imported, failed, errors))
}

// ============================================================
// Circuit Breakers
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v1/admin/connectors/circuit-breakers",
    tag = "Connectors",
    responses(
        (status = 200, description = "Circuit breaker states", body = DataEnvelope<CircuitBreakerStates>),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_circuit_breakers(
    State(state): State<AppState>,
) -> Result<Json<Value>, OrionError> {
    let states = state.connector_registry.circuit_breaker_states().await;
    Ok(data_response(json!({
        "enabled": state.connector_registry.circuit_breaker_enabled(),
        // F21: breakers are node-local state. The *reset* path fans out over
        // the epoch bus, which made an unqualified read actively misleading —
        // it reads like cluster state because its sibling mutation is. Say
        // whose map this is.
        "scope": "node",
        "instance_id": state.cluster.instance_id,
        "breakers": states,
    })))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/connectors/circuit-breakers/{key}",
    tag = "Connectors",
    params(("key" = String, Path, description = "Circuit breaker key (channel:connector)")),
    responses(
        (status = 200, description = "Circuit breaker reset", body = DataEnvelope<CircuitBreakerReset>),
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

    // F21: in cluster mode a 404 for "not on this node" is wrong. Breakers are
    // node-local, so the key an operator wants to clear is very often open on a
    // *different* replica than the one the load balancer routed the reset to —
    // and the fan-out below is what actually clears it. 404 only when there is
    // no other node it could be on.
    if !found && !state.cluster.enabled {
        return Err(OrionError::NotFound(format!(
            "Circuit breaker '{key}' not found"
        )));
    }

    audit_log(
        &state.audit_queue,
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
    Ok(data_response(json!({
        "reset": true,
        "key": key,
        // Whether *this* node held the key. The fan-out clears it everywhere
        // regardless; `false` in cluster mode means "not here, broadcast anyway".
        "found_on_this_node": found,
    })))
}
