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
pub async fn reload_engine(
    state: &AppState,
) -> Result<Arc<super::RuntimeGeneration>, crate::errors::OrionError> {
    reload_engine_with_opts(state, ReloadOpts::default()).await
}

/// Epoch-driven resync from the database, run by the watcher when another
/// node's mutation advanced the config epoch.
///
/// `scope` is what the bumping node said it changed. It used to be nothing at
/// all — the epoch was a bare counter — so this always did the widest resync
/// there is: reload every connector and evict every cached SQL, MongoDB and
/// cache pool. One workflow activation was therefore a fleet-wide reconnect
/// storm, every node dropping every pooled connection for a change that
/// touched no connector.
///
/// [`EpochScope::Definitions`](crate::cluster::EpochScope::Definitions) skips the connector half. That is sound in the
/// direction that matters: the channel loader keys its per-channel reuse on
/// the connector registry's generation token
/// (`channel::registry::DepsFingerprint`), and not reloading connectors leaves
/// that token where it is, so a channel whose connectors did not change is
/// carried over with its limiters and semaphores intact — which is exactly
/// what should happen. It also honours that module's standing rule, *never
/// evict a pool without loading connectors*, by doing neither.
///
/// An unknown scope reads as [`EpochScope::All`](crate::cluster::EpochScope::All), so a bump from a newer node
/// this one does not understand costs the storm rather than a missed change.
pub async fn resync_from_db(
    state: &AppState,
    scope: crate::cluster::EpochScope,
) -> Result<Arc<super::RuntimeGeneration>, crate::errors::OrionError> {
    resync_from_db_with_opts(
        state,
        scope,
        ReloadOpts {
            kafka_restart_jitter: true,
        },
    )
    .await
}

pub(crate) async fn resync_from_db_with_opts(
    state: &AppState,
    scope: crate::cluster::EpochScope,
    opts: ReloadOpts,
) -> Result<Arc<super::RuntimeGeneration>, crate::errors::OrionError> {
    reload_with_scope(state, opts, Some(scope), true).await
}

/// Local connector writes already invalidate the affected names. Rebuild
/// their dependent guards without dropping every unrelated connection pool.
pub(crate) async fn reload_connectors_after_commit(
    state: &AppState,
) -> Result<Arc<super::RuntimeGeneration>, crate::errors::OrionError> {
    reload_with_scope(
        state,
        ReloadOpts::default(),
        Some(crate::cluster::EpochScope::Connectors),
        false,
    )
    .await
}

/// Rebuild and publish the next generation, returning it: the caller that
/// caused a reload learns what *that* generation could not load, with no
/// window in which another reload's generation stands in for it — this is
/// read under `reload_lock`, right after the publish.
#[tracing::instrument(skip(state, opts))]
pub async fn reload_engine_with_opts(
    state: &AppState,
    opts: ReloadOpts,
) -> Result<Arc<super::RuntimeGeneration>, crate::errors::OrionError> {
    reload_with_scope(state, opts, None, false).await
}

/// Build and publish the exact entity revisions declared in `target_revision`'s
/// committed [`crate::extensions::deployment_store::CanonicalManifest`] before
/// marking convergence in the node reconciler.
pub async fn reconcile_deployment_revision(
    state: &AppState,
    target_revision: u64,
) -> Result<
    crate::extensions::reconciler::RuntimeNodeStatus,
    crate::extensions::deployment_store::DeploymentError,
> {
    use crate::extensions::deployment_store::{EntityKind, EntityRef};
    use crate::extensions::reconciler::CandidateBuildIssue;

    state.composition.reconciler.observe_desired_head();
    let build_res = reload_with_manifest(
        state,
        ReloadOpts::default(),
        None,
        false,
        Some(target_revision),
        true,
    )
    .await;

    let mut build_issues = Vec::new();
    match build_res {
        Ok(published) => {
            if let Some(rev_row) = state.composition.deployments.get_revision(target_revision) {
                for q in published.channels.quarantined() {
                    let rev = rev_row
                        .manifest
                        .members
                        .iter()
                        .find(|m| {
                            m.entity.kind == EntityKind::Channel && m.entity.id == q.channel_id
                        })
                        .map(|m| m.entity.revision)
                        .unwrap_or(1);
                    build_issues.push(CandidateBuildIssue::PerEntityExternal {
                        entity: EntityRef {
                            kind: EntityKind::Channel,
                            id: q.channel_id.clone(),
                            revision: rev,
                        },
                        reason: q.reason.clone(),
                    });
                }
            }
        }
        Err(reload_err) => {
            build_issues.push(CandidateBuildIssue::WholeBuildIntegrity {
                code: "ENGINE_RELOAD_FAILED".to_string(),
                message: reload_err.to_string(),
            });
        }
    }

    let status_res = state
        .composition
        .reconciler
        .build_and_publish_candidate(target_revision, build_issues);
    let _ = state
        .composition
        .reconciler
        .persist_local_status_to_db_pool(&state.db_pool)
        .await;
    status_res
}

async fn manifest_definitions(
    state: &AppState,
    target_revision: u64,
) -> Result<crate::storage::repositories::ActiveDefinitions, crate::errors::OrionError> {
    use std::collections::HashMap;

    use crate::extensions::deployment_store::{ArtifactState, EntityKind, ManifestMember};
    use crate::storage::models::{Channel, EntityStatus, Model, Plugin, Workflow};
    use crate::storage::repositories::{channels, models, plugins, versioned, workflows};

    let rev_row = state
        .composition
        .deployments
        .get_revision(target_revision)
        .ok_or_else(|| {
            crate::errors::OrionError::NotFound(format!(
                "Deployment revision {target_revision} not found"
            ))
        })?;
    let manifest = &rev_row.manifest;

    for p in &manifest.providers {
        let Some(adm) = state.composition.deployments.get_admission(&p.digest) else {
            return Err(crate::errors::OrionError::validation(format!(
                "provider '{}' ({}) is not admitted",
                p.provider_id, p.digest
            )));
        };
        if adm.revoked || state.composition.admission.is_digest_revoked(&p.digest) {
            return Err(crate::errors::OrionError::validation(format!(
                "provider '{}' ({}) is revoked",
                p.provider_id, p.digest
            )));
        }
    }

    let now = chrono::Utc::now().naive_utc();
    let parse_artifact_json =
        |member: &ManifestMember| -> Result<Option<serde_json::Value>, crate::errors::OrionError> {
            let Some(art) = state
                .composition
                .deployments
                .get_artifact(&member.definition_digest)
            else {
                return Ok(None);
            };
            if art.state != ArtifactState::Active {
                return Err(crate::errors::OrionError::validation(format!(
                    "member '{}' artifact '{}' is not ACTIVE",
                    member.entity.id, member.definition_digest
                )));
            }
            let Some(bytes) = art.bytes else {
                return Ok(None);
            };
            Ok(serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        };

    let mut active_channels = Vec::new();
    let mut active_workflows = Vec::new();
    let mut active_plugins = Vec::new();
    let mut active_models = Vec::new();
    let mut rollout_overrides: HashMap<(String, u64), i64> = HashMap::new();
    let mut workflow_counts: HashMap<String, usize> = HashMap::new();

    for m in &manifest.members {
        if m.entity.kind == EntityKind::Workflow {
            *workflow_counts.entry(m.entity.id.clone()).or_insert(0) += 1;
        }
    }

    for m in &manifest.members {
        match m.entity.kind {
            EntityKind::Channel => {
                let version = i64::try_from(m.entity.revision).unwrap_or(i64::MAX);
                let req_wf = m
                    .requires
                    .iter()
                    .find(|r| r.kind == EntityKind::Workflow)
                    .map(|r| r.id.clone());

                let mut ch = if let Some(val) = parse_artifact_json(m)?
                    && val.is_object()
                    && (val.get("protocol").is_some()
                        || val.get("channel_type").is_some()
                        || val.get("route_pattern").is_some()
                        || val.get("topic").is_some())
                {
                    let protocol = val
                        .get("protocol")
                        .and_then(|v| v.as_str())
                        .unwrap_or("rest")
                        .to_string();
                    let methods_json = match val.get("methods") {
                        Some(v) if !v.is_null() => serde_json::to_string(v).ok(),
                        _ if protocol == "rest" => Some("[\"POST\"]".to_string()),
                        _ => None,
                    };
                    Channel {
                        channel_id: m.entity.id.clone(),
                        version,
                        name: val
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&m.entity.id)
                            .to_string(),
                        description: val
                            .get("description")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        channel_type: val
                            .get("channel_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("sync")
                            .to_string(),
                        protocol,
                        methods_json,
                        route_pattern: val
                            .get("route_pattern")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        topic: val
                            .get("topic")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        consumer_group: val
                            .get("consumer_group")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        transport_config_json: serde_json::to_string(
                            val.get("transport_config")
                                .unwrap_or(&serde_json::json!({})),
                        )
                        .unwrap_or_else(|_| "{}".to_string()),
                        workflow_id: val
                            .get("workflow_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                            .or_else(|| req_wf.clone()),
                        config_json: serde_json::to_string(
                            val.get("config").unwrap_or(&serde_json::json!({})),
                        )
                        .unwrap_or_else(|_| "{}".to_string()),
                        status: EntityStatus::Active.as_str().to_string(),
                        priority: val.get("priority").and_then(|v| v.as_i64()).unwrap_or(0),
                        tags_json: serde_json::to_string(
                            val.get("tags").unwrap_or(&serde_json::json!([])),
                        )
                        .unwrap_or_else(|_| "[]".to_string()),
                        created_at: now,
                        updated_at: now,
                    }
                } else {
                    versioned::get_version::<Channel>(
                        &state.db_pool,
                        &channels::spec(),
                        &m.entity.id,
                        version,
                    )
                    .await
                    .map_err(|_| {
                        crate::errors::OrionError::validation(format!(
                            "Manifest channel '{}' revision {} definition not found",
                            m.entity.id, m.entity.revision
                        ))
                    })?
                };

                ch.status = EntityStatus::Active.as_str().to_string();
                if let Some(wf_id) = req_wf {
                    ch.workflow_id = Some(wf_id);
                }
                if !m.rollouts.is_empty()
                    && let Some(ref wf_id) = ch.workflow_id
                {
                    for interval in &m.rollouts {
                        let pct =
                            i64::from(interval.end_bucket.saturating_sub(interval.start_bucket));
                        rollout_overrides.insert((wf_id.clone(), interval.workflow_revision), pct);
                    }
                }
                active_channels.push(ch);
            }
            EntityKind::Workflow => {}
            EntityKind::Plugin => {
                let version = i64::try_from(m.entity.revision).unwrap_or(i64::MAX);
                let mut plugin = versioned::get_version::<Plugin>(
                    &state.db_pool,
                    &plugins::spec(),
                    &m.entity.id,
                    version,
                )
                .await
                .map_err(|_| {
                    crate::errors::OrionError::validation(format!(
                        "Manifest plugin '{}' revision {} definition not found",
                        m.entity.id, m.entity.revision
                    ))
                })?;
                plugin.status = EntityStatus::Active.as_str().to_string();
                active_plugins.push(plugin);
            }
            EntityKind::Model => {
                let version = i64::try_from(m.entity.revision).unwrap_or(i64::MAX);
                let mut model = versioned::get_version::<Model>(
                    &state.db_pool,
                    &models::spec(),
                    &m.entity.id,
                    version,
                )
                .await
                .map_err(|_| {
                    crate::errors::OrionError::validation(format!(
                        "Manifest model '{}' revision {} definition not found",
                        m.entity.id, m.entity.revision
                    ))
                })?;
                model.status = EntityStatus::Active.as_str().to_string();
                active_models.push(model);
            }
            EntityKind::Connection => {
                let conn = state
                    .composition
                    .deployments
                    .get_connection(&m.entity.id, m.entity.revision)
                    .ok_or_else(|| {
                        crate::errors::OrionError::validation(format!(
                            "Manifest connection '{}' revision {} not found",
                            m.entity.id, m.entity.revision
                        ))
                    })?;
                if conn.configuration_digest != m.definition_digest {
                    return Err(crate::errors::OrionError::validation(format!(
                        "Manifest connection '{}' revision {} digest mismatch",
                        m.entity.id, m.entity.revision
                    )));
                }
            }
            EntityKind::Source => {}
        }
    }

    for m in &manifest.members {
        if m.entity.kind != EntityKind::Workflow {
            continue;
        }
        let version = i64::try_from(m.entity.revision).unwrap_or(i64::MAX);
        let mut wf = if let Some(val) = parse_artifact_json(m)?
            && val.is_object()
            && val.get("tasks").is_some()
        {
            Workflow {
                workflow_id: m.entity.id.clone(),
                version,
                name: val
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&m.entity.id)
                    .to_string(),
                description: val
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                priority: val.get("priority").and_then(|v| v.as_i64()).unwrap_or(0),
                status: EntityStatus::Active.as_str().to_string(),
                rollout_percentage: val
                    .get("rollout_percentage")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(100),
                condition_json: serde_json::to_string(
                    val.get("condition")
                        .unwrap_or(&serde_json::Value::Bool(true)),
                )
                .unwrap_or_else(|_| "true".to_string()),
                tasks_json: serde_json::to_string(
                    val.get("tasks").unwrap_or(&serde_json::json!([])),
                )
                .unwrap_or_else(|_| "[]".to_string()),
                tags_json: serde_json::to_string(val.get("tags").unwrap_or(&serde_json::json!([])))
                    .unwrap_or_else(|_| "[]".to_string()),
                loop_json: val
                    .get("loop")
                    .filter(|v| !v.is_null())
                    .and_then(|v| serde_json::to_string(v).ok()),
                continue_on_error: val
                    .get("continue_on_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                created_at: now,
                updated_at: now,
            }
        } else {
            versioned::get_version::<Workflow>(
                &state.db_pool,
                &workflows::spec(),
                &m.entity.id,
                version,
            )
            .await
            .map_err(|_| {
                crate::errors::OrionError::validation(format!(
                    "Manifest workflow '{}' revision {} definition not found",
                    m.entity.id, m.entity.revision
                ))
            })?
        };

        wf.status = EntityStatus::Active.as_str().to_string();
        if let Some(&pct) = rollout_overrides.get(&(m.entity.id.clone(), m.entity.revision)) {
            wf.rollout_percentage = pct;
        } else if workflow_counts.get(&m.entity.id).copied().unwrap_or(1) == 1 {
            wf.rollout_percentage = 100;
        }
        active_workflows.push(wf);
    }

    active_channels.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.channel_id.cmp(&b.channel_id))
    });
    active_workflows.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.workflow_id.cmp(&b.workflow_id))
            .then_with(|| b.version.cmp(&a.version))
    });
    active_plugins.sort_by(|a, b| a.plugin_id.cmp(&b.plugin_id));
    active_models.sort_by(|a, b| a.model_id.cmp(&b.model_id));

    Ok(crate::storage::repositories::ActiveDefinitions {
        channels: active_channels,
        workflows: active_workflows,
        plugins: active_plugins,
        models: active_models,
    })
}

async fn reload_with_scope(
    state: &AppState,
    opts: ReloadOpts,
    scope: Option<crate::cluster::EpochScope>,
    evict_connector_pools: bool,
) -> Result<Arc<super::RuntimeGeneration>, crate::errors::OrionError> {
    let (head_rev, _) = state.composition.deployments.current_head();
    let manifest_rev = (head_rev > 0).then_some(head_rev);
    reload_with_manifest(
        state,
        opts,
        scope,
        evict_connector_pools,
        manifest_rev,
        false,
    )
    .await
}

async fn reload_with_manifest(
    state: &AppState,
    opts: ReloadOpts,
    scope: Option<crate::cluster::EpochScope>,
    evict_connector_pools: bool,
    manifest_revision: Option<u64>,
    enforce_publication_barrier: bool,
) -> Result<Arc<super::RuntimeGeneration>, crate::errors::OrionError> {
    let start = std::time::Instant::now();

    // One reload at a time, process-wide. Everything below is a
    // read-modify-write over the database rows and the published generation;
    // overlapping reloads read the same rows and the loser's publish wins.
    // See `AppStateInner::reload_lock`.
    let _reload_guard = state.reload_lock.lock().await;

    let prev_connector_state = if scope.is_some_and(|scope| scope.touches_connectors()) {
        Some((
            state.connector_registry.snapshot_configs(),
            state.connector_registry.config_generation(),
            state.connector_registry.load_issues().await,
        ))
    } else {
        None
    };

    let result = async {
        // Registry replacement belongs to the same serialized operation as
        // channel construction, including peer-driven and connector reloads.
        if scope.is_some_and(|scope| scope.touches_connectors()) {
            state
                .connector_registry
                .reload(state.repos.connectors.as_ref())
                .await?;
        }
        let crate::storage::repositories::ActiveDefinitions {
            channels,
            workflows: active_workflows,
            plugins: plugin_rows,
            models: model_rows,
        } = match manifest_revision {
            Some(target_rev) => manifest_definitions(state, target_rev).await?,
            None => state.repos.active_definitions().await?,
        };
        let channels = crate::engine::filter_channels(channels, &state.config.channel_filter);
        // The outgoing generation, read once: the base the new engine is built
        // from and the reuse cache the new channel estate is built against.
        // Reading it once is what makes those agree.
        let current = state.runtime.load();

        // The plugin half decides how the engine is built. An unchanged
        // active plugin set compiles on the running engine's compiler and
        // swaps with `with_program`, which keeps the `Registry` (its
        // functions, the plugin functions and the profile layer) and the
        // engine's config. A changed set — an activation, an archive, a new
        // digest — registers the loaded plugin functions on a registry
        // assembled afresh and builds a new engine. Either way every row is
        // compiled first and a workflow the registry cannot run is quarantined
        // per channel rather than failing the reload, both halves are built
        // before anything is published, and a failure here leaves the
        // previous generation serving whole. Back when the channel estate was
        // published first, this `?` could leave the node *permanently* mixed:
        // new guards over old workflows, until some later reload happened to
        // succeed.
        let plugins_changed =
            crate::plugin::PluginSet::fingerprint_of(&plugin_rows) != current.plugins.fingerprint();
        let (new_engine, functions, plugins, engine_issues) = if plugins_changed {
            let plugins = Arc::new(
                crate::plugin::load_active(
                    plugin_rows,
                    state.repos.plugins.as_ref(),
                    state.plugins.as_ref(),
                    &state.config.plugins,
                )
                .await,
            );
            let functions = Arc::new(
                crate::engine::FunctionRegistry::builtin()
                    .with_entries(plugins.entries())
                    .map_err(|e| crate::errors::OrionError::Config {
                        message: format!("plugin function registry: {e}"),
                    })?,
            );
            let compiler = plugin_compiler(state, &plugins);
            let loaded = crate::engine::build_engine_workflows(
                &channels,
                &active_workflows,
                &crate::engine::EngineLoad {
                    compiler: &compiler,
                    plugins: &plugins,
                },
            );
            let engine = crate::engine::serving_engine(compiler.link(loaded.workflows));
            (engine, functions, plugins, loaded.issues)
        } else {
            // The running engine's compiler: its registry, so a reload that
            // keeps the plugins registers nothing. The boot generation has no
            // engine to carry, and its registry is assembled as for a changed
            // set.
            let compiler = match &current.engine {
                Some(engine) => engine.compiler(),
                None => plugin_compiler(state, &current.plugins),
            };
            let loaded = crate::engine::build_engine_workflows(
                &channels,
                &active_workflows,
                &crate::engine::EngineLoad {
                    compiler: &compiler,
                    plugins: &current.plugins,
                },
            );
            let program = compiler.link(loaded.workflows);
            let engine = match &current.engine {
                Some(engine) => engine.with_program(program),
                None => crate::engine::serving_engine(program),
            };
            (
                engine,
                current.functions.clone(),
                current.plugins.clone(),
                loaded.issues,
            )
        };

        // The model half, over the outgoing generation's set (I188). Its
        // adapters were compiled on the process's expression engine, which
        // no reload replaces, so a model whose manifest is unchanged keeps
        // its compiled adapters and only a new or changed manifest is
        // compiled; `ModelSet::reload` carries them. A workflow naming a
        // model the set does not serve is quarantined with the reason, the
        // way a workflow naming an unavailable plugin function is.
        let models = Arc::new(crate::model::ModelSet::reload(
            &current.models,
            &model_rows,
            &state.config.models,
            state.models.is_some(),
            &state.exprs,
        ));
        let mut engine_issues = engine_issues;
        engine_issues.extend(super::models::load_issues(
            &channels,
            &active_workflows,
            &models,
        ));
        if manifest_revision.is_some() && !engine_issues.is_empty() {
            let details: Vec<String> = engine_issues
                .iter()
                .map(|i| format!("channel '{}': {}", i.channel, i.reason))
                .collect();
            return Err(crate::errors::OrionError::validation(format!(
                "Manifest workflow compilation failed: {}",
                details.join("; ")
            )));
        }

        // Channels that fail to load are quarantined — refused at every
        // ingress — and the reload proceeds (F35). It used to abort here,
        // which meant one unparseable `config_json` failed every activate,
        // archive, delete and rollout with a 500, and stopped the cluster
        // epoch watcher resyncing all nodes.
        // The quarantine set is recorded on the generation and read back with
        // `ChannelSnapshot::quarantined` (reported via `/health`), so it is
        // deliberately not propagated as an error here.
        let new_channels = state
            .channel_loader
            .build(
                &current.channels,
                &channels,
                crate::channel::ReloadDeps {
                    vars: state.vars.as_ref(),
                    connector_registry: &state.connector_registry,
                    cache_pool: &state.caches.cache_pool,
                    exprs: &state.exprs,
                    jwks: &state.jwks,
                    http_client: &state.http_client,
                    allow_private_token_urls: state.config.oauth2_login.allow_private_token_urls,
                    instance_oauth_providers: &state.config.oauth2_login.providers,
                    oidc_discovery: &state.oidc_discovery,
                    global_trace_storage: &state.config.trace_storage,
                    cron_enabled: state.config.cron.enabled,
                },
                engine_issues,
            )
            .await;

        if enforce_publication_barrier && let Some(target_rev) = manifest_revision {
            let local = state.composition.reconciler.local_status();
            let (latest_head, _) = state.composition.deployments.current_head();
            let desired = local.desired_revision.max(latest_head);
            if target_rev < desired || target_rev <= local.applied_revision {
                return Err(crate::errors::OrionError::Conflict(format!(
                    "candidate revision {target_rev} is superseded (applied={}, desired={desired})",
                    local.applied_revision
                )));
            }
        }

        if scope.is_some_and(|scope| scope.touches_connectors()) && evict_connector_pools {
            state.caches.evict_all_pools().await;
        }

        // One store, both halves. There is no window in which a reader is held
        // off, so this needs neither a timeout nor a carefully-scoped drop
        // before the Kafka restart below — and no window in which a request is
        // admitted by one generation and executed by another, which is what
        // two stores here could not avoid however they were ordered.
        let generation = state.runtime.publish_with_estate(
            new_engine,
            Arc::new(new_channels),
            functions,
            plugins,
            models,
            Some(state.connector_registry.snapshot_configs()),
            state.config.engine.run_budget_limits(),
        );
        let published = state.runtime.load();

        // Sweep the plugin compile cache down to what this generation serves.
        // The cache is a compile-time dedup keyed by digest that never shrank
        // on its own, so drafts, self-test failures, superseded versions,
        // deleted plugins and one-off `validate`s stayed resident until
        // restart. A loaded plugin's handler holds its own component, so this
        // cannot disturb serving; the slack spares recent uploads that are
        // compiled but not yet activated.
        if let Some(runtime) = state.plugins.as_ref() {
            let keep: std::collections::HashSet<&str> = published
                .plugins
                .plugins
                .iter()
                .map(|p| p.digest.as_str())
                .collect();
            let evicted = runtime.retain(&keep, crate::plugin::WasmRuntime::RETAIN_SLACK);
            if evicted > 0 {
                tracing::debug!(
                    evicted,
                    "swept superseded plugin components from the compile cache"
                );
            }
        }

        // Drop circuit breakers whose channel or connector this generation no
        // longer carries — a deleted connector or an archived channel — so
        // their entries do not sit in the map until LRU pressure reclaims them.
        // Runs on every node (the reload does), keyed off this reload's live
        // channels and the registry's live connectors.
        //
        // A breaker's stored channel is the routing identity stamped into
        // `metadata.channel` — the channel *name*, which is what `RouteTable`
        // and `require_serviceable` key on and what the `"{channel}:{connector}"`
        // breaker key is built from — not the `channel_id` storage PK (a UUID
        // in general). Pruning against `channel_id` never matched a single
        // breaker's channel, so every reload wiped the whole map — the exact
        // fan-out the cluster breaker-reset test pins.
        let live_channels: std::collections::HashSet<&str> =
            channels.iter().map(|c| c.name.as_str()).collect();
        state
            .connector_registry
            .prune_breakers(&live_channels)
            .await;

        // Warm what `models.preload` selects, off the publish's critical
        // path: the generation serves now, and a request that arrives before
        // its model is resident shares the load in flight.
        super::models::spawn_preload(
            super::models::PreloadDeps::from_state(state),
            state.runtime.load(),
            &active_workflows,
        );

        // Update active workflows gauge
        crate::metrics::set_active_workflows(active_workflows.len() as f64);

        // Restart Kafka consumer if async channel topics changed
        if state.config.kafka.enabled {
            restart_kafka_consumer_if_needed(state, &channels, opts).await;
        }

        tracing::info!(
            generation,
            workflow_count = active_workflows.len(),
            channel_count = channels.len(),
            "Runtime generation published"
        );
        Ok(published)
    }
    .await;

    let duration = start.elapsed().as_secs_f64();
    crate::metrics::record_engine_reload_duration(duration);

    // The degraded flag is set here rather than at the call sites, so it holds
    // for every way a reload is triggered — an admin mutation, an explicit
    // `POST /engine/reload`, and the cluster epoch watcher's resync. A failure
    // leaves this node serving the *previous* generation: correct, but not what
    // the database says, and nothing else would report that.
    match &result {
        Ok(_) => {
            crate::metrics::record_engine_reload("success");
            state
                .reload_degraded
                .store(false, std::sync::atomic::Ordering::Release);
        }
        Err(e) => {
            if let Some((prev_configs, prev_gen, prev_issues)) = prev_connector_state {
                state
                    .connector_registry
                    .restore_snapshot(prev_configs, prev_gen, prev_issues)
                    .await;
            }
            crate::metrics::record_engine_reload("failure");
            state
                .reload_degraded
                .store(true, std::sync::atomic::Ordering::Release);
            tracing::error!(
                error = %e,
                "Engine reload failed: this node is still serving the previous \
                 generation, so its running config no longer matches the database"
            );
        }
    }

    result
}

/// A dataflow-rs 4 compiler over a registry assembled afresh from this
/// node's live components ([`super::build_registry`]) with `plugins`'
/// functions registered: what a reload that changes the plugin set compiles
/// on, as its dataflow-rs 3 half assembles its handlers afresh.
pub(crate) fn plugin_compiler(
    state: &AppState,
    plugins: &crate::plugin::PluginSet,
) -> dataflow_rs::program::Compiler {
    let mut registry = super::build_registry(state);
    crate::engine::register_plugin_functions(&mut registry, plugins.functions().cloned());
    registry.compiler(dataflow_rs::program::CompileOptions::default())
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

    let mut handle_guard = state.kafka.consumer_handle.lock().await;

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
        // A shutdown that times out (a hung broker) hands back the still-running
        // task; park it so repeated reloads during an outage cannot accumulate
        // rdkafka clients without bound.
        if let Some(straggler) = old_handle.shutdown().await {
            state.kafka.stragglers.park(straggler);
        }
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
            state.kafka.ingest_status.set_degraded(false);
        }
        Ok(None) => {
            tracing::info!("No Kafka topics configured or from DB, consumer not started");
            state.kafka.ingest_status.set_degraded(false);
        }
        Err(e) => {
            state.kafka.ingest_status.set_degraded(true);
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
        crate::bootstrap::IngestDeps {
            runtime: state.runtime.clone(),
            exprs: state.exprs.clone(),
            vars: state.vars.clone(),
            kafka_producer: state.kafka.producer.clone(),
            instance_id: state
                .cluster
                .enabled
                .then(|| state.cluster.instance_id.clone()),
            trace_repo: state.repos.traces.clone(),
            persistence_queue: state.trace_persistence_queue.clone(),
            max_result_size_bytes: state.config.trace_queue.max_result_size_bytes,
        },
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
    if !state.kafka.ingest_status.claim_supervisor() {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        let mut backoff_ms = crate::kafka::consumer::INITIAL_RETRY_BACKOFF_MS;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            let mut handle_guard = state.kafka.consumer_handle.lock().await;
            // Draining: do not resurrect a consumer mid-shutdown. (Checked
            // under the mutex like every other exit, so even this release
            // cannot race a failing reload's claim — and a spawn lost to a
            // drain-window race would only ever supervise a node that is
            // shutting down.)
            if !state.ready.load(std::sync::atomic::Ordering::Acquire) {
                state.kafka.ingest_status.release_supervisor();
                break;
            }
            if handle_guard.is_some() {
                // Another reload already restarted the consumer.
                state.kafka.ingest_status.release_supervisor();
                break;
            }
            let channels = match state.repos.channels.list_active().await {
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
                    state.kafka.ingest_status.release_supervisor();
                    state.kafka.ingest_status.set_degraded(false);
                    tracing::info!("Kafka consumer restored by restart supervisor");
                    break;
                }
                Ok(None) => {
                    // Nothing to ingest any more — idle, not degraded.
                    state.kafka.ingest_status.release_supervisor();
                    state.kafka.ingest_status.set_degraded(false);
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
