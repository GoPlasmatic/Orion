//! Building and publishing a deployment revision.
//!
//! The one way a node's serving generation changes after boot: the node
//! reconciler (the v2 apply, rollback and the cluster reconcile loop) names a
//! committed revision, and this builds every member of its manifest and
//! publishes the result as one generation. A node with no committed
//! deployment serves the empty generation bootstrap published.

use std::sync::Arc;

use crate::runtime::state::AppState;

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

    state.reconciler.observe_desired_head();
    let build_res = build_and_publish(state, target_revision).await;

    let mut build_issues = Vec::new();
    match build_res {
        Ok(published) => {
            if let Some(rev_row) = state.deployments.get_revision(target_revision) {
                for q in published.channels.quarantined() {
                    let rev = rev_row
                        .manifest
                        .members
                        .iter()
                        .find(|m| {
                            m.entity.kind == EntityKind::Channel && m.entity.id == q.channel_id
                        })
                        .map(|m| m.entity.revision)
                        .unwrap_or(crate::extensions::Revision::FIRST);
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
        .reconciler
        .build_and_publish_candidate(target_revision, build_issues);
    let _ = state
        .reconciler
        .persist_local_status_to_db_pool(&state.db_pool)
        .await;
    status_res
}

/// What one manifest's members build into: the rows the generation builder
/// reads, and the plugin components by digest.
struct ManifestDefinitions {
    channels: Vec<crate::storage::models::Channel>,
    workflows: Vec<crate::storage::models::Workflow>,
    plugins: Vec<crate::storage::models::Plugin>,
    plugin_components: std::collections::HashMap<String, Vec<u8>>,
    /// What the manifest's connections over workflow packages may call as
    /// their dependencies.
    dependencies: std::collections::BTreeMap<
        String,
        (
            crate::extensions::ConnectionReference,
            std::collections::BTreeSet<String>,
        ),
    >,
}

async fn manifest_definitions(
    state: &AppState,
    manifest: &crate::extensions::deployment_store::CanonicalManifest,
) -> Result<ManifestDefinitions, crate::errors::OrionError> {
    use std::collections::HashMap;

    use orion_api::v2::Definition;

    use crate::errors::OrionError;
    use crate::extensions::deployment_store::{ArtifactState, EntityKind, ManifestMember};

    for p in &manifest.providers {
        let Some(adm) = state.deployments.get_admission(&p.digest) else {
            return Err(OrionError::validation(format!(
                "provider '{}' ({}) is not admitted",
                p.provider_id, p.digest
            )));
        };
        if adm.revoked
            || state
                .composition
                .admission()
                .ledger()
                .is_digest_revoked(&p.digest)
        {
            return Err(OrionError::validation(format!(
                "provider '{}' ({}) is revoked",
                p.provider_id, p.digest
            )));
        }
    }

    // Every member is read from the definition it names, and from nowhere
    // else: a member whose definition is missing, inactive, or of another
    // kind fails the build rather than serving something else. All of them
    // are read in one round trip.
    let digests: Vec<String> = manifest
        .members
        .iter()
        .filter(|m| {
            matches!(
                m.entity.kind,
                EntityKind::Channel | EntityKind::Workflow | EntityKind::Plugin
            )
        })
        .map(|m| m.definition_digest.clone())
        .collect();
    let stored = state
        .deployments
        .fetch_artifacts(&state.db_pool, &digests)
        .await
        .map_err(|e| OrionError::internal(format!("reading the manifest's definitions: {e}")))?;
    let definition = |member: &ManifestMember| -> Result<Definition, OrionError> {
        let what = || {
            format!(
                "{} '{}' revision {}",
                member.entity.kind.as_str(),
                member.entity.id,
                member.entity.revision
            )
        };
        // Two members may name one definition, so it is borrowed, not taken.
        let artifact = stored.get(&member.definition_digest).ok_or_else(|| {
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
        let bytes = artifact.bytes.as_deref().ok_or_else(|| {
            OrionError::validation(format!(
                "{}: definition '{}' has no stored bytes",
                what(),
                member.definition_digest
            ))
        })?;
        let parsed = super::definitions::parse(bytes)
            .map_err(|e| OrionError::validation(format!("{}: {e}", what())))?;
        if parsed.kind() != member.entity.kind {
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
                    m.entity.revision.get(),
                    workflow_id,
                )?);
            }
            EntityKind::Workflow => {}
            EntityKind::Plugin => {
                let Definition::Plugin(def) = definition(m)? else {
                    unreachable!("kind checked above")
                };
                let (row, bytes) =
                    super::definitions::plugin_row(&def, &m.entity.id, m.entity.revision.get())?;
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
                workflow_package_serves(state, &conn).await?;
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
        let mut wf = super::definitions::workflow_row(&def, &m.entity.id, m.entity.revision.get())?;
        if let Some(&pct) = rollout_overrides.get(&(m.entity.id.clone(), m.entity.revision.get())) {
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

    let dependencies =
        crate::extensions::workflow_provider::manifest_dependencies(&state.deployments, manifest)
            .map_err(OrionError::validation)?;

    Ok(ManifestDefinitions {
        channels: active_channels,
        workflows: active_workflows,
        plugins: active_plugins,
        plugin_components,
        dependencies,
    })
}

/// A connection over a workflow provider package is deployed only while the
/// package is admitted, still passes the hermetic check it passed at upload,
/// and is registered on this node.
async fn workflow_package_serves(
    state: &AppState,
    conn: &crate::extensions::deployment_store::ConnectionRevisionRow,
) -> Result<(), crate::errors::OrionError> {
    use crate::errors::OrionError;
    use crate::extensions::ImplementationKind;

    let store = &state.deployments;
    let Some(package) = store
        .get_package(&conn.provider_digest)
        .filter(|p| p.implementation == ImplementationKind::Workflow)
    else {
        return Ok(());
    };
    let what = || {
        format!(
            "connection '{}' runs on the workflow package '{}' ({})",
            conn.connection_id, package.provider_id, package.package_digest
        )
    };
    let admitted = store
        .get_admission(&package.package_digest)
        .is_some_and(|a| !a.revoked)
        && !state
            .composition
            .admission()
            .ledger()
            .is_digest_revoked(&package.package_digest);
    if !admitted {
        return Err(OrionError::validation(format!(
            "{}, which is not admitted",
            what()
        )));
    }
    if let Err(problems) = crate::extensions::workflow_provider::WorkflowPackage::from_store(
        store,
        &state.db_pool,
        &package,
    )
    .await
    {
        return Err(OrionError::validation(format!(
            "{}, which this node refuses: {}",
            what(),
            crate::extensions::workflow_provider::describe(&problems)
        )));
    }
    if state
        .composition
        .providers()
        .get(ImplementationKind::Workflow, &package.package_digest)
        .is_none()
    {
        return Err(OrionError::validation(format!(
            "{}, which is not registered on this node",
            what()
        )));
    }
    Ok(())
}

/// Two channels of one deployment claiming one route at one priority would
/// serve it arbitrarily, the second one dead: the revision fails, naming
/// both. So does a channel a platform route shadows: under
/// `server.data_mounts` a channel's route is also served at
/// `{mount}{route}`, and a platform route there wins, so the channel would
/// never run (#279) — only the `"/"` mount can produce one.
fn refuse_route_collisions(
    channels: &[crate::storage::models::Channel],
    drivers: &crate::extensions::ChannelDriverRegistry,
    data_mounts: &[String],
) -> Result<(), crate::errors::OrionError> {
    use crate::channel::routing::{declared_route, methods_overlap};
    let routes: Vec<_> = channels
        .iter()
        .map(|c| (c, declared_route(c, drivers)))
        .collect();
    for (channel, declared) in &routes {
        for (route, _) in declared {
            for mount in data_mounts {
                let served = if mount == "/" {
                    route.clone()
                } else {
                    format!("{mount}{route}")
                };
                if let Some(platform) =
                    orion_common::platform_routes::shadowed_platform_route(&served)
                {
                    return Err(crate::errors::OrionError::validation(format!(
                        "channel '{}': with server.data_mounts entry '{mount}' its route \
                         would be served at {served}, which collides with the platform \
                         route '{platform}'. The platform route wins, so the channel would \
                         never run. Change the route_pattern or the mount.",
                        channel.channel_id
                    )));
                }
            }
        }
    }
    for (i, (first, first_routes)) in routes.iter().enumerate() {
        for (second, second_routes) in &routes[i + 1..] {
            if first.priority != second.priority {
                continue;
            }
            for (route, methods) in first_routes {
                for (other_route, other_methods) in second_routes {
                    if route == other_route && methods_overlap(methods, other_methods) {
                        return Err(crate::errors::OrionError::validation(format!(
                            "channels '{}' and '{}' both claim {route} at priority {}: give \
                             one another route, narrower methods or a higher priority",
                            first.channel_id, second.channel_id, first.priority
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

/// The connectors a manifest generation serves the built-in functions
/// through: each Connection member over a built-in driver, named by its id,
/// its configuration the revision's `public_config` with its secret
/// references in place. A Connection over an uploaded provider serves only
/// `connector_call` and is not here.
fn manifest_connectors(
    state: &AppState,
    manifest: &crate::extensions::deployment_store::CanonicalManifest,
) -> Result<Vec<crate::connector::StoredConnector>, crate::errors::OrionError> {
    use crate::extensions::deployment_store::EntityKind;
    let mut connectors = Vec::new();
    for member in &manifest.members {
        if member.entity.kind != EntityKind::Connection {
            continue;
        }
        let Some(row) = state
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
            name: row.connection_id.to_string(),
            connector_type: connector_type.to_string(),
            config_json: serde_json::to_string(&config)?,
        });
    }
    Ok(connectors)
}

/// Build every member of `revision`'s manifest and publish the result as one
/// generation, unless a newer revision is already desired or applied here.
async fn build_and_publish(
    state: &AppState,
    revision: u64,
) -> Result<Arc<super::RuntimeGeneration>, crate::errors::OrionError> {
    let start = std::time::Instant::now();

    // One build at a time, process-wide. Everything below is a
    // read-modify-write over the published generation; overlapping builds
    // read the same generation and the loser's publish wins. See
    // `AppStateInner::reload_lock`.
    let _reload_guard = state.reload_lock.lock().await;

    // A generation takes its connectors from the manifest's Connections,
    // every time. A failed build restores what the registry held.
    let prev_connector_state = (
        state.connector_registry.snapshot_configs(),
        state.connector_registry.config_generation(),
        state.connector_registry.load_issues().await,
    );

    let result = async {
        let manifest = state
            .deployments
            .revision(&state.db_pool, revision)
            .await
            .map_err(|e| crate::errors::OrionError::internal(e.to_string()))?
            .map(|rev| rev.manifest.clone())
            .ok_or_else(|| {
                crate::errors::OrionError::NotFound(format!(
                    "Deployment revision {revision} not found"
                ))
            })?;
        // What the manifest names and a peer stored is read in first, and
        // the admitted workflow packages are registered before anything that
        // deploys a connection over one is built.
        state
            .deployments
            .load_for_manifest(&state.db_pool, &manifest)
            .await
            .map_err(|e| crate::errors::OrionError::internal(e.to_string()))?;
        super::workflow_providers::sync(state).await;
        // Registry replacement belongs to the same serialized operation as
        // channel construction.
        state
            .connector_registry
            .load(manifest_connectors(state, &manifest)?)
            .await;
        let ManifestDefinitions {
            channels,
            workflows: active_workflows,
            plugins: plugin_rows,
            plugin_components,
            dependencies,
        } = manifest_definitions(state, &manifest).await?;
        let channels = crate::engine::filter_channels(channels, &state.config.channel_filter);
        refuse_route_collisions(
            &channels,
            state.composition.channels(),
            &state.config.server.data_mounts,
        )?;
        // Each workflow is granted the connections its member requires.
        let connector_authority = Arc::new(
            super::authority::ConnectorAuthority::from_manifest(&manifest, &active_workflows)
                .with_dependencies(dependencies),
        );
        // The outgoing generation, read once: the base the new engine is built
        // from and the reuse cache the new channel estate is built against.
        // Reading it once is what makes those agree.
        let current = state.runtime.load();

        let EngineHalf {
            engine: new_engine,
            functions,
            plugins,
            issues: engine_issues,
        } = compile_engine(
            state,
            &current,
            &channels,
            &active_workflows,
            plugin_rows,
            &plugin_components,
        )
        .await?;
        refuse_engine_issues(&engine_issues)?;

        // Channels that fail to load are quarantined — refused at every
        // ingress — and the build proceeds (F35). The quarantine set is
        // recorded on the generation and read back with
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
                    channel_drivers: state.composition.channels(),
                },
                engine_issues,
            )
            .await;

        // The publication barrier: a revision older than what this node
        // already desires or applied never replaces it.
        let local = state.reconciler.local_status();
        let (latest_head, _) = state.deployments.current_head();
        let desired = local.desired_revision.max(latest_head);
        if revision < desired || revision <= local.applied_revision {
            return Err(crate::errors::OrionError::Conflict(format!(
                "candidate revision {revision} is superseded (applied={}, desired={desired})",
                local.applied_revision
            )));
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
    // for every way a build is triggered — an apply, a rollback, and the
    // cluster reconcile loop. A failure leaves this node serving the
    // *previous* generation: correct, but not what is committed, and nothing
    // else would report that.
    match &result {
        Ok(_) => {
            crate::metrics::record_engine_reload("success");
            state
                .reload_degraded
                .store(false, std::sync::atomic::Ordering::Release);
        }
        Err(e) => {
            let (prev_configs, prev_gen, prev_issues) = prev_connector_state;
            state
                .connector_registry
                .restore_snapshot(prev_configs, prev_gen, prev_issues)
                .await;
            crate::metrics::record_engine_reload("failure");
            state
                .reload_degraded
                .store(true, std::sync::atomic::Ordering::Release);
            tracing::error!(
                error = %e,
                "Deployment build failed: this node is still serving the previous \
                 generation, so it does not serve what is committed"
            );
        }
    }

    result
}

/// The engine half of a candidate generation, compiled but not published.
struct EngineHalf {
    engine: dataflow_rs::Engine,
    functions: Arc<crate::engine::FunctionRegistry>,
    plugins: Arc<crate::plugin::PluginSet>,
    issues: Vec<crate::channel::ChannelLoadIssue>,
}

/// Compile `workflows` into the engine a generation following `current`
/// would serve. Builds nothing `current` holds and publishes nothing.
async fn compile_engine(
    state: &AppState,
    current: &super::RuntimeGeneration,
    channels: &[crate::storage::models::Channel],
    workflows: &[crate::storage::models::Workflow],
    plugin_rows: Vec<crate::storage::models::Plugin>,
    plugin_components: &std::collections::HashMap<String, Vec<u8>>,
) -> Result<EngineHalf, crate::errors::OrionError> {
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
    let half = if plugins_changed {
        let plugins = Arc::new(
            crate::plugin::load_active(
                plugin_rows,
                plugin_components,
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
            channels,
            workflows,
            &crate::engine::EngineLoad {
                compiler: &compiler,
                plugins: &*plugins,
            },
        );
        let engine = crate::engine::serving_engine(compiler.link(loaded.workflows));
        EngineHalf {
            engine,
            functions,
            plugins,
            issues: loaded.issues,
        }
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
            channels,
            workflows,
            &crate::engine::EngineLoad {
                compiler: &compiler,
                plugins: &*current.plugins,
            },
        );
        let program = compiler.link(loaded.workflows);
        let engine = if boot {
            crate::engine::serving_engine(program)
        } else {
            current.engine.with_program(program)
        };
        EngineHalf {
            engine,
            functions: current.functions.clone(),
            plugins: current.plugins.clone(),
            issues: loaded.issues,
        }
    };
    Ok(half)
}

/// A workflow the engine could not compile fails the whole revision.
fn refuse_engine_issues(
    issues: &[crate::channel::ChannelLoadIssue],
) -> Result<(), crate::errors::OrionError> {
    if issues.is_empty() {
        return Ok(());
    }
    let details: Vec<String> = issues
        .iter()
        .map(|i| format!("channel '{}': {}", i.channel, i.reason))
        .collect();
    Err(crate::errors::OrionError::validation(format!(
        "Manifest workflow compilation failed: {}",
        details.join("; ")
    )))
}

/// Whether `manifest` would build on this node: every member read from its
/// definition, the routes free, and every workflow compiled against the
/// plugins the manifest carries. What a deployment plan and apply ask before
/// committing a candidate, so a head the node cannot build is refused rather
/// than committed and left unconverged. Compiles against the serving
/// generation and publishes nothing; channel-level quarantines (a guard that
/// cannot compile) still only quarantine.
pub async fn check_candidate(
    state: &AppState,
    manifest: &crate::extensions::deployment_store::CanonicalManifest,
) -> Result<(), crate::errors::OrionError> {
    state
        .deployments
        .load_for_manifest(&state.db_pool, manifest)
        .await
        .map_err(|e| crate::errors::OrionError::internal(e.to_string()))?;
    super::workflow_providers::sync(state).await;
    let ManifestDefinitions {
        channels,
        workflows,
        plugins,
        plugin_components,
        dependencies: _,
    } = manifest_definitions(state, manifest).await?;
    let channels = crate::engine::filter_channels(channels, &state.config.channel_filter);
    refuse_route_collisions(
        &channels,
        state.composition.channels(),
        &state.config.server.data_mounts,
    )?;
    let current = state.runtime.load();
    let half = compile_engine(
        state,
        &current,
        &channels,
        &workflows,
        plugins,
        &plugin_components,
    )
    .await?;
    refuse_engine_issues(&half.issues)
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
