//! Trace listing & polling: the payload-free list projection (S14) and the
//! token- or admin-gated single-trace read (R12).

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::errors::OrionError;
use crate::server::extract::{OrionQuery, PeerAddr};
// Referenced by the `#[utoipa::path]` `body = ErrorResponse` annotations below.
use crate::server::routes::openapi::ErrorResponse;
use crate::server::routes::openapi::{DataEnvelope, TraceDetail, TracePageEnvelope};
use crate::server::routes::response_helpers::data_response;
use crate::server::state::AppState;
use crate::storage::models::TraceListItemResponse;
use crate::storage::repositories::traces::TraceFilter;

#[utoipa::path(
    get,
    path = "/api/v1/admin/traces",
    tag = "Traces",
    params(TraceFilter),
    responses(
        (status = 200, description = "Page of traces", body = TracePageEnvelope),
        (status = 400, description = "Malformed cursor, or cursor combined with offset or a non-default sort", body = ErrorResponse),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_traces(
    State(state): State<AppState>,
    OrionQuery(filter): OrionQuery<TraceFilter>,
) -> Result<Json<Value>, OrionError> {
    let result = state.repos.traces.list_paginated(&filter).await?;
    // Payload-free projection (S14): `input_json` holds the caller's request
    // body and `result_json`/`task_trace_json` the full engine message, so a
    // list row is every caller's traffic in one response — including rows
    // persisted before S10 masked credential headers. Payloads are served
    // one trace at a time by `GET /traces/{id}`, mirroring the DLQ list.
    let rows: Vec<TraceListItemResponse> = result
        .data
        .iter()
        .map(TraceListItemResponse::from)
        .collect();
    // `total` and `next_cursor` are both conditional (D8), so this page is
    // assembled here rather than through `paginated_response`.
    let mut body = json!({
        "data": rows,
        "limit": result.limit,
        "offset": result.offset,
    });
    if let Some(total) = result.total {
        body["total"] = json!(total);
    }
    if let Some(cursor) = result.next_cursor {
        body["next_cursor"] = json!(cursor);
    }
    Ok(Json(body))
}

/// Query parameters for `GET /traces/{id}`.
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct TraceAccessQuery {
    /// **Deprecated.** The capability token returned with the async 202. Use
    /// the `x-trace-token` header instead: a URL is not a private place. It
    /// reaches browser history, reverse-proxy and CDN access logs, analytics,
    /// `Referer` headers on anything the page loads next, and every chat
    /// window a support ticket is pasted into — none of which the header
    /// touches. Still accepted for clients that cannot set headers; responses
    /// that authorise this way carry a `Deprecation` header, and
    /// `orion_trace_token_query_reads_total` counts them so an operator can
    /// see whether anything still depends on it.
    token: Option<String>,
}

/// A trace read never belongs in a shared cache.
///
/// The body carries the submission's full result, and the capability that
/// authorised it travels in `x-trace-token` (or, worse, the query string) —
/// **not** in `Authorization`, which is the header that makes a shared cache
/// treat a response as private by default (RFC 9111 §3.5). Without this a
/// proxy is entitled to store a trace body and hand it to the next caller who
/// arrives with the same URL. The sign-in redirect already says `no-store` for
/// the same class of reason; this is the other place a response is a secret.
const NO_STORE: (axum::http::header::HeaderName, &str) =
    (axum::http::header::CACHE_CONTROL, "no-store");

/// Which lane authorised a trace read — the answer decides whether the
/// response is also a deprecation notice.
#[derive(Clone, Copy, PartialEq)]
enum TraceLane {
    /// An admin credential, or the `x-trace-token` header.
    Supported,
    /// The `?token=` query parameter (deprecated).
    QueryToken,
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/traces/{id}",
    tag = "Traces",
    description = "\
Fetch one trace. Access follows a two-lane rule (R12): present either a \
valid admin credential, or — for async submissions — the `trace_token` \
returned with the 202, in the `x-trace-token` header. Traces without a token \
(sync traces, DLQ retries, rows from before 1.0.0) are admin-plane only when \
admin auth is enabled.\n\n\
The `?token=` query parameter is a **deprecated** alternative for clients \
that cannot set headers. Prefer the header: a URL is not a private place, \
and the token leaks into browser history, proxy and CDN logs, `Referer` \
headers and anywhere a link is pasted. Reads authorised that way answer with \
a `Deprecation` header.\n\n\
Every response carries `Cache-Control: no-store` — the body is the \
submission's result and the capability is not an `Authorization` header, so \
nothing else stops a shared cache storing it.",
    params(
        ("id" = String, Path, description = "Trace ID"),
        TraceAccessQuery,
    ),
    responses(
        (status = 200, description = "Trace status and result", body = DataEnvelope<TraceDetail>),
        (status = 401, description = "Missing or wrong trace token / admin credential", body = ErrorResponse),
        (status = 404, description = "Trace not found", body = ErrorResponse),
    )
)]
#[tracing::instrument(skip(state, headers, query))]
pub(crate) async fn get_trace(
    State(state): State<AppState>,
    Path(id): Path<String>,
    OrionQuery(query): OrionQuery<TraceAccessQuery>,
    PeerAddr(peer): PeerAddr,
    headers: axum::http::HeaderMap,
) -> Result<Response, OrionError> {
    // This route carries its own auth (the admin middleware does not guard it),
    // so it has to carry the middleware's *brute-force* protection too. Without
    // this, the trace token was the one credential on the whole surface that
    // could be guessed at full speed: unlimited attempts, none counted, no
    // lockout. Identify the caller exactly as the middleware and the rate
    // limiter do, so a spoofed `X-Forwarded-For` cannot mint a fresh budget.
    let client = crate::server::rate_limit::client_ip_from_parts(
        peer.as_ref(),
        &headers,
        state.trusted_proxies(),
    );
    if state.config.admin_auth.enabled
        && let Some(remaining) = state.admin_auth_failures.locked_for(&client)
    {
        crate::metrics::record_admin_auth_failure("locked_out");
        tracing::warn!(
            client = %client,
            remaining_ms = remaining.as_millis() as u64,
            "Trace read refused: client is in failed-auth backoff"
        );
        return Err(OrionError::Unauthorized(
            "This trace requires its trace_token (returned with the async 202) or an admin credential".into(),
        ));
    }

    let trace = state.repos.traces.get_by_id(&id).await?;

    // R12 access rule. Lane 1: a valid admin credential (only meaningful when
    // admin auth is enabled — the middleware no longer guards this route, so
    // the check happens here). Lane 2: the per-submission capability token.
    // Tokenless traces stay on the admin trust model: open when auth is
    // disabled (the whole admin plane is), admin-only when enabled.
    //
    // A missing trace 404s before this check, so an unauthorized caller can
    // distinguish "exists" from "does not exist". Deliberate: trace ids are
    // v4 UUIDs, so there is nothing to enumerate (the id-listing endpoint is
    // admin-guarded, and its rows carry no payloads since S14), and the
    // distinction is what makes a wrong-id-vs-wrong-token mistake debuggable.
    let auth_cfg = &state.config.admin_auth;
    let is_admin = auth_cfg.enabled
        && crate::server::admin_auth::headers_present_valid_key(&headers, auth_cfg);
    // Which lane answered, tracked rather than inferred: the response says
    // `Deprecation` only when the query parameter is what got the caller in,
    // and a caller who sends both is using the supported one.
    let mut lane = TraceLane::Supported;
    if !is_admin {
        let header_token = headers
            .get("x-trace-token")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let presented = match header_token {
            Some(token) => Some(token),
            None => {
                if query.token.is_some() {
                    lane = TraceLane::QueryToken;
                }
                query.token.clone()
            }
        };
        let allowed = match trace.access_token_hash.as_deref() {
            Some(stored) => presented
                .as_deref()
                .is_some_and(|t| crate::server::admin_auth::trace_token_matches(t, stored)),
            None => !auth_cfg.enabled,
        };
        if !allowed {
            if auth_cfg.enabled {
                let lockout = state.admin_auth_failures.record_failure(&client);
                crate::metrics::record_admin_auth_failure("invalid_key");
                tracing::warn!(
                    client = %client,
                    lockout_ms = lockout.map(|d| d.as_millis() as u64),
                    "Trace read refused: neither a valid trace token nor an admin credential"
                );
            }
            return Err(OrionError::Unauthorized(
                "This trace requires its trace_token (returned with the async 202) or an admin credential".into(),
            ));
        }
        // Counted only once the token actually authorised: a wrong `?token=`
        // is a failed auth, already counted as one, and counting it here too
        // would make the deprecation gauge read as usage by clients that have
        // no valid token at all.
        if lane == TraceLane::QueryToken {
            crate::metrics::record_trace_token_query_read();
            tracing::debug!(
                trace_id = %id,
                "Trace read authorised by the deprecated `?token=` query parameter"
            );
        }
        // A correct token clears the budget, the way a valid admin key does —
        // otherwise a legitimate poller inherits a lockout from whoever else
        // shares its address.
        if auth_cfg.enabled {
            state.admin_auth_failures.record_success(&client);
        }
    } else {
        state.admin_auth_failures.record_success(&client);
    }

    let mut response = json!({
        "id": trace.id,
        "status": trace.status,
        "mode": trace.mode,
        "channel": trace.channel,
        "channel_id": trace.channel_id,
        "created_at": trace.created_at,
    });

    use crate::storage::models;
    if trace.status == models::TRACE_STATUS_COMPLETED {
        if let Some(ref result_str) = trace.result_json
            && let Ok(mut result_val) = serde_json::from_str::<Value>(result_str)
        {
            // S14: the stored message's `context.metadata` carries the
            // request headers (masked since S10 — but rows persisted before
            // that upgrade hold them in plaintext). Strip it from the read
            // projection; pollers need `data`/`payload`, not the submitter's
            // request context.
            if let Some(ctx) = result_val.get_mut("context").and_then(Value::as_object_mut) {
                ctx.remove("metadata");
            }
            response["message"] = result_val;
        }
    } else if trace.status == models::TRACE_STATUS_FAILED
        && let Some(ref err) = trace.error_message
    {
        response["error"] = json!(err);
    }

    if let Some(ref started) = trace.started_at {
        response["started_at"] = json!(started);
    }
    if let Some(ref completed) = trace.completed_at {
        response["completed_at"] = json!(completed);
    }
    if let Some(duration) = trace.duration_ms {
        response["duration_ms"] = json!(duration);
    }
    if let Some(ref tt) = trace.task_trace_json
        && let Ok(mut v) = serde_json::from_str::<Value>(tt)
    {
        strip_step_metadata(&mut v);
        response["task_trace_json"] = v;
    }

    // `no-store` on both lanes; `Deprecation` only on the one being retired.
    let mut out = data_response(response).into_response();
    let headers = out.headers_mut();
    headers.insert(NO_STORE.0, axum::http::HeaderValue::from_static(NO_STORE.1));
    if lane == TraceLane::QueryToken {
        headers.insert(
            axum::http::HeaderName::from_static("deprecation"),
            axum::http::HeaderValue::from_static("true"),
        );
    }
    Ok(out)
}

/// Apply the S14 strip to every step snapshot inside a stored task trace.
///
/// The strip above covers `result_json`, but each `ExecutionStep` holds its own
/// full `Message` clone carrying the same `context.metadata` — so the identical
/// request headers were returned verbatim one field further down. Only four
/// header names are masked at ingress, which left everything else readable
/// through this path.
///
/// New rows do not need this: `runner::trace_options` sets `redact_paths`, so
/// the header map is never cloned into a step. It stays for rows already on
/// disk, which is why it is a read-side walk rather than a migration.
fn strip_step_metadata(trace: &mut Value) {
    fn drop_metadata(context: Option<&mut Value>) {
        if let Some(obj) = context.and_then(Value::as_object_mut) {
            obj.remove("metadata");
        }
    }

    let Some(steps) = trace.get_mut("steps").and_then(Value::as_array_mut) else {
        return;
    };
    for step in steps {
        drop_metadata(step.get_mut("message").and_then(|m| m.get_mut("context")));
        // A `map` task's per-mapping snapshots are whole-context clones of
        // their own, so they carry the header map independently of the step's
        // `message` — one more copy per mapping, not per task.
        if let Some(contexts) = step
            .get_mut("mapping_contexts")
            .and_then(Value::as_array_mut)
        {
            for context in contexts {
                drop_metadata(Some(context));
            }
        }
    }
}
