//! `orion-server package` — the packaging half of the K-stream promotion
//! design.
//!
//! A package is the channels, workflows, and connectors of one service,
//! named and versioned so the service ships between instances as a unit —
//! the module boundary of Orion's modular-monolith model. One instance runs
//! many packages; each promotes and rolls back independently.
//!
//! Packaging lives here, in the CLI, not in the server: the server provides
//! per-kind primitives (upsert import, activation pre-flight, deferred
//! reload, package receipts) and this module composes them. Everything talks
//! to a running instance's admin API over HTTP (`--server` +
//! `ORION_ADMIN_TOKEN`), except `lint`, which is fully offline.
//!
//! The artifact is one JSON document: a `package` header,
//! `requires` boundaries, and the three entity arrays in the exact shapes
//! the `/import` endpoints accept. `package.content_hash` is computed over
//! the entities' *importable content* — each entry projected through the
//! same `storage::content` canonicalization the server hashes with (K10) —
//! so DB-owned fields (`status`, `version`, timestamps) never make two
//! artifacts differ.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use orion_client::{OrionClient, StatusCode, paths, query_string};

use orion::storage::content;
use orion::storage::repositories::channels::CreateChannelRequest;
use orion::storage::repositories::connectors::CreateConnectorRequest;
use orion::storage::repositories::plugins::CreatePluginRequest;
use orion::storage::repositories::workflows::CreateWorkflowRequest;

type CliError = Box<dyn std::error::Error>;

// ============================================================
// Artifact shapes
// ============================================================

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PackageArtifact {
    pub(crate) package: PackageMeta,
    #[serde(default)]
    pub(crate) requires: Requires,
    /// The fourth member: plugins, each in the shape `/plugins/import`
    /// accepts — `plugin_id`, `manifest`, `digest`, `tags`, and the component
    /// as base64 when the export carried it. Omitted from the document *and*
    /// the hash when empty, so a package without plugins hashes exactly as it
    /// did before plugins existed and every applied receipt stays valid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) plugins: Vec<Value>,
    #[serde(default)]
    pub(crate) connectors: Vec<Value>,
    #[serde(default)]
    pub(crate) workflows: Vec<Value>,
    #[serde(default)]
    pub(crate) channels: Vec<Value>,
}

/// A plugin the package uses but does not carry: the target must hold this
/// digest active under this id before the package can apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PluginRequirement {
    pub(crate) id: String,
    pub(crate) digest: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PackageMeta {
    pub(crate) name: String,
    pub(crate) version: String,
    /// The Orion version that exported this artifact — informational.
    #[serde(default)]
    pub(crate) orion: String,
    pub(crate) content_hash: String,
    /// Where the artifact came from: a server URL for `export`, a directory
    /// for `compile`. Informational.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) exported_from: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) exported_at: String,
}

/// Declared external dependencies: names this package uses but
/// deliberately does not contain, so closures stay small. `plan` verifies
/// they exist and are active in the target.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct Requires {
    #[serde(default)]
    pub(crate) channels: Vec<String>,
    #[serde(default)]
    pub(crate) connectors: Vec<String>,
    /// Plugins the workflows call that the artifact does not carry, by id
    /// and digest: `plan` checks the target serves exactly that digest.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) plugins: Vec<PluginRequirement>,
}

/// The importable content of one `plugins[]` entry — the projection the
/// server hashes a stored row with (`plugin_content`), computed here from the
/// import item: the manifest (TOML text or the object), the digest it names
/// or the hash of the component it carries, and its tags.
fn plugin_import_content(entry: &Value) -> Result<Value, CliError> {
    let req: CreatePluginRequest = serde_json::from_value(entry.clone())
        .map_err(|e| format!("plugin entry does not parse as an import item: {e}"))?;
    let manifest = match &req.manifest {
        Value::String(text) => orion::plugin::Manifest::parse(text),
        other => serde_json::from_value::<orion::plugin::Manifest>(other.clone())
            .map_err(|e| {
                vec![orion::errors::FieldError::new(
                    "manifest",
                    "INVALID",
                    e.to_string(),
                )]
            })
            .and_then(orion::plugin::Manifest::validated),
    }
    .map_err(|errors| {
        format!(
            "plugin entry '{}': {}",
            req.plugin_id.as_deref().unwrap_or("?"),
            errors
                .iter()
                .map(|e| format!("{}: {}", e.path, e.message))
                .collect::<Vec<_>>()
                .join("; ")
        )
    })?;
    let digest = match (&req.digest, &req.component) {
        (Some(digest), _) => digest.clone(),
        (None, Some(component)) => {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(component.trim())
                .map_err(|e| format!("plugin '{}': component is not base64: {e}", manifest.name))?;
            orion::plugin::WasmRuntime::digest(&bytes)
        }
        (None, None) => {
            return Err(format!(
                "plugin '{}': the entry carries neither a component nor a digest",
                manifest.name
            )
            .into());
        }
    };
    Ok(content::plugin_request_content(
        &serde_json::to_value(&manifest)?,
        &digest,
        &req.tags,
    ))
}

/// A `plugins[]` entry as the set loader sees it: the parsed manifest and
/// the digest the entry names or carries.
fn plugin_definition(
    index: usize,
    entry: &Value,
) -> Result<orion::definitions::PluginDefinition, CliError> {
    let content = plugin_import_content(entry)?;
    let manifest: orion::plugin::Manifest = serde_json::from_value(content["manifest"].clone())?;
    Ok(orion::definitions::PluginDefinition {
        origin: format!("plugins[{index}]"),
        manifest,
        digest: content["digest"].as_str().map(str::to_string),
        component_path: None,
    })
}

/// Project every entry of one entity array through its import shape. Fails on
/// an entry that does not parse as that shape — such an artifact could not
/// apply anyway.
fn project_entries<T: serde::de::DeserializeOwned>(
    entries: &[Value],
    label: &str,
    project: impl Fn(&T) -> Value,
) -> Result<Vec<Value>, CliError> {
    entries
        .iter()
        .map(|entry| {
            let req: T = serde_json::from_value(entry.clone())
                .map_err(|e| format!("{label} entry does not parse as an import item: {e}"))?;
            Ok(project(&req))
        })
        .collect()
}

/// The package-level hash: each entity array projected entry-by-entry
/// through the shared importable-content canonicalization, then hashed as
/// one document.
pub(crate) fn artifact_content_hash(artifact: &PackageArtifact) -> Result<String, CliError> {
    let mut doc = json!({
        "connectors": project_entries::<CreateConnectorRequest>(
            &artifact.connectors, "connector", content::connector_request_content)?,
        "workflows": project_entries::<CreateWorkflowRequest>(
            &artifact.workflows, "workflow", content::workflow_request_content)?,
        "channels": project_entries::<CreateChannelRequest>(
            &artifact.channels, "channel", content::channel_request_content)?,
    });
    // The key is present only when there is something under it: a package
    // without plugins must hash exactly as it did before the member existed,
    // or every applied receipt on every target would read as a conflict.
    if !artifact.plugins.is_empty() {
        doc["plugins"] = Value::Array(
            artifact
                .plugins
                .iter()
                .map(plugin_import_content)
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    Ok(content::content_hash(&doc))
}

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

fn read_artifact(path: &str) -> Result<PackageArtifact, CliError> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read '{path}': {e}"))?;
    let artifact: PackageArtifact = serde_json::from_str(&raw)
        .map_err(|e| format!("'{path}' is not a package artifact: {e}"))?;
    Ok(artifact)
}

/// The receipt endpoint for this artifact's package.
fn receipt_path(artifact: &PackageArtifact) -> String {
    paths::package(&artifact.package.name)
}

/// The `/import` endpoint for one of the four entity kinds the artifact
/// carries. The kinds are a closed set spelled by this module's own loops.
fn import_path_for(kind: &str) -> &'static str {
    match kind {
        "plugins" => paths::PLUGINS_IMPORT,
        "connectors" => paths::CONNECTORS_IMPORT,
        "workflows" => paths::WORKFLOWS_IMPORT,
        _ => paths::CHANNELS_IMPORT,
    }
}

/// The `PATCH …/status` endpoint for an activation intent's kind.
fn status_path_for(kind: &str, id: &str) -> String {
    match kind {
        "plugins" => paths::plugin_status(id),
        "workflows" => paths::workflow_status(id),
        _ => paths::channel_status(id),
    }
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

pub(crate) async fn run_export(
    server: &str,
    tag: Option<&str>,
    channel_ids: &[String],
    name: &str,
    version: &str,
    output: Option<&str>,
    include_artifacts: bool,
) -> Result<(), CliError> {
    if tag.is_none() && channel_ids.is_empty() {
        return Err("select the package's channels with --tag or --channels".into());
    }
    let client = admin_client(server, format!("package={name}@{version} export"))?;

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
            version: version.to_string(),
            orion: env!("CARGO_PKG_VERSION").to_string(),
            content_hash: String::new(),
            exported_from: server.to_string(),
            exported_at: chrono::Utc::now().to_rfc3339(),
        },
        requires: Requires {
            channels: required_channels,
            connectors: required_connectors,
            plugins: required_plugins,
        },
        plugins,
        connectors,
        workflows,
        channels,
    };
    artifact.package.content_hash = artifact_content_hash(&artifact)?;

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

/// `N connectors, N workflows, N channels`, with the plugin count in front
/// only when the artifact carries any — the pre-plugin line otherwise.
pub(crate) fn member_counts(artifact: &PackageArtifact) -> String {
    let base = format!(
        "{} connectors, {} workflows, {} channels",
        artifact.connectors.len(),
        artifact.workflows.len(),
        artifact.channels.len(),
    );
    if artifact.plugins.is_empty() {
        base
    } else {
        format!("{} plugins, {base}", artifact.plugins.len())
    }
}

// ============================================================
// lint (offline)
// ============================================================

pub(crate) fn run_lint(file: &str) -> Result<(), CliError> {
    let artifact = read_artifact(file)?;
    let mut errors: Vec<String> = Vec::new();

    if artifact.package.name.trim().is_empty() {
        errors.push("package.name is empty".to_string());
    }
    if artifact.package.version.trim().is_empty() {
        errors.push("package.version is empty".to_string());
    }

    // The hash is part of the contract: an artifact edited without
    // re-hashing would defeat the receipt comparison downstream.
    match artifact_content_hash(&artifact) {
        Ok(actual) if actual != artifact.package.content_hash => errors.push(format!(
            "package.content_hash does not match the entities — expected {actual}"
        )),
        Ok(_) => {}
        Err(e) => errors.push(e.to_string()),
    }

    // Everything below the package envelope is a definition set, checked by
    // the shared pass. `requires` is this container's boundary: names the
    // target instance is expected to already have. The set's plugins are the
    // artifact's own entries, so a workflow naming one of their functions is
    // checked against the manifest that travels with it.
    let (set, boundary, mut findings) = artifact_as_set(&artifact);
    let registry = match set.function_registry() {
        Ok(registry) => registry,
        Err(reason) => {
            errors.push(format!("plugins: {reason}"));
            orion::engine::FunctionRegistry::builtin()
                .with_entries(Vec::new())
                .expect("the built-in registry extends by nothing")
        }
    };
    findings.extend(orion::definitions::check(&set, &boundary, true, &registry));

    for finding in findings.iter().filter(|f| !f.is_error()) {
        eprintln!("{finding}");
    }
    errors.extend(findings.iter().filter(|f| f.is_error()).map(|f| {
        // The package surface reports one flat line per problem; the
        // structured form is what `lint <dir>` renders.
        format!("{}: {}", f.entity, f.message)
    }));

    if errors.is_empty() {
        println!(
            "'{file}' is a valid package: {}@{} — {}",
            artifact.package.name,
            artifact.package.version,
            member_counts(&artifact),
        );
        Ok(())
    } else {
        for error in &errors {
            eprintln!("error: {error}");
        }
        Err(format!("{} lint error(s) in '{file}'", errors.len()).into())
    }
}

/// Project an artifact into the shared [`DefinitionSet`] shape, keeping the
/// `channels[2]`-style origins the package surface has always reported.
///
/// The third member of the result is what the plugin entries could not
/// give the set: an entry that does not parse as an import item is a finding
/// here, in the same voice as an entity that does not.
fn artifact_as_set(
    artifact: &PackageArtifact,
) -> (
    orion::definitions::DefinitionSet,
    orion::definitions::Boundary,
    Vec<orion::definitions::Diagnostic>,
) {
    use orion::definitions::Entity;
    let mut entries = Vec::new();
    for (i, doc) in artifact.connectors.iter().enumerate() {
        entries.push((Entity::Connector, format!("connectors[{i}]"), doc.clone()));
    }
    for (i, doc) in artifact.workflows.iter().enumerate() {
        entries.push((Entity::Workflow, format!("workflows[{i}]"), doc.clone()));
    }
    for (i, doc) in artifact.channels.iter().enumerate() {
        entries.push((Entity::Channel, format!("channels[{i}]"), doc.clone()));
    }
    let boundary = orion::definitions::Boundary {
        channels: artifact.requires.channels.clone(),
        connectors: artifact.requires.connectors.clone(),
    };
    let mut set = orion::definitions::DefinitionSet::from_entries(entries);
    let mut findings = Vec::new();
    for (i, entry) in artifact.plugins.iter().enumerate() {
        match plugin_definition(i, entry) {
            Ok(plugin) => set.plugins.push(plugin),
            Err(e) => findings.push(orion::definitions::Diagnostic::error(
                "parse.plugin",
                format!("plugins[{i}]"),
                e.to_string(),
            )),
        }
    }
    (set, boundary, findings)
}

// ============================================================
// plan
// ============================================================

/// The receipt verdict `plan` and `apply` both start from.
enum ReceiptState {
    Fresh,
    Staged,
    AppliedSame,
    AppliedConflict,
}

async fn check_receipt(
    client: &OrionClient,
    artifact: &PackageArtifact,
) -> Result<ReceiptState, CliError> {
    let receipts: Option<Value> = client.get_data_opt(&receipt_path(artifact)).await?;
    let Some(receipts) = receipts else {
        return Ok(ReceiptState::Fresh);
    };
    let row = receipts["versions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["version"] == artifact.package.version.as_str())
        .cloned();
    Ok(match row {
        None => ReceiptState::Fresh,
        Some(row) if row["state"] == "applied" => {
            if row["content_hash"] == artifact.package.content_hash.as_str() {
                ReceiptState::AppliedSame
            } else {
                ReceiptState::AppliedConflict
            }
        }
        Some(_) => ReceiptState::Staged,
    })
}

pub(crate) async fn run_plan(server: &str, file: &str) -> Result<(), CliError> {
    let artifact = read_artifact(file)?;
    verify_hash(&artifact)?;
    let package = format!("{}@{}", artifact.package.name, artifact.package.version);
    let client = admin_client(server, format!("package={package} plan"))?;

    // The immutability gate first: a reused applied version is dead on
    // arrival, and nothing below can change that.
    match check_receipt(&client, &artifact).await? {
        ReceiptState::AppliedConflict => {
            return Err(format!(
                "{package} is already applied on {server} with different content — an \
                 applied package version is immutable; bump the package version"
            )
            .into());
        }
        ReceiptState::AppliedSame => {
            println!("{package} is already applied with identical content — apply is a no-op");
        }
        ReceiptState::Staged => {
            println!("{package} is staged here; apply may update it in place");
        }
        ReceiptState::Fresh => {}
    }

    // `requires` boundaries must exist in the target — each set fetched once
    // (the exports are unpaginated K12 snapshots, so no listing clamp can
    // hide a boundary on a large estate).
    let mut failures = 0usize;
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
        for task in orion::engine::leaf_tasks(tasks) {
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
    for (kind, items) in [
        ("plugins", &artifact.plugins),
        ("connectors", &artifact.connectors),
        ("workflows", &artifact.workflows),
        ("channels", &artifact.channels),
    ] {
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
                    || message.contains("No draft version")
                    // A reference apply's ordering satisfies: quoted, and of
                    // the dependency kind activated before this entity.
                    || (existence
                        && resolved_by_order
                            .iter()
                            .any(|name| message.contains(&format!("'{name}'"))));
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

fn verify_hash(artifact: &PackageArtifact) -> Result<(), CliError> {
    let actual = artifact_content_hash(artifact)?;
    if actual != artifact.package.content_hash {
        return Err(format!(
            "package.content_hash does not match the entities (expected {actual}) — \
             re-run `package lint` after editing an artifact"
        )
        .into());
    }
    Ok(())
}

/// `(kind, id, rollout)` of every entity the artifact marks `activate: true`,
/// in dependency order: workflows before the channels that name them. The one
/// place the intent fields are read, so plan and apply cannot disagree on
/// their spelling.
fn activation_intents(artifact: &PackageArtifact) -> Vec<(&'static str, String, Option<i64>)> {
    let mut intents = Vec::new();
    // Plugins first: a workflow's activation gate needs every plugin
    // function it names to be dispatchable, and only an active plugin is.
    for entry in &artifact.plugins {
        if entry["activate"] == true
            && let Some(id) = entry["plugin_id"].as_str()
        {
            intents.push(("plugins", id.to_string(), None));
        }
    }
    for entry in &artifact.workflows {
        if entry["activate"] == true
            && let Some(id) = entry["workflow_id"].as_str()
        {
            intents.push((
                "workflows",
                id.to_string(),
                entry["rollout_percentage"].as_i64(),
            ));
        }
    }
    for entry in &artifact.channels {
        if entry["activate"] == true
            && let Some(id) = entry["channel_id"].as_str()
        {
            intents.push(("channels", id.to_string(), None));
        }
    }
    intents
}

// ============================================================
// apply
// ============================================================

pub(crate) async fn run_apply(server: &str, file: &str) -> Result<(), CliError> {
    let artifact = read_artifact(file)?;
    verify_hash(&artifact)?;
    let package = format!("{}@{}", artifact.package.name, artifact.package.version);
    let client = admin_client(server, format!("package={package}"))?;

    // Phase 1 — claim the receipt as staged. This is the atomic
    // same-version-different-content rejection (K14), and doubles as the
    // guard against two concurrent applies.
    if matches!(
        check_receipt(&client, &artifact).await?,
        ReceiptState::AppliedSame
    ) {
        println!("{package} is already applied with identical content — nothing to do");
        return Ok(());
    }
    client
        .put_data::<Value>(
            &receipt_path(&artifact),
            &json!({
                "version": artifact.package.version,
                "content_hash": artifact.package.content_hash,
                "state": "staged",
            }),
        )
        .await
        .map_err(|e| format!("could not claim the receipt: {e}"))?;

    // Phase 2 — stage everything as drafts, in dependency order. Plugins
    // first, so their components are stored before anything names their
    // functions; connector import reloads the connector registry
    // server-side, so workflow activation's registry gate sees them.
    for (kind, items) in [
        ("plugins", &artifact.plugins),
        ("connectors", &artifact.connectors),
        ("workflows", &artifact.workflows),
        ("channels", &artifact.channels),
    ] {
        if items.is_empty() {
            continue;
        }
        let outcome: Value = client
            .post_data(
                &format!("{}?on_conflict=new_version", import_path_for(kind)),
                &Value::Array(items.to_vec()),
            )
            .await?;
        let failed = outcome["failed"].as_u64().unwrap_or(0);
        println!(
            "staged {kind}: {} written, {} unchanged, {failed} failed",
            outcome["imported"], outcome["unchanged"]
        );
        if failed > 0 {
            for error in outcome["errors"].as_array().into_iter().flatten() {
                eprintln!(
                    "error: {kind}[{}]: {}",
                    error["index"],
                    error["error"].as_str().unwrap_or("?")
                );
            }
            return Err(
                "staging failed; nothing was activated and the receipt stays \
                 staged — fix the artifact and re-run (a staged receipt may be re-put)"
                    .into(),
            );
        }
        // Plugins activate as soon as they are staged, reload included: a
        // workflow's create-time gate validates every function name against
        // the *published* registry, so a workflow calling a plugin function
        // cannot even be staged until the plugin is active and loaded. That
        // is one extra reload per package that carries plugins, and the one
        // place apply activates before all staging is done — a plugin that
        // no workflow names yet is harmless to have active.
        if kind == "plugins" {
            for (_, id, _) in activation_intents(&artifact)
                .into_iter()
                .filter(|(k, _, _)| *k == "plugins")
            {
                match client
                    .patch_data::<Value>(
                        &status_path_for("plugins", &id),
                        &json!({"status": orion_api::STATUS_ACTIVE}),
                    )
                    .await
                {
                    Ok(_) => println!("activated plugins '{id}'"),
                    // Staged as `unchanged`: the version is already active.
                    Err(e) if e.status() == Some(StatusCode::NOT_FOUND) => {
                        println!("plugins '{id}' is already active (unchanged)")
                    }
                    Err(e) => {
                        eprintln!("error: activating plugins '{id}': {e}");
                        return Err(format!(
                            "activation stopped at plugins '{id}'. Nothing else was activated; \
                             the receipt stays staged — fix the cause and re-run apply"
                        )
                        .into());
                    }
                }
            }
        }
    }

    // Phase 3 — activate in dependency order with the reload deferred (K4):
    // one engine rebuild and one cluster epoch bump at the end, not one per
    // entity. Plugins were activated in phase 2, above.
    for (kind, id, rollout) in activation_intents(&artifact)
        .into_iter()
        .filter(|(k, _, _)| *k != "plugins")
    {
        let mut body = json!({"status": orion_api::STATUS_ACTIVE});
        if let Some(pct) = rollout {
            body["rollout_percentage"] = json!(pct);
        }
        let result = client
            .patch_data::<Value>(
                &format!("{}?reload=defer", status_path_for(kind, &id)),
                &body,
            )
            .await;
        match result {
            Ok(_) => println!("activated {kind} '{id}'"),
            // Staging just succeeded, so the entity exists; a 404 on its
            // activation can only be "no draft version" — the `unchanged`
            // staging left it active as-is. Matched on the status, not the
            // message, so a rewording cannot turn this benign no-op into a
            // mid-package abort. One thing `unchanged` does NOT cover: the
            // content hash deliberately excludes `rollout_percentage`, so a
            // rollout-only change hashes as unchanged and must land through
            // the rollout endpoint or it is silently dropped while apply
            // reports success.
            Err(e) if e.status() == Some(StatusCode::NOT_FOUND) => {
                if let Some(pct) = rollout {
                    client
                        .patch_data::<Value>(
                            &format!("{}?reload=defer", paths::workflow_rollout(&id)),
                            &json!({"rollout_percentage": pct}),
                        )
                        .await
                        .map_err(|e| {
                            format!(
                                "setting rollout for {kind} '{id}' failed: {e}. Everything \
                                 before it is active but the engine has NOT been reloaded; \
                                 the receipt stays staged — fix the cause and re-run apply \
                                 (idempotent), or run POST /engine/reload to serve what did \
                                 activate"
                            )
                        })?;
                    println!("{kind} '{id}' is already active (unchanged); rollout set to {pct}%");
                } else {
                    println!("{kind} '{id}' is already active (unchanged)");
                }
            }
            Err(e) => {
                eprintln!("error: activating {kind} '{id}': {e}");
                return Err(format!(
                    "activation stopped at {kind} '{id}'. Everything before it is \
                     active but the engine has NOT been reloaded; everything after is \
                     staged as drafts. The receipt stays staged — fix the cause and \
                     re-run apply (idempotent), or run POST /engine/reload to serve \
                     what did activate"
                )
                .into());
            }
        }
    }

    // Phase 4 — one reload, one epoch bump.
    client
        .post_data_empty::<Value>(paths::ENGINE_RELOAD)
        .await
        .map_err(|e| format!("entities are active but the engine reload failed: {e}"))?;

    // Phase 5 — flip the receipt.
    client
        .put_data::<Value>(
            &receipt_path(&artifact),
            &json!({
                "version": artifact.package.version,
                "content_hash": artifact.package.content_hash,
                "state": "applied",
            }),
        )
        .await?;

    println!("applied {package} to {server}");
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
