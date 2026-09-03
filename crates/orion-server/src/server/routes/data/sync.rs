//! Synchronous data-plane processing: the engine run, response envelope
//! construction, sanitization (G1), and trace persistence + response caching
//! for completed sync requests.

use std::time::Instant;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::channel::guards::{Admission, CacheStoreCtx};
use crate::errors::OrionError;
use crate::metrics;
use crate::queue::trace_record::{CompletedTrace, TracePlan, route_store_completed};
use crate::server::state::AppState;

/// Build an HTTP response from a pre-serialized JSON string, avoiding
/// the double-serialization that `Json<Value>` would incur.
///
/// G9: assembled directly rather than through `Response::builder()`, whose
/// `body()` returns a `Result` that had to be `.expect()`ed — on **every
/// successful data request**, the hottest path in the product, inside a crate
/// that sets `#![warn(clippy::panic)]`. The inputs are a status code and a
/// static header, so there was never a real failure to handle; building the
/// value directly removes the `Result` instead of asserting past it.
fn json_response(status: StatusCode, body: String) -> Response {
    let mut response = Response::new(axum::body::Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    response
}

/// Persist the completed trace through the configured storage mode and
/// fire-and-forget cache the serialized response if a cache context was
/// produced by [`check_response_cache`]. Records the trace-store phase in
/// the per-request profile when one is in scope.
///
/// `cache_body` is the client-facing serialization: cache hits are returned
/// to callers verbatim, so the cached copy must be the sanitized envelope
/// (G1), while the persisted trace keeps `trace.response_json` full detail.
async fn persist_trace_and_cache(
    state: &AppState,
    channel_config: &std::sync::Arc<crate::channel::ChannelRuntimeConfig>,
    plan: &TracePlan,
    trace: &CompletedTrace<'_>,
    cache_body: &str,
    cache_context: &Option<CacheStoreCtx>,
    profile: Option<&std::sync::Arc<crate::engine::profile::ProfileCollector>>,
) {
    // The cache store below is not part of the trace decision: a sampled-out or
    // `errors_only` trace still populates the response cache.
    if plan.persists() {
        let effective_trace = channel_config.trace_storage;
        let trace_store_start = Instant::now();
        route_store_completed(
            &effective_trace,
            &*state.repos.traces,
            &state.trace_persistence_queue,
            trace,
        )
        .await;
        if let Some(p) = profile {
            p.set_trace_store(trace_store_start.elapsed());
        }
    }

    // Fire-and-forget cache store. N2: never cache a response carrying task
    // errors — one transient downstream failure would otherwise be pinned
    // for the full TTL and replayed to every caller, long after the
    // dependency recovered.
    if trace.has_errors {
        tracing::debug!(
            channel = trace.channel,
            "Response has task errors; not caching"
        );
        return;
    }
    if let Some((key, cache, ttl)) = cache_context
        && let Err(e) = cache.set_ex(key, cache_body, *ttl).await
    {
        tracing::debug!(channel = trace.channel, error = %e, "Failed to cache response");
    }
}

/// Build the synchronous response envelope.
///
/// R23: there were four `json!` literals producing one documented shape — the
/// full-detail body, the sanitized public body, the profile variant, and the
/// cached copy — against a `ProcessResponse` mirror whose own doc comment said
/// it is *"never constructed at runtime"*. Four writers and no reader is how a
/// schema drifts from the thing it describes. One writer now, and
/// `the_response_envelope_matches_its_documented_schema` deserializes what it
/// produces back into `ProcessResponse` with `deny_unknown_fields`, so the
/// mirror is pinned to reality rather than to good intentions.
///
/// `request_id` is present only on the sanitized body: the full messages live
/// in the trace, and the id is how a caller correlates the two.
pub(super) fn response_envelope(
    id: &str,
    data: Value,
    errors: Vec<Value>,
    request_id: Option<String>,
) -> Value {
    let mut envelope = json!({
        "id": id,
        "status": "ok",
        "data": data,
        "errors": errors,
    });
    if let Some(request_id) = request_id {
        envelope["request_id"] = json!(request_id);
    }
    envelope
}

/// The reserved path a shaped channel's workflow writes its response control
/// to: `data._orion.response`.
///
/// Under `_orion` because that namespace is already the platform's half of the
/// document (B3 reserved it at the envelope level for `profile`), so a workflow
/// output key cannot collide with it by accident.
const RESPONSE_CONTROL_KEY: &str = "_orion";

/// A shaped response, as drained from `data._orion.response`.
struct ShapedResponse {
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: String,
}

/// The cached form of a shaped response.
///
/// A shaped hit has to replay the status and headers, not just the body — a
/// `201 Created` that came back `200` on the second identical request would be
/// a cache that quietly rewrites the contract. Wrapped under a distinctive key
/// so a plain envelope body left in the cache from before the channel was
/// switched to `shaped` fails to parse and falls back, rather than being
/// misread as a shaped one.
#[derive(serde::Deserialize)]
struct CachedShaped {
    #[serde(rename = "_orion_shaped")]
    shaped: CachedShapedInner,
}

#[derive(serde::Deserialize)]
struct CachedShapedInner {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

/// Write side of [`CachedShaped`], borrowing what it serializes.
///
/// The read side has to own its fields (it is deserialized from a cache entry);
/// the write side has the response in hand and would otherwise clone the whole
/// body and header list to hand them to serde. Same wire shape, enforced by
/// `a_cached_shaped_response_replays_its_status_and_headers` round-tripping one
/// through the other.
#[derive(serde::Serialize)]
struct CachedShapedRef<'a> {
    #[serde(rename = "_orion_shaped")]
    shaped: CachedShapedInnerRef<'a>,
}

#[derive(serde::Serialize)]
struct CachedShapedInnerRef<'a> {
    status: u16,
    headers: &'a [(String, String)],
    body: &'a str,
}

/// Read `data._orion.response` out of the workflow output, if the workflow set
/// one, and turn it into a response.
///
/// Removing the key is part of the job: it is control, not content, and a
/// caller receiving `_orion` back in their own body would be receiving the
/// mechanism rather than the result.
///
/// Every failure here is deliberately *soft* — an absent, malformed, or
/// disallowed field falls back to the platform's own answer rather than 500ing.
/// A shaped channel whose workflow forgot to set a status should serve the
/// workflow's data with a `200`, not fail the request; the alternative turns a
/// Append the platform's own `Set-Cookie` values to a finished response.
///
/// `append`, never `insert`: the workflow may already have set cookies of its
/// own, and a session cookie replaced by the state-clearing one would be a
/// sign-in that completes and immediately signs the user out.
fn append_cookies(mut response: Response, cookies: &[String]) -> Response {
    for value in cookies {
        match axum::http::HeaderValue::from_str(value) {
            Ok(header) => {
                response
                    .headers_mut()
                    .append(axum::http::header::SET_COOKIE, header);
            }
            // Soft, like every other failure on the response path — but this
            // one is worth a warning rather than a debug line, because the
            // cookie it drops is the one retiring a spent OAuth2 state.
            Err(e) => tracing::warn!(error = %e, "Dropping an unencodable platform cookie"),
        }
    }
    response
}

/// The `302` that begins a sign-in, when the workflow ran first.
///
/// Built here as well as in the guard because the two legs answer from
/// different places; the shape is one function's worth of duplication and the
/// alternative is threading a `Response` back through the guard layer, which
/// is deliberately axum-free.
fn oauth_redirect_response(redirect: crate::channel::oauth2_login::Redirect) -> Response {
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::FOUND;
    for (name, value) in [
        (axum::http::header::LOCATION, redirect.location),
        (axum::http::header::SET_COOKIE, redirect.set_cookie),
        (axum::http::header::CACHE_CONTROL, "no-store".to_string()),
    ] {
        if let Ok(header) = axum::http::HeaderValue::from_str(&value) {
            response.headers_mut().insert(name, header);
        }
    }
    response
}

/// Take `data._orion.oauth2.authorize` out of the workflow's output.
///
/// Drained rather than read, and for the same reason `_orion.response` is: it
/// is control, not content, and leaving it in the body would put it in the
/// caller's response and the persisted trace. The empty-namespace cleanup
/// mirrors [`drain_shaped_response`] so a channel using both does not end up
/// with a hollow `_orion`.
fn drain_authorize_contribution(data: &mut Value) -> Option<Value> {
    let obj = data.as_object_mut()?;
    let namespace = obj.get_mut(RESPONSE_CONTROL_KEY)?.as_object_mut()?;
    let oauth2 = namespace.get_mut("oauth2")?.as_object_mut()?;
    let contributed = oauth2.remove("authorize");
    if oauth2.is_empty() {
        namespace.remove("oauth2");
    }
    if namespace.is_empty() {
        obj.remove(RESPONSE_CONTROL_KEY);
    }
    contributed
}

/// One dropped response declaration, in the same `{code, message}` shape a task
/// error takes so a caller reading `errors[]` needs no second vocabulary.
///
/// `path` points at the declaration inside `data._orion.response` rather than
/// at a `task_id`: the workflow succeeded, and what failed is a *statement it
/// made about the response*, which belongs to no single task.
fn drop_entry(code: &str, message: impl Into<String>, path: &str) -> Value {
    json!({ "code": code, "message": message.into(), "path": path })
}

/// Push one header after checking it is representable on the wire.
///
/// Checked here rather than in [`shaped_response`] so that everything a
/// shaped response can refuse is refused in one place, while the envelope that
/// reports the refusal is still being built. A name or value rejected at send
/// time would have nowhere to be recorded.
fn push_header(
    headers: &mut Vec<(String, String)>,
    name: &str,
    value: &str,
    path: &str,
    drops: &mut Vec<Value>,
) {
    if axum::http::HeaderName::try_from(name).is_err()
        || axum::http::HeaderValue::try_from(value).is_err()
    {
        drops.push(drop_entry(
            "RESPONSE_HEADER_DROPPED",
            format!("response header '{name}' is not valid HTTP; it was not set"),
            path,
        ));
        return;
    }
    headers.push((name.to_string(), value.to_string()));
}

/// cosmetic authoring slip into an outage.
///
/// **Soft is not the same as silent** (#312). Every drop below appends an entry
/// to `drops`, which the caller folds into the response envelope's `errors` and
/// counts in `orion_response_drops_total`. Before that, a workflow that
/// declared a session cookie whose `secure` evaluated to a string got a `200`
/// with no `Set-Cookie`, a trace that recorded a clean success, and one line on
/// the server's stdout — which reads to a browser exactly like the browser
/// having refused the cookie. The request still ships, because that part of the
/// posture is right; it just no longer ships claiming nothing happened.
fn drain_shaped_response(
    data: &mut Value,
    cfg: &crate::channel::config::ChannelResponseConfig,
    drops: &mut Vec<Value>,
) -> Option<ShapedResponse> {
    let obj = data.as_object_mut()?;
    let namespace = obj.get_mut(RESPONSE_CONTROL_KEY)?.as_object_mut()?;
    let control = namespace.remove("response")?;
    // Drop `_orion` entirely once it is empty, so the caller's body is not left
    // carrying a hollow namespace.
    let namespace_empty = namespace.is_empty();
    if namespace_empty {
        obj.remove(RESPONSE_CONTROL_KEY);
    }

    let status = control
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|s| u16::try_from(s).ok())
        .and_then(|s| StatusCode::from_u16(s).ok())
        .unwrap_or(StatusCode::OK);

    let mut headers = Vec::new();
    if let Some(map) = control.get("headers").and_then(Value::as_object) {
        for (name, value) in map {
            let lower = name.to_ascii_lowercase();
            let path = format!("_orion.response.headers.{lower}");
            if !cfg.allows_header(&lower) {
                drops.push(drop_entry(
                    "RESPONSE_HEADER_NOT_ALLOWED",
                    format!(
                        "response header '{lower}' is not in this channel's \
                         response.allowed_headers; it was not set"
                    ),
                    &path,
                ));
                continue;
            }
            // An array sets the header once per element. `set-cookie` is the
            // header that needs it — one response routinely issues a session
            // cookie and clears a spent one — but nothing here is specific to
            // it, and a repeated `link` or `vary` is equally legal.
            match value {
                Value::String(s) => push_header(&mut headers, &lower, s, &path, drops),
                Value::Array(items) => {
                    for item in items {
                        match item.as_str() {
                            Some(s) => push_header(&mut headers, &lower, s, &path, drops),
                            // Reported, not skipped silently: the bare
                            // `continue` this replaces is why an array of
                            // cookies used to vanish with no warning and no
                            // error (#298).
                            None => drops.push(drop_entry(
                                "RESPONSE_HEADER_DROPPED",
                                format!(
                                    "an entry of response header '{lower}' is not a \
                                     string; that entry was not set"
                                ),
                                &path,
                            )),
                        }
                    }
                }
                _ => drops.push(drop_entry(
                    "RESPONSE_HEADER_DROPPED",
                    format!(
                        "response header '{lower}' is neither a string nor an array \
                         of strings; it was not set"
                    ),
                    &path,
                )),
            }
        }
    }

    // The declarative cookie form, gated on its own switch rather than on the
    // header allowlist. Appended after `headers` so a channel using both gets
    // the raw `set-cookie` values first, in the order it wrote them.
    if cfg.cookies {
        if let Some(list) = control.get("cookies").and_then(Value::as_array) {
            for (i, spec) in list.iter().enumerate() {
                let path = format!("_orion.response.cookies[{i}]");
                match crate::channel::cookies::format_set_cookie(spec) {
                    Ok(value) => push_header(&mut headers, "set-cookie", &value, &path, drops),
                    // The cookie is still dropped — a malformed `Set-Cookie` is
                    // worse than none — but the response now says so. For an
                    // auth cookie the difference between set and not set is the
                    // whole request, and this failure presents to a browser
                    // exactly like the browser having refused the cookie.
                    Err(reason) => drops.push(drop_entry(
                        "RESPONSE_COOKIE_DROPPED",
                        format!("{reason}; the cookie was not set"),
                        &path,
                    )),
                }
            }
        }
    } else if control.get("cookies").is_some() {
        drops.push(drop_entry(
            "RESPONSE_COOKIES_DISABLED",
            "the workflow declared response cookies but the channel does not enable them \
             (response.cookies); none were set",
            "_orion.response.cookies",
        ));
        tracing::warn!(
            "workflow set response cookies but the channel does not enable \
             them (response.cookies); dropping them"
        );
    }

    // `body_path` names a field of the (already drained) data document to send
    // instead of the whole thing — the usual case, since the workflow's scratch
    // fields are rarely the response. `raw` sends a string field verbatim
    // rather than as a JSON string, which is how a shaped channel returns CSV,
    // XML or plain text.
    // Borrowed, not cloned: the selection is only read — `as_str` for the raw
    // case and `to_string` for the JSON one — so deep-copying the whole output
    // document to serialize it would be pure waste on every shaped response.
    let selected: &Value = match control.get("body_path").and_then(Value::as_str) {
        // A leading `data.` is optional. `message.data()` *is* the `data`
        // document — a mapping writing `data.order` lands at `order` here — but
        // authors write the paths with the prefix everywhere else in a
        // workflow, so accepting both spellings avoids a null body that looks
        // like a bug in the workflow rather than a mismatch of conventions.
        Some(path) => path
            .strip_prefix("data.")
            .unwrap_or(path)
            .split('.')
            .try_fold(&*data, |acc, segment| acc.get(segment))
            .unwrap_or(&Value::Null),
        None => data,
    };
    let raw = control.get("raw").and_then(Value::as_bool).unwrap_or(false);
    let body = match (raw, selected.as_str()) {
        (true, Some(s)) => s.to_string(),
        _ => serde_json::to_string(selected).unwrap_or_else(|_| "null".to_string()),
    };

    Some(ShapedResponse {
        status,
        headers,
        body,
    })
}

/// Build the HTTP response for a shaped channel.
///
/// JSON first, then the workflow's headers over the top, through the shared
/// `apply_headers` — which owns the insert-then-append rule (#298).
fn shaped_response(shaped: ShapedResponse, channel: &str) -> Response {
    let mut response = json_response(shaped.status, shaped.body);
    super::apply_headers(&mut response, &shaped.headers, channel);
    response
}

/// Serialize a response envelope, mapping the (unreachable in practice)
/// serializer failure to the one `Internal` message every envelope site used
/// to spell out for itself.
fn serialize_envelope(envelope: &Value) -> Result<String, OrionError> {
    serde_json::to_string(envelope)
        .map_err(|e| OrionError::internal_from("Failed to serialize response", e))
}

/// Generic replacement for engine error messages on the data plane (G1).
const SANITIZED_ERROR_MESSAGE: &str =
    "Task processing failed; full detail is available in the trace";

/// Map engine `ErrorInfo` entries to the shape the caller sees: code and
/// task_id, with the message decided by `verbose`.
///
/// Sanitized (`verbose = false`) is the production contract: raw messages can
/// embed upstream URLs, connector names and driver errors, which must not reach
/// anonymous data-plane callers, and the persisted trace keeps the originals.
///
/// Verbose is the development one. Outside production the placeholder is pure
/// friction — the author of the workflow is the caller, and every failed
/// iteration otherwise costs a second round trip to the trace API to learn what
/// the first one already knew. `AppConfig::verbose_errors` picks between them
/// and refuses the unsafe combination at startup, so this function does not
/// re-derive the policy; it is handed the answer.
fn sanitize_errors(errors: &[dataflow_rs::ErrorInfo], verbose: bool) -> Vec<Value> {
    errors
        .iter()
        .map(|e| {
            let mut entry = json!({
                "code": e.code,
                "message": if verbose { e.message.as_str() } else { SANITIZED_ERROR_MESSAGE },
            });
            if let Some(ref task_id) = e.task_id {
                entry["task_id"] = json!(task_id);
            }
            entry
        })
        .collect()
}

/// Serve a response-cache hit.
///
/// For an envelope channel the cached string is already the client-facing
/// serialization and goes out verbatim with no work at all — which is why the
/// shaped branch is gated on the channel's own config rather than attempted
/// speculatively. Parsing first and falling back would put a full JSON parse of
/// every cached body on the one path built to do nothing, and throw it away for
/// every channel that never opted in.
///
/// For a shaped channel the entry is a [`CachedShaped`] carrying the status and
/// headers alongside the body, because replaying a `201 Created` as a bare
/// `200` would let the cache silently rewrite the channel's contract on the
/// second identical request. An entry left over from before the channel was
/// switched to `shaped` does not parse, and is served as a plain body.
pub(super) fn cached_response(body: String, shaped: bool, channel: &str) -> Response {
    if shaped && let Ok(cached) = serde_json::from_str::<CachedShaped>(&body) {
        return shaped_response(
            ShapedResponse {
                status: StatusCode::from_u16(cached.shaped.status).unwrap_or(StatusCode::OK),
                headers: cached.shaped.headers,
                body: cached.shaped.body,
            },
            channel,
        );
    }
    json_response(StatusCode::OK, body)
}

/// Core sync processing logic shared between simple HTTP and REST routes.
/// Every ingress guard — origin allow-list, rate limit, validation, dedup,
/// response-cache lookup, backpressure — has already run in the caller's
/// `apply_guards` call, before the sync/async split; what arrives here is the
/// resulting [`Admission`].
///
/// Returns a pre-serialized `Response` so the JSON is serialized exactly once.
pub(super) async fn process_sync_for_channel(
    state: &AppState,
    channel: &str,
    data: Value,
    metadata: Value,
    channel_config: std::sync::Arc<crate::channel::ChannelRuntimeConfig>,
    profile_requested: bool,
    admission: Admission,
) -> Result<Response, OrionError> {
    let profile = profile_requested.then(crate::engine::profile::ProfileCollector::new);
    let Admission {
        backpressure_permit: _backpressure_permit,
        cache_store: cache_context,
        timeout_ms,
        // Nothing redelivers an HTTP request, so the claim is left standing
        // for the rest of the window: a replay of the same idempotency key is
        // a second delivery and belongs in the `409` branch, whatever this
        // one's outcome turns out to be.
        dedup_claim: _dedup_claim,
        // Already merged into `metadata` by the caller (data/mod.rs).
        auth_claims: _,
        oauth,
    } = admission;
    // `response_cookies` is appended to whatever the workflow answers,
    // including a failure: the OAuth2 state cookie has been spent by the time
    // the workflow runs and must not survive the response that spent it.
    //
    // That is enforced once, below, rather than at each place a response is
    // built — which is what this comment used to claim and the code did not
    // do. `append_cookies` sat on the three success arms only, so a callback
    // whose workflow timed out, hit an engine error, or overran
    // `max_result_size_bytes` answered 504/500 and left the spent cookie in
    // the browser until its own `exp`. Clearing that cookie is the *only*
    // single-use enforcement Orion performs (`oauth2_login.rs`), so the
    // outcomes where it was skipped are exactly the ones a replay would follow.
    let (response_cookies, oauth_authorize, oauth_return_to) = match oauth {
        Some(o) => (o.response_cookies, o.authorize, o.return_to),
        None => (Vec::new(), None, None),
    };

    let result: Result<Response, OrionError> = async {
        // O1: `channel` is safe to use as a metric label below because the caller
        // has already resolved it against the registry — an unknown name is a 404
        // before it reaches here, so callers cannot grow Prometheus label
        // cardinality by inventing path segments.

        let start = Instant::now();
        let sticky_identity = crate::engine::utils::rollout_identity(
            &metadata,
            &state.config.engine.rollout_sticky_header,
        );

        // A2: when the channel opted in via `config.tracing.task_details = true`,
        // use the with-trace engine entry point so per-step inputs/outputs are
        // captured for persistence.
        // Bounded by the same budget the persisted row is capped at, so the
        // post-hoc `serialize_task_trace_capped` check becomes a backstop rather
        // than the only defence.
        let capture =
            channel_config
                .trace_storage
                .task_details
                .then(|| crate::engine::TraceCapture {
                    max_snapshot_bytes: state.config.trace_queue.max_result_size_bytes,
                });

        // The shared post-admission step: message build, engine snapshot, the
        // deadline arm, the `has_errors` rule and the two metrics. What stays here
        // is response shaping and persistence.
        let execution = crate::engine::execute_admitted(
            &state.engine,
            channel,
            &data,
            &metadata,
            crate::engine::ExecOpts {
                timeout_ms,
                capture,
                // The routing bucket rides beside the context rather than inside
                // `data`, so it never reaches the caller and never needs stripping
                // back out.
                routing_bucket: Some(crate::engine::utils::rollout_bucket_for_identity(
                    sticky_identity,
                )),
                profile: profile.as_ref(),
            },
        )
        .await;
        if let Some(ref p) = profile {
            p.set_workflow_total(execution.duration);
        }
        let crate::engine::Execution {
            message,
            task_trace,
            outcome,
            duration: engine_duration,
        } = execution;

        // One derivation of the label, shared with every other transport. The
        // change this made here: a run that finished with task errors used to be
        // counted `ok`, so a channel failing every request reported a 100% success
        // rate while the Kafka and async paths counted the same runs as `error`.
        metrics::record_message(channel, outcome.status_label());
        metrics::record_message_duration(channel, engine_duration.as_secs_f64());

        match outcome {
            crate::engine::RunOutcome::Timeout(ms) => {
                metrics::record_error("timeout");
                Err(OrionError::Timeout {
                    channel: channel.to_string(),
                    timeout_ms: ms,
                })
            }
            crate::engine::RunOutcome::EngineError(e) => {
                metrics::record_error("engine");
                Err(OrionError::Engine(e))
            }
            // A synchronous caller is handed its task errors in the envelope with
            // a 200 — the response carries the whole story, and a workflow that
            // partially succeeded still has data worth returning. Only the metric
            // label changed with the shared step: `orion_messages_total` counts
            // this as `error` now, as the Kafka and async paths always did.
            crate::engine::RunOutcome::Ok | crate::engine::RunOutcome::WorkflowErrors(_) => {
                // The *request's* duration — guards, execution, and the response
                // build below. `execute_admitted` already recorded the engine
                // call's own latency on `orion_message_duration_seconds`; this one
                // is what the trace row reports.
                let duration = start.elapsed();
                let duration_ms = duration.as_secs_f64() * 1000.0;

                // Shaped channels drain their response control out of the workflow
                // output *before* the envelope is built, so `_orion.response`
                // reaches neither the caller's body nor the persisted trace as
                // content. A channel left on the default `envelope` mode never
                // looks, which is what keeps an incidental `_orion` key in some
                // workflow's data inert.
                let mut data_out: Value = message.data().into();
                // #312: what the workflow declared and did not get. Empty for
                // every channel on the default `envelope` mode, which never looks
                // at `_orion.response` at all.
                let mut response_drops: Vec<Value> = Vec::new();
                let shaped = channel_config
                    .parsed_config
                    .response
                    .as_ref()
                    .filter(|cfg| cfg.is_shaped())
                    .and_then(|cfg| drain_shaped_response(&mut data_out, cfg, &mut response_drops));
                for entry in &response_drops {
                    let field = |k: &str| {
                        entry
                            .get(k)
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                    };
                    let (kind, path, message) = (field("code"), field("path"), field("message"));
                    tracing::warn!(
                        channel = channel,
                        code = %kind,
                        path = %path,
                        reason = %message,
                        "response declaration dropped"
                    );
                    metrics::record_response_drop(channel, kind);
                }

                // #307, `run_workflow_on_authorize`: the redirect is built after
                // the workflow so the workflow can contribute to it, and the
                // contribution is drained here the way `_orion.response` is —
                // out of the body, before the envelope and the trace are built.
                //
                // A workflow that shaped its own response wins: that is how it
                // refuses a sign-in (an unknown tenant, a maintenance window)
                // without Orion needing a vocabulary for refusal. Only when it
                // shaped nothing does the sign-in proceed.
                //
                // Decided here, where the workflow's contribution is, but
                // *returned* below with the shaped response — because this block
                // used to `return`, and everything that writes the trace is below
                // it. A channel with `run_workflow_on_authorize` therefore wrote
                // no trace row under any `trace_storage.mode`, `sync` included,
                // even though the workflow really ran: it can write rows, call
                // connectors and emit task errors, and a failing sign-in was
                // invisible to `GET /api/v1/data/traces`. The `record_response_drop`
                // entries logged just above went with it, having no envelope to
                // ride on.
                let mut authorize_outcome: Option<Result<Response, OrionError>> = None;
                if let Some(login) = oauth_authorize {
                    if shaped.is_none() {
                        let contributed = drain_authorize_contribution(&mut data_out);
                        authorize_outcome = Some(
                            match login.begin(contributed.as_ref(), oauth_return_to.as_deref()) {
                                Ok(redirect) => {
                                    crate::metrics::record_oauth_login(
                                        channel,
                                        crate::channel::OAuthLeg::Authorize,
                                        "ok",
                                    );
                                    Ok(oauth_redirect_response(redirect))
                                }
                                Err(e) => {
                                    tracing::error!(
                                        channel = channel,
                                        error = %e,
                                        "Could not build the OAuth2 authorize redirect"
                                    );
                                    Err(OrionError::internal("could not begin the sign-in"))
                                }
                            },
                        );
                    } else {
                        tracing::debug!(
                            channel = channel,
                            "Workflow shaped its own response on the authorize leg; not redirecting"
                        );
                    }
                }

                let mut response = response_envelope(
                    message.id(),
                    data_out,
                    message
                        .errors()
                        .iter()
                        .filter_map(|e| serde_json::to_value(e).ok())
                        .chain(response_drops.iter().cloned())
                        .collect(),
                    None,
                );

                // Serialize the full-detail envelope exactly once — reused for the
                // size check, trace storage, and (on the error-free hot path) the
                // cache and HTTP body with no re-serialization by Axum.
                let response_json = serialize_envelope(&response)?;

                // G1: when workflow errors are present, the client-facing body
                // (and the cached copy) carry sanitized entries plus a
                // correlation id; the persisted trace keeps the full detail.
                //
                // The two envelopes differ only in `errors` and `request_id`, so
                // the public one is made by overwriting those two keys in place —
                // rebuilding it would deep-convert the whole workflow output a
                // second time.
                // A dropped declaration counts. Not for the message metric —
                // `record_message` above reads the engine outcome, and the workflow
                // did succeed — but for the three things this flag actually gates:
                // the trace is *stored* under an errors-only policy (otherwise the
                // one trace carrying the evidence is the one sampled away), the
                // trace row is findable by an operator searching for failures, and
                // an incomplete response is not written to the response cache.
                let has_errors = message.has_errors() || !response_drops.is_empty();
                let public_json = if has_errors {
                    response["errors"] = Value::Array(sanitize_errors(
                        message.errors(),
                        state.config.verbose_errors(),
                    ));
                    response["request_id"] =
                        json!(crate::request_context::request_id().unwrap_or_default());
                    Some(serialize_envelope(&response)?)
                } else {
                    None
                };

                let max_result_size = state.config.trace_queue.max_result_size_bytes;
                if max_result_size > 0 && response_json.len() > max_result_size {
                    metrics::record_error("result_size_exceeded");
                    // The message names the knob (G15): this error surfaces in
                    // the response and the log, and "a size cap fired" without
                    // saying which sends the operator to the connector-level
                    // caps, which never produce this error.
                    return Err(OrionError::ResponseTooLarge(format!(
                        "Result size {} bytes exceeds trace_queue.max_result_size_bytes ({} bytes)",
                        response_json.len(),
                        max_result_size
                    )));
                }

                // Decided before the two strings below are built: under `off`,
                // `errors_only` on a clean run, or any sample_rate < 1, both are
                // pure waste — and a full copy of the request payload is not a
                // cheap kind of waste on this path.
                let plan = TracePlan::decide(&channel_config.trace_storage, has_errors);
                let (input_json, task_trace_json) = if plan.persists() {
                    (
                        serde_json::to_string(&data).ok(),
                        crate::engine::utils::serialize_task_trace_capped(
                            task_trace.as_ref(),
                            max_result_size,
                            channel,
                        ),
                    )
                } else {
                    (None, None)
                };
                // What a cache hit will replay. A shaped channel caches its status
                // and headers alongside the body, so the second identical request
                // is answered with the same contract as the first; an envelope
                // channel caches the client-facing string exactly as before.
                //
                // Built only when something will actually store it. `persist_trace_and_cache`
                // skips the write when there is no cache context or the run carried
                // errors, and a channel with no response cache is the common case —
                // so doing this unconditionally spent a full copy of the body plus a
                // serialize pass, per request, on nothing. Serialized from borrows so
                // the copy is the one the cache needs rather than a second one.
                // A response that sets a cookie is never cached (#298). The cache
                // key is built from the method, path params, query and payload —
                // never from who is calling — so replaying a stored `set-cookie`
                // hands the first caller's session to everyone who repeats their
                // request for the TTL. The cookie is *output*, so folding it into
                // the key would not help either.
                //
                // The whole store is suppressed rather than just the shaped
                // serialization: falling through to the envelope body would cache
                // a document a shaped channel cannot replay, quietly answering the
                // second request with a different contract.
                // `response_cookies` counts for the same reason the workflow's own
                // do, and it is the sharper case: the OAuth2 callback's cookie
                // retires a spent state, so replaying it would hand the next
                // caller a cleared cookie for a sign-in that was not theirs — and
                // the body it rides on is one user's session.
                // The authorize redirect carries the freshly minted state cookie in
                // its own `Set-Cookie`, so it counts here too — belt to the brace
                // of the create-time refusal of `cache` alongside `oauth2_login`.
                // Routing the redirect through this tail is what first made it
                // reachable by the caching branch at all.
                let sets_cookie = !response_cookies.is_empty()
                    || authorize_outcome.is_some()
                    || shaped
                        .as_ref()
                        .is_some_and(|s| s.headers.iter().any(|(n, _)| n == "set-cookie"));
                if sets_cookie {
                    tracing::debug!(
                        channel = channel,
                        "Response sets a cookie; not caching (it is per-caller)"
                    );
                }
                let cache_context = if sets_cookie { None } else { cache_context };

                let will_cache = cache_context.is_some() && !has_errors;
                let shaped_cache_json = shaped.as_ref().filter(|_| will_cache).and_then(|s| {
                    serde_json::to_string(&CachedShapedRef {
                        shaped: CachedShapedInnerRef {
                            status: s.status.as_u16(),
                            headers: &s.headers,
                            body: &s.body,
                        },
                    })
                    .ok()
                });
                let cache_body = shaped_cache_json
                    .as_deref()
                    .or(public_json.as_deref())
                    .unwrap_or(&response_json);

                persist_trace_and_cache(
                    state,
                    &channel_config,
                    &plan,
                    &CompletedTrace {
                        mode: "sync",
                        channel,
                        channel_id: Some(channel_config.channel.channel_id.as_str()),
                        input_json: input_json.as_deref(),
                        response_json: &response_json,
                        duration_ms,
                        has_errors,
                        task_trace_json: task_trace_json.as_deref(),
                    },
                    cache_body,
                    &cache_context,
                    profile.as_ref(),
                )
                .await;

                // The authorize-leg redirect, now that the trace is persisted.
                // Mutually exclusive with `shaped` — it is built only when the
                // workflow shaped nothing — so the order of these two is a
                // reading preference, not a rule.
                if let Some(outcome) = authorize_outcome {
                    return outcome;
                }

                // A shaped channel's body is whatever the workflow chose, so there
                // is no envelope to append `_orion.profile` to. Profiling still
                // records its timings (they reach the trace and the metrics); only
                // the response-body copy is envelope-only.
                if let Some(shaped) = shaped {
                    return Ok(shaped_response(shaped, channel));
                }

                // Profile mode: rebuild the response with `_orion.profile`
                // appended and re-serialize. Only paid when profiling is on.
                //
                // B3 shape lock: the debug surface lives under a single
                // top-level `_orion` namespace so future debug fields (e.g.
                // `_orion.task_trace`) can be added without colliding with
                // workflow-level output keys that callers control.
                if let Some(ref p) = profile {
                    let mut response_with_profile = response;
                    response_with_profile["_orion"] = json!({ "profile": p.to_json() });
                    return Ok(json_response(
                        StatusCode::OK,
                        serialize_envelope(&response_with_profile)?,
                    ));
                }

                Ok(json_response(
                    StatusCode::OK,
                    public_json.unwrap_or(response_json),
                ))
            }
        }
    }
    .await;

    // The one place the platform's cookies reach the wire, so no outcome can
    // be added later that quietly skips them. An error is materialised into
    // its response here rather than propagated, because `IntoResponse` runs
    // above this frame and has nothing to attach; the body is byte-identical
    // either way, since it is the same `OrionError::into_response`. With no
    // cookies to carry — every channel that does not declare `oauth2_login` —
    // the error propagates exactly as before.
    match result {
        Ok(response) => Ok(append_cookies(response, &response_cookies)),
        Err(e) if !response_cookies.is_empty() => {
            Ok(append_cookies(e.into_response(), &response_cookies))
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::routes::data::ProcessResponse;

    /// R23: `ProcessResponse` is a never-constructed mirror that exists only so
    /// the OpenAPI document describes the real shape — and the real shape was
    /// built by four separate `json!` literals, one of which (the plain success
    /// body) omits `request_id` while another adds it. Nothing checked the two
    /// against each other, and they had already drifted.
    ///
    /// Deserializing with `deny_unknown_fields` catches drift in both
    /// directions: a key the envelope emits and the schema lacks fails here,
    /// and a required schema field the envelope omits fails here too.
    #[test]
    fn the_response_envelope_matches_its_documented_schema() {
        let shapes = [
            // Clean run: no request_id.
            response_envelope("msg-1", json!({"ok": true}), vec![], None),
            // Sanitized run: request_id present, errors non-empty.
            response_envelope(
                "msg-2",
                json!({"partial": 1}),
                vec![json!({"code": "TASK_FAILED", "message": SANITIZED_ERROR_MESSAGE})],
                Some("req-abc".to_string()),
            ),
            // Sanitized run carrying a task_id.
            response_envelope(
                "msg-3",
                Value::Null,
                vec![json!({
                    "code": "TASK_FAILED",
                    "message": SANITIZED_ERROR_MESSAGE,
                    "task_id": "t1",
                })],
                Some("req-def".to_string()),
            ),
        ];

        for shape in shapes {
            let parsed = serde_json::from_value::<ProcessResponse>(shape.clone());
            assert!(
                parsed.is_ok(),
                "the documented schema does not describe what we send: {shape} — {:?}",
                parsed.err()
            );
        }
    }

    /// The profile variant is the same envelope plus the `_orion` namespace
    /// (B3), built by the one site that appends it.
    #[test]
    fn the_profile_variant_also_matches_the_schema() {
        let mut shape = response_envelope("msg-4", json!({}), vec![], None);
        shape["_orion"] = json!({ "profile": {"version": 2} });
        let parsed = serde_json::from_value::<ProcessResponse>(shape.clone());
        assert!(
            parsed.is_ok(),
            "profile variant does not match the schema: {shape} — {:?}",
            parsed.err()
        );
    }

    /// `ErrorInfo` is `#[non_exhaustive]` as of dataflow-rs 3.1, so the builder
    /// is the only cross-crate construction path.
    fn info(code: &str, message: &str, task_id: Option<&str>) -> dataflow_rs::ErrorInfo {
        let mut b = dataflow_rs::ErrorInfo::builder(code, message);
        if let Some(task_id) = task_id {
            b = b.task_id(task_id);
        }
        b.build()
    }

    fn sample_errors() -> Vec<dataflow_rs::ErrorInfo> {
        vec![
            info(
                "TASK_FAILED",
                "raw upstream detail that must not leak",
                Some("t1"),
            ),
            info("OTHER", "another", None),
        ]
    }

    /// `sanitize_errors` output is what the envelope carries, so it must satisfy
    /// the `ProcessTaskError` half of the schema — in *both* verbosity modes,
    /// since both reach the wire.
    #[test]
    fn error_entries_match_their_documented_schema_either_way() {
        use crate::server::routes::data::ProcessTaskError;
        for verbose in [false, true] {
            for e in &sanitize_errors(&sample_errors(), verbose) {
                let parsed = serde_json::from_value::<ProcessTaskError>(e.clone());
                assert!(
                    parsed.is_ok(),
                    "error entry (verbose={verbose}) does not match the schema: {e} — {:?}",
                    parsed.err()
                );
            }
        }
    }

    /// The production contract: no engine text reaches the caller.
    #[test]
    fn sanitized_errors_replace_every_message() {
        let errors = sanitize_errors(&sample_errors(), false);
        assert!(
            errors
                .iter()
                .all(|e| e["message"] == SANITIZED_ERROR_MESSAGE),
            "{errors:?}"
        );
    }

    /// The development contract: the engine's own text is what the author needs.
    #[test]
    fn verbose_errors_keep_the_engine_message() {
        let errors = sanitize_errors(&sample_errors(), true);
        assert_eq!(
            errors[0]["message"],
            "raw upstream detail that must not leak"
        );
        assert_eq!(errors[1]["message"], "another");
        // Code and task_id are unchanged by verbosity — only `message` moves.
        assert_eq!(errors[0]["code"], "TASK_FAILED");
        assert_eq!(errors[0]["task_id"], "t1");
    }
}
