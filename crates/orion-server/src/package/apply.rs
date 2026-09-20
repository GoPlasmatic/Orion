//! `package apply`: claim the receipt, stage every member as a draft,
//! activate in dependency order with the reload deferred, reload once,
//! check the reloaded generation serves the package, then flip the receipt
//! — and the pieces `plan` shares with it.
//!
//! Written against [`AdminApi`], so the CLI (over HTTP) and the server's
//! boot-time apply (in-process) run one sequence.

use orion_api::PackageInventory;
use orion_client::{StatusCode, paths, query_string};
use serde_json::{Value, json};

use super::artifact::{
    PackageArtifact, Requires, activation_intents, declared_range, members, package_members,
};
use super::transport::AdminApi;
use super::{Baseline, Error, PruneMode, PrunePlan, Reporter};
use crate::signatures::{Kind, Outcome, Subject};

/// How [`apply`] treats what it finds on the target.
pub struct ApplyOptions<'a, A> {
    /// `--prune`: the mode, and the client its removals are sent with —
    /// their own change context, so the audit trail groups them.
    pub prune: Option<(PruneMode, &'a A)>,
    /// What `--signatures` attached, for the re-sign check on a version the
    /// target already runs.
    pub signed: &'a [(Subject, Outcome)],
    /// Re-apply a version a later one superseded — the CLI's rollback — or
    /// leave the target on the newer version, as a node applying its
    /// configured packages at startup does.
    pub roll_back_superseded: bool,
}

/// What [`apply`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// Staged, activated, reloaded, verified serving, receipt applied.
    Applied,
    /// A superseded version re-applied: it is `current` again.
    RolledBack { from: String },
    /// The package's current version already: nothing written, and the
    /// target serves it.
    AlreadyApplied,
    /// The current version, with only its re-signed plugins and models
    /// written.
    Resigned,
    /// A later version is current and `roll_back_superseded` was off:
    /// nothing written, and what the target still holds of this version
    /// serves.
    Superseded { by: String },
}

/// How long `apply` waits for the target to admit a model it staged before
/// activating it. Admission is a fetch, a parse and a probe, so it is
/// seconds for a small model and minutes for a large one over a slow
/// bucket; the wait is bounded so a target whose worker is down fails the
/// apply rather than hanging it.
pub const ADMISSION_WAIT: std::time::Duration = std::time::Duration::from_secs(900);

/// The receipt endpoint for this artifact's package.
pub fn receipt_path(artifact: &PackageArtifact) -> String {
    paths::package(&artifact.package.name)
}

/// The `/import` endpoint for one of the five entity kinds the artifact
/// carries. The kinds are a closed set spelled by this module's own loops.
pub fn import_path_for(kind: &str) -> &'static str {
    match kind {
        "plugins" => paths::PLUGINS_IMPORT,
        "models" => paths::MODELS_IMPORT,
        "connectors" => paths::CONNECTORS_IMPORT,
        "workflows" => paths::WORKFLOWS_IMPORT,
        _ => paths::CHANNELS_IMPORT,
    }
}

/// The `PATCH …/status` endpoint for an activation intent's kind.
pub fn status_path_for(kind: &str, id: &str) -> String {
    match kind {
        "plugins" => paths::plugin_status(id),
        "models" => paths::model_status(id),
        "workflows" => paths::workflow_status(id),
        _ => paths::channel_status(id),
    }
}

/// The admin path of one entity, for its `DELETE`.
pub fn entity_path_for(kind: &str, id: &str) -> String {
    match kind {
        "plugins" => paths::plugin(id),
        "models" => paths::model(id),
        "workflows" => paths::workflow(id),
        _ => paths::channel(id),
    }
}

/// `plan` and `apply` hold the target to the artifact's `requires.orion`,
/// before anything is written. The version is read from `GET
/// /engine/status`, on the admin plane these verbs already use — not
/// `/health`, which answers `503` on a degraded database. In a cluster the
/// answer is the version of whichever node served the request, so apply
/// after a rolling upgrade has finished.
pub async fn check_target_version(
    client: &impl AdminApi,
    server: &str,
    artifact: &PackageArtifact,
    package: &str,
) -> Result<(), Error> {
    let Some(range) = declared_range(artifact)? else {
        return Ok(());
    };
    let status: orion_api::EngineStatusResponse = client.get_data(paths::ENGINE_STATUS).await?;
    if status.version.is_empty() {
        return Err(format!(
            "could not read the target's version from {server} — refusing to apply a package \
             that declares requires.orion"
        )
        .into());
    }
    range.check_target(package, server, &status.version)?;
    Ok(())
}

/// Whether the target holds every `storage` connector the carried models
/// fetch through — `requires.storage`, checked by `plan` and again by
/// `apply` before it stages anything, because an import of a model whose
/// connector is missing fails at write with the receipt already claimed.
/// One export sweep matched by `name`, as `requires.connectors` is checked:
/// a model reference names a connector by its name, and `GET
/// /connectors/{id}` is keyed by the row's id.
pub async fn missing_storage(
    client: &impl AdminApi,
    artifact: &PackageArtifact,
    report: &dyn Reporter,
) -> Result<usize, Error> {
    if artifact.requires.storage.is_empty() {
        return Ok(0);
    }
    let stored: Value = client.get_data(paths::CONNECTORS_EXPORT).await?;
    let mut missing = 0usize;
    for name in &artifact.requires.storage {
        let row = stored
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| c["name"].as_str() == Some(name));
        match row {
            Some(row) if row["connector_type"] == "storage" => {}
            Some(row) => {
                report.err(&format!(
                    "error: required storage connector '{name}' exists in the target but is a \
                     '{}' connector — a model artifact is fetched through a storage connector",
                    row["connector_type"].as_str().unwrap_or("?")
                ));
                missing += 1;
            }
            None => {
                report.err(&format!(
                    "error: required storage connector '{name}' does not exist in the target — \
                     the models this package carries are fetched through it"
                ));
                missing += 1;
            }
        }
    }
    Ok(missing)
}

/// The receipt verdict `plan` and `apply` both start from.
pub enum ReceiptState {
    Fresh,
    Staged,
    /// Applied with this content, and still the package's `current` version:
    /// the target already serves it, so apply is a no-op.
    AppliedCurrent,
    /// Applied with this content once, but a later version has been applied
    /// since (`current` names it): re-applying is the documented rollback.
    AppliedSuperseded {
        current: String,
    },
    AppliedConflict,
}

/// The verdict, and the `GET /packages/{name}` body it came from (`null`
/// for a package with no receipts) — `--prune` reads its baseline there.
pub async fn check_receipt(
    client: &impl AdminApi,
    artifact: &PackageArtifact,
) -> Result<(ReceiptState, Value), Error> {
    let receipts: Option<Value> = client.get_data_opt(&receipt_path(artifact)).await?;
    let Some(receipts) = receipts else {
        return Ok((ReceiptState::Fresh, Value::Null));
    };
    Ok((receipt_state(&receipts, artifact), receipts))
}

/// The verdict for `artifact` from a `GET /packages/{name}` body.
pub fn receipt_state(receipts: &Value, artifact: &PackageArtifact) -> ReceiptState {
    let version = artifact.package.version.as_str();
    let row = receipts["versions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["version"] == version);
    match row {
        None => ReceiptState::Fresh,
        Some(row) if row["state"] == "applied" => {
            if row["content_hash"] != artifact.package.content_hash.as_str() {
                return ReceiptState::AppliedConflict;
            }
            // `current` is the newest applied receipt. An applied version that
            // is not it was superseded, and its entities may no longer be what
            // the target serves — a hash match on the receipt alone says
            // nothing about the estate.
            match receipts["current"]["version"].as_str() {
                Some(current) if current != version => ReceiptState::AppliedSuperseded {
                    current: current.to_string(),
                },
                _ => ReceiptState::AppliedCurrent,
            }
        }
        Some(_) => ReceiptState::Staged,
    }
}

/// The receipt `--prune` measures from, in a `GET /packages/{name}` body:
/// the package's `current` one — for a rollback, the version being rolled
/// back from — as its version and, when it recorded one, its inventory.
/// `None` on the no-op path: re-running the same deploy must not re-archive
/// what someone re-created since.
pub fn prune_baseline(
    receipts: &Value,
    state: &ReceiptState,
) -> Option<(String, Option<PackageInventory>)> {
    if matches!(
        state,
        ReceiptState::AppliedCurrent | ReceiptState::AppliedConflict
    ) {
        return None;
    }
    let current = receipts.get("current").filter(|c| !c.is_null())?;
    let version = current["version"].as_str()?.to_string();
    let inventory = current
        .get("inventory")
        .filter(|i| !i.is_null())
        .and_then(|i| serde_json::from_value(i.clone()).ok());
    Some((version, inventory))
}

/// What `--prune` would remove on this apply, measured from
/// [`prune_baseline`].
pub async fn prune_plan_for(
    client: &impl AdminApi,
    artifact: &PackageArtifact,
    receipts: &Value,
    state: &ReceiptState,
) -> Result<PrunePlan, Error> {
    let Some((version, inventory)) = prune_baseline(receipts, state) else {
        return Ok(PrunePlan::default());
    };
    let baseline = format!("{}@{version}", artifact.package.name);
    let next = package_members(artifact).inventory();
    let measure = |others: &[(String, PackageInventory)]| {
        super::prune_plan(
            Some(Baseline {
                package: &baseline,
                inventory: inventory.as_ref(),
            }),
            &next,
            others,
        )
    };
    let plan = measure(&[]);
    if plan.is_empty() {
        return Ok(plan);
    }
    // The ownership guard: an entity that moved to another package is that
    // package's now, whatever this one's receipt says.
    let rows: Value = client
        .get_data(&format!(
            "{}{}",
            paths::PACKAGES,
            query_string(&[
                ("current", Some("true".to_string())),
                ("limit", Some("1000".to_string())),
            ])
        ))
        .await?;
    let others: Vec<(String, PackageInventory)> = rows
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["name"] != artifact.package.name.as_str())
        .filter_map(|row| {
            let inventory = serde_json::from_value(row.get("inventory")?.clone()).ok()?;
            Some((
                format!(
                    "{}@{}",
                    row["name"].as_str()?,
                    row["version"].as_str().unwrap_or("?")
                ),
                inventory,
            ))
        })
        .collect();
    Ok(measure(&others))
}

/// The removals something outside the prune still depends on. A workflow
/// or a connector is checked here, against the target's active rows,
/// because no server gate refuses its removal; a plugin or a model the
/// server refuses itself, with a `409`.
pub async fn prune_refusals(
    client: &impl AdminApi,
    artifact: &PackageArtifact,
    plan: &PrunePlan,
) -> Result<Vec<super::Refusal>, Error> {
    let active = |path: &str| {
        format!(
            "{path}{}",
            query_string(&[("status", Some(orion_api::STATUS_ACTIVE.to_string()))])
        )
    };
    let fetch = |wanted: bool, path: &'static str| async move {
        if wanted {
            client.get_data::<Vec<Value>>(&active(path)).await
        } else {
            Ok(Vec::new())
        }
    };
    let workflows_pruned = plan.removals().any(|r| r.kind == "workflows");
    let connectors_pruned = plan.removals().any(|r| r.kind == "connectors");
    let channels = fetch(workflows_pruned, paths::CHANNELS_EXPORT).await?;
    let workflows = fetch(connectors_pruned, paths::WORKFLOWS_EXPORT).await?;
    let models = fetch(connectors_pruned, paths::MODELS_EXPORT).await?;
    Ok(super::refusals(
        plan,
        &super::References {
            carried_channels: &artifact.channels,
            carried_workflows: &artifact.workflows,
            carried_models: &artifact.models,
            active_channels: &channels,
            active_workflows: &workflows,
            active_models: &models,
        },
        crate::engine::FunctionRegistry::builtin(),
    ))
}

/// What a removal does to one kind, as a verb.
pub fn prune_verb(kind: &str, mode: PruneMode) -> &'static str {
    match (mode, kind) {
        (PruneMode::Delete, _) => "delete",
        (PruneMode::Archive, "connectors") => "disable",
        (PruneMode::Archive, _) => "archive",
    }
}

/// `plan`'s and `apply`'s account of a prune before anything is removed.
pub fn print_prune_plan(plan: &PrunePlan, mode: PruneMode, report: &dyn Reporter) {
    let baseline = plan.baseline.as_deref().unwrap_or("the current version");
    if plan.baseline_without_inventory {
        report.out(&format!(
            "note: {baseline} was applied before receipts recorded what they carried — nothing \
             to prune this time; this apply records an inventory, so the next one can"
        ));
    }
    for (kind, count) in &plan.emptied {
        report.err(&format!(
            "warning: this artifact carries no {kind} — all {count} {kind} of {baseline} will be \
             pruned"
        ));
    }
    for removal in plan.removals() {
        report.out(&format!(
            "  {:<10} {:<28} prune: {} (in {baseline}, not in this artifact)",
            removal.kind,
            removal.id,
            prune_verb(removal.kind, mode)
        ));
    }
    for (removal, owner) in &plan.kept {
        report.out(&format!(
            "  {:<10} {:<28} keep: now carried by {owner}",
            removal.kind, removal.id
        ));
    }
}

/// Carry out `removals`, in order, with every engine reload deferred to the
/// apply's one. An entity already archived or gone is not an error — a
/// re-run after a failure finds the first removals done. Any other refusal
/// stops the apply, naming the entity.
pub async fn run_removals(
    client: &impl AdminApi,
    removals: &[super::Removal],
    mode: PruneMode,
    report: &dyn Reporter,
) -> Result<(), Error> {
    if removals.is_empty() {
        return Ok(());
    }
    let stop = |removal: &super::Removal, e: &dyn std::fmt::Display| -> Error {
        report.err(&format!(
            "error: pruning {} '{}' was refused: {e}",
            removal.kind, removal.id
        ));
        format!(
            "prune stopped at {} '{}'. Removals and activations before it are committed but \
             the engine has NOT been reloaded; the receipt stays staged — remove the outside \
             reference (or drop --prune) and re-run apply (idempotent)",
            removal.kind, removal.id
        )
        .into()
    };
    // A connector is addressed by its row id, a package names it by name.
    let connectors: Vec<Value> = if removals.iter().any(|r| r.kind == "connectors") {
        client.get_data(paths::CONNECTORS_EXPORT).await?
    } else {
        Vec::new()
    };
    for removal in removals {
        let (kind, id) = (removal.kind, removal.id.as_str());
        let verb = prune_verb(kind, mode);
        let result = if kind == "connectors" {
            let Some(row) = connectors.iter().find(|c| c["name"] == id) else {
                report.out(&format!("pruned {kind} '{id}' (already gone)"));
                continue;
            };
            let row_id = row["id"].as_str().unwrap_or_default();
            match mode {
                PruneMode::Archive if row["enabled"] == false => {
                    report.out(&format!("pruned {kind} '{id}' (already disabled)"));
                    continue;
                }
                PruneMode::Archive => client
                    .put_data::<Value>(&paths::connector(row_id), &json!({"enabled": false}))
                    .await
                    .map(|_| ()),
                PruneMode::Delete => client.delete(&paths::connector(row_id)).await,
            }
        } else {
            match mode {
                PruneMode::Archive => client
                    .patch_data::<Value>(
                        &format!("{}?reload=defer", status_path_for(kind, id)),
                        &json!({"status": orion_api::STATUS_ARCHIVED}),
                    )
                    .await
                    .map(|_| ()),
                PruneMode::Delete => {
                    client
                        .delete(&format!("{}?reload=defer", entity_path_for(kind, id)))
                        .await
                }
            }
        };
        match result {
            Ok(()) => report.out(&format!("pruned {kind} '{id}' ({verb}d)")),
            // Archive answers 404 for "no active version", delete for
            // "no such entity": either way the removal already happened.
            Err(e) if e.status() == Some(StatusCode::NOT_FOUND) => {
                report.out(&format!("pruned {kind} '{id}' (already {verb}d)"))
            }
            Err(e) => return Err(stop(removal, &e)),
        }
    }
    Ok(())
}

/// The prune a `stage_activate_reload` runs around its activations.
pub struct PruneRun<'a, A> {
    pub client: &'a A,
    pub plan: &'a PrunePlan,
    pub mode: PruneMode,
}

/// The load issues a server reports, when it reports them: from the answer
/// in hand, or — from a server older than that field — the lists an admin's
/// `/health` carries. `None` when neither says.
pub async fn load_issues_or_health(
    client: &impl AdminApi,
    reported: Option<orion_api::EngineLoadIssues>,
) -> Option<orion_api::EngineLoadIssues> {
    if reported.is_some() {
        return reported;
    }
    let health: Value = client.get(paths::HEALTH).await.ok()?;
    // Detail withheld, or a server that predates the lists.
    health.get("channels")?;
    let list = |value: &Value| value.clone();
    serde_json::from_value(json!({
        "channels": list(&health["channels"]["quarantined"]),
        "plugins": list(&health["plugins"]["failed_to_load"]),
        "models": list(&health["models"]["failed_to_load"]),
        "connectors": list(&health["connectors"]["failed_to_load"]),
    }))
    .ok()
}

/// Phase 4b: whether the generation the reload published serves what the
/// artifact carries. `Err` names every member it quarantined.
pub async fn verify_serving(
    client: &impl AdminApi,
    server: &str,
    package: &str,
    artifact: &PackageArtifact,
    reported: Option<orion_api::EngineLoadIssues>,
    receipt_note: &str,
    report: &dyn Reporter,
) -> Result<(), Error> {
    let Some(issues) = load_issues_or_health(client, reported).await else {
        report.err(&format!(
            "warning: {server} does not report load issues (older than this CLI?) — could not \
             verify that {package} is serving; check its /health"
        ));
        return Ok(());
    };
    let quarantined = super::quarantined_members(&package_members(artifact), &issues);
    if quarantined.is_empty() {
        return Ok(());
    }
    report.err(&format!(
        "error: {} {} of {package} {} quarantined on {server}:",
        quarantined.len(),
        if quarantined.len() == 1 {
            "entity"
        } else {
            "entities"
        },
        if quarantined.len() == 1 { "is" } else { "are" },
    ));
    for entity in &quarantined {
        report.err(&format!("  {entity}"));
    }
    Err(format!("{package} is not serving on {server} — {receipt_note}").into())
}

/// Apply `artifact` (hash verified, signatures attached) to the target
/// `client` reaches. `target` names it in messages.
///
/// # Errors
///
/// Every refusal, with the line an operator reads: a target outside
/// `requires.orion`, an immutability conflict, a missing storage connector,
/// a `--prune` refusal, a failed stage or activation, and a member the
/// reloaded generation quarantined.
pub async fn apply<A: AdminApi>(
    client: &A,
    target: &str,
    artifact: &PackageArtifact,
    opts: &ApplyOptions<'_, A>,
    report: &dyn Reporter,
) -> Result<ApplyOutcome, Error> {
    let package = format!("{}@{}", artifact.package.name, artifact.package.version);
    let server = target;
    // Before the receipt is claimed: a target outside the declared range
    // gets zero writes.
    check_target_version(client, server, artifact, &package).await?;

    // Phase 1 — claim the receipt as staged. This is the atomic
    // same-version-different-content rejection (K14), and doubles as the
    // guard against two concurrent applies.
    let (receipt, receipts) = check_receipt(client, artifact).await?;
    match &receipt {
        ReceiptState::AppliedConflict => {
            return Err(format!(
                "{package} is already applied on {server} with different content — an \
                 applied package version is immutable; bump the package version"
            )
            .into());
        }
        ReceiptState::AppliedCurrent => {
            if opts.prune.is_some() {
                report.out(&format!("{package} is already applied — nothing to prune"));
            }
            // The content is applied, but a signature is not content: a
            // re-apply with a new key's signatures must still reach the rows.
            let resign = resigned_members(client, artifact, opts.signed).await?;
            if resign.plugins.is_empty() && resign.models.is_empty() {
                verify_applied(
                    client,
                    server,
                    &package,
                    artifact,
                    "the receipt is already applied and stays so; fix the cause, then \
                     POST /engine/reload",
                    report,
                )
                .await?;
                report.out(&format!(
                    "{package} is already applied with identical content — nothing to do"
                ));
                return Ok(ApplyOutcome::AlreadyApplied);
            }
            for (kind, entries) in [("plugins", &resign.plugins), ("models", &resign.models)] {
                for entry in entries {
                    let id = entry["plugin_id"]
                        .as_str()
                        .or(entry["model_id"].as_str())
                        .unwrap_or("?");
                    report.out(&format!("re-signed {kind} '{id}'"));
                }
            }
            // Only the re-signed members, and the receipt stays as it is:
            // the content it records did not move.
            let reloaded = stage_activate_reload(client, &resign, None, report).await?;
            verify_serving(
                client,
                server,
                &package,
                artifact,
                reloaded.load_issues,
                "the receipt is already applied and stays so; fix the cause and re-run apply",
                report,
            )
            .await?;
            report.out(&format!(
                "applied the new signatures of {package} to {server}"
            ));
            return Ok(ApplyOutcome::Resigned);
        }
        ReceiptState::AppliedSuperseded { current } if !opts.roll_back_superseded => {
            report.err(&format!(
                "warning: {package} is superseded on {server} by {}@{current} — left as it is; \
                 run `orion-server package apply` to roll back deliberately",
                artifact.package.name
            ));
            verify_applied(
                client,
                server,
                &package,
                artifact,
                "the receipt is superseded and stays so; fix the cause, then POST /engine/reload",
                report,
            )
            .await?;
            return Ok(ApplyOutcome::Superseded {
                by: format!("{}@{current}", artifact.package.name),
            });
        }
        ReceiptState::AppliedSuperseded { current } => report.out(&format!(
            "{package} was superseded by {}@{current} — re-applying it as a rollback",
            artifact.package.name
        )),
        ReceiptState::Fresh | ReceiptState::Staged => {}
    }
    // Before the claim: a model import fails at write without its storage
    // connector, and that is better learned with nothing claimed.
    if missing_storage(client, artifact, report).await? > 0 {
        return Err(
            "the target lacks a storage connector this package's models are fetched \
             through; create it there (or carry it in the package) and re-run apply"
                .into(),
        );
    }
    // Also before the claim: what `--prune` removes, measured from the
    // receipt read above, and every removal something outside it still
    // depends on — refused with nothing written, so nothing is half-pruned.
    let prune_plan = match opts.prune {
        Some((mode, prune_client)) => {
            let plan = prune_plan_for(client, artifact, &receipts, &receipt).await?;
            print_prune_plan(&plan, mode, report);
            let refusals = prune_refusals(client, artifact, &plan).await?;
            if !refusals.is_empty() {
                for refusal in &refusals {
                    report.err(&format!("error: {refusal}"));
                }
                return Err(format!(
                    "--prune refused {} removal(s); nothing was written — remove the outside \
                     reference, or drop --prune",
                    refusals.len()
                )
                .into());
            }
            Some((plan, mode, prune_client))
        }
        None => None,
    };
    let inventory = package_members(artifact).inventory();
    let rollback = match &receipt {
        ReceiptState::AppliedSuperseded { current } => Some(current.clone()),
        _ => None,
    };
    // A superseded version is re-applied without the claim: the receipt
    // store refuses applied → staged, and the hash already matches, so the
    // K14 check has nothing to reject. Without the claim two concurrent
    // rollbacks to one version are not excluded — both stage and activate
    // the same content, and phase 5's put is an idempotent touch, so they
    // converge rather than conflict.
    if rollback.is_none() {
        client
            .put_data::<Value>(
                &receipt_path(artifact),
                &json!({
                    "version": artifact.package.version,
                    "content_hash": artifact.package.content_hash,
                    "state": "staged",
                    "inventory": inventory,
                }),
            )
            .await
            .map_err(|e| format!("could not claim the receipt: {e}"))?;
    }

    let prune_run = prune_plan
        .as_ref()
        .map(|(plan, mode, prune_client)| PruneRun {
            client: *prune_client,
            plan,
            mode: *mode,
        });
    let reloaded = stage_activate_reload(client, artifact, prune_run.as_ref(), report).await?;

    // Phase 4b — "applied" must mean serving. The reload succeeds when an
    // entity does not load: it is quarantined and everything else serves.
    // A member of this package quarantined by the generation just published
    // fails the apply here, before the flip, so the receipt stays staged.
    verify_serving(
        client,
        server,
        &package,
        artifact,
        reloaded.load_issues,
        if rollback.is_some() {
            "the rollback's receipt was not moved; fix the cause and re-run apply"
        } else {
            "the receipt stays staged; fix the cause and re-run apply"
        },
        report,
    )
    .await?;

    // Phase 5 — flip the receipt.
    client
        .put_data::<Value>(
            &receipt_path(artifact),
            &json!({
                "version": artifact.package.version,
                "content_hash": artifact.package.content_hash,
                "state": "applied",
                "inventory": inventory,
            }),
        )
        .await?;

    report.out(&format!("applied {package} to {server}"));
    Ok(match rollback {
        Some(from) => ApplyOutcome::RolledBack {
            from: format!("{}@{from}", artifact.package.name),
        },
        None => ApplyOutcome::Applied,
    })
}

/// A version the target already holds, checked without a write: applied
/// is not serving when a node's config changed since (cron switched off,
/// trust keys rotated) and it quarantines what the receipt says is applied.
/// One read of `GET /engine/status`.
async fn verify_applied(
    client: &impl AdminApi,
    server: &str,
    package: &str,
    artifact: &PackageArtifact,
    receipt_note: &str,
    report: &dyn Reporter,
) -> Result<(), Error> {
    let status: orion_api::EngineStatusResponse = client.get_data(paths::ENGINE_STATUS).await?;
    if status.load_issues.is_some() {
        verify_serving(
            client,
            server,
            package,
            artifact,
            status.load_issues,
            receipt_note,
            report,
        )
        .await?;
    }
    Ok(())
}

/// Phases 2–4 of `apply`: stage every member as drafts in dependency order,
/// activate in dependency order with the reload deferred, then reload once.
/// With `prune`, the removed channels go between staging and activation and
/// everything else it removes after activation, inside the same reload.
/// Returns the reload's answer.
pub async fn stage_activate_reload<A: AdminApi>(
    client: &A,
    artifact: &PackageArtifact,
    prune: Option<&PruneRun<'_, A>>,
    report: &dyn Reporter,
) -> Result<orion_api::EngineReloadedResponse, Error> {
    // Phase 2 — stage everything as drafts, in dependency order. Plugins
    // first, so their components are stored before anything names their
    // functions; connector import reloads the connector registry
    // server-side, so a model's reference resolves and workflow
    // activation's registry gate sees them; models before the workflows
    // that name them.
    for (kind, items) in members(artifact) {
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
        report.out(&format!(
            "staged {kind}: {} written, {} unchanged, {failed} failed",
            outcome["imported"], outcome["unchanged"]
        ));
        if failed > 0 {
            for error in outcome["errors"].as_array().into_iter().flatten() {
                report.err(&format!(
                    "error: {kind}[{}]: {}",
                    error["index"],
                    error["error"].as_str().unwrap_or("?")
                ));
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
            for (_, id, _) in activation_intents(artifact)
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
                    Ok(_) => report.out(&format!("activated plugins '{id}'")),
                    // Staged as `unchanged`: the version is already active.
                    Err(e) if e.status() == Some(StatusCode::NOT_FOUND) => {
                        report.out(&format!("plugins '{id}' is already active (unchanged)"))
                    }
                    Err(e) => {
                        report.err(&format!("error: activating plugins '{id}': {e}"));
                        return Err(format!(
                            "activation stopped at plugins '{id}'. Nothing else was activated; \
                             the receipt stays staged — fix the cause and re-run apply"
                        )
                        .into());
                    }
                }
            }
        }
        // A model import queues admission on the target — the fetch, the
        // digest check, the parse and the probe — and activation is refused
        // until the verdict is `passed`. Wait for it here, model by model,
        // so phase 3 can activate them in order with everything else.
        if kind == "models" {
            for (_, id, _) in activation_intents(artifact)
                .into_iter()
                .filter(|(k, _, _)| *k == "models")
            {
                wait_for_admission(client, &id, report).await?;
            }
        }
    }

    // Phase 2b — the channels `--prune` removes, before activation: the
    // route and name gates read active rows, so a route moving to a new
    // channel id could not activate while the old channel still holds it.
    // The reload is deferred, so traffic sees the swap at phase 4 only.
    if let Some(run) = prune {
        run_removals(run.client, &run.plan.early, run.mode, report).await?;
    }
    let archived_early = prune.is_some_and(|run| !run.plan.early.is_empty());

    // Phase 3 — activate in dependency order with the reload deferred (K4):
    // one engine rebuild and one cluster epoch bump at the end, not one per
    // entity. Plugins were activated in phase 2, above.
    for (kind, id, rollout) in activation_intents(artifact)
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
            Ok(_) => report.out(&format!("activated {kind} '{id}'")),
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
                    report.out(&format!(
                        "{kind} '{id}' is already active (unchanged); rollout set to {pct}%"
                    ));
                } else {
                    report.out(&format!("{kind} '{id}' is already active (unchanged)"));
                }
            }
            Err(e) => {
                report.err(&format!("error: activating {kind} '{id}': {e}"));
                return Err(format!(
                    "activation stopped at {kind} '{id}'. Everything before it is \
                     active but the engine has NOT been reloaded; everything after is \
                     staged as drafts.{} The receipt stays staged — fix the cause and \
                     re-run apply (idempotent), or run POST /engine/reload to serve \
                     what did activate",
                    if archived_early {
                        " The channels --prune archived are still served until that reload."
                    } else {
                        ""
                    }
                )
                .into());
            }
        }
    }

    // Phase 3b — the rest of what `--prune` removes: workflows first, then
    // what they called, once the new versions no longer name them.
    if let Some(run) = prune {
        run_removals(run.client, &run.plan.late, run.mode, report).await?;
    }

    // Phase 4 — one reload, one epoch bump. The answer describes the
    // generation this reload published, which is what phase 4b verifies.
    let reloaded: orion_api::EngineReloadedResponse = client
        .post_data_empty(paths::ENGINE_RELOAD)
        .await
        .map_err(|e| format!("entities are active but the engine reload failed: {e}"))?;
    Ok(reloaded)
}

/// The members whose signature `--signatures` changed against what the
/// target's latest version holds — what a re-apply of an applied version
/// must still write. Everything else is left out, so the partial artifact
/// stages only them.
pub async fn resigned_members(
    client: &impl AdminApi,
    artifact: &PackageArtifact,
    signed: &[(Subject, Outcome)],
) -> Result<PackageArtifact, Error> {
    let mut resign = PackageArtifact {
        package: artifact.package.clone(),
        requires: Requires::default(),
        plugins: Vec::new(),
        models: Vec::new(),
        connectors: Vec::new(),
        workflows: Vec::new(),
        channels: Vec::new(),
    };
    if !signed
        .iter()
        .any(|(_, outcome)| matches!(outcome, Outcome::Signed { .. }))
    {
        return Ok(resign);
    }
    let stored = |rows: &Value, key: &str, id: &str| -> Option<(String, String)> {
        rows.as_array()?
            .iter()
            .find(|row| row[key] == id)
            .map(|row| {
                (
                    row["signature"].as_str().unwrap_or_default().to_string(),
                    row["status"].as_str().unwrap_or_default().to_string(),
                )
            })
    };
    let plugins: Value = if artifact.plugins.is_empty() {
        Value::Null
    } else {
        client.get_data(paths::PLUGINS_EXPORT).await?
    };
    let models: Value = if artifact.models.is_empty() {
        Value::Null
    } else {
        client.get_data(paths::MODELS_EXPORT).await?
    };
    let entries = artifact.plugins.iter().chain(artifact.models.iter());
    for (entry, (subject, outcome)) in entries.zip(signed) {
        let Outcome::Signed { signature, .. } = outcome else {
            continue;
        };
        let (rows, key, into) = match subject.kind {
            Kind::Plugin => (&plugins, "plugin_id", &mut resign.plugins),
            Kind::Model => (&models, "model_id", &mut resign.models),
        };
        let current = stored(rows, key, &subject.id);
        if current.as_ref().is_none_or(|(stored_signature, status)| {
            stored_signature != signature || status != orion_api::STATUS_ACTIVE
        }) {
            into.push(entry.clone());
        }
    }
    Ok(resign)
}

/// Poll `GET /models/{id}` until the target's admission of the latest
/// version leaves `pending` — `passed` lets phase 3 activate it, `failed`
/// stops the apply naming the stage and reason, because activation would
/// be refused with the same message and nothing after it could serve.
/// Bounded by [`ADMISSION_WAIT`], with a line on stderr every few seconds
/// so a long fetch does not read as a hang.
pub async fn wait_for_admission(
    client: &impl AdminApi,
    id: &str,
    report: &dyn Reporter,
) -> Result<(), Error> {
    let started = std::time::Instant::now();
    let mut last_report = std::time::Instant::now();
    loop {
        let row: Value = client.get_data(&paths::model(id)).await?;
        let state = row["admission"]["state"].as_str().unwrap_or("pending");
        match state {
            "passed" => {
                report.out(&format!(
                    "admitted models '{id}' on the target ({} parameters, {:.1} ms probe)",
                    row["stats"]["parameters"],
                    row["stats"]["probe_ms"].as_f64().unwrap_or(0.0)
                ));
                return Ok(());
            }
            "failed" => {
                return Err(format!(
                    "the target refused model '{id}' at admission stage '{}': {} — fix the \
                     artifact or the reference (POST /models/{id}/admit retries it); the \
                     receipt stays staged, and nothing was activated",
                    row["admission"]["stage"].as_str().unwrap_or("unknown"),
                    row["admission"]["reason"]
                        .as_str()
                        .unwrap_or("no reason recorded")
                )
                .into());
            }
            _ => {}
        }
        if started.elapsed() > ADMISSION_WAIT {
            return Err(format!(
                "model '{id}' is still pending admission on the target after {}s — is the \
                 target's model_admission worker running (see /health)? The receipt stays \
                 staged; re-run apply once GET /models/{id} reports admission.state 'passed'",
                ADMISSION_WAIT.as_secs()
            )
            .into());
        }
        if last_report.elapsed() >= std::time::Duration::from_secs(5) {
            report.err(&format!(
                "waiting for the target to admit models '{id}' ({}s)",
                started.elapsed().as_secs()
            ));
            last_report = std::time::Instant::now();
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::super::artifact::test_support::*;
    use super::*;

    /// An applied receipt is a no-op only while it is the package's
    /// `current` version; once a later version superseded it, re-applying it
    /// is the rollback and must run.
    #[test]
    fn an_applied_receipt_is_a_no_op_only_while_current() {
        let mut artifact = artifact(Vec::new());
        artifact.package.content_hash = "sha256:aaa".to_string();
        let receipts = |current: &str| {
            json!({
                "name": "p",
                "current": {"version": current, "state": "applied"},
                "versions": [
                    {"version": "1.1.0", "content_hash": "sha256:bbb", "state": "applied"},
                    {"version": "1.0.0", "content_hash": "sha256:aaa", "state": "applied"},
                ],
            })
        };
        assert!(matches!(
            receipt_state(&receipts("1.0.0"), &artifact),
            ReceiptState::AppliedCurrent
        ));
        assert!(matches!(
            receipt_state(&receipts("1.1.0"), &artifact),
            ReceiptState::AppliedSuperseded { current } if current == "1.1.0"
        ));

        artifact.package.content_hash = "sha256:ccc".to_string();
        assert!(matches!(
            receipt_state(&receipts("1.1.0"), &artifact),
            ReceiptState::AppliedConflict
        ));
        artifact.package.version = "2.0.0".to_string();
        assert!(matches!(
            receipt_state(&receipts("1.1.0"), &artifact),
            ReceiptState::Fresh
        ));
    }

    /// `--prune` measures from the package's `current` receipt — for a
    /// rollback, the version rolled back from — and from nothing on the
    /// no-op path.
    #[test]
    fn prune_measures_from_the_current_receipt_except_on_the_no_op_path() {
        let receipts = json!({
            "current": {"version": "1.1.0", "state": "applied",
                        "inventory": {"channels": ["only-in-1.1.0"]}},
        });
        let superseded = ReceiptState::AppliedSuperseded {
            current: "1.1.0".to_string(),
        };
        let (version, inventory) =
            prune_baseline(&receipts, &superseded).expect("a rollback has a baseline");
        assert_eq!(version, "1.1.0");
        assert_eq!(
            inventory.expect("recorded").channels,
            ["only-in-1.1.0".to_string()]
        );
        assert!(prune_baseline(&receipts, &ReceiptState::AppliedCurrent).is_none());
        let (_, inventory) = prune_baseline(
            &json!({"current": {"version": "0.9.0"}}),
            &ReceiptState::Fresh,
        )
        .expect("a baseline");
        assert!(inventory.is_none(), "a receipt from before inventories");
        assert!(prune_baseline(&Value::Null, &ReceiptState::Fresh).is_none());
    }
}
