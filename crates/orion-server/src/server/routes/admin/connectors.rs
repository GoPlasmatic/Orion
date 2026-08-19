use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde_json::{Value, json};

use crate::connector::mask_connector;
use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::extract::{OrionJson, OrionQuery};
use crate::server::routes::openapi::{
    CircuitBreakerReset, CircuitBreakerStates, ConnectorExportItem, ConnectorListItem,
    DataEnvelope, ImportResult, PaginatedEnvelope,
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
        .reload(state.repos.connectors.as_ref())
        .await?;
    state.cluster.bump_config_epoch().await
}

/// Evict cached connection pools for a connector whose config may have changed.
async fn evict_connector_pools(state: &AppState, connector_name: &str) {
    state.caches.sql_pool_cache.evict(connector_name).await;
    state.caches.cache_pool.evict_pool(connector_name).await;
    state.caches.mongo_pool_cache.evict(connector_name).await;
    state.caches.smtp_pool_cache.evict(connector_name).await;
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
    for workflow in state.repos.workflows.list_active().await? {
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
    let result = state.repos.connectors.list_paginated(&filter).await?;
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
    let connector = state.repos.connectors.create(&req).await?;
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
    let connector = state.repos.connectors.get_by_id(&id).await?;
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
    let stored = state.repos.connectors.get_by_id(&id).await?;
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
                    OrionError::internal(format!(
                        "Stored connector '{id}' has unknown type '{}'",
                        stored.connector_type
                    ))
                },
            )?,
        };
        crate::validation::validate_connector_config(effective_type, config)?;
    }
    let connector = state.repos.connectors.update(&id, &req).await?;
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
    let connector = state.repos.connectors.get_by_id(&id).await?;
    state.repos.connectors.delete(&id).await?;
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
            conflicts against stored rows and duplicates within the batch, without writing. \
            `?on_conflict=new_version` upserts instead of refusing an existing name (K2): connectors \
            are unversioned, so an existing connector is updated in place (`updated`), and identical \
            content is reported `unchanged` — re-importing the same artifact is a no-op. Per-item \
            outcomes are in `results`.", body = DataEnvelope<ImportResult>),
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
    let repo = state.repos.connectors.clone();
    let probe = state.repos.connectors.clone();
    let upsert_state = state.clone();
    let outcome = super::import_items::<CreateConnectorRequest, _, _, _, _, _, _, _, _>(
        items,
        query.dry_run,
        query.on_conflict,
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
            upsert: |c: CreateConnectorRequest, dry_run: bool| {
                let state = upsert_state.clone();
                async move { upsert_connector(&state, c, dry_run).await }
            },
        },
    )
    .await;
    if query.dry_run {
        return Ok(super::import_response(true, outcome));
    }
    // K5: one row per written entity, plus the batch summary row.
    for id in outcome.written() {
        audit_log(&state.audit_queue, &principal, "import", "connector", id);
    }
    audit_log(
        &state.audit_queue,
        &principal,
        "import",
        "connector",
        &format!("{} imported", outcome.imported),
    );
    // Reload registry once at the end if anything was written.
    if outcome.imported > 0 {
        reload_connectors(&state).await?;
    }
    Ok(super::import_response(false, outcome))
}

/// K2: one connector item under `on_conflict=new_version`.
///
/// Connectors are unversioned, so "new version" degrades to an in-place
/// update — the same wholesale config replacement `PUT /connectors/{id}`
/// performs. The item was matched by `name`, so the rename guard cannot
/// apply; the pool eviction the PUT path runs is preserved here, because a
/// changed config must not keep serving through a cached connection pool.
async fn upsert_connector(
    state: &AppState,
    req: CreateConnectorRequest,
    dry_run: bool,
) -> Result<super::ImportAction, OrionError> {
    use super::ImportAction;

    let existing = match state.repos.connectors.get_by_name(&req.name).await {
        Ok(existing) => existing,
        Err(OrionError::NotFound(_)) => {
            if !dry_run {
                state.repos.connectors.create(&req).await?;
            }
            return Ok(ImportAction::Created);
        }
        Err(e) => return Err(e),
    };

    if crate::storage::content::connector_content(&existing)?
        == crate::storage::content::connector_request_content(&req)
    {
        return Ok(ImportAction::Unchanged);
    }
    if !dry_run {
        state
            .repos
            .connectors
            .update(
                &existing.id,
                &UpdateConnectorRequest {
                    name: None, // matched on name — it cannot change here
                    connector_type: Some(req.connector_type),
                    config: Some(req.config),
                    enabled: Some(req.enabled.unwrap_or(true)),
                    tags: Some(req.tags),
                },
            )
            .await?;
        evict_connector_pools(state, &existing.name).await;
    }
    Ok(ImportAction::Updated)
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

// ============================================================
// Connector Export / Validate
// ============================================================

#[utoipa::path(
    get,
    path = "/api/v1/admin/connectors/export",
    tag = "Connectors",
    params(ConnectorFilter),
    responses(
        (status = 200, description = "Exported connectors in the shape `/import` accepts, secrets masked", body = DataEnvelope<Vec<ConnectorExportItem>>),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn export_connectors(
    State(state): State<AppState>,
    // Pagination in the filter is overridden by definition — an export
    // returns everything that matches; `?tag=` (K6) is honoured.
    OrionQuery(filter): OrionQuery<ConnectorFilter>,
) -> Result<Json<Value>, OrionError> {
    // K12: one repeatable-read transaction — the export is a consistent
    // snapshot, not a sequence of independent page queries.
    let rows = state.repos.connectors.snapshot(&filter).await?;

    // An export has to emit the shape `/import` accepts, which is not the shape
    // `GET /connectors` returns: `ConnectorResponse` carries `config_json` as a
    // *string*, while `CreateConnectorRequest` takes `config` as an object. A
    // bundle in the read shape parses on import, silently defaults `config` to
    // `{}`, and fails with a missing-field error naming a key the operator can
    // see right there in the file.
    //
    // Masking still happens first and is not skippable: `mask_connector` is the
    // only constructor of `ConnectorResponse` and the stored row does not
    // serialize at all (D27). `env://` references survive it as references, so
    // a connector authored that way round-trips intact; one holding literal
    // secrets exports with `******` in their place and needs them re-supplied,
    // which is the documented trade for a bundle that is safe to commit.
    let data: Vec<Value> = rows
        .iter()
        .map(|connector| {
            let masked = mask_connector(connector);
            json!({
                "id": masked.id,
                "name": masked.name,
                "connector_type": masked.connector_type,
                "config": serde_json::from_str::<Value>(&masked.config_json)
                    .unwrap_or_else(|_| json!({})),
                "enabled": masked.enabled,
                "tags": masked.tags,
                // K10: over the stored (unmasked) content, like every entity
                // response — import ignores it like the other extras.
                "content_hash": masked.content_hash,
            })
        })
        .collect();
    Ok(data_response(data))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/connectors/validate",
    tag = "Connectors",
    request_body = CreateConnectorRequest,
    responses(
        (status = 200, description = "Validation result", body = super::ValidationEnvelope),
    )
)]
#[tracing::instrument(skip(req))]
pub(crate) async fn validate_connector(
    OrionJson(req): OrionJson<CreateConnectorRequest>,
) -> Result<Json<super::ValidationEnvelope>, OrionError> {
    // R20 again: the create-path validator verbatim, so `valid: true` keeps
    // meaning "`POST /connectors` would accept this".
    let errors = match crate::validation::validate_create_connector(&req) {
        Ok(()) => Vec::new(),
        Err(e) => super::issues_from_error(e),
    };

    // An `env://` reference that is unset on *this* host is a warning, not an
    // error: a bundle is routinely validated on a machine that holds none of
    // the production secrets, and refusing there would make the endpoint
    // useless for exactly the promotion flow it exists to support.
    let mut warnings = Vec::new();
    let mut config = req.config.clone();
    if let Err(e) = crate::connector::secrets::resolve_in_place(
        &mut config,
        &crate::connector::secrets::default_resolvers(),
        "config",
    )
    .await
    {
        warnings.push(super::ValidationIssue {
            field: "config".to_string(),
            message: format!(
                "a secret reference does not resolve on this host: {e}. \
                 The connector will fail to load where the value is unset."
            ),
        });
    }

    // An SMTP connector without TLS moves credentials and message content in
    // cleartext. Legitimate for a localhost/dev relay, so a warning — but a
    // loud one, per the #262 design.
    if req.connector_type == crate::connector::ConnectorType::Smtp
        && req.config.get("tls").and_then(serde_json::Value::as_str) == Some("none")
    {
        warnings.push(super::ValidationIssue {
            field: "config.tls".to_string(),
            message: "tls is 'none': credentials and message content travel in                       cleartext. Acceptable only for a local dev relay — use                       'starttls' (587) or 'implicit' (465) anywhere real."
                .to_string(),
        });
    }

    Ok(Json(super::ValidationEnvelope::new(errors, warnings)))
}

// ============================================================
// Connector connectivity probe
// ============================================================

/// What a probe found.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ProbeResult {
    /// `true` when the backend answered.
    reachable: bool,
    /// R29: `false` when no probe exists for this connector (`es`, `kafka`,
    /// or a `db` connector with a `mongodb://` URL) — a permanent capability
    /// gap, not an outage. Without this field the two were indistinguishable
    /// in the response shape, and operator tooling read "not implemented" as
    /// downtime.
    supported: bool,
    /// Connector type, so a caller need not re-read the connector to know
    /// which kind of probe ran.
    connector_type: String,
    /// What the probe did, named plainly — the operator is entitled to know
    /// whether their production system was contacted and how.
    probe: &'static str,
    /// Failure detail. Present only when `reachable` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ProbeEnvelope {
    data: ProbeResult,
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/connectors/{id}/test",
    tag = "Connectors",
    params(("id" = String, Path, description = "Connector ID")),
    responses(
        (status = 200, description = "Probe result. A backend that cannot be reached is \
            still a 200 — the probe ran and this is its answer; `reachable: false` is the \
            finding, not a server error.", body = ProbeEnvelope),
        (status = 404, description = "Connector not found"),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn test_connector(
    State(state): State<AppState>,
    Path(id): Path<String>,
    principal: Option<Extension<AdminPrincipal>>,
) -> Result<Json<ProbeEnvelope>, OrionError> {
    let connector = state.repos.connectors.get_by_id(&id).await?;

    // Recorded because a probe reaches out to a production system: for HTTP it
    // issues a real request with the connector's real credentials. That is a
    // side effect, and the audit trail should say who caused it — the same
    // reasoning that put `POST /workflows/{id}/test` on the trail (O7).
    super::audit_log(&state.audit_queue, &principal, "test", "connector", &id);

    // One construction site: `reachable` is derived from the outcome, so a
    // fourth early exit cannot report a failure as reachable. `supported` is
    // owned by `probe_connector` — the one place that knows which configs
    // have a probe — so the two cannot drift; the early exits (secret
    // resolution, config parse) fire before a config exists to ask, and
    // judge it by the connector type alone.
    let type_supported = !matches!(connector.connector_type.as_str(), "es" | "kafka");
    let result = |probe: &'static str, supported: bool, outcome: Result<(), String>| {
        Json(ProbeEnvelope {
            data: ProbeResult {
                reachable: outcome.is_ok(),
                supported,
                connector_type: connector.connector_type.clone(),
                probe,
                error: outcome.err(),
            },
        })
    };

    // Probe the *stored* definition with its secrets resolved, rather than the
    // registry's copy: a connector that failed to load has no registry entry,
    // and that is exactly when an operator reaches for this endpoint.
    let mut config_value: Value = serde_json::from_str(&connector.config_json)
        .map_err(|e| OrionError::internal(format!("stored connector config is not JSON: {e}")))?;

    // `ConnectorConfig` is internally tagged on `type`, but the type lives in
    // its own column — inject it exactly as the registry load does, so a
    // connector authored the documented way (no redundant `"type"` inside
    // `config`) parses here too.
    if let Some(obj) = config_value.as_object_mut() {
        obj.insert(
            "type".to_string(),
            Value::String(connector.connector_type.clone()),
        );
    }

    // S21: shape-check *before* resolving secrets. A serde error over the
    // resolved document can quote a resolved secret verbatim (a string
    // credential in a numeric field, say) — the same detail the read API
    // masks. Over the stored document the message can only quote the
    // operator's own references.
    if let Err(e) = <crate::connector::ConnectorConfig as serde::Deserialize>::deserialize(
        // Deserializing from `&Value` checks the shape without cloning the
        // document the way `from_value` (which takes ownership) would force.
        &config_value,
    ) {
        return Ok(result("config parse", type_supported, Err(e.to_string())));
    }
    if let Err(e) = crate::connector::secrets::resolve_in_place(
        &mut config_value,
        &crate::connector::secrets::default_resolvers(),
        "config",
    )
    .await
    {
        return Ok(result(
            "secret resolution",
            type_supported,
            Err(e.client_message()),
        ));
    }
    let config = match serde_json::from_value::<crate::connector::ConnectorConfig>(config_value) {
        Ok(config) => config,
        Err(e) => {
            // The stored shape parsed above, so only the resolved values can
            // be at fault — and the message could quote them. Log it, don't
            // serve it (same redaction as the probe errors below).
            tracing::warn!(connector = %connector.name, error = %e, "resolved connector config failed to parse");
            return Ok(result(
                "config parse",
                type_supported,
                Err(
                    "the config parses as stored but not with its secret references \
                     resolved; the parse error is in the server log"
                        .to_string(),
                ),
            ));
        }
    };

    let (probe, supported, outcome) = probe_connector(&state, &connector.name, &config).await;
    Ok(result(probe, supported, outcome))
}

/// Run the probe appropriate to the connector's type. Returns what the probe
/// did, whether one exists for this config at all (`supported`), and its
/// outcome — supportedness lives here, next to the arms that define it, so
/// implementing a probe for a currently-unsupported kind cannot leave a stale
/// `supported: false` behind in the handler.
///
/// Every probe is read-only. The HTTP one is the only that touches a third
/// party, and it is the reason the endpoint is worth having at all: a wrong
/// bearer token is invisible until the first real request otherwise.
async fn probe_connector(
    state: &AppState,
    name: &str,
    config: &crate::connector::ConnectorConfig,
) -> (&'static str, bool, Result<(), String>) {
    use crate::connector::ConnectorConfig;

    match config {
        // R29: a `db` connector pointing at MongoDB has no probe — the SQL
        // pool below cannot dial `mongodb://`, so routing it to `probe_db`
        // reported a permanent capability gap as an outage on every call.
        ConnectorConfig::Db(db) if crate::connector::is_mongo_url(&db.connection_string) => (
            "not implemented for this connector type",
            false,
            Err("connectivity probing is not implemented for MongoDB connectors yet".to_string()),
        ),
        ConnectorConfig::Db(db) => ("SELECT 1", true, probe_db(state, name, db).await),
        ConnectorConfig::Cache(cache) => (
            "cache read of a probe key",
            true,
            probe_cache(state, name, cache).await,
        ),
        ConnectorConfig::Http(http) => (
            // #268: for a managed-OAuth2 connector this acquires a real
            // token first, so the probe validates the whole OAuth setup —
            // token URL, credentials, grant — before any workflow needs it.
            "GET the configured URL (acquiring the OAuth2 token first, if managed)",
            true,
            probe_http(state, name, http).await,
        ),
        ConnectorConfig::Smtp(smtp) => (
            "SMTP connect + EHLO + auth (no mail sent)",
            true,
            probe_smtp(state, name, smtp).await,
        ),
        ConnectorConfig::Storage(storage) => (
            "signed HEAD of the bucket",
            true,
            probe_storage(state, storage).await,
        ),
        ConnectorConfig::Es(_) | ConnectorConfig::Kafka(_) => (
            "not implemented for this connector type",
            false,
            Err(format!(
                "connectivity probing is not implemented for '{}' connectors yet; \
                 Kafka brokers are covered by `orion-server test-connectivity`",
                config.connector_type()
            )),
        ),
    }
}

/// The cheapest statement every supported SQL backend accepts.
async fn probe_db(
    state: &AppState,
    name: &str,
    db: &crate::connector::DbConnectorConfig,
) -> Result<(), String> {
    let pool = state
        .caches
        .sql_pool_cache
        .get_pool(name, db)
        .await
        .map_err(|e| e.client_message())?;
    sqlx::query("SELECT 1")
        .execute(&pool)
        .await
        .map(|_| ())
        .map_err(|e| {
            // S21: a raw sqlx error names databases, roles and host:port —
            // the same detail the pool arm above redacts. Log it, don't
            // serve it.
            tracing::warn!(connector = %name, error = %e, "connectivity probe query failed");
            // The pool may be cached from an earlier probe, so an `execute`
            // failure is not proof a connection existed — only an error the
            // database itself reported is. Claiming "connected" on a
            // connection-level failure would steer the operator away from
            // the network/DNS/down-host diagnosis this endpoint exists for.
            if matches!(e, sqlx::Error::Database(_)) {
                "connected, but the probe query failed; the driver error is in the server log"
                    .to_string()
            } else {
                "the probe could not complete against the database — it may be \
                 unreachable; the driver error is in the server log"
                    .to_string()
            }
        })
}

/// A read, not a write: the probe must not leave anything behind in a store a
/// workflow shares.
async fn probe_cache(
    state: &AppState,
    name: &str,
    cache: &crate::connector::CacheConnectorConfig,
) -> Result<(), String> {
    let backend = state
        .caches
        .cache_pool
        .get_backend(
            crate::connector::cache_backend::CachePurpose::Workflow,
            name,
            cache,
        )
        .await
        .map_err(|e| e.client_message())?;
    backend
        .get("__orion_connectivity_probe__")
        .await
        .map(|_| ())
        .map_err(|e| {
            // S21: same redaction as `probe_db` — the backend error can
            // carry the Redis host:port. And like `probe_db`, the backend
            // handle may be cached from an earlier probe, so a failed read
            // is not proof a connection existed — the wording must not
            // claim one.
            tracing::warn!(connector = %name, error = %e, "connectivity probe read failed");
            "the probe read failed — the backend may be unreachable; the driver \
             error is in the server log"
                .to_string()
        })
}

/// One signed HEAD of the bucket root — reachability plus whether the store
/// accepts the credentials. Any HTTP status is "reachable and answering":
/// a 403 here usually means the credentials lack bucket-level HeadBucket
/// rights while object operations still work, and a probe should not report
/// a permission nuance as an outage; a transport error is the real failure.
async fn probe_storage(
    state: &AppState,
    storage: &crate::connector::StorageConnectorConfig,
) -> Result<(), String> {
    use crate::connector::sigv4;

    let (scheme, host, path) = storage.address(None)?;
    if !storage.allow_private_urls
        && let Err(msg) =
            crate::validation::validate_url_not_private(&format!("{scheme}://{host}{path}")).await
    {
        return Err(format!("SSRF protection: {msg}"));
    }
    let amz_date = sigv4::amz_date_now();
    let ctx = sigv4::SigningContext {
        access_key: &storage.access_key,
        secret_key: &storage.secret_key,
        session_token: storage.session_token.as_deref(),
        region: &storage.region,
        service: "s3",
        host: &host,
        path: &path,
        amz_date: &amz_date,
    };
    let mut req = state
        .http_client
        .head(format!("{scheme}://{host}{path}"))
        .timeout(std::time::Duration::from_millis(storage.timeout_ms));
    for (name, value) in sigv4::sign_headers(&ctx, "HEAD") {
        req = req.header(name, value);
    }
    match req.send().await {
        Ok(response) if response.status().is_success() || response.status().as_u16() < 500 => {
            Ok(())
        }
        Ok(response) => Err(format!("the store answered HTTP {}", response.status())),
        // S21: transport errors can carry the endpoint; that is the
        // diagnostic, credentials never appear in it.
        Err(e) => Err(e.to_string()),
    }
}

/// Connect, EHLO, negotiate TLS and authenticate against an SMTP connector —
/// without sending mail.
///
/// Goes through the same cached transport `send_email` uses, so what this
/// probes is what real sends get: the pool-open path (including the S6
/// private-address check), the TLS mode, and the credentials — lettre
/// authenticates as part of establishing the connection, so a refused
/// password surfaces here instead of on the first OTP email.
async fn probe_smtp(
    state: &AppState,
    name: &str,
    smtp: &crate::connector::SmtpConnectorConfig,
) -> Result<(), String> {
    let transport = state
        .caches
        .smtp_pool_cache
        .get_transport(name, smtp)
        .await
        .map_err(|e| e.to_string())?;
    match transport.test_connection().await {
        Ok(true) => Ok(()),
        Ok(false) => Err("the SMTP server did not answer the probe NOOP".to_string()),
        // The lettre error names the host/port and the server's reply — the
        // diagnostic an operator needs; credentials never appear in it.
        Err(e) => Err(e.to_string()),
    }
}

/// Issue one request against an HTTP connector's configured URL.
///
/// Goes through `http_common::execute_request` — the same function `http_call`
/// uses — rather than a bare `client.get(url)`. That is the difference between
/// a probe and a probe that means something: it applies the connector's auth,
/// its configured headers, its `allow_private_urls` exemption and the
/// per-hop-revalidated redirect policy. A probe taking a different path could
/// pass where real traffic fails, or (as a bare GET does) report every
/// correctly-configured authenticated endpoint as a credential failure because
/// it never sent the credential.
async fn probe_http(
    state: &AppState,
    name: &str,
    http: &crate::connector::HttpConnectorConfig,
) -> Result<(), String> {
    use crate::engine::functions::http_common;

    // #268: the effective auth — for oauth2 this is a real token acquisition
    // through the manager, which is precisely the setup this probe validates.
    let auth = crate::connector::oauth::effective_auth(
        state.connector_registry.oauth(),
        name,
        http,
    )
    .await
    .map_err(|e| e.to_string())?;

    let url = http_common::build_url(&http.url, None);
    match http_common::execute_request(
        &state.http_client,
        http,
        http_common::RequestSpec {
            method: &reqwest::Method::GET,
            url: &url,
            task_headers: None,
            body: None,
            body_format: http_common::BodyFormat::default(),
            // Text, not Json: this probes reachability and credentials, and a
            // 2xx whose body happens not to be JSON is still a reachable
            // endpoint.
            response_format: http_common::ResponseFormat::Text,
            timeout: std::time::Duration::from_secs(5),
            auth: auth.as_deref(),
        },
    )
    .await
    {
        Ok(_) => Ok(()),
        // `execute_request` already turns a 4xx/5xx into an error naming the
        // status, so an unreachable host and a refused credential both arrive
        // here with the detail an operator needs. Reporting a 401 as
        // "reachable" would be technically true and practically useless — a
        // wrong token is the failure this endpoint exists to surface.
        Err(e) => Err(e.to_string()),
    }
}
