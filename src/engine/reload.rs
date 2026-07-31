//! Engine reload and the Kafka-consumer lifecycle that rides along with it.
//!
//! R25: this lived in `server/routes/mod.rs` — 226 lines of engine and Kafka
//! lifecycle with no HTTP in them, in a file whose `api_routes()` is fourteen
//! lines and whose name says *route table*. The admin handlers that trigger a
//! reload and the cluster epoch watcher both call in here; neither is a route.

use std::sync::Arc;

use crate::server::state::AppState;

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
        let channels = crate::engine::filter_channels(channels, &state.config.channel_filter);
        let active_workflows = state.workflow_repo.list_active().await?;
        let (workflows, engine_issues) =
            crate::engine::build_engine_workflows(&channels, &active_workflows);

        // Build the new engine outside the write lock to minimize lock hold time.
        // Clone the current engine Arc, build new workflows, then swap atomically.
        let current_engine = state.engine.load();
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
        // The quarantine set is recorded on the registry itself and read back
        // with `ChannelRegistry::quarantined` (reported via `/health`), so it
        // is deliberately not propagated as an error here.
        state
            .channel_registry
            .reload(
                &channels,
                &state.connector_registry,
                &state.cache_pool,
                &state.datalogic,
                &state.config.trace_storage,
                engine_issues,
            )
            .await;

        // Publishing the new engine is a single atomic store. There is no
        // window in which a reader is held off, so this needs neither a timeout
        // nor a carefully-scoped drop before the Kafka restart below.
        state.engine.store(new_engine);

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

    let all_topics = crate::kafka::merge_kafka_topics(&state.config.kafka, channels);

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

    // K7: the old handle is gone. A start failure below must not leave that
    // as the permanent end state with every probe green — flag ingestion as
    // degraded and hand recovery to the supervisor.
    match try_start_ingest(state, channels) {
        Ok(Some(new_handle)) => {
            tracing::info!(
                topics = ?new_topic_set,
                "Kafka consumer restarted with updated topics"
            );
            *handle_guard = Some(new_handle);
            state.kafka_ingest_status.set_degraded(false);
        }
        Ok(None) => {
            tracing::info!("No Kafka topics configured or from DB, consumer not started");
            state.kafka_ingest_status.set_degraded(false);
        }
        Err(e) => {
            state.kafka_ingest_status.set_degraded(true);
            crate::metrics::record_error("kafka_restart");
            tracing::error!(
                error = %e,
                "Failed to restart Kafka consumer; ingestion is down — retrying with backoff"
            );
            drop(handle_guard);
            spawn_kafka_restart_supervisor(state);
        }
    }
}

/// Start an ingest consumer for the current channel set through the same
/// builder startup uses ([`crate::bootstrap::start_kafka_ingest`]), so the
/// boot, reload and supervisor paths cannot drift. `Ok(None)` when the
/// merged topic list is empty. The error is stringified because the caller
/// holds it across a spawned task boundary.
fn try_start_ingest(
    state: &AppState,
    channels: &[crate::storage::models::Channel],
) -> Result<Option<crate::kafka::consumer::ConsumerHandle>, String> {
    crate::bootstrap::start_kafka_ingest(
        &state.config.kafka,
        channels,
        state.engine.clone(),
        state.channel_registry.clone(),
        state.datalogic.clone(),
        state.kafka_producer.clone(),
        state
            .cluster
            .enabled
            .then(|| state.cluster.instance_id.as_str()),
    )
    .map_err(|e| e.to_string())
}

/// Supervise a Kafka consumer that failed to (re)start: retry with capped
/// exponential backoff (1 s doubling to 60 s) until a consumer is running
/// again, then clear the degraded flag (K7). The channel list is re-read
/// from the database on every attempt, so topic changes made while
/// ingestion was down are honoured. At most one supervisor runs per
/// process; it stands down when another reload restores the consumer first,
/// or when the node starts draining.
///
/// Slot invariant: every exit path releases the supervisor slot while the
/// handle mutex is still held. A failed reload runs its start attempt and
/// `set_degraded(true)` inside that same mutex, so by the time it drops the
/// mutex and calls `claim_supervisor` the slot is either free (this
/// supervisor's final critical section already ran — the claim succeeds and
/// a fresh supervisor spawns) or claimed by a supervisor that is still
/// looping and will retry the new failure itself. Releasing *after* the
/// mutex was a TOCTOU: a reload failing in the unlock→release gap saw the
/// slot occupied, spawned nothing, and left the process degraded with no
/// supervisor until the next reload.
pub fn spawn_kafka_restart_supervisor(state: &AppState) {
    if !state.kafka_ingest_status.claim_supervisor() {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        let mut backoff_ms = crate::kafka::consumer::INITIAL_RETRY_BACKOFF_MS;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            let mut handle_guard = state.kafka_consumer_handle.lock().await;
            // Draining: do not resurrect a consumer mid-shutdown. (Checked
            // under the mutex like every other exit, so even this release
            // cannot race a failing reload's claim — and a spawn lost to a
            // drain-window race would only ever supervise a node that is
            // shutting down.)
            if !state.ready.load(std::sync::atomic::Ordering::Acquire) {
                state.kafka_ingest_status.release_supervisor();
                break;
            }
            if handle_guard.is_some() {
                // Another reload already restarted the consumer.
                state.kafka_ingest_status.release_supervisor();
                break;
            }
            let channels = match state.channel_repo.list_active().await {
                Ok(channels) => {
                    crate::engine::filter_channels(channels, &state.config.channel_filter)
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        backoff_ms,
                        "Kafka restart supervisor could not list channels; will retry"
                    );
                    drop(handle_guard);
                    backoff_ms = crate::kafka::consumer::next_backoff_ms(backoff_ms);
                    continue;
                }
            };
            let started = try_start_ingest(&state, &channels);
            match started {
                Ok(Some(handle)) => {
                    *handle_guard = Some(handle);
                    // Slot before flag: both are Release stores, so an
                    // observer that sees the degraded flag cleared also
                    // sees the slot free.
                    state.kafka_ingest_status.release_supervisor();
                    state.kafka_ingest_status.set_degraded(false);
                    tracing::info!("Kafka consumer restored by restart supervisor");
                    break;
                }
                Ok(None) => {
                    // Nothing to ingest any more — idle, not degraded.
                    state.kafka_ingest_status.release_supervisor();
                    state.kafka_ingest_status.set_degraded(false);
                    tracing::info!("No Kafka topics remain; restart supervisor standing down");
                    break;
                }
                Err(e) => {
                    crate::metrics::record_error("kafka_restart");
                    tracing::error!(
                        error = %e,
                        backoff_ms,
                        "Kafka consumer restart failed; ingestion still down"
                    );
                    drop(handle_guard);
                    backoff_ms = crate::kafka::consumer::next_backoff_ms(backoff_ms);
                }
            }
        }
    });
}
