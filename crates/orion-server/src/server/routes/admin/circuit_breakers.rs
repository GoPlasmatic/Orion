//! Circuit breakers: one per `(channel, connector)` pairing, node-local, and
//! a reset that fans out to every node in a cluster.

use axum::extract::{Path, State};
use axum::{Extension, Json};
use orion_api::{CircuitBreakerResetResponse, CircuitBreakerStatesResponse, Data};

use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::routes::response_helpers::data_response;
use crate::server::state::AppState;

use super::audit_log;

#[utoipa::path(
    get,
    path = "/api/v2/admin/connectors/circuit-breakers",
    tag = "Circuit Breakers",
    responses(
        (status = 200, description = "Circuit breaker states", body = Data<CircuitBreakerStatesResponse>),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_circuit_breakers(
    State(state): State<AppState>,
) -> Json<Data<CircuitBreakerStatesResponse>> {
    let states = state.connector_registry.circuit_breaker_states().await;
    data_response(CircuitBreakerStatesResponse {
        enabled: state.connector_registry.circuit_breaker_enabled(),
        // F21: breakers are node-local state. The *reset* path fans out to
        // every node, which made an unqualified read actively misleading — it
        // reads like cluster state because its sibling mutation is. Say whose
        // map this is.
        scope: "node".to_string(),
        instance_id: state.cluster.instance_id.clone(),
        breakers: states.into_iter().collect(),
    })
}

#[utoipa::path(
    post,
    path = "/api/v2/admin/connectors/circuit-breakers/{key}",
    tag = "Circuit Breakers",
    params(("key" = String, Path, description = "Circuit breaker key (channel:connector)")),
    responses(
        (status = 200, description = "Circuit breaker reset", body = Data<CircuitBreakerResetResponse>),
        (status = 404, description = "Circuit breaker not found"),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn reset_circuit_breaker(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(key): Path<String>,
) -> Result<Json<Data<CircuitBreakerResetResponse>>, OrionError> {
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
    // Breakers are node-local (D3); fan the reset out through the breaker
    // epoch so one API call resets the same key on every node.
    if state.cluster.enabled {
        let breaker_epoch = state.cluster.repo.request_breaker_reset(&key).await?;
        state
            .cluster
            .last_seen_breaker_epoch
            .fetch_max(breaker_epoch, std::sync::atomic::Ordering::AcqRel);
    }
    Ok(data_response(CircuitBreakerResetResponse {
        reset: true,
        key,
        // Whether *this* node held the key. The fan-out clears it everywhere
        // regardless; `false` in cluster mode means "not here, broadcast anyway".
        found_on_this_node: found,
    }))
}
