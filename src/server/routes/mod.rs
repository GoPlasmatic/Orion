pub mod admin;
pub mod data;
pub mod openapi;

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

    #[cfg(feature = "swagger-ui")]
    let router = router.merge(
        utoipa_swagger_ui::SwaggerUi::new("/docs")
            .url("/api/v1/openapi.json", openapi::ApiDoc::openapi()),
    );

    router
}

#[utoipa::path(
    get,
    path = "/health",
    tag = "Operational",
    responses(
        (status = 200, description = "Service healthy"),
        (status = 503, description = "Service degraded"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn health_check(State(state): State<AppState>) -> impl IntoResponse {
    let uptime = chrono::Utc::now() - state.start_time;

    // Check database connectivity
    let db_healthy = state.workflow_repo.ping().await.is_ok();

    // Check engine state — independently verify the engine lock is acquirable
    let mut workflows_loaded = 0;
    let engine_healthy = match tokio::time::timeout(
        std::time::Duration::from_secs(state.config.engine.health_check_timeout_secs),
        state.engine.read(),
    )
    .await
    {
        Ok(guard) => {
            workflows_loaded = guard.workflows().len();
            true
        }
        Err(_) => false,
    };

    // Collect circuit breaker states
    let cb_states = state.connector_registry.circuit_breaker_states().await;

    let overall_healthy = db_healthy && engine_healthy;
    let status_str = if overall_healthy { "ok" } else { "degraded" };
    let http_status = if overall_healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    let body = json!({
        "status": status_str,
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": uptime.num_seconds(),
        "workflows_loaded": workflows_loaded,
        "components": {
            "database": if db_healthy { "ok" } else { "error" },
            "engine": if engine_healthy { "ok" } else { "error" },
        },
        "connectors": {
            "circuit_breakers": cb_states,
        }
    });

    (http_status, Json(body))
}

#[utoipa::path(
    get,
    path = "/metrics",
    tag = "Operational",
    responses(
        (status = 200, description = "Prometheus metrics", content_type = "text/plain"),
    )
)]
pub(crate) async fn metrics_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    let metrics = state.metrics_handle.render();
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        metrics,
    )
}

/// Liveness probe — always returns 200 if the process is running.
/// Use for Kubernetes `livenessProbe`.
pub(crate) async fn liveness_check() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({ "status": "ok" })))
}

/// Readiness probe — checks DB, engine, and startup readiness.
/// Use for Kubernetes `readinessProbe`.
pub(crate) async fn readiness_check(State(state): State<AppState>) -> impl IntoResponse {
    use std::sync::atomic::Ordering;

    let db_healthy = state.workflow_repo.ping().await.is_ok();
    let engine_healthy = tokio::time::timeout(
        std::time::Duration::from_secs(state.config.engine.health_check_timeout_secs),
        state.engine.read(),
    )
    .await
    .is_ok();
    let initialized = state.ready.load(Ordering::Acquire);

    let all_ready = db_healthy && engine_healthy && initialized;
    let http_status = if all_ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    let body = json!({
        "status": if all_ready { "ready" } else { "not_ready" },
        "components": {
            "database": if db_healthy { "ok" } else { "error" },
            "engine": if engine_healthy { "ok" } else { "error" },
            "initialized": initialized,
        }
    });

    (http_status, Json(body))
}

/// Reload the engine with all active channels and workflows from the database.
#[tracing::instrument(skip(state))]
pub async fn reload_engine(state: &AppState) -> Result<(), crate::errors::OrionError> {
    let start = std::time::Instant::now();

    let result = async {
        let channels = state.channel_repo.list_active().await?;
        let active_workflows = state.workflow_repo.list_active().await?;
        let workflows = crate::engine::build_engine_workflows(&channels, &active_workflows);

        // Build the new engine outside the write lock to minimize lock hold time.
        // Clone the current engine Arc, build new workflows, then swap atomically.
        let current_engine = state.engine.read().await.clone();
        let new_engine = Arc::new(current_engine.with_new_workflows(workflows));

        let mut engine_write = tokio::time::timeout(
            std::time::Duration::from_secs(state.config.engine.reload_timeout_secs),
            state.engine.write(),
        )
        .await
        .map_err(|_| {
            crate::errors::OrionError::Internal(
                "Engine reload timed out waiting for write lock".into(),
            )
        })?;
        *engine_write = new_engine;

        // Rebuild channel registry
        state.channel_registry.reload(&channels).await;

        // Update active workflows gauge
        crate::metrics::set_active_workflows(active_workflows.len() as f64);

        // Restart Kafka consumer if async channel topics changed
        #[cfg(feature = "kafka")]
        if state.config.kafka.enabled {
            restart_kafka_consumer_if_needed(state, &channels).await;
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
#[cfg(feature = "kafka")]
async fn restart_kafka_consumer_if_needed(
    state: &AppState,
    channels: &[crate::storage::models::Channel],
) {
    use std::collections::HashSet;

    // Build merged topic list: config-file + DB async channels
    let mut all_topics = state.config.kafka.topics.clone();
    for ch in channels {
        if (ch.protocol == "kafka" || ch.channel_type == "async")
            && let Some(ref topic) = ch.topic
            && !all_topics.iter().any(|t| t.topic == *topic)
        {
            all_topics.push(crate::config::TopicMapping {
                topic: topic.clone(),
                channel: ch.name.clone(),
            });
        }
    }

    // Compare with currently tracked topics (stored in consumer handle)
    let new_topic_set: HashSet<String> = all_topics.iter().map(|t| t.topic.clone()).collect();

    // Always restart: we don't track the old topic set, so rebuild unconditionally.
    // The brief consumer restart (milliseconds) is acceptable during an engine reload.
    let mut handle_guard = state.kafka_consumer_handle.lock().await;

    // Shut down existing consumer
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

    match crate::kafka::consumer::start_consumer(
        &merged_config,
        state.engine.clone(),
        dlq_producer,
        dlq_topic,
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
