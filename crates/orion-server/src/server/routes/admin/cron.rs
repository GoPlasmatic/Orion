//! The operator view of the schedule: what ran, what is waiting, and what is
//! next.
//!
//! Read-only apart from two deliberate mutations. Runtime state is kept *out*
//! of the authored channel response on purpose — a channel row is an immutable
//! versioned definition, and writing a moving cursor into it would make an
//! immutable record mutable — so "when does this next fire?" is a separate
//! request against a separate shape.
//!
//! The listing follows the trace DLQ's split: a narrow summary per row, and the
//! diagnostic detail only when one occurrence is asked for by id.

use axum::Json;
use axum::extract::{Path, State};
use axum::{Extension, http::StatusCode};
use serde_json::Value;

use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::extract::OrionQuery;
use crate::server::routes::openapi::{DataEnvelope, PaginatedEnvelope};
use crate::server::routes::response_helpers::{data_response, paginated_response};
use crate::server::state::AppState;
use crate::storage::models::{
    CronOccurrenceResponse, CronOccurrenceSummaryResponse, CronScheduleStatusResponse,
};
use crate::storage::repositories::cron::{CronOccurrenceFilter, status, trigger};

use super::audit_log;

#[utoipa::path(
    get,
    path = "/api/v1/admin/cron/occurrences",
    tag = "Cron",
    params(CronOccurrenceFilter),
    responses(
        (status = 200, description = "Paginated occurrences, newest first. Summaries only — \
            fetch one by id for the failure reason, the trace id and the lease detail", body = PaginatedEnvelope<CronOccurrenceSummaryResponse>),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_occurrences(
    State(state): State<AppState>,
    OrionQuery(filter): OrionQuery<CronOccurrenceFilter>,
) -> Result<Json<Value>, OrionError> {
    let result = state.repos.cron.list_paginated(&filter).await?;
    let rows: Vec<CronOccurrenceSummaryResponse> = result
        .data
        .iter()
        .map(CronOccurrenceSummaryResponse::from)
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
    path = "/api/v1/admin/cron/occurrences/{id}",
    tag = "Cron",
    params(("id" = String, Path, description = "Occurrence id")),
    responses(
        (status = 200, description = "One occurrence in full", body = DataEnvelope<CronOccurrenceResponse>),
        (status = 404, description = "No such occurrence", body = crate::server::routes::openapi::ErrorResponse),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn get_occurrence(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    let occurrence = state.repos.cron.get_by_id(&id).await?;
    Ok(data_response(CronOccurrenceResponse::from(&occurrence)))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/cron/occurrences/{id}/retry",
    tag = "Cron",
    params(("id" = String, Path, description = "Occurrence id")),
    responses(
        (status = 200, description = "Occurrence reset to `pending`; the next worker poll picks it up", body = DataEnvelope<CronOccurrenceResponse>),
        (status = 404, description = "No such occurrence", body = crate::server::routes::openapi::ErrorResponse),
        (status = 409, description = "The occurrence is not in a retryable state — \
            `completed` work is re-run by triggering the channel, not by retrying a finished occurrence", body = crate::server::routes::openapi::ErrorResponse),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn retry_occurrence(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OrionError> {
    // The retry keeps the occurrence's identity and its `scheduled_for`: it is
    // another attempt at the work that was due then, not a new piece of work.
    // That is what makes `trigger.scheduled_for` usable as an idempotency key
    // by the workflow — two attempts at one occurrence agree on it.
    let occurrence = state.repos.cron.requeue(&id).await?;
    audit_log(
        &state.audit_queue,
        &principal,
        "retry",
        "cron_occurrence",
        &id,
    );
    Ok(data_response(CronOccurrenceResponse::from(&occurrence)))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/cron/status",
    tag = "Cron",
    responses(
        (status = 200, description = "One row per active cron channel: its schedule, where its \
            cursor has got to, its most recent occurrence and its backlog", body = DataEnvelope<Vec<CronScheduleStatusResponse>>),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn cron_status(State(state): State<AppState>) -> Result<Json<Value>, OrionError> {
    // One generation: the schedules reported and the cursors they are matched
    // against come from the same read, so a reload mid-response cannot produce
    // a row describing one channel's expression against another's cursor.
    let generation = state.runtime.load();
    let descriptors = generation.channels.cron_descriptors();
    let cursors = state.repos.cron.schedule_states().await?;

    let mut rows = Vec::with_capacity(descriptors.len());
    for descriptor in descriptors {
        let cursor = cursors
            .iter()
            .find(|c| c.channel_id == descriptor.channel_id);
        let latest = state
            .repos
            .cron
            .latest_for_channel(&descriptor.channel_id)
            .await?;
        rows.push(CronScheduleStatusResponse {
            channel_id: descriptor.channel_id.clone(),
            channel_name: descriptor.channel_name.clone(),
            schedule: descriptor.expression.clone(),
            timezone: descriptor.timezone.name().to_string(),
            // Absent until the reconciler's first pass over this channel, which
            // is at most one poll interval after activation.
            next_fire_at: cursor.map(|c| c.next_fire_at),
            paused_at: cursor.and_then(|c| c.paused_at),
            last_status: latest.as_ref().map(|o| o.status.clone()),
            last_scheduled_for: latest.as_ref().map(|o| o.scheduled_for),
            last_completed_at: latest.as_ref().and_then(|o| o.completed_at),
            pending: state
                .repos
                .cron
                .pending_count(Some(&descriptor.channel_id))
                .await?,
        });
    }
    Ok(data_response(rows))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/channels/{id}/trigger",
    tag = "Cron",
    params(("id" = String, Path, description = "Channel id of an active cron channel")),
    responses(
        (status = 202, description = "An occurrence was created; a worker picks it up on its next \
            poll. It runs through the same claim, singleton and execution path a scheduled \
            occurrence does", body = DataEnvelope<CronOccurrenceResponse>),
        (status = 400, description = "Not an active cron channel", body = crate::server::routes::openapi::ErrorResponse),
        (status = 409, description = "An occurrence already exists for this instant — retry in a second", body = crate::server::routes::openapi::ErrorResponse),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn trigger_channel(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), OrionError> {
    let generation = state.runtime.load();
    let Some(runtime) = generation.channels.cron_by_channel_id(&id) else {
        // Deliberately one message for "not a cron channel", "not active" and
        // "quarantined": all three mean the same thing to the caller — there is
        // no schedule here to run — and `/health` is where the distinction
        // between them lives.
        return Err(OrionError::validation(format!(
            "Channel '{id}' is not an active cron channel on this node, so there is no \
             schedule to trigger. Only a `protocol: \"cron\"` channel that is active and \
             loaded can be triggered."
        )));
    };
    let descriptor = runtime
        .cron
        .as_ref()
        .expect("cron_by_channel_id only returns cron channels");

    // The occurrence is stamped with *now*, so it takes its place in the ledger
    // beside the scheduled ones and is claimed by the same query. It is not a
    // side door: the singleton applies, so triggering a `forbid` channel while
    // its scheduled run is in flight is recorded as `skipped_singleton` rather
    // than running alongside it.
    let scheduled_for = state.repos.cron.db_now().await?;
    let occurrence_id = uuid::Uuid::now_v7().to_string();
    let created = state
        .repos
        .cron
        .insert_occurrence(crate::storage::repositories::cron::NewOccurrence {
            id: &occurrence_id,
            channel_id: &descriptor.channel_id,
            channel_name: &descriptor.channel_name,
            channel_version: runtime.channel.version,
            workflow_id: descriptor.workflow_id.as_deref(),
            trigger: trigger::MANUAL,
            scheduled_for,
            status: status::PENDING,
            error_message: None,
        })
        .await?;
    if !created {
        // The identity index refused it: a scheduled occurrence already owns
        // this instant. A second's wait is a real fix, and inventing a
        // different instant would put a manual run at a time nothing scheduled.
        return Err(OrionError::Conflict(format!(
            "An occurrence for channel '{id}' already exists at {scheduled_for} — \
             a scheduled run claimed this instant. Try again in a second."
        )));
    }

    crate::metrics::record_cron_occurrence(status::PENDING);
    audit_log(&state.audit_queue, &principal, "trigger", "channel", &id);
    let occurrence = state.repos.cron.get_by_id(&occurrence_id).await?;
    Ok((
        StatusCode::ACCEPTED,
        data_response(CronOccurrenceResponse::from(&occurrence)),
    ))
}
