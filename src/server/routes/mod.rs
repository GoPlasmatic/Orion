pub mod admin;
pub mod data;
pub mod openapi;
pub mod response_helpers;

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use utoipa::OpenApi;

use crate::server::state::AppState;

pub fn api_routes() -> Router<AppState> {
    let router = Router::new()
        .route("/health", get(health_check))
        .route("/healthz", get(liveness_check))
        .route("/readyz", get(readiness_check))
        .route("/metrics", get(metrics_endpoint))
        .nest("/api/v1/admin", admin::admin_routes())
        .nest("/api/v1/data", data::data_routes());

    router.merge(
        utoipa_swagger_ui::SwaggerUi::new("/docs")
            .url("/api/v1/openapi.json", openapi::ApiDoc::openapi()),
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
names and failure reasons) is included only for requests presenting a valid \
admin credential; anonymous callers get status, version, uptime and coarse \
per-component states. Probes should use `/healthz` and `/readyz`.",
    responses(
        (status = 200, description = "Service healthy"),
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
    let db_healthy = state.workflow_repo.ping().await.is_ok();

    // Check engine state — independently verify the engine lock is acquirable
    let mut workflows_loaded = 0;
    let engine_healthy = match tokio::time::timeout(
        std::time::Duration::from_secs(state.config.engine.health_check_timeout_secs),
        crate::engine::acquire_engine_read(&state.engine),
    )
    .await
    {
        Ok(engine) => {
            workflows_loaded = engine.workflows().len();
            true
        }
        Err(_) => false,
    };

    // Collect circuit breaker states
    let cb_states = state.connector_registry.circuit_breaker_states().await;

    // F16: enabled connectors that failed to load are absent from the
    // registry, so every workflow using one fails at request time. Report
    // them here rather than leaving a boot-time log line as the only signal.
    let connector_issues = state.connector_registry.load_issues().await;

    // F35: channels that failed to load are quarantined — refused at every
    // ingress — while the rest of the instance serves normally. This is the
    // only signal that they are not being served.
    let quarantined_channels = state.channel_registry.quarantined().await;

    // Degraded, not unhealthy: the rest of the instance still serves traffic,
    // and returning 503 would take a node out of its load balancer over a
    // connector or channel that may be used by nothing currently in flight.
    let overall_healthy = db_healthy && engine_healthy;
    let fully_loaded = connector_issues.is_empty() && quarantined_channels.is_empty();
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
            "engine": if engine_healthy { "ok" } else { "error" },
            "connectors": if connector_issues.is_empty() { "ok" } else { "degraded" },
            "channels": if quarantined_channels.is_empty() { "ok" } else { "degraded" },
        },
    });
    if show_detail {
        body["git_hash"] = json!(env!("GIT_HASH"));
        body["build_timestamp"] = json!(env!("BUILD_TIMESTAMP"));
        body["workflows_loaded"] = json!(workflows_loaded);
        body["connectors"] = json!({
            "circuit_breakers": cb_states,
            "failed_to_load": connector_issues,
        });
        body["channels"] = json!({
            "quarantined": quarantined_channels,
        });
    }

    (http_status, Json(body))
}

#[utoipa::path(
    get,
    path = "/metrics",
    tag = "Operational",
    description = "\
Prometheus exposition endpoint. Guarded by the same admin credential as \
`/api/v1/admin/*` when `admin_auth.enabled` is true — scrapers must be \
configured with the key.",
    responses(
        (status = 200, description = "Prometheus metrics", content_type = "text/plain"),
    )
)]
pub(crate) async fn metrics_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    // Sample DB pool stats on each scrape
    crate::metrics::set_db_pool_size(state.db_pool.size() as f64);
    crate::metrics::set_db_pool_idle(state.db_pool.num_idle() as f64);

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
        (status = 200, description = "Process is alive"),
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

/// Readiness probe — checks DB, engine, cluster Redis, and startup readiness.
/// Use for Kubernetes `readinessProbe`.
#[utoipa::path(
    get,
    path = "/readyz",
    tag = "Operational",
    operation_id = "readiness_probe",
    summary = "Readiness probe",
    description = "\
Readiness probe. Reports `ready` only when the database responds, the engine \
lock is acquirable, startup has completed, and — in cluster mode — the shared \
Redis answers `PING`. The Redis check matters because that degradation is \
otherwise silent: deduplication fails open, the shared response cache misses, \
and cluster rate limiting stops enforcing, all while the data plane keeps \
returning 200s.

The `components.cluster_redis` field is present only in cluster mode. \
Unauthenticated, so probes work without provisioning an admin key.",
    responses(
        (status = 200, description = "All components ready — `{\"status\":\"ready\",\"components\":{...}}`"),
        (status = 503, description = "At least one component is not ready — same body shape with `\"status\":\"not_ready\"`"),
    )
)]
pub(crate) async fn readiness_check(State(state): State<AppState>) -> impl IntoResponse {
    use std::sync::atomic::Ordering;

    let db_healthy = state.workflow_repo.ping().await.is_ok();
    let engine_healthy = tokio::time::timeout(
        std::time::Duration::from_secs(state.config.engine.health_check_timeout_secs),
        crate::engine::acquire_engine_read(&state.engine),
    )
    .await
    .is_ok();
    let initialized = state.ready.load(Ordering::Acquire);
    let redis_healthy = cluster_redis_healthy(&state).await;

    let all_ready = db_healthy && engine_healthy && initialized && redis_healthy.unwrap_or(true);
    let http_status = if all_ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    let mut components = json!({
        "database": if db_healthy { "ok" } else { "error" },
        "engine": if engine_healthy { "ok" } else { "error" },
        "initialized": initialized,
    });
    if let Some(healthy) = redis_healthy {
        components["cluster_redis"] = json!(if healthy { "ok" } else { "error" });
    }

    let body = json!({
        "status": if all_ready { "ready" } else { "not_ready" },
        "components": components,
    });

    (http_status, Json(body))
}

/// Options for [`reload_engine_with_opts`].
#[derive(Clone, Copy, Default)]
pub struct ReloadOpts {
    /// Add 0–5 s of per-node jitter before a full Kafka consumer restart.
    /// Epoch-driven reloads fire on every node near-simultaneously; without
    /// jitter they would all leave and rejoin the consumer group at once.
    pub kafka_restart_jitter: bool,
}

/// Reload the engine with all active channels and workflows from the database.
pub async fn reload_engine(state: &AppState) -> Result<(), crate::errors::OrionError> {
    reload_engine_with_opts(state, ReloadOpts::default()).await
}

/// Epoch-driven full resync from the database, run by the watcher when
/// another node's mutation advanced the config epoch. Beyond a plain engine
/// reload it also refreshes the connector registry and evicts all cached
/// connector pools — a remote node cannot know *which* connector changed,
/// and pools rebuild lazily on next use.
pub async fn resync_from_db(state: &AppState) -> Result<(), crate::errors::OrionError> {
    state
        .connector_registry
        .reload(state.connector_repo.as_ref())
        .await?;
    state.sql_pool_cache.evict_all().await;
    state.mongo_pool_cache.evict_all().await;
    state.cache_pool.evict_all_pools().await;
    reload_engine_with_opts(
        state,
        ReloadOpts {
            kafka_restart_jitter: true,
        },
    )
    .await
}

#[tracing::instrument(skip(state, opts))]
pub async fn reload_engine_with_opts(
    state: &AppState,
    opts: ReloadOpts,
) -> Result<(), crate::errors::OrionError> {
    let start = std::time::Instant::now();

    let result = async {
        let channels = state.channel_repo.list_active().await?;
        let channels = crate::engine::filter_channels(channels, &state.config.channels);
        let active_workflows = state.workflow_repo.list_active().await?;
        let (workflows, engine_issues) =
            crate::engine::build_engine_workflows(&channels, &active_workflows);

        // Build the new engine outside the write lock to minimize lock hold time.
        // Clone the current engine Arc, build new workflows, then swap atomically.
        let current_engine = crate::engine::acquire_engine_read(&state.engine).await;
        let new_engine = Arc::new(
            current_engine
                .with_new_workflows(workflows)
                .map_err(crate::errors::OrionError::Engine)?,
        );

        // Rebuild the channel registry BEFORE swapping the engine, so a
        // channel is never reachable through the new engine before its guards
        // exist. Channels that fail to load are quarantined — refused at every
        // ingress — and the reload proceeds (F35). It used to abort here,
        // which meant one unparseable `config_json` failed every activate,
        // archive, delete and rollout with a 500, and stopped the cluster
        // epoch watcher resyncing all nodes.
        // The issue list is recorded on the registry itself (and reported via
        // `/health`), so it is deliberately not propagated as an error here.
        let _quarantined = state
            .channel_registry
            .reload(
                &channels,
                &state.connector_registry,
                &state.cache_pool,
                &state.datalogic,
                &state.config.tracing.storage,
                engine_issues,
            )
            .await;

        let mut engine_write = tokio::time::timeout(
            std::time::Duration::from_secs(state.config.engine.reload_timeout_secs),
            crate::engine::acquire_engine_write(&state.engine),
        )
        .await
        .map_err(|_| {
            crate::errors::OrionError::Internal(
                "Engine reload timed out waiting for write lock".into(),
            )
        })?;
        *engine_write = new_engine;
        // Release the write lock before the Kafka restart: request handlers
        // block on engine reads while it is held, and the epoch-driven
        // restart path below sleeps a 0–5 s jitter.
        drop(engine_write);

        // Update active workflows gauge
        crate::metrics::set_active_workflows(active_workflows.len() as f64);

        // Restart Kafka consumer if async channel topics changed
        if state.config.kafka.enabled {
            restart_kafka_consumer_if_needed(state, &channels, opts).await;
        }

        tracing::info!(
            workflow_count = active_workflows.len(),
            channel_count = channels.len(),
            "Engine reloaded"
        );
        Ok(())
    }
    .await;

    let duration = start.elapsed().as_secs_f64();
    crate::metrics::record_engine_reload_duration(duration);

    match &result {
        Ok(()) => crate::metrics::record_engine_reload("success"),
        Err(_) => crate::metrics::record_engine_reload("failure"),
    }

    result
}

/// Restart the Kafka consumer when async channel topic mappings have changed.
///
/// Merges config-file topics with DB-driven async channel topics. If the set
/// of topics differs from what the current consumer is subscribed to, the old
/// consumer is shut down and a new one is started.
async fn restart_kafka_consumer_if_needed(
    state: &AppState,
    channels: &[crate::storage::models::Channel],
    opts: ReloadOpts,
) {
    use std::collections::HashSet;

    // Build merged topic list: config-file + DB async channels
    let mut all_topics = state.config.kafka.topics.clone();
    for ch in channels {
        if (ch.protocol == crate::storage::models::ChannelProtocol::Kafka.as_str()
            || ch.channel_type == "async")
            && let Some(ref topic) = ch.topic
            && !all_topics.iter().any(|t| t.topic == *topic)
        {
            all_topics.push(crate::config::TopicMapping {
                topic: topic.clone(),
                channel: ch.name.clone(),
            });
        }
    }

    let new_topic_set: HashSet<String> = all_topics.iter().map(|t| t.topic.clone()).collect();

    let mut handle_guard = state.kafka_consumer_handle.lock().await;

    // Optimisation: if topics haven't changed, pause/resume instead of full restart
    if let Some(ref existing_handle) = *handle_guard
        && *existing_handle.topics() == new_topic_set
    {
        tracing::info!("Kafka topics unchanged, pausing consumer during engine swap");
        if let Err(e) = existing_handle.pause() {
            tracing::warn!(error = %e, "Failed to pause Kafka consumer, falling back to full restart");
        } else {
            // Brief sleep to allow in-flight messages to finish processing
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if let Err(e) = existing_handle.resume() {
                tracing::error!(error = %e, "Failed to resume Kafka consumer after engine reload");
                // Fall through to full restart below
            } else {
                tracing::info!("Kafka consumer resumed after engine reload");
                return;
            }
        }
    }

    // Full restart path: pause first to minimize gap, then shutdown and restart
    if opts.kafka_restart_jitter {
        let jitter_ms = rand::random_range(0..=5000u64);
        tracing::info!(
            jitter_ms,
            "Jittering Kafka consumer restart (epoch-driven reload)"
        );
        tokio::time::sleep(std::time::Duration::from_millis(jitter_ms)).await;
    }
    if let Some(ref existing_handle) = *handle_guard {
        let _ = existing_handle.pause(); // Best-effort pause before shutdown
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    if let Some(old_handle) = handle_guard.take() {
        tracing::info!("Shutting down Kafka consumer for topic refresh...");
        old_handle.shutdown().await;
    }

    if all_topics.is_empty() {
        tracing::info!("No Kafka topics configured or from DB, consumer not started");
        return;
    }

    let merged_config = crate::config::KafkaIngestConfig {
        topics: all_topics,
        ..state.config.kafka.clone()
    };

    let dlq_producer = if state.config.kafka.dlq.enabled {
        state.kafka_producer.clone()
    } else {
        None
    };
    let dlq_topic = if state.config.kafka.dlq.enabled {
        Some(state.config.kafka.dlq.topic.clone())
    } else {
        None
    };

    let static_member = state
        .cluster
        .enabled
        .then(|| state.cluster.instance_id.as_str());
    match crate::kafka::consumer::start_consumer(
        &merged_config,
        state.engine.clone(),
        state.channel_registry.clone(),
        state.datalogic.clone(),
        dlq_producer,
        dlq_topic,
        static_member,
    ) {
        Ok(new_handle) => {
            tracing::info!(
                topics = ?new_topic_set,
                "Kafka consumer restarted with updated topics"
            );
            *handle_guard = Some(new_handle);
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to restart Kafka consumer");
        }
    }
}
