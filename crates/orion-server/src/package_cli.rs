//! `orion-server package` — the packaging half of the K-stream promotion
//! design.
//!
//! A package is the channels, workflows, and connectors of one service,
//! named and versioned so the service ships between instances as a unit —
//! the module boundary of Orion's modular-monolith model. One instance runs
//! many packages; each promotes and rolls back independently.
//!
//! The server provides per-kind primitives (upsert import, activation
//! pre-flight, deferred reload, package receipts); the library's
//! [`orion::package`] composes them into lint, apply and prune, and this
//! module is the CLI shell over it: flags, the HTTP client, and the verbs
//! only an operator runs (`export`, `plan`, `diff`). Everything talks to a
//! running instance's admin API over HTTP (`--server` +
//! `ORION_ADMIN_TOKEN`), except `lint`, which is fully offline.

use serde_json::{Value, json};

use orion_client::{OrionClient, paths, query_string};

use orion::package::apply::{
    ReceiptState, check_receipt, check_target_version, import_path_for, missing_storage,
    print_prune_plan, prune_plan_for, prune_refusals, status_path_for,
};
use orion::package::artifact::{
    ModelRequirement, PluginRequirement, activation_intents, literal_model_ids, members,
    model_import_content, package_members, plugin_definition, plugin_import_content, read_artifact,
    verify_hash,
};
pub(crate) use orion::package::artifact::{
    PackageArtifact, PackageMeta, Requires, artifact_content_hash, member_counts,
};
use orion::package::{Console, PruneMode};
use orion::storage::content;
use orion::storage::repositories::channels::CreateChannelRequest;
use orion::storage::repositories::connectors::CreateConnectorRequest;
use orion::storage::repositories::workflows::CreateWorkflowRequest;

type CliError = Box<dyn std::error::Error>;

// ============================================================
// Admin-API client
// ============================================================

/// The shared `orion-client` transport, configured for promotion runs: no
/// request timeout (bulk imports of a large package may run long — the
/// historical behaviour of this CLI), `ORION_ADMIN_TOKEN` as the bearer
/// credential, and the operation's `X-Orion-Change-Context` on every call
/// (K5) so the audit trail groups the whole promotion.
///
/// The typed [`orion_client::ClientError`] this client returns replaces the
/// `ApiError` that used to live here — callers still branch on `status()`
/// instead of matching prose, and its `HTTP {status} {code}: {message}`
/// Display is the same line this CLI has always printed.
fn admin_client(server: &str, change_context: String) -> Result<OrionClient, CliError> {
    let mut client = OrionClient::with_timeout(server, None)?;
    if let Some(token) = std::env::var("ORION_ADMIN_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
    {
        client = client.with_api_key(token, None);
    }
    // Same warning `orion-cli` prints: the token over plain http to anything
    // but the local machine crosses the network in the clear.
    if client.sends_credential_in_clear() {
        eprintln!(
            "warning: ORION_ADMIN_TOKEN will be sent over plain http to {server} — use https for any server that is not local"
        );
    }
    Ok(client.with_change_context(change_context))
}

/// The plugin functions an artifact's own plugins declare — what apply's
/// ordering makes available before any workflow activates.
fn provided_plugin_functions(artifact: &PackageArtifact) -> Vec<String> {
    artifact
        .plugins
        .iter()
        .enumerate()
        .filter_map(|(i, entry)| plugin_definition(i, entry).ok())
        .flat_map(|p| {
            p.manifest
                .function_names()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Collect the `name` field of every row in an exported entity array.
fn names_of(export: &Value, field: &str) -> std::collections::HashSet<String> {
    export
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| row[field].as_str().map(str::to_string))
        .collect()
}

// ============================================================
// export
// ============================================================

/// What `--version` asked for: a literal version, or the keyword `content`
/// for one derived from the artifact's content hash.
pub(crate) enum VersionSpec<'a> {
    Literal(&'a str),
    Content { prefix: Option<&'a str> },
}

impl<'a> VersionSpec<'a> {
    /// `--version` and `--version-prefix`, checked against the receipt rule
    /// now rather than at `apply`, where the target would refuse them.
    pub(crate) fn parse(version: &'a str, prefix: Option<&'a str>) -> Result<Self, CliError> {
        use orion::storage::content::{CONTENT_VERSION_KEYWORD, check_version_prefix};
        if version == CONTENT_VERSION_KEYWORD {
            if let Some(prefix) = prefix {
                check_version_prefix(prefix)?;
            }
            return Ok(Self::Content { prefix });
        }
        if prefix.is_some() {
            return Err("--version-prefix only applies with --version content".into());
        }
        orion::validation::package_key(
            "--version",
            version,
            orion::validation::MAX_PACKAGE_VERSION_LEN,
        )?;
        Ok(Self::Literal(version))
    }

    /// The version for an artifact whose content hashes to `content_hash`.
    pub(crate) fn resolve(&self, content_hash: &str) -> Result<String, CliError> {
        Ok(match self {
            Self::Literal(version) => (*version).to_string(),
            Self::Content { prefix } => {
                orion::storage::content::content_version(content_hash, *prefix)?
            }
        })
    }

    /// How to name the version before the hash is known.
    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Literal(version) => version,
            Self::Content { .. } => orion::storage::content::CONTENT_VERSION_KEYWORD,
        }
    }

    /// With a derived version, say what the hash — and so the version —
    /// does not see: a rollout-only change keeps it.
    pub(crate) fn note_what_the_hash_excludes(&self, artifact: &PackageArtifact) {
        if matches!(self, Self::Content { .. })
            && artifact
                .workflows
                .iter()
                .any(|w| w.get("rollout_percentage").is_some())
        {
            eprintln!(
                "note: rollout_percentage is not part of the content hash — a rollout-only \
                 change keeps this version and re-applies as a no-op; change --version-prefix \
                 to force a new one"
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_export(
    server: &str,
    tag: Option<&str>,
    channel_ids: &[String],
    name: &str,
    version: &str,
    version_prefix: Option<&str>,
    requires_orion: Option<&str>,
    output: Option<&str>,
    include_artifacts: bool,
) -> Result<(), CliError> {
    if tag.is_none() && channel_ids.is_empty() {
        return Err("select the package's channels with --tag or --channels".into());
    }
    orion::validation::package_key("--name", name, orion::validation::MAX_PACKAGE_NAME_LEN)?;
    let version = VersionSpec::parse(version, version_prefix)?;
    // An export has no set to read a range from; the flag is the only way to
    // declare one.
    let requires_orion = requires_orion
        .map(|range| {
            orion::version::OrionRequirement::parse(range)
                .map(|r| r.as_str().to_string())
                .map_err(|e| format!("--requires-orion {e}"))
        })
        .transpose()?;
    let client = admin_client(server, format!("package={name}@{} export", version.label()))?;

    // 1. The selected channels.
    let mut channels: Vec<Value> = Vec::new();
    if let Some(tag) = tag {
        let listed: Value = client
            .get_data(&format!(
                "{}{}",
                paths::CHANNELS_EXPORT,
                query_string(&[("tag", Some(tag.to_string()))])
            ))
            .await?;
        channels.extend(listed.as_array().cloned().unwrap_or_default());
    }
    for id in channel_ids {
        channels.push(client.get_data(&paths::channel(id)).await?);
    }
    if channels.is_empty() {
        return Err("the selector matched no channels".into());
    }
    let channel_names: Vec<String> = channels
        .iter()
        .filter_map(|c| c["name"].as_str().map(str::to_string))
        .collect();

    // 2. Their workflows, via each channel's workflow_id.
    let mut workflow_ids: Vec<String> = Vec::new();
    for channel in &channels {
        match channel["workflow_id"].as_str() {
            Some(wf) if !wf.is_empty() => {
                if !workflow_ids.iter().any(|w| w == wf) {
                    workflow_ids.push(wf.to_string());
                }
            }
            _ => eprintln!(
                "warning: channel '{}' names no workflow_id and can never activate",
                channel["name"].as_str().unwrap_or("?")
            ),
        }
    }
    let mut workflows = Vec::new();
    let mut connector_names: Vec<String> = Vec::new();
    let mut required_channels: Vec<String> = Vec::new();
    let mut plugin_deps: Vec<PluginRequirement> = Vec::new();
    for id in &workflow_ids {
        workflows.push(client.get_data(&paths::workflow(id)).await?);
        // 3. The dependency closure, from the server's own walk (K9).
        let deps: Value = client.get_data(&paths::workflow_dependencies(id)).await?;
        for c in deps["connectors"].as_array().into_iter().flatten() {
            if let Some(name) = c["connector"].as_str()
                && !connector_names.iter().any(|n| n == name)
            {
                connector_names.push(name.to_string());
            }
        }
        // The plugin closure: the version and digest each plugin function
        // resolves to on the source, so the artifact carries — or requires —
        // exactly the component the workflow was running against.
        for p in deps["plugins"].as_array().into_iter().flatten() {
            if let (Some(pid), Some(digest)) = (p["id"].as_str(), p["digest"].as_str()) {
                let requirement = PluginRequirement {
                    id: pid.to_string(),
                    digest: digest.to_string(),
                };
                if !plugin_deps.contains(&requirement) {
                    plugin_deps.push(requirement);
                }
            }
        }
        for function in deps["unresolved_functions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            eprintln!(
                "warning: workflow '{id}' names function '{function}', which the source does \
                 not dispatch — its plugin is archived or not loaded, so the artifact cannot \
                 carry it and the workflow will not activate on the target"
            );
        }
        for target in deps["channels"].as_array().into_iter().flatten() {
            if let Some(target) = target.as_str()
                && !channel_names.iter().any(|n| n == target)
                && !required_channels.iter().any(|n| n == target)
            {
                // A channel_call outside the selection is a boundary, not a
                // member: it goes to `requires` for `plan` to verify.
                required_channels.push(target.to_string());
            }
        }
        if deps["has_dynamic_channel_calls"] == true {
            eprintln!(
                "warning: workflow '{id}' resolves channel_call targets dynamically — \
                 the requires list cannot be complete"
            );
        }
    }

    // 4. The referenced connectors, from one export sweep.
    let all_connectors: Value = client.get_data(paths::CONNECTORS_EXPORT).await?;
    let mut connectors = Vec::new();
    let mut required_connectors: Vec<String> = Vec::new();
    for name in &connector_names {
        match all_connectors
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| c["name"].as_str() == Some(name))
        {
            Some(connector) => connectors.push(connector.clone()),
            None => {
                eprintln!(
                    "warning: connector '{name}' is referenced but not stored on the \
                     source — recorded under requires.connectors"
                );
                required_connectors.push(name.clone());
            }
        }
    }

    // 4b. The plugins those functions resolve to, from one export sweep of
    //     the active rows — with the components inlined when asked, so the
    //     artifact can install them on a target that has never seen them. A
    //     dependency the sweep cannot match by id *and* digest goes to
    //     `requires.plugins`: the target must already serve that exact
    //     component, and `plan` checks that it does.
    let mut plugins: Vec<Value> = Vec::new();
    let mut required_plugins: Vec<PluginRequirement> = Vec::new();
    if !plugin_deps.is_empty() {
        let active: Value = client
            .get_data(&format!(
                "{}{}",
                paths::PLUGINS_EXPORT,
                query_string(&[
                    ("status", Some(orion_api::STATUS_ACTIVE.to_string())),
                    (
                        "include_artifacts",
                        include_artifacts.then(|| "true".to_string())
                    ),
                ])
            ))
            .await?;
        for dep in &plugin_deps {
            match active.as_array().into_iter().flatten().find(|p| {
                p["plugin_id"].as_str() == Some(&dep.id)
                    && p["digest"].as_str() == Some(&dep.digest)
            }) {
                Some(row) => {
                    let mut entry = row.clone();
                    if let Some(obj) = entry.as_object_mut() {
                        obj.insert("activate".to_string(), json!(true));
                    }
                    plugins.push(entry);
                }
                None => {
                    eprintln!(
                        "warning: plugin '{}' at {} is used but its active row on the source \
                         does not match — recorded under requires.plugins",
                        dep.id, dep.digest
                    );
                    required_plugins.push(dep.clone());
                }
            }
        }
        if !include_artifacts && !plugins.is_empty() {
            eprintln!(
                "note: {} plugin(s) recorded by manifest and digest only; the target must \
                 already hold the component, or export with --include-artifacts",
                plugins.len()
            );
        }
    }

    // 4c. The models the workflows name by literal id, from one export
    //     sweep of the active rows: each carried as its reference — the
    //     target fetches the bytes through a storage connector of the same
    //     name, which goes to `requires.storage` unless the package carries
    //     it. A model the source does not serve active goes to
    //     `requires.models`, at the version and digest the source holds when
    //     it holds any, and `plan` checks the target does.
    let mut models: Vec<Value> = Vec::new();
    let mut required_models: Vec<ModelRequirement> = Vec::new();
    let mut required_storage: Vec<String> = Vec::new();
    let mut model_ids: Vec<String> = Vec::new();
    for workflow in &workflows {
        for id in literal_model_ids(workflow) {
            if !model_ids.contains(&id) {
                model_ids.push(id);
            }
        }
    }
    if !model_ids.is_empty() {
        let active: Value = client
            .get_data(&format!(
                "{}{}",
                paths::MODELS_EXPORT,
                query_string(&[("status", Some(orion_api::STATUS_ACTIVE.to_string()))])
            ))
            .await?;
        let carried_connectors: Vec<&str> = connectors
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect();
        for id in &model_ids {
            match active
                .as_array()
                .into_iter()
                .flatten()
                .find(|m| m["model_id"].as_str() == Some(id))
            {
                Some(row) => {
                    let mut entry = row.clone();
                    if let Some(obj) = entry.as_object_mut() {
                        obj.insert("activate".to_string(), json!(true));
                    }
                    if let Some(name) = row["artifact"]["connector"].as_str()
                        && !carried_connectors.contains(&name)
                        && !required_storage.iter().any(|s| s == name)
                    {
                        required_storage.push(name.to_string());
                    }
                    models.push(entry);
                }
                None => {
                    let stored: Option<Value> = client.get_data_opt(&paths::model(id)).await?;
                    let requirement = match stored {
                        Some(row) => ModelRequirement {
                            id: id.clone(),
                            version: row["version"].as_i64().unwrap_or(0),
                            digest: row["digest"].as_str().unwrap_or("").to_string(),
                        },
                        None => ModelRequirement {
                            id: id.clone(),
                            version: 0,
                            digest: String::new(),
                        },
                    };
                    eprintln!(
                        "warning: model '{id}' is named by a workflow but not active on the \
                         source — recorded under requires.models"
                    );
                    required_models.push(requirement);
                }
            }
        }
    }

    // 5. Carry activation intent: DB-owned `status` does not survive import,
    //    so the artifact says what apply should activate.
    for entity in workflows.iter_mut().chain(channels.iter_mut()) {
        if entity["status"] == "active"
            && let Some(obj) = entity.as_object_mut()
        {
            obj.insert("activate".to_string(), json!(true));
        }
    }

    let mut artifact = PackageArtifact {
        package: PackageMeta {
            name: name.to_string(),
            version: String::new(),
            orion: env!("CARGO_PKG_VERSION").to_string(),
            content_hash: String::new(),
            exported_from: server.to_string(),
            exported_at: chrono::Utc::now().to_rfc3339(),
        },
        requires: Requires {
            orion: requires_orion,
            channels: required_channels,
            connectors: required_connectors,
            plugins: required_plugins,
            models: required_models,
            storage: required_storage,
        },
        plugins,
        models,
        connectors,
        workflows,
        channels,
    };
    // The version is outside the hash, so it can be derived from it.
    artifact.package.content_hash = artifact_content_hash(&artifact)?;
    artifact.package.version = version.resolve(&artifact.package.content_hash)?;
    version.note_what_the_hash_excludes(&artifact);

    let rendered = serde_json::to_string_pretty(&artifact)?;
    match output {
        Some(path) => {
            std::fs::write(path, rendered).map_err(|e| format!("write '{path}': {e}"))?;
            println!(
                "wrote {}@{} ({}) to {path}",
                artifact.package.name,
                artifact.package.version,
                member_counts(&artifact),
            );
        }
        None => println!("{rendered}"),
    }
    Ok(())
}

// ============================================================
// lint (offline)
// ============================================================

pub(crate) fn run_lint(file: &str) -> Result<(), CliError> {
    let artifact = read_artifact(file)?;
    let report = orion::package::lint_artifact(&artifact)?;
    for warning in &report.warnings {
        eprintln!("{warning}");
    }
    if report.errors.is_empty() {
        println!(
            "'{file}' is a valid package: {}@{} — {}",
            artifact.package.name,
            artifact.package.version,
            member_counts(&artifact),
        );
        Ok(())
    } else {
        for error in &report.errors {
            eprintln!("error: {error}");
        }
        Err(format!("{} lint error(s) in '{file}'", report.errors.len()).into())
    }
}

// ============================================================
// plan
// ============================================================

/// `plan`'s warnings about a target that would not serve part of the
/// artifact: a capability it has switched off, or a member it already
/// quarantines.
fn plan_capability_warnings(artifact: &PackageArtifact, status: &orion_api::EngineStatusResponse) {
    if let Some(caps) = &status.capabilities {
        if !caps.cron {
            for channel in &artifact.channels {
                if channel["protocol"] == "cron"
                    && channel["activate"] == true
                    && let Some(id) = channel["channel_id"].as_str()
                {
                    eprintln!(
                        "warning: target has cron.enabled = false — channels/{id} would be \
                         refused at activation (and quarantined on any node that loads it)"
                    );
                }
            }
        }
        if !caps.plugins && (!artifact.plugins.is_empty() || !artifact.requires.plugins.is_empty())
        {
            eprintln!(
                "warning: target has plugins.enabled = false — the plugins this package \
                 carries or requires cannot load there, and the workflows calling them would \
                 be quarantined"
            );
        }
        let names_models = artifact
            .workflows
            .iter()
            .any(|w| !literal_model_ids(w).is_empty());
        if !caps.models
            && (!artifact.models.is_empty() || !artifact.requires.models.is_empty() || names_models)
        {
            eprintln!(
                "warning: target has models.enabled = false — the models this package names \
                 cannot load there, and the workflows naming them would be quarantined"
            );
        }
    }
    if let Some(issues) = &status.load_issues {
        for entity in orion::package::quarantined_members(&package_members(artifact), issues) {
            eprintln!("warning: {entity} (already quarantined on the target)");
        }
    }
}

pub(crate) async fn run_plan(
    server: &str,
    file: &str,
    signatures: Option<&str>,
    prune: Option<PruneMode>,
) -> Result<(), CliError> {
    let mut artifact = read_artifact(file)?;
    verify_hash(&artifact)?;
    // Before anything is asked of the target: the dry-run imports below run
    // the trust check, so `plan` needs the signatures as much as `apply`.
    orion::package::sign::attach_from_dir(&mut artifact, signatures, &Console)?;
    let package = format!("{}@{}", artifact.package.name, artifact.package.version);
    let client = admin_client(server, format!("package={package} plan"))?;
    check_target_version(&client, server, &artifact, &package).await?;

    // The immutability gate first: a reused applied version is dead on
    // arrival, and nothing below can change that.
    let (receipt, receipts) = check_receipt(&client, &artifact).await?;
    match &receipt {
        ReceiptState::AppliedConflict => {
            return Err(format!(
                "{package} is already applied on {server} with different content — an \
                 applied package version is immutable; bump the package version"
            )
            .into());
        }
        ReceiptState::AppliedCurrent => {
            println!("{package} is already applied with identical content — apply is a no-op");
            if prune.is_some() {
                println!("{package} is already applied — nothing to prune");
            }
        }
        ReceiptState::AppliedSuperseded { current } => {
            println!(
                "{package} is applied here but superseded by {}@{current} — apply will roll \
                 the entities back to this content",
                artifact.package.name
            );
        }
        ReceiptState::Staged => {
            println!("{package} is staged here; apply may update it in place");
        }
        ReceiptState::Fresh => {}
    }

    // What the target is configured to run, and what it already refuses: a
    // member this node would quarantine is predictable before anything is
    // written. Warnings — the dry-run gates below are what fail a plan, and
    // on the node that answers they already refuse most of these.
    if let Ok(status) = client
        .get_data::<orion_api::EngineStatusResponse>(paths::ENGINE_STATUS)
        .await
    {
        plan_capability_warnings(&artifact, &status);
    }

    // `requires` boundaries must exist in the target — each set fetched once
    // (the exports are unpaginated K12 snapshots, so no listing clamp can
    // hide a boundary on a large estate).
    let mut failures = 0usize;

    // What the previous applied version carried and this artifact does not.
    // Without `--prune` it is only mentioned; with it, every removal is
    // listed and one something else still depends on is a blocking issue.
    let prune_plan = prune_plan_for(&client, &artifact, &receipts, &receipt).await?;
    match prune {
        Some(mode) => {
            print_prune_plan(&prune_plan, mode, &Console);
            for refusal in prune_refusals(&client, &artifact, &prune_plan).await? {
                eprintln!("error: {refusal}");
                failures += 1;
            }
        }
        None if !prune_plan.is_empty() => {
            let count = prune_plan.removals().count();
            println!(
                "note: {count} {} of {} {} not in this artifact; apply --prune would remove {}",
                if count == 1 { "entity" } else { "entities" },
                prune_plan
                    .baseline
                    .as_deref()
                    .unwrap_or("the current version"),
                if count == 1 { "is" } else { "are" },
                if count == 1 { "it" } else { "them" },
            );
        }
        None => {}
    }
    // Channels `--prune` archives before activation: a route or name gate
    // naming one of them is resolved by the prune, not a conflict.
    let pruned_early: Vec<&str> = if prune.is_some() {
        prune_plan.early.iter().map(|r| r.id.as_str()).collect()
    } else {
        Vec::new()
    };
    if !artifact.requires.connectors.is_empty() {
        let stored = names_of(&client.get_data(paths::CONNECTORS_EXPORT).await?, "name");
        for name in &artifact.requires.connectors {
            if !stored.contains(name) {
                eprintln!("error: required connector '{name}' does not exist in the target");
                failures += 1;
            }
        }
    }
    if !artifact.requires.channels.is_empty() {
        let active: Value = client
            .get_data(&format!(
                "{}{}",
                paths::CHANNELS_EXPORT,
                query_string(&[("status", Some(orion_api::STATUS_ACTIVE.to_string()))])
            ))
            .await?;
        let active = names_of(&active, "name");
        for name in &artifact.requires.channels {
            if !active.contains(name) {
                eprintln!("error: required channel '{name}' is not active in the target");
                failures += 1;
            }
        }
    }
    // Plugins, by digest: a required plugin must be active on the target at
    // exactly the component the workflows ran against, and a carried plugin
    // that names a digest without the bytes needs the target to hold them —
    // an import of it would otherwise fail at write, after staging began.
    if !artifact.requires.plugins.is_empty() || !artifact.plugins.is_empty() {
        let stored: Value = client.get_data(paths::PLUGINS_EXPORT).await?;
        let rows: Vec<&Value> = stored.as_array().into_iter().flatten().collect();
        for req in &artifact.requires.plugins {
            let active = rows.iter().any(|p| {
                p["plugin_id"].as_str() == Some(&req.id)
                    && p["digest"].as_str() == Some(&req.digest)
                    && p["status"] == orion_api::STATUS_ACTIVE
            });
            if !active {
                eprintln!(
                    "error: required plugin '{}' is not active in the target at {} — install \
                     and activate that version first, or export with --include-artifacts",
                    req.id, req.digest
                );
                failures += 1;
            }
        }
        for entry in &artifact.plugins {
            if entry.get("component").is_some() {
                continue;
            }
            let (id, digest) = (
                entry["plugin_id"].as_str().unwrap_or("?"),
                entry["digest"].as_str().unwrap_or("?"),
            );
            if !rows.iter().any(|p| p["digest"].as_str() == Some(digest)) {
                eprintln!(
                    "error: plugin '{id}' is carried by digest only and the target does not \
                     hold {digest} — export with --include-artifacts"
                );
                failures += 1;
            }
        }
    }

    // Models by id: a required model must be active on the target, at the
    // digest the source served when the requirement names one; and every
    // storage connector the carried models fetch through must be there.
    if !artifact.requires.models.is_empty() {
        let stored: Value = client.get_data(paths::MODELS_EXPORT).await?;
        for req in &artifact.requires.models {
            let active = stored.as_array().into_iter().flatten().any(|m| {
                m["model_id"].as_str() == Some(&req.id)
                    && m["status"] == orion_api::STATUS_ACTIVE
                    && (req.digest.is_empty() || m["digest"].as_str() == Some(&req.digest))
            });
            if !active {
                eprintln!(
                    "error: required model '{}' is not active in the target{} — register and \
                     activate it first, or carry it in the package",
                    req.id,
                    if req.digest.is_empty() {
                        String::new()
                    } else {
                        format!(" at {}", req.digest)
                    }
                );
                failures += 1;
            }
        }
    }
    failures += missing_storage(&client, &artifact, &Console).await?;

    // A workflow's create-time gate validates function names against the
    // target's *published* registry, so one calling a function of a plugin
    // this package carries is refused by the dry-run import until apply has
    // activated that plugin. That refusal is apply's ordering at work, not a
    // blocking issue: reported as pending when every unknown function the
    // workflow names is one the package's plugins provide.
    let provided_functions = provided_plugin_functions(&artifact);
    let target_functions: std::collections::HashSet<String> = if provided_functions.is_empty() {
        std::collections::HashSet::new()
    } else {
        names_of(&client.get_data(paths::FUNCTIONS).await?, "name")
    };
    let pending_plugin_function = |item: &Value| -> bool {
        let Some(tasks) = item.get("tasks") else {
            return false;
        };
        let mut saw_one = false;
        for task in orion::engine::leaf_tasks(tasks, item.get("loop")) {
            let Some(name) = task
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            if provided_functions.iter().any(|f| f == name) && !target_functions.contains(name) {
                saw_one = true;
            }
        }
        saw_one
    };

    // Per-entity actions from the servers' own dry-runs (K2).
    for (kind, items) in members(&artifact) {
        if items.is_empty() {
            continue;
        }
        let outcome: Value = client
            .post_data(
                &format!(
                    "{}?dry_run=true&on_conflict=new_version",
                    import_path_for(kind)
                ),
                &Value::Array(items.to_vec()),
            )
            .await?;
        for result in outcome["results"].as_array().into_iter().flatten() {
            let id = result["id"].as_str().unwrap_or("(generated)");
            let action = result["action"].as_str().unwrap_or("?");
            // The hash excludes rollout, so `unchanged` can still carry a
            // rollout intent — apply lands it via the rollout endpoint; say
            // so, or a rollout-only package looks like a full no-op here.
            let rollout_note = if kind == "workflows" && action == "unchanged" {
                activation_intents(&artifact)
                    .into_iter()
                    .find(|(k, i, pct)| *k == kind && i == id && pct.is_some())
                    .and_then(|(_, _, pct)| pct)
                    .map(|pct| format!(" (rollout will be set to {pct}%)"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            println!("  {kind:<10} {id:<28} {action}{rollout_note}");
        }
        for error in outcome["errors"].as_array().into_iter().flatten() {
            let index = error["index"].as_u64().unwrap_or(u64::MAX) as usize;
            let message = error["error"].as_str().unwrap_or("?");
            if kind == "workflows"
                && let Some(item) = items.get(index)
                && pending_plugin_function(item)
            {
                let id = item["workflow_id"].as_str().unwrap_or("(generated)");
                println!(
                    "  {kind:<10} {id:<28} gate pending apply order: {message} (a plugin \
                     function this package installs first)"
                );
                continue;
            }
            eprintln!("error: {kind}[{index}]: {message}");
            failures += 1;
        }
    }

    // Activation gates (K3), evaluated against the *current* state — so a
    // finding can name something that only exists once apply's ordered
    // staging and activation have run. Those are reported as pending, not
    // failures. Classification is by message (the dry-run envelope carries no
    // machine code yet), kept deliberately narrow: only existence-shaped
    // findings qualify, and a referenced dependency must be one this package
    // provides *of the kind apply's ordering resolves* — a type mismatch or
    // route collision mentions package names too, and apply cannot fix those.
    // A workflow's activation gate names connectors *and* plugin functions,
    // and apply stages and activates the package's plugins before any
    // workflow, so a function one of them declares is resolved by order too.
    let mut provided_connectors: Vec<String> = artifact
        .connectors
        .iter()
        .filter_map(|c| c["name"].as_str().map(str::to_string))
        .collect();
    provided_connectors.extend(provided_plugin_functions(&artifact));
    let provided_workflows: Vec<String> = artifact
        .workflows
        .iter()
        .filter_map(|w| w["workflow_id"].as_str().map(str::to_string))
        .collect();
    for (kind, id, _) in activation_intents(&artifact) {
        let outcome = client
            .patch_data::<Value>(
                &format!("{}?dry_run=true", status_path_for(kind, &id)),
                &json!({"status": orion_api::STATUS_ACTIVE}),
            )
            .await;
        let outcome = match outcome {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: {kind} '{id}' activation pre-flight failed: {e}");
                failures += 1;
                continue;
            }
        };
        let resolved_by_order = if kind == "workflows" {
            &provided_connectors
        } else {
            &provided_workflows
        };
        for finding in outcome["errors"].as_array().into_iter().flatten() {
            let message = finding["message"].as_str().unwrap_or("");
            let existence = [
                "not found",
                "No draft version",
                "has no active version",
                // The workflow gate's spelling for a plugin function the
                // generation does not dispatch yet.
                "are not available on this node",
            ]
            .iter()
            .any(|phrase| message.contains(phrase));
            let pending =
                // The planned entity itself is absent or draft-less — staging
                // creates it before activation runs.
                message.starts_with(&format!("Workflow '{id}' not found"))
                    || message.starts_with(&format!("Channel '{id}' not found"))
                    || message.starts_with(&format!("Plugin '{id}' not found"))
                    || message.starts_with(&format!("Model '{id}' not found"))
                    || message.contains("No draft version")
                    // A reference apply's ordering satisfies: quoted, and of
                    // the dependency kind activated before this entity.
                    || (existence
                        && resolved_by_order
                            .iter()
                            .any(|name| message.contains(&format!("'{name}'"))))
                    // A collision with a channel `--prune` archives first —
                    // the two spellings of the name and route gates.
                    || (kind == "channels"
                        && pruned_early.iter().any(|held| {
                            message.contains(&format!("active channel id '{held}'"))
                                || message.contains(&format!("(id {held})"))
                        }));
            if pending {
                println!("  {kind:<10} {id:<28} gate pending apply order: {message}");
            } else {
                eprintln!("error: {kind} '{id}' would not activate: {message}");
                failures += 1;
            }
        }
    }

    if failures > 0 {
        Err(format!("plan found {failures} blocking issue(s)").into())
    } else {
        println!("plan: {package} applies cleanly to {server}");
        Ok(())
    }
}

// ============================================================
// prune
// ============================================================

/// `--prune`'s value on the command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum PruneArg {
    /// Archive what is no longer carried, and disable a connector.
    Archive,
    /// Delete what is no longer carried, every version of it.
    Delete,
}

impl From<PruneArg> for PruneMode {
    fn from(arg: PruneArg) -> Self {
        match arg {
            PruneArg::Archive => PruneMode::Archive,
            PruneArg::Delete => PruneMode::Delete,
        }
    }
}

// ============================================================
// apply
// ============================================================

pub(crate) async fn run_apply(
    server: &str,
    file: &str,
    signatures: Option<&str>,
    prune: Option<PruneMode>,
) -> Result<(), CliError> {
    let mut artifact = read_artifact(file)?;
    verify_hash(&artifact)?;
    let package = format!("{}@{}", artifact.package.name, artifact.package.version);
    // Before anything is sent: an orphan or malformed `.sig` stops here.
    let signed = orion::package::sign::attach_from_dir(&mut artifact, signatures, &Console)?;
    let client = admin_client(server, format!("package={package}"))?;
    let prune_client = match prune {
        Some(_) => Some(admin_client(server, format!("package={package} prune"))?),
        None => None,
    };
    let opts = orion::package::ApplyOptions {
        prune: prune.zip(prune_client.as_ref()),
        signed: &signed,
        roll_back_superseded: true,
    };
    orion::package::apply(&client, server, &artifact, &opts, &Console).await?;
    Ok(())
}

// ============================================================
// diff
// ============================================================

pub(crate) async fn run_diff(server: &str, file: &str) -> Result<(), CliError> {
    let artifact = read_artifact(file)?;
    let package = format!("{}@{}", artifact.package.name, artifact.package.version);
    let client = admin_client(server, format!("package={package} diff"))?;

    // Server-side content hashes (K10) against the artifact's per-entity
    // projections — the same canonicalization on both sides. One export
    // sweep per kind rather than one GET per entity: the exports are K12
    // snapshots and already carry `content_hash`, which is all diff compares.
    let mut rows: Vec<(String, &'static str)> = Vec::new();
    for (kind, entries, key_field, export_path) in [
        (
            "plugin",
            &artifact.plugins,
            "plugin_id",
            paths::PLUGINS_EXPORT,
        ),
        ("model", &artifact.models, "model_id", paths::MODELS_EXPORT),
        (
            "connector",
            &artifact.connectors,
            "name",
            paths::CONNECTORS_EXPORT,
        ),
        (
            "workflow",
            &artifact.workflows,
            "workflow_id",
            paths::WORKFLOWS_EXPORT,
        ),
        (
            "channel",
            &artifact.channels,
            "channel_id",
            paths::CHANNELS_EXPORT,
        ),
    ] {
        if entries.is_empty() {
            continue;
        }
        let export: Value = client.get_data(export_path).await?;
        for entry in entries {
            let Some(key) = entry[key_field].as_str() else {
                continue;
            };
            let expected = match kind {
                "plugin" => content::content_hash(&plugin_import_content(entry)?),
                "model" => content::content_hash(&model_import_content(entry)?),
                "connector" => {
                    let req: CreateConnectorRequest = serde_json::from_value(entry.clone())?;
                    content::content_hash(&content::connector_request_content(&req))
                }
                "workflow" => {
                    let req: CreateWorkflowRequest = serde_json::from_value(entry.clone())?;
                    content::content_hash(&content::workflow_request_content(&req))
                }
                _ => {
                    let req: CreateChannelRequest = serde_json::from_value(entry.clone())?;
                    content::content_hash(&content::channel_request_content(&req))
                }
            };
            let stored = export
                .as_array()
                .into_iter()
                .flatten()
                .find(|row| row[key_field].as_str() == Some(key));
            let state = match stored {
                None => "missing",
                Some(row) if row["content_hash"].as_str() == Some(expected.as_str()) => "unchanged",
                Some(_) => "changed",
            };
            rows.push((format!("{kind} '{key}'"), state));
        }
    }

    for (label, state) in &rows {
        println!("  {state:<10} {label}");
    }
    let differences = rows
        .iter()
        .filter(|(_, state)| *state != "unchanged")
        .count();
    if differences > 0 {
        Err(format!(
            "{differences} entity(ies) differ between '{file}' and {server} — the \
             estate has drifted from the artifact"
        )
        .into())
    } else {
        println!("no drift: {package} matches {server}");
        Ok(())
    }
}
