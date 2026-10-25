//! Engine reload.
//!
//! R25: this lived in `server/routes/mod.rs` — 226 lines of engine and
//! consumer lifecycle with no HTTP in them, in a file whose `api_routes()` is fourteen
//! lines and whose name says *route table*. The admin handlers that trigger a
//! reload and the cluster epoch watcher both call in here; neither is a route.

use std::sync::Arc;

use crate::server::state::AppState;

/// Reload the engine with all active channels and workflows from the database.
///
/// Rebuilds and publishes the next generation, returning it: the caller that
/// caused a reload learns what *that* generation could not load, with no
/// window in which another reload's generation stands in for it — this is
/// read under `reload_lock`, right after the publish.
#[tracing::instrument(skip(state))]
pub async fn reload_engine(
    state: &AppState,
) -> Result<Arc<super::RuntimeGeneration>, crate::errors::OrionError> {
    reload_with_scope(state, None, PoolEviction::Keep).await
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
    reload_with_scope(state, Some(scope), PoolEviction::OnConnectorChange).await
}

/// Local connector writes already invalidate the affected names. Rebuild
/// their dependent guards without dropping every unrelated connection pool.
pub(crate) async fn reload_connectors_after_commit(
    state: &AppState,
) -> Result<Arc<super::RuntimeGeneration>, crate::errors::OrionError> {
    reload_with_scope(
        state,
        Some(crate::cluster::EpochScope::Connectors),
        PoolEviction::Keep,
    )
    .await
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
        Reload {
            scope: None,
            pools: PoolEviction::Keep,
            manifest_revision: Some(target_revision),
            barrier: PublicationBarrier::Enforce,
        },
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

    use orion_api::v2::Definition;

    use crate::errors::OrionError;
    use crate::extensions::deployment_store::{ArtifactState, EntityKind, ManifestMember};

    let rev_row = state
        .composition
        .deployments
        .get_revision(target_revision)
        .ok_or_else(|| {
            OrionError::NotFound(format!("Deployment revision {target_revision} not found"))
        })?;
    let manifest = &rev_row.manifest;

    for p in &manifest.providers {
        let Some(adm) = state.composition.deployments.get_admission(&p.digest) else {
            return Err(OrionError::validation(format!(
                "provider '{}' ({}) is not admitted",
                p.provider_id, p.digest
            )));
        };
        if adm.revoked || state.composition.admission.is_digest_revoked(&p.digest) {
            return Err(OrionError::validation(format!(
                "provider '{}' ({}) is revoked",
                p.provider_id, p.digest
            )));
        }
    }

    // Every member is read from the definition it names, and from nowhere
    // else: a member whose definition is missing, inactive, or of another
    // kind fails the build rather than serving something else.
    let definition = |member: &ManifestMember| -> Result<Definition, OrionError> {
        let what = || {
            format!(
                "{} '{}' revision {}",
                member.entity.kind.as_str(),
                member.entity.id,
                member.entity.revision
            )
        };
        let artifact = state
            .composition
            .deployments
            .get_artifact(&member.definition_digest)
            .ok_or_else(|| {
                OrionError::validation(format!(
                    "{}: definition '{}' is not stored",
                    what(),
                    member.definition_digest
                ))
            })?;
        if artifact.state != ArtifactState::Active {
            return Err(OrionError::validation(format!(
                "{}: definition '{}' is not ACTIVE",
                what(),
                member.definition_digest
            )));
        }
        let bytes = artifact.bytes.ok_or_else(|| {
            OrionError::validation(format!(
                "{}: definition '{}' has no stored bytes",
                what(),
                member.definition_digest
            ))
        })?;
        let parsed = super::definitions::parse(&bytes)
            .map_err(|e| OrionError::validation(format!("{}: {e}", what())))?;
        if parsed.kind() != member.entity.kind.as_str() {
            return Err(OrionError::validation(format!(
                "{}: names a {} definition",
                what(),
                parsed.kind()
            )));
        }
        Ok(parsed)
    };

    let mut active_channels = Vec::new();
    let mut active_workflows = Vec::new();
    let mut active_plugins = Vec::new();
    let mut plugin_components = HashMap::new();
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
                let Definition::Channel(def) = definition(m)? else {
                    unreachable!("kind checked above")
                };
                let workflow_id = m
                    .requires
                    .iter()
                    .find(|r| r.kind == EntityKind::Workflow)
                    .map(|r| r.id.clone());
                for interval in &m.rollouts {
                    if let Some(ref wf_id) = workflow_id {
                        let pct =
                            i64::from(interval.end_bucket.saturating_sub(interval.start_bucket));
                        rollout_overrides.insert((wf_id.clone(), interval.workflow_revision), pct);
                    }
                }
                active_channels.push(super::definitions::channel_row(
                    &def,
                    &m.entity.id,
                    m.entity.revision,
                    workflow_id,
                )?);
            }
            EntityKind::Workflow => {}
            EntityKind::Plugin => {
                let Definition::Plugin(def) = definition(m)? else {
                    unreachable!("kind checked above")
                };
                let (row, bytes) =
                    super::definitions::plugin_row(&def, &m.entity.id, m.entity.revision)?;
                plugin_components.insert(row.digest.clone(), bytes);
                active_plugins.push(row);
            }
            EntityKind::Model => {
                return Err(OrionError::validation(format!(
                    "model '{}' revision {}: models are not deployable through a manifest",
                    m.entity.id, m.entity.revision
                )));
            }
            EntityKind::Connection => {
                let conn = state
                    .composition
                    .deployments
                    .get_connection(&m.entity.id, m.entity.revision)
                    .ok_or_else(|| {
                        OrionError::validation(format!(
                            "Manifest connection '{}' revision {} not found",
                            m.entity.id, m.entity.revision
                        ))
                    })?;
                if conn.configuration_digest != m.definition_digest {
                    return Err(OrionError::validation(format!(
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
        let Definition::Workflow(def) = definition(m)? else {
            unreachable!("kind checked above")
        };
        let mut wf = super::definitions::workflow_row(&def, &m.entity.id, m.entity.revision)?;
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

    Ok(crate::storage::repositories::ActiveDefinitions {
        channels: active_channels,
        workflows: active_workflows,
        plugins: active_plugins,
        plugin_components,
        models: Vec::new(),
    })
}

/// The connectors a manifest generation serves the built-in functions
/// through: each Connection member over a built-in driver, named by its id,
/// its configuration the revision's `public_config` with its secret
/// references in place. A Connection over an uploaded provider serves only
/// `connector_call` and is not here.
fn manifest_connectors(
    state: &AppState,
    target_revision: u64,
) -> Result<Vec<crate::connector::StoredConnector>, crate::errors::OrionError> {
    use crate::extensions::deployment_store::EntityKind;
    let revision = state
        .composition
        .deployments
        .get_revision(target_revision)
        .ok_or_else(|| {
            crate::errors::OrionError::NotFound(format!(
                "Deployment revision {target_revision} not found"
            ))
        })?;
    let mut connectors = Vec::new();
    for member in &revision.manifest.members {
        if member.entity.kind != EntityKind::Connection {
            continue;
        }
        let Some(row) = state
            .composition
            .deployments
            .get_connection(&member.entity.id, member.entity.revision)
        else {
            continue;
        };
        let Some(driver) = crate::extensions::foundation::builtin_driver(&row.provider_digest)
        else {
            continue;
        };
        let Some((connector_type, config)) =
            crate::extensions::foundation::builtin_connector(driver, row.config_with_locators())
        else {
            continue;
        };
        connectors.push(crate::connector::StoredConnector {
            id: format!("{}@{}", row.connection_id, row.revision),
            name: row.connection_id.clone(),
            connector_type: connector_type.to_string(),
            config_json: serde_json::to_string(&config)?,
        });
    }
    Ok(connectors)
}

/// Whether a reload that changes connectors drops the cached pools built
/// from them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PoolEviction {
    /// The caller has already evicted what its change invalidated (a local
    /// connector write), or nothing it does can invalidate a pool.
    Keep,
    /// Evict every pool when connectors changed: a peer's change, whose
    /// names this node does not know.
    OnConnectorChange,
}

/// Whether the publish waits on the node reconciler's publication barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublicationBarrier {
    Enforce,
    Skip,
}

/// What one reload builds and how it publishes.
#[derive(Debug, Clone, Copy)]
struct Reload {
    /// What the triggering change touched; `None` is a full reload.
    scope: Option<crate::cluster::EpochScope>,
    pools: PoolEviction,
    /// The committed deployment revision to build from, or the v1 tables.
    manifest_revision: Option<u64>,
    barrier: PublicationBarrier,
}

async fn reload_with_scope(
    state: &AppState,
    scope: Option<crate::cluster::EpochScope>,
    pools: PoolEviction,
) -> Result<Arc<super::RuntimeGeneration>, crate::errors::OrionError> {
    let (head_rev, _) = state.composition.deployments.current_head();
    reload_with_manifest(
        state,
        Reload {
            scope,
            pools,
            manifest_revision: (head_rev > 0).then_some(head_rev),
            barrier: PublicationBarrier::Skip,
        },
    )
    .await
}

async fn reload_with_manifest(
    state: &AppState,
    reload: Reload,
) -> Result<Arc<super::RuntimeGeneration>, crate::errors::OrionError> {
    let Reload {
        scope,
        pools,
        manifest_revision,
        barrier,
    } = reload;
    let evict_connector_pools = pools == PoolEviction::OnConnectorChange;
    let enforce_publication_barrier = barrier == PublicationBarrier::Enforce;
    let start = std::time::Instant::now();

    // One reload at a time, process-wide. Everything below is a
    // read-modify-write over the database rows and the published generation;
    // overlapping reloads read the same rows and the loser's publish wins.
    // See `AppStateInner::reload_lock`.
    let _reload_guard = state.reload_lock.lock().await;

    // A generation built from a manifest takes its connectors from the
    // manifest's Connections, every time; one built from the v1 tables
    // reloads them when the change touched connectors.
    let loads_connectors =
        manifest_revision.is_some() || scope.is_some_and(|scope| scope.touches_connectors());
    let connector_generation_before = state.connector_registry.config_generation();
    let prev_connector_state = if loads_connectors {
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
        match manifest_revision {
            Some(revision) => {
                state
                    .connector_registry
                    .load(manifest_connectors(state, revision)?)
                    .await;
            }
            None if loads_connectors => {
                state
                    .connector_registry
                    .reload(state.repos.connectors.as_ref())
                    .await?;
            }
            None => {}
        }
        let crate::storage::repositories::ActiveDefinitions {
            channels,
            workflows: active_workflows,
            plugins: plugin_rows,
            plugin_components,
            models: model_rows,
        } = match manifest_revision {
            Some(target_rev) => manifest_definitions(state, target_rev).await?,
            None => state.repos.active_definitions().await?,
        };
        let channels = crate::engine::filter_channels(channels, &state.config.channel_filter);
        // A generation built from a manifest grants each workflow the
        // connections its member requires; any other grants none.
        let connector_authority = Arc::new(
            manifest_revision
                .and_then(|rev| state.composition.deployments.get_revision(rev))
                .map(|rev| {
                    super::authority::ConnectorAuthority::from_manifest(
                        &rev.manifest,
                        &active_workflows,
                    )
                })
                .unwrap_or_default(),
        );
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
                    &plugin_components,
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
            // keeps the plugins registers nothing. The boot generation's
            // engine is empty, with no functions to carry, so its registry is
            // assembled as for a changed set.
            let boot = current.id == 0;
            let compiler = if boot {
                plugin_compiler(state, &current.plugins)
            } else {
                current.engine.compiler()
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
            let engine = if boot {
                crate::engine::serving_engine(program)
            } else {
                current.engine.with_program(program)
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

        // A connector change evicts every pool even when no config text
        // moved: the secret behind an unchanged reference may have rotated.
        // A manifest rebuild evicts when its connector set moved, and keeps
        // the pools of a rebuild that changed nothing about them.
        let connectors_moved =
            state.connector_registry.config_generation() != connector_generation_before;
        let connector_change = scope.is_some_and(|scope| scope.touches_connectors());
        if evict_connector_pools && (connector_change || (loads_connectors && connectors_moved)) {
            state.caches.evict_all_pools().await;
        }

        // One store, both halves. There is no window in which a reader is held
        // off, so this needs neither a timeout nor a carefully-scoped drop
        // — and no window in which a request is
        // admitted by one generation and executed by another, which is what
        // two stores here could not avoid however they were ordered.
        let generation = state
            .runtime
            .publish_parts(super::generation::GenerationParts {
                connectors: Some(state.connector_registry.snapshot_configs()),
                run_budget_limits: state.config.engine.run_budget_limits(),
                connector_authority,
                ..super::generation::GenerationParts::new(
                    new_engine,
                    Arc::new(new_channels),
                    functions,
                    plugins,
                    models,
                )
            });
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
