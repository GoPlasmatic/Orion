pub mod admin;
pub mod data;
pub mod openapi;
pub mod response_helpers;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{any, get};
use axum::{Json, Router};
use serde_json::json;
use utoipa::OpenApi;

use crate::server::state::AppState;

/// What the main listener's router should contain, resolved from config by
/// [`crate::server::build_router`].
///
/// A struct rather than three positional scalars: two of the three are bare
/// `bool`s with the same type, so a transposition at the call site would
/// compile and silently unregister the wrong surface.
#[derive(Debug, Clone)]
pub struct RouteOptions {
    /// Bounds admin request bodies independently of the data plane (R16) —
    /// see [`admin::admin_routes`].
    pub max_admin_body_size: usize,
    /// Gates `/docs` and `/api/v1/openapi.json` (S17, resolved by
    /// [`crate::config::AppConfig::docs_enabled`]): the spec publishes the
    /// whole admin API surface anonymously, so production deployments keep it
    /// off by default. When disabled the routes are simply not registered —
    /// both paths fall through to the 404 fallback rather than answering 401,
    /// so their very existence is not advertised.
    pub docs_enabled: bool,
    /// Gates `/metrics` on **this** listener (O12). False both when
    /// `metrics.enabled = false` and when `metrics.bind_addr` has moved the
    /// endpoint to its own listener; in either case the path 404s here rather
    /// than answering 200 with an empty body.
    pub metrics_enabled: bool,
    /// Extra prefixes the data plane is served at, beyond `/api/v1/data`
    /// (`server.data_mounts`). Empty is the default and registers nothing.
    pub data_mounts: Vec<String>,
}

/// The platform routes a data mount must never shadow — the single source the
/// mount validation and the channel-activation gate both consult, so the two
/// cannot drift.
///
/// The invariant these hold: **platform routes are single-segment at root, or
/// under `/api`.** That is what lets a future platform route be added without
/// silently stealing a path from a channel already serving it.
pub(crate) const PLATFORM_ROUTES: &[&str] = &[
    "/health", "/healthz", "/readyz", "/metrics", "/docs", "/api",
];

/// Whether `prefix` claims `path`: the same string, or `path` sitting under it
/// at a `/` boundary. Allocation-free, so it is safe on the request path.
///
/// One rule, because four sites had grown their own `format!("{p}/")` copy of
/// it — the mount reserved-prefix check, the mount nesting check, the
/// rate-limit route classifier, and this function.
pub(crate) fn path_claims(prefix: &str, path: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The platform route `served_path` would be shadowed by, if any.
pub(crate) fn shadowed_platform_route(served_path: &str) -> Option<&'static str> {
    PLATFORM_ROUTES
        .iter()
        .copied()
        .find(|p| path_claims(p, served_path))
}

/// The main listener's router.
pub fn api_routes(options: RouteOptions) -> Router<AppState> {
    let RouteOptions {
        max_admin_body_size,
        docs_enabled,
        metrics_enabled,
        data_mounts,
    } = options;
    let router = Router::new()
        .route("/health", get(health_check))
        .route("/healthz", get(liveness_check))
        .route("/readyz", get(readiness_check))
        .nest("/api/v1/admin", admin::admin_routes(max_admin_body_size))
        .nest("/api/v1/data", data::data_routes());

    // Extra data-plane mounts (`server.data_mounts`). `.route`, never
    // `.nest` or `.fallback`:
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
    let router = data_mounts.iter().fold(router, |router, mount| {
        let pattern = if mount == "/" {
            "/{*path}".to_string()
        } else {
            format!("{mount}/{{*path}}")
        };
        router.route(&pattern, any(data::dynamic_handler))
    });

    // O12: registered only when this listener actually serves metrics.
    // Unconditional registration meant `metrics.enabled = false` answered 200
    // with an empty body from an orphan recorder — a scrape target that looked
    // healthy and reported nothing, forever.
    let router = if metrics_enabled {
        router.route("/metrics", get(metrics_endpoint))
    } else {
        router
    };

    let router = if docs_enabled {
        router.merge(
            utoipa_swagger_ui::SwaggerUi::new("/docs")
                .url("/api/v1/openapi.json", openapi::ApiDoc::openapi()),
        )
    } else {
        router
    };

    router
        // R9: without these, an unmatched path and every method mismatch
        // returned a zero-length body, violating the documented contract that
        // "every non-2xx response uses the ErrorResponse envelope". Clients
        // that parse the body on error saw a JSON decode failure instead of an
        // error code. Registered inside the request-id scope (see server::mod
        // layer order) so both carry `x-request-id`.
        .fallback(|| async {
            crate::errors::OrionError::NotFound("No route matches this path".to_string())
        })
        .method_not_allowed_fallback(|| async {
            crate::errors::OrionError::MethodNotAllowed(
                "The HTTP method is not allowed for this path".to_string(),
            )
        })
}

#[utoipa::path(
    get,
    path = "/health",
    tag = "Operational",
    description = "\
Detailed health report. Always reachable, but when `admin_auth.enabled` is \
true the topology detail (`git_hash`, `build_timestamp`, `workflows_loaded`, \
the circuit-breaker map, connector load failures and quarantined channels — \
names and failure reasons) is included only for requests presenting a valid \
admin credential; anonymous callers get status, version, uptime and coarse \
per-component states. Probes should use `/healthz` and `/readyz`.",
    responses(
        (status = 200, description = "Service healthy", body = crate::server::routes::openapi::HealthStatus),
        (status = 503, description = "Service degraded"),
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

    let workflows_loaded = workflows_loaded(&state);

    // Collect circuit breaker states
    let cb_states = state.connector_registry.circuit_breaker_states().await;

    // F16: enabled connectors that failed to load are absent from the
    // registry, so every workflow using one fails at request time. Report
    // them here rather than leaving a boot-time log line as the only signal.
    let connector_issues = state.connector_registry.load_issues().await;

    // F35: channels that failed to load are quarantined — refused at every
    // ingress — while the rest of the instance serves normally. This is the
    // only signal that they are not being served.
    let quarantined_channels = state.channel_registry.quarantined();

    // O10/K7: dead Kafka ingestion is otherwise silent — HTTP keeps serving
    // 200s while no message is consumed. Absent entirely when Kafka is off.
    let kafka_state = kafka_component(&state);

    // Degraded, not unhealthy: the rest of the instance still serves traffic,
    // and returning 503 would take a node out of its load balancer over a
    // connector or channel that may be used by nothing currently in flight.
    let (tasks_state, task_reports) = tasks_component(&state);
    let overall_healthy = db_healthy;
    let fully_loaded = connector_issues.is_empty()
        && quarantined_channels.is_empty()
        && kafka_state != Some("error")
        && tasks_state == "ok"
        && !(state.cluster.enabled && state.cluster.propagation_degraded());
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
    let show_detail = !auth_cfg.enabled
        || crate::server::admin_auth::headers_present_valid_key(&headers, auth_cfg);

    let mut body = json!({
        "status": status_str,
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": uptime.num_seconds(),
        "components": {
            "database": if db_healthy { "ok" } else { "error" },
            // Constant by construction — see `workflows_loaded` (O16).
            "engine": "ok",
            "connectors": if connector_issues.is_empty() { "ok" } else { "degraded" },
            "channels": if quarantined_channels.is_empty() { "ok" } else { "degraded" },
            "background_tasks": tasks_state,
        },
    });
    if let Some(kafka) = kafka_state {
        body["components"]["kafka"] = json!(kafka);
    }
    // Cluster mode only: outside it there are no peers to propagate to.
    // `degraded`, not `error`, and absent from `/readyz` on purpose — this
    // node is serving the change correctly; it is the peers that have not
    // heard, and taking this one out of rotation would not tell them.
    if state.cluster.enabled {
        body["components"]["config_propagation"] = json!(if state.cluster.propagation_degraded() {
            "degraded"
        } else {
            "ok"
        });
    }
    if show_detail {
        body["git_hash"] = json!(env!("GIT_HASH"));
        body["build_timestamp"] = json!(env!("BUILD_TIMESTAMP"));
        body["workflows_loaded"] = json!(workflows_loaded);
        body["connectors"] = json!({
            // F21: node-local, like the admin endpoint's copy.
            "circuit_breaker_scope": "node",
            "circuit_breakers": cb_states,
            "failed_to_load": connector_issues,
        });
        body["channels"] = json!({
            "quarantined": quarantined_channels,
        });
        // O9: task names are internal topology, so the per-task breakdown
        // rides with the other admin-only detail. The coarse
        // `components.background_tasks` above is what a monitor keys on.
        body["background_tasks"] = json!(
            task_reports
                .iter()
                .map(|r| json!({
                    "name": r.name,
                    "state": r.state.as_str(),
                    "restarts": r.restarts,
                    "required": r.criticality == crate::runtime::Criticality::Required,
                }))
                .collect::<Vec<_>>()
        );
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
`/api/v1/admin/*` when `admin_auth.enabled` is true, so scrapers must be \
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
dependency checks, so a database or Redis outage must not restart the pod. \
Use `/readyz` for rotation decisions and `/health` for a detailed report. \
Unauthenticated, so probes work without provisioning an admin key.",
    responses(
        (status = 200, description = "Process is alive", body = crate::server::routes::openapi::HealthStatus),
    )
)]
pub(crate) async fn liveness_check() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({ "status": "ok" })))
}

/// PING the shared cluster Redis. `None` outside cluster mode (there is no
/// shared Redis to check); `Some(false)` when this node cannot reach it.
///
/// Readiness has to cover it because the degradation is silent: dedup fails
/// open, the shared response cache misses, and cluster rate limiting stops
/// enforcing — all with 200s on the data plane. A node in that state must
/// leave the load-balancer rotation.
async fn cluster_redis_healthy(state: &AppState) -> Option<bool> {
    let mut conn = state.cluster.redis.clone()?;
    let ping = async move {
        let pong: redis::RedisResult<String> = redis::cmd("PING").query_async(&mut conn).await;
        match pong {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(error = %e, "Cluster Redis ping failed; reporting not ready");
                false
            }
        }
    };
    Some(
        tokio::time::timeout(
            std::time::Duration::from_secs(state.config.engine.health_check_timeout_secs),
            ping,
        )
        .await
        .unwrap_or(false),
    )
}

/// How many workflows the running engine holds.
///
/// O16: this used to acquire the engine read lock under
/// `engine.health_check_timeout_secs` and report `None` on timeout — a real
/// check in the `RwLock` era. The engine is an `ArcSwap` now: `load()` is
/// lock-free and infallible, so the probe cannot fail and there is nothing
/// for the timeout to bound (`health_check_timeout_secs` still bounds the
/// cluster-Redis ping above). Both probes keep serving a constant
/// `"engine": "ok"` component for response-shape stability — monitors key on
/// the field — not because anything is checked.
fn workflows_loaded(state: &AppState) -> usize {
    state.engine.load().workflows().len()
}

/// Coarse state of the Kafka ingest consumer for `/health` and `/readyz`:
/// `None` when Kafka is disabled, so non-Kafka deployments carry no `kafka`
/// component at all (O10).
///
/// `"ok"` covers both a running consumer and one intentionally not started
/// (no topics to consume). `"error"` means ingestion should be running and
/// is not: the K7 degraded flag is set (a consumer restart failed and the
/// supervisor has not recovered it yet), or the consume loop itself died.
fn kafka_component(state: &AppState) -> Option<&'static str> {
    if !state.config.kafka.enabled {
        return None;
    }
    if state.kafka.ingest_status.is_degraded() {
        return Some("error");
    }
    let consumer_dead = match state.kafka.consumer_handle.try_lock() {
        Ok(guard) => guard.as_ref().is_some_and(|h| h.is_finished()),
        // A reload holds the lock mid-restart. The degraded flag above is
        // the authoritative down signal and it said healthy — a probe must
        // not block on (or fail during) a routine restart.
        Err(_) => false,
    };
    Some(if consumer_dead { "error" } else { "ok" })
}

/// Coarse state of the node's supervised background tasks (the trace
/// dispatcher and persistence pool, the audit writer, the retention jobs, the
/// DLQ retry consumer, the cluster epoch watcher).
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

/// Readiness probe — checks DB, engine, cluster Redis, Kafka ingestion,
/// background tasks, and startup readiness. Use for Kubernetes
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
running, — in cluster mode — the shared Redis answers `PING`, and — \
with Kafka enabled — the ingest consumer is not degraded. The \
`components.engine` field is a constant `\"ok\"` kept for response-shape \
stability: the engine snapshot is lock-free and cannot be unavailable once \
the process serves. Both conditional checks matter because those degradations are \
otherwise silent: without Redis, deduplication fails open, the shared \
response cache misses, and cluster rate limiting stops enforcing; with the \
consumer down, no message is ingested — all while the data plane keeps \
returning 200s.

The `components.background_tasks` is `error` when a required task — the trace \
dispatcher, the persistence workers, the audit writer, the DLQ retry \
consumer, the cluster epoch watcher — has stopped for good; each of those \
fails silently otherwise, dropping traces or audit rows while the data plane \
keeps answering 200s. The `components.cluster_redis` field is present only in \
cluster mode, and `components.kafka` only when `kafka.enabled` is true. \
Unauthenticated, so probes work without provisioning an admin key.",
    responses(
        (status = 200, description = "All components ready", body = crate::server::routes::openapi::HealthStatus),
        (status = 503, description = "At least one component is not ready — same body shape with `\"status\":\"not_ready\"`"),
    )
)]
pub(crate) async fn readiness_check(State(state): State<AppState>) -> impl IntoResponse {
    use std::sync::atomic::Ordering;

    let initialized = state.ready.load(Ordering::Acquire);
    // The dependency probes share no state and each carries its own
    // `health_check_timeout_secs` window, so running them sequentially made a
    // probe's worst case the *sum* of those windows — long past a typical
    // `timeoutSeconds: 1`, reporting not-ready for a reason that is not the
    // actual degradation.
    let (db_ping, redis_healthy) = tokio::join!(state.ping_db(), cluster_redis_healthy(&state));
    let db_healthy = db_ping.is_ok();
    let kafka_state = kafka_component(&state);
    let (tasks_state, _) = tasks_component(&state);

    let all_ready = db_healthy
        && initialized
        && redis_healthy.unwrap_or(true)
        && kafka_state != Some("error")
        && tasks_state != "error";
    let http_status = if all_ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    let mut components = json!({
        "database": if db_healthy { "ok" } else { "error" },
        // Constant by construction — see `workflows_loaded` (O16).
        "engine": "ok",
        "initialized": initialized,
        "background_tasks": tasks_state,
    });
    if let Some(healthy) = redis_healthy {
        components["cluster_redis"] = json!(if healthy { "ok" } else { "error" });
    }
    if let Some(kafka) = kafka_state {
        components["kafka"] = json!(kafka);
    }

    let body = json!({
        "status": if all_ready { "ready" } else { "not_ready" },
        "components": components,
    });

    (http_status, Json(body))
}
