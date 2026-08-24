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
use orion::storage::repositories::workflows::CreateWorkflowRequest;

type CliError = Box<dyn std::error::Error>;

// ============================================================
// Artifact shapes
// ============================================================

#[derive(Debug, Serialize, Deserialize)]
struct PackageArtifact {
    package: PackageMeta,
    #[serde(default)]
    requires: Requires,
    #[serde(default)]
    connectors: Vec<Value>,
    #[serde(default)]
    workflows: Vec<Value>,
    #[serde(default)]
    channels: Vec<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PackageMeta {
    name: String,
    version: String,
    /// The Orion version that exported this artifact — informational.
    #[serde(default)]
    orion: String,
    content_hash: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    exported_from: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    exported_at: String,
}

/// Declared external dependencies: names this package uses but
/// deliberately does not contain, so closures stay small. `plan` verifies
/// they exist and are active in the target.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Requires {
    #[serde(default)]
    channels: Vec<String>,
    #[serde(default)]
    connectors: Vec<String>,
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
fn artifact_content_hash(artifact: &PackageArtifact) -> Result<String, CliError> {
    Ok(content::content_hash(&json!({
        "connectors": project_entries::<CreateConnectorRequest>(
            &artifact.connectors, "connector", content::connector_request_content)?,
        "workflows": project_entries::<CreateWorkflowRequest>(
            &artifact.workflows, "workflow", content::workflow_request_content)?,
        "channels": project_entries::<CreateChannelRequest>(
            &artifact.channels, "channel", content::channel_request_content)?,
    })))
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

/// The `/import` endpoint for one of the three entity kinds the artifact
/// carries. The kinds are a closed set spelled by this module's own loops.
fn import_path_for(kind: &str) -> &'static str {
    match kind {
        "connectors" => paths::CONNECTORS_IMPORT,
        "workflows" => paths::WORKFLOWS_IMPORT,
        _ => paths::CHANNELS_IMPORT,
    }
}

/// The `PATCH …/status` endpoint for an activation intent's kind.
fn status_path_for(kind: &str, id: &str) -> String {
    match kind {
        "workflows" => paths::workflow_status(id),
        _ => paths::channel_status(id),
    }
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
        },
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
                "wrote {}@{} ({} connectors, {} workflows, {} channels) to {path}",
                artifact.package.name,
                artifact.package.version,
                artifact.connectors.len(),
                artifact.workflows.len(),
                artifact.channels.len(),
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
    // target instance is expected to already have.
    let (set, boundary) = artifact_as_set(&artifact);
    let findings = orion::definitions::check(&set, &boundary, true);

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
            "'{file}' is a valid package: {}@{} — {} connectors, {} workflows, {} channels",
            artifact.package.name,
            artifact.package.version,
            artifact.connectors.len(),
            artifact.workflows.len(),
            artifact.channels.len(),
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
fn artifact_as_set(
    artifact: &PackageArtifact,
) -> (
    orion::definitions::DefinitionSet,
    orion::definitions::Boundary,
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
    (
        orion::definitions::DefinitionSet::from_entries(entries),
        boundary,
    )
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

    // Per-entity actions from the servers' own dry-runs (K2).
    for (kind, items) in [
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
            eprintln!(
                "error: {kind}[{}]: {}",
                error["index"],
                error["error"].as_str().unwrap_or("?")
            );
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
    let provided_connectors: Vec<String> = artifact
        .connectors
        .iter()
        .filter_map(|c| c["name"].as_str().map(str::to_string))
        .collect();
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
            let existence = ["not found", "No draft version", "has no active version"]
                .iter()
                .any(|phrase| message.contains(phrase));
            let pending =
                // The planned entity itself is absent or draft-less — staging
                // creates it before activation runs.
                message.starts_with(&format!("Workflow '{id}' not found"))
                    || message.starts_with(&format!("Channel '{id}' not found"))
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

    // Phase 2 — stage everything as drafts, in dependency order. Connector
    // import reloads the connector registry server-side, so workflow
    // activation's registry gate sees them.
    for (kind, items) in [
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
    }

    // Phase 3 — activate in dependency order with the reload deferred (K4):
    // one engine rebuild and one cluster epoch bump at the end, not one per
    // entity.
    for (kind, id, rollout) in activation_intents(&artifact) {
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
