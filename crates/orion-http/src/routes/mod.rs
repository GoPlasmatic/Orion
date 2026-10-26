pub mod admin;
pub mod data;
pub mod openapi;
pub mod response_helpers;
pub mod v2_admin;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{any, get};
use axum::{Json, Router};
use serde_json::json;
use utoipa::OpenApi;

use crate::runtime::state::AppState;

/// What the main listener's router should contain, resolved from config by
/// [`crate::build_router`].
///
/// A struct rather than three positional scalars: two of the three are bare
/// `bool`s with the same type, so a transposition at the call site would
/// compile and silently unregister the wrong surface.
#[derive(Debug, Clone)]
pub struct RouteOptions {
    /// Bounds admin request bodies independently of the data plane —
    /// see [`admin::admin_routes`].
    pub max_admin_body_size: usize,
    /// The admin body limit for the plugin routes alone: base64 of the
    /// largest component plus room for the manifest and import framing.
    pub plugin_body_size: usize,
    /// Gates `/docs` and `/api/v2/openapi.json` (resolved by
    /// [`crate::config::AppConfig::docs_enabled`]): the spec publishes the
    /// whole admin API surface anonymously, so production deployments keep it
    /// off by default. When disabled the routes are simply not registered —
    /// both paths fall through to the 404 fallback rather than answering 401,
    /// so their very existence is not advertised.
    pub docs_enabled: bool,
    /// Gates `/metrics` on **this** listener. False both when
    /// `metrics.enabled = false` and when `metrics.bind_addr` has moved the
    /// endpoint to its own listener; in either case the path 404s here rather
    /// than answering 200 with an empty body.
    pub metrics_enabled: bool,
    /// Extra prefixes the data plane is served at, beyond `/api/v2/data`
    /// (`server.data_mounts`). Empty is the default and registers nothing.
    pub data_mounts: Vec<String>,
}

/// The main listener's router: four groups, each carrying its own platform
/// rate limit and, for the admin plane, admin authentication.
///
/// | Group | Routes | Limit | Admin auth |
/// |---|---|---|---|
/// | admin | `/api/v2/admin/**` and its fallback | `admin_rps` | yes |
/// | single-trace read | `/api/v2/admin/traces/{id}` | `admin_rps` | no — it authenticates itself |
/// | data | `/api/v2/data/**`, every `server.data_mounts` prefix | `data_rps` | no |
/// | operational | `/health`, `/healthz`, `/readyz`, `/metrics`, the docs, the 404/405 fallbacks | default | `/metrics` only |
///
/// Each layer is attached to its group's router, so what guards a request is
/// decided by the route it matched, never by re-reading its path. Rate
/// limiting is outer to admin auth, so a rejected key has been counted by the
/// limiter: the reverse left credential guessing unthrottled, because auth
/// answers 401 without calling the next layer.
pub fn api_routes(state: &AppState, options: RouteOptions) -> Router<AppState> {
    let RouteOptions {
        max_admin_body_size,
        plugin_body_size,
        docs_enabled,
        metrics_enabled,
        data_mounts,
    } = options;
    let auth = state.config.admin_auth.enabled;
    let admin_limit = crate::rate_limit::GroupLimit::admin(state);
    let data_limit = crate::rate_limit::GroupLimit::data(state);
    let operational_limit = crate::rate_limit::GroupLimit::operational(state);

    // The admin plane, with its own catch-all inside the guard. A nested
    // router's fallback is not a route: axum flattens the nest, so under a
    // root data mount an unregistered admin path matched the mount's
    // `/{*path}` and reached the unauthenticated data plane, where a channel
    // could claim it. `/api/v2/admin` and `/api/v2/admin/{*rest}` as real
    // routes are more specific than any mount, so every admin path is
    // answered here — 401 to an anonymous caller, 404 to an authenticated
    // one — and never by a channel.
    let admin = admin::admin_routes(max_admin_body_size)
        .merge(v2_admin::v2_admin_routes(
            max_admin_body_size,
            plugin_body_size.max(max_admin_body_size),
        ))
        .route("/", any(not_found))
        .route("/{*rest}", any(not_found))
        .method_not_allowed_fallback(method_not_allowed);
    let admin = with_admin_auth(admin, state, auth);
    let admin = with_limit(admin, admin_limit.clone());

    // The single-trace read authenticates itself (admin credential, or the
    // per-submission capability token from the async 202), so it is outside
    // the admin guard and inside the admin budget.
    let single_trace = Router::new().route(
        crate::admin_auth::SINGLE_TRACE_PATH,
        get(data::traces::get_trace).fallback(method_not_allowed),
    );
    let single_trace = with_limit(single_trace, admin_limit);

    // The data plane, at `/api/v2/data` and at every extra mount
    // (`server.data_mounts`). A mount is `.route`, never `.nest` or
    // `.fallback`:
    //
    // - `.nest("/", …)` panics outright ("Nesting at the root is no longer
    //   supported"), and a nested handler would see the path with the prefix
    //   stripped — whereas `dynamic_handler` matches `route_pattern`s
    //   PREFIX-FREE, so it needs the full path a root-mounted route gives it.
    // - `.fallback` yields no `MatchedPath`, so the metrics middleware would
    //   fall back to the raw URI and put every legacy URL into an unbounded
    //   Prometheus `path` label. `.route("/{*path}")` yields the constant
    //   label `/{*path}`.
    //
    // Static platform routes win over a catch-all in matchit regardless of
    // registration order, so `/health` and friends keep their own handlers
    // even under a `"/"` mount.
    let data_plane = data_mounts.iter().fold(
        Router::new().nest("/api/v2/data", data::data_routes()),
        |router, mount| {
            let pattern = if mount == "/" {
                "/{*path}".to_string()
            } else {
                format!("{mount}/{{*path}}")
            };
            router.route(&pattern, any(data::dynamic_handler))
        },
    );
    let data_plane = with_limit(data_plane, data_limit);

    // The operational routes and the platform's 404/405 fallbacks.
    let operational = Router::new()
        .route("/health", get(health_check))
        .route("/healthz", get(liveness_check))
        .route("/readyz", get(readiness_check));
    // O12: registered only when this listener actually serves metrics.
    // Unconditional registration meant `metrics.enabled = false` answered 200
    // with an empty body from an orphan recorder — a scrape target that looked
    // healthy and reported nothing, forever. Admin-guarded where it is served.
    let operational = if metrics_enabled {
        operational.merge(with_admin_auth(
            Router::new().route(crate::admin_auth::METRICS_PATH, get(metrics_endpoint)),
            state,
            auth,
        ))
    } else {
        operational
    };
    let operational = if docs_enabled {
        operational.merge(
            utoipa_swagger_ui::SwaggerUi::new("/docs")
                .url("/api/v2/openapi.json", openapi::ApiDoc::openapi()),
        )
    } else {
        operational
    };
    // R9: without these, an unmatched path and every method mismatch
    // returned a zero-length body, violating the documented contract that
    // "every non-2xx response uses the ErrorResponse envelope". Clients that
    // parse the body on error saw a JSON decode failure instead of an error
    // code. Registered inside the request-id scope (see the layer order in
    // `build_router`) so both carry `x-request-id`.
    let operational = operational
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed);
    let operational = with_limit(operational, operational_limit);

    operational
        .nest("/api/v2/admin", admin)
        .merge(single_trace)
        .merge(data_plane)
}

/// `router` behind admin authentication, when it is enabled.
fn with_admin_auth(router: Router<AppState>, state: &AppState, enabled: bool) -> Router<AppState> {
    if enabled {
        router.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::admin_auth::admin_auth_middleware,
        ))
    } else {
        router
    }
}

/// `router` metered against `limit`, when `[rate_limit]` is on.
fn with_limit(
    router: Router<AppState>,
    limit: Option<crate::rate_limit::GroupLimit>,
) -> Router<AppState> {
    match limit {
        Some(limit) => router.layer(axum::middleware::from_fn_with_state(
            limit,
            crate::rate_limit::rate_limit_middleware,
        )),
        None => router,
    }
}

async fn not_found() -> crate::errors::OrionError {
    crate::errors::OrionError::not_found("No route matches this path".to_string())
}

async fn method_not_allowed() -> crate::errors::OrionError {
    crate::errors::OrionError::method_not_allowed(
        "The HTTP method is not allowed for this path".to_string(),
    )
}

#[utoipa::path(
    get,
    path = "/health",
    tag = "Operational",
    description = "\
Detailed health report. Always reachable, but when `admin_auth.enabled` is \
true the topology detail (`git_hash`, `build_timestamp`, `workflows_loaded`, \
the circuit-breaker map, connector load failures and quarantined channels — \
names and failure reasons) is included only for requests presenting a valid admin credential; \
anonymous callers get status, version, uptime and coarse per-component states. \
Probes should use `/healthz` and `/readyz`.",
    responses(
        (status = 200, description = "Service healthy, or degraded with the database reachable", body = orion_api::HealthResponse),
        (status = 503, description = "The database is unreachable; same body shape", body = orion_api::HealthResponse),
    )
)]
#[tracing::instrument(skip(state, headers))]
pub(crate) async fn health_check(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let uptime = chrono::Utc::now() - state.start_time;

    // Check database connectivity
    let db_healthy = state.ping_db().await.is_ok();

    // One generation for the whole report: the workflow count and the
    // quarantine list below come from the same build, where they used to be
    // read from two independently swapped values and could describe different
    // ones.
    let generation = state.runtime.load();
    let workflows_loaded = workflows_loaded(&generation);

    // Collect circuit breaker states
    let cb_states = state.connector_registry.circuit_breaker_states().await;

    // Every load issue, from the one collector a deployment apply and
    // `GET /engine/status` answer from too, so the three surfaces cannot
    // disagree about what is quarantined.
    //
    // F16: enabled connectors that failed to load are absent from the
    // registry, so every workflow using one fails at request time. F35:
    // channels that failed to load are quarantined — refused at every
    // ingress — while the rest of the instance serves normally. A plugin that
    // did not load on this node — no artifact, a component that will not
    // compile, a failed self-test, or the sandbox being off while an active
    // row exists — quarantines the workflows naming its functions the same
    // way. Each list here is the only signal for its failure.
    let issues = crate::runtime::load_issues::collect(&generation, &state.connector_registry).await;
    let connector_issues = &issues.connectors;
    let quarantined_channels = &issues.channels;
    let plugin_issues = &issues.plugins;

    // Dead scheduling is otherwise silent: a node whose reconciler errors
    // on every pass is alive, restarts nothing, and simply stops firing. The
    // supervisor cannot see it, because nothing crashed.
    let cron_state = cron_component(&state, &generation);

    // Degraded, not unhealthy: the rest of the instance still serves traffic,
    // and returning 503 would take a node out of its load balancer over a
    // connector or channel that may be used by nothing currently in flight.
    let (tasks_state, task_reports) = tasks_component(&state);

    // A deployment build that failed leaves this node serving the previous
    // generation: correct, but not what is committed. Nothing else reports
    // that — the reconciler retrying it has no caller to tell.
    let reload_degraded = state
        .reload_degraded
        .load(std::sync::atomic::Ordering::Acquire);

    let overall_healthy = db_healthy;
    let fully_loaded = connector_issues.is_empty()
        && quarantined_channels.is_empty()
        && cron_state != Some("degraded")
        && tasks_state == "ok"
        && !reload_degraded;
    let status_str = if overall_healthy && fully_loaded {
        "ok"
    } else {
        "degraded"
    };
    let http_status = if overall_healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    // O9: names, failure reasons, build provenance and the breaker map are
    // internal topology. They are served only when the caller could read the
    // same detail from the admin plane anyway: either admin auth is disabled
    // (dev — the whole admin API is open) or a valid admin key is presented.
    // The coarse per-component states stay public so a monitor can see
    // *that* something is degraded without learning *what*.
    let auth_cfg = &state.config.admin_auth;
    let show_detail =
        !auth_cfg.enabled || crate::admin_auth::headers_present_valid_key(&headers, auth_cfg);

    let ok_or = |fine: bool, otherwise: &str| if fine { "ok" } else { otherwise }.to_string();
    let mut components = std::collections::BTreeMap::from([
        ("database".to_string(), ok_or(db_healthy, "error")),
        // Constant by construction — see `workflows_loaded` (O16).
        ("engine".to_string(), "ok".to_string()),
        (
            "connectors".to_string(),
            ok_or(connector_issues.is_empty(), "degraded"),
        ),
        (
            "channels".to_string(),
            ok_or(quarantined_channels.is_empty(), "degraded"),
        ),
        ("background_tasks".to_string(), tasks_state.to_string()),
        // `degraded`, not `error`, and absent from `/readyz`: this node is
        // serving, just not the newest revision. Taking it out of rotation
        // would trade a stale-config problem for an availability one.
        (
            "engine_reload".to_string(),
            ok_or(!reload_degraded, "degraded"),
        ),
        // `disabled` is a state, not a fault: a node without the sandbox
        // serves everything else. It degrades only when an active plugin
        // row exists that this node could not load.
        (
            "plugins".to_string(),
            if state.plugins.is_none() && plugin_issues.is_empty() {
                "disabled".to_string()
            } else {
                ok_or(plugin_issues.is_empty(), "degraded")
            },
        ),
    ]);
    if let Some(cron) = cron_state {
        components.insert("cron".to_string(), cron.to_string());
    }
    let mut body = orion_api::HealthResponse {
        status: status_str.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_seconds: uptime.num_seconds(),
        components,
        ..Default::default()
    };
    if show_detail {
        let build = orion_common::version::build();
        body.git_hash = Some(build.git_hash.to_string());
        body.build_timestamp = Some(build.timestamp.to_string());
        body.workflows_loaded = Some(workflows_loaded);
        body.connectors = Some(json!({
            // F21: node-local, like the admin endpoint's copy.
            "circuit_breaker_scope": "node",
            "circuit_breakers": cb_states,
            "failed_to_load": connector_issues,
        }));
        body.channels = Some(json!({
            "quarantined": quarantined_channels,
        }));
        body.plugins = Some(json!({
            "loaded": generation.plugins.plugins.iter().map(|p| json!({
                "plugin": p.id,
                "version": p.version,
                "digest": p.digest,
                "functions": p.functions,
                "compile_ms": p.compile_ms,
            })).collect::<Vec<_>>(),
            "failed_to_load": plugin_issues,
        }));
        // O9: task names are internal topology, so the per-task breakdown
        // rides with the other admin-only detail. The coarse
        // `components.background_tasks` above is what a monitor keys on.
        if cron_state.is_some() {
            body.cron = Some(json!({
                "last_reconcile_at": state.cron_status.last_reconcile_ok(),
                "reconcile_age_secs": state.cron_status.reconcile_age_secs(),
                "oldest_pending_age_secs": state.cron_status.oldest_pending_secs(),
                "lease_renewal_failures": state.cron_status.renewal_failures(),
                "scheduled_channels": generation.channels.cron_descriptors().len(),
            }));
        }
        body.background_tasks = Some(json!(
            task_reports
                .iter()
                .map(|r| json!({
                    "name": r.name,
                    "state": r.state.as_str(),
                    "restarts": r.restarts,
                    "required": r.criticality == crate::runtime::Criticality::Required,
                }))
                .collect::<Vec<_>>()
        ));
    }

    (http_status, Json(body))
}

#[utoipa::path(
    get,
    path = "/metrics",
    tag = "Operational",
    description = "\
Prometheus exposition endpoint. Registered only when `metrics.enabled` is \
true — otherwise the path 404s, so a deployment with metrics off is not \
mistaken for a working scrape target.

On this listener it is guarded by the same admin credential as \
`/api/v2/admin/*` when `admin_auth.enabled` is true, so scrapers must be \
configured with the key. Setting `metrics.bind_addr` instead moves the \
endpoint to a dedicated unauthenticated listener on a private interface and \
removes it from this one entirely.",
    responses(
        (status = 200, description = "Prometheus metrics", content_type = "text/plain"),
    )
)]
pub(crate) async fn metrics_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    // Sample DB pool stats on each scrape
    let (pool_size, pool_idle) = state.pool_stats();
    crate::metrics::set_db_pool_size(pool_size as f64);
    crate::metrics::set_db_pool_idle(pool_idle as f64);

    let metrics = state.metrics_handle.render();
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        metrics,
    )
}

/// Liveness probe — always returns 200 if the process is running.
/// Use for Kubernetes `livenessProbe`.
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "Operational",
    operation_id = "liveness_probe",
    summary = "Liveness probe",
    description = "\
Liveness probe. Returns `200 {\"status\":\"ok\"}` as long as the process is \
running and the HTTP server is accepting connections — it performs no \
dependency checks, so a database outage must not restart the pod. \
Use `/readyz` for rotation decisions and `/health` for a detailed report. \
Unauthenticated, so probes work without provisioning an admin key.",
    responses(
        (status = 200, description = "Process is alive", body = orion_api::LivenessResponse),
    )
)]
pub(crate) async fn liveness_check() -> Json<orion_api::LivenessResponse> {
    Json(orion_api::LivenessResponse {
        status: "ok".to_string(),
    })
}

/// How many workflows the running engine holds.
///
/// This used to acquire the engine read lock under
/// `engine.health_check_timeout_secs` and report `None` on timeout — a real
/// check in the `RwLock` era. The engine is an `ArcSwap` now: `load()` is
/// lock-free and infallible, so the probe cannot fail and there is nothing
/// for the timeout to bound (`health_check_timeout_secs` bounds the database
/// ping instead). Both probes keep serving a constant
/// `"engine": "ok"` component for response-shape stability — monitors key on
/// the field — not because anything is checked.
fn workflows_loaded(generation: &crate::runtime::RuntimeGeneration) -> usize {
    generation.workflows().len()
}

/// Coarse state of the cron scheduler, or `None` when this node has nothing to
/// say about schedules.
///
/// Three answers, and the middle one is the whole reason this exists:
///
/// * `None` — the scheduler is off *and* no cron channel is loaded. Nothing to
///   report; a node that does not schedule is not a broken node.
/// * `"degraded"` — either the scheduler is off while active cron channels
///   exist (they are quarantined, so their schedules will never fire), or the
///   reconciler has not completed a pass in long enough that occurrences are
///   now being missed. Both are states in which every liveness signal on the
///   instance is green and the declared schedules are simply not running.
/// * `"ok"` — schedules are being reconciled.
///
/// Unlike `background_tasks`, this is not derived from whether a task is
/// alive: the loops swallow per-tick errors on purpose, so a reconciler failing
/// against an unreachable database stays `Running` forever. What is reported
/// here is whether it is *achieving* anything.
///
/// It degrades `/health` but does **not** fail `/readyz`: this node still serves every request
/// correctly, and taking it out of the load balancer would remove capacity
/// without making a single occurrence run.
fn cron_component(
    state: &AppState,
    generation: &crate::runtime::RuntimeGeneration,
) -> Option<&'static str> {
    let has_channels = generation.channels.has_cron_channels();
    if !state.config.cron.enabled {
        // Quarantined active cron channels are already in
        // `channels.quarantined`, so `has_cron_channels` is false here — the
        // signal has to come from the stored estate instead, which the
        // quarantine list carries.
        let quarantined_cron = generation
            .channels
            .quarantined()
            .iter()
            .any(|issue| issue.reason.contains("cron.enabled = false"));
        return quarantined_cron.then_some("degraded");
    }
    if !has_channels {
        return Some("ok");
    }
    Some(
        if state
            .cron_status
            .is_degraded(state.config.cron.poll_interval())
        {
            "degraded"
        } else {
            "ok"
        },
    )
}

/// Coarse state of the node's supervised background tasks (the trace
/// dispatcher and persistence pool, the audit writer, the retention jobs, the
/// DLQ retry consumer, the cluster reconciler).
///
/// `"error"` means at least one `Required` task has stopped for good, which is
/// the state that used to be invisible: a dead persistence worker dropped
/// every trace routed to it while `/readyz` kept answering `ready`.
/// `"degraded"` covers a task the supervisor is currently restarting, and an
/// `Optional` one that has given up — retention stopping does not make a node
/// unfit to serve.
fn tasks_component(state: &AppState) -> (&'static str, Vec<crate::runtime::TaskReport>) {
    let report = state.tasks.report();
    let component = if report
        .iter()
        .any(crate::runtime::TaskReport::blocks_readiness)
    {
        "error"
    } else if report.iter().any(crate::runtime::TaskReport::is_degraded) {
        "degraded"
    } else {
        "ok"
    };
    (component, report)
}

/// Readiness probe — checks DB, engine, background tasks, and
/// startup readiness. Use for Kubernetes
/// `readinessProbe`.
#[utoipa::path(
    get,
    path = "/readyz",
    tag = "Operational",
    operation_id = "readiness_probe",
    summary = "Readiness probe",
    description = "\
Readiness probe. Reports `ready` only when the database responds, startup \
has completed, every background task the node cannot work without is still \
running. In cluster mode the shared dedup store, response cache and rate \
limits live in that same database, so its check covers them. The \
`components.engine` field is a constant `\"ok\"` kept for response-shape \
stability: the engine snapshot is lock-free and cannot be unavailable once \
the process serves.

The `components.background_tasks` is `error` when a required task — the trace \
dispatcher, the persistence workers, the audit writer, the DLQ retry \
consumer, the cluster reconciler — has stopped for good; each of those \
fails silently otherwise, dropping traces or audit rows while the data plane \
keeps answering 200s. \
Unauthenticated, so probes work without provisioning an admin key.",
    responses(
        (status = 200, description = "All components ready", body = orion_api::ReadinessResponse),
        (status = 503, description = "At least one component is not ready — same body shape with `\"status\":\"not_ready\"`", body = orion_api::ReadinessResponse),
    )
)]
pub(crate) async fn readiness_check(State(state): State<AppState>) -> impl IntoResponse {
    use std::sync::atomic::Ordering;

    let initialized = state.ready.load(Ordering::Acquire);
    let db_ping = state.ping_db().await;
    let db_healthy = db_ping.is_ok();
    let (tasks_state, _) = tasks_component(&state);
    let all_ready = db_healthy && initialized && tasks_state != "error";
    let http_status = if all_ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    let body = orion_api::ReadinessResponse {
        status: if all_ready { "ready" } else { "not_ready" }.to_string(),
        components: orion_api::ReadinessComponents {
            database: if db_healthy { "ok" } else { "error" }.to_string(),
            // Constant by construction — see `workflows_loaded` (O16).
            engine: "ok".to_string(),
            initialized,
            background_tasks: tasks_state.to_string(),
        },
    };

    (http_status, Json(body))
}
