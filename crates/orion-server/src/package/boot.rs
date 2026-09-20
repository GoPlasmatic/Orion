//! `[packages] apply`: the artifacts a node applies to itself at startup.
//!
//! Runs after the first generation is published and the background tasks
//! are up, with the listener already bound, so `/healthz` answers while a
//! long model admission is in flight and `/readyz` answers `503` until
//! every package is serving. Each artifact goes through [`super::apply()`]
//! over [`InProcessAdmin`] — the sequence `orion-server package apply` runs,
//! with the same gates, receipts and audit rows — in list order, stopping
//! at the first failure, which
//! [`BootPackages::fail`](crate::runtime::boot_packages::BootPackages::fail)
//! turns into a process exit.
//!
//! Two differences from the CLI, both about restarts. The same artifact
//! again is a no-op, verified serving. And a version another deploy has
//! since superseded is left as it is: a node restarting on an old image in
//! the middle of a rolling deploy must not roll the whole cluster back.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::{
    AdminApi, ApplyOptions, ApplyOutcome, Error, InProcessAdmin, PackageArtifact, Reporter,
};
use crate::runtime::boot_packages::BootState;
use crate::server::state::AppState;

/// Who a startup apply's receipts and audit rows name.
pub const PRINCIPAL: &str = "system:boot-packages";

/// The cluster lease one node holds while it applies one package: renewed
/// well inside its TTL, so a dead holder's peers take over within a minute.
const LEASE_TTL_SECS: u64 = 60;
const LEASE_RENEW: Duration = Duration::from_secs(20);
/// How often a node waiting on a peer's apply looks again.
const PEER_POLL: Duration = Duration::from_secs(2);

/// Apply every `[packages] apply` artifact, recording the outcome on
/// `state.packages`. Never panics and never returns an error: a failure is
/// recorded there, which is what stops the process.
pub async fn run(state: AppState) {
    if state.packages.is_empty() {
        return;
    }
    let files = state.config.packages.apply.clone();
    let budget = state.config.packages.apply_timeout_secs;
    let work = apply_all(&state, &files);
    if budget == 0 {
        work.await;
        return;
    }
    if tokio::time::timeout(Duration::from_secs(budget), work)
        .await
        .is_err()
    {
        let snapshot = state.packages.snapshot();
        let index = snapshot
            .iter()
            .position(|e| !e.state.is_serving())
            .unwrap_or(0);
        let error = format!(
            "package '{}' failed to apply at startup: not done within \
             packages.apply_timeout_secs ({budget}s)",
            files[index]
        );
        tracing::error!("{error}");
        state.packages.fail(index, error);
    }
}

/// What `validate-config` and a starting node check before any package is
/// applied: every artifact exists, parses, hashes to its `content_hash` and
/// lints clean, and `signatures_dir` is a directory. Each problem is
/// `(entry index, message)`, the message naming the setting.
pub fn check_files(config: &crate::config::PackagesConfig) -> Vec<(usize, String)> {
    let mut problems = Vec::new();
    for (index, file) in config.apply.iter().enumerate() {
        let check = || -> Result<(), Error> {
            let artifact = super::read_artifact(file)?;
            super::verify_hash(&artifact)?;
            let lint = super::lint_artifact(&artifact)?;
            if lint.errors.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "{} lint error(s): {}",
                    lint.errors.len(),
                    lint.errors.join("; ")
                )
                .into())
            }
        };
        if let Err(e) = check() {
            problems.push((index, format!("packages.apply[{index}] '{file}': {e}")));
        }
    }
    if let Some(dir) = &config.signatures_dir
        && !std::path::Path::new(dir).is_dir()
    {
        problems.push((
            0,
            format!("packages.signatures_dir '{dir}' is not a directory"),
        ));
    }
    problems
}

async fn apply_all(state: &AppState, files: &[String]) {
    // Every artifact is checked before the first is applied: a typo in the
    // third path must not leave the first two applied and the node down.
    if let Some((index, problem)) = check_files(&state.config.packages).into_iter().next() {
        let error =
            format!("[packages] failed to apply at startup: {problem} — nothing was applied");
        tracing::error!("{error}");
        state.packages.fail(index, error);
        return;
    }
    for (index, file) in files.iter().enumerate() {
        state
            .packages
            .update(index, |entry| entry.state = BootState::Applying);
        match apply_one(state, index, file).await {
            Ok(outcome) => state.packages.update(index, |entry| entry.state = outcome),
            Err(e) => {
                let error = format!("package '{file}' failed to apply at startup: {e}");
                tracing::error!("{error}");
                state.packages.fail(index, error);
                return;
            }
        }
    }
    tracing::info!(
        packages = files.len(),
        "every [packages] artifact is applied and serving"
    );
}

async fn apply_one(state: &AppState, index: usize, file: &str) -> Result<BootState, Error> {
    let mut artifact = super::read_artifact(file)?;
    super::verify_hash(&artifact)?;
    let package = format!("{}@{}", artifact.package.name, artifact.package.version);
    let report = Log {
        package: package.clone(),
    };
    // Before anything is written: what `package lint` refuses would be
    // refused by the target halfway through.
    let lint = super::lint_artifact(&artifact)?;
    for warning in &lint.warnings {
        report.err(&format!("warning: {warning}"));
    }
    if !lint.errors.is_empty() {
        return Err(format!(
            "{} lint error(s): {}",
            lint.errors.len(),
            lint.errors.join("; ")
        )
        .into());
    }
    state.packages.update(index, |entry| {
        entry.name = artifact.package.name.clone();
        entry.version = artifact.package.version.clone();
        entry.content_hash = artifact.package.content_hash.clone();
    });
    let signed = super::sign::attach_from_dir(
        &mut artifact,
        state.config.packages.signatures_dir.as_deref(),
        &report,
    )?;
    let api = InProcessAdmin::new(state.clone(), PRINCIPAL, format!("package={package} boot"));
    let _lease = if state.cluster.enabled {
        single_flight(state, &api, &artifact).await?
    } else {
        None
    };
    let opts = ApplyOptions {
        prune: None,
        signed: &signed,
        roll_back_superseded: false,
    };
    let outcome = super::apply(&api, "this node", &artifact, &opts, &report).await?;
    Ok(match outcome {
        ApplyOutcome::Applied | ApplyOutcome::RolledBack { .. } => BootState::Applied,
        ApplyOutcome::AlreadyApplied | ApplyOutcome::Resigned => BootState::AlreadyApplied,
        ApplyOutcome::Superseded { .. } => BootState::Superseded,
    })
}

/// In a cluster, one node applies a package at a time — the one holding the
/// `package-apply:<name>` lease — because a staged receipt is no lock: two
/// nodes booting on one artifact would both stage and activate. The others
/// wait until the receipt says this version is applied, then reload their
/// own generation from the database so the check that follows sees what the
/// peer wrote; or until the lease is theirs, when a holder died mid-apply.
///
/// `Some` holds the lease, renewed until it is dropped.
async fn single_flight(
    state: &AppState,
    api: &InProcessAdmin,
    artifact: &PackageArtifact,
) -> Result<Option<LeaseRenewal>, Error> {
    let gate = Arc::new(crate::cluster::JobLeaseGate::new(
        state.cluster.repo.clone(),
        state.cluster.instance_id.clone(),
    ));
    let job = format!("package-apply:{}", artifact.package.name);
    loop {
        if gate.try_acquire(&job, LEASE_TTL_SECS).await {
            let (gate, job) = (gate.clone(), job.clone());
            return Ok(Some(LeaseRenewal(tokio::spawn(async move {
                loop {
                    tokio::time::sleep(LEASE_RENEW).await;
                    gate.try_acquire(&job, LEASE_TTL_SECS).await;
                }
            }))));
        }
        if applied_on_target(api, artifact).await? {
            tracing::info!(
                package = %artifact.package.name,
                version = %artifact.package.version,
                "applied by a peer — reloading this node's generation"
            );
            crate::runtime::reload_engine(state)
                .await
                .map_err(|e| format!("reloading after a peer applied the package: {e}"))?;
            return Ok(None);
        }
        tokio::time::sleep(PEER_POLL).await;
    }
}

/// Whether the target's receipts record this version as applied.
async fn applied_on_target(api: &impl AdminApi, artifact: &PackageArtifact) -> Result<bool, Error> {
    let receipts: Option<Value> = api
        .get_data_opt(&orion_client::paths::package(&artifact.package.name))
        .await?;
    Ok(receipts.is_some_and(|receipts| {
        receipts["versions"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|row| {
                row["version"] == artifact.package.version.as_str() && row["state"] == "applied"
            })
    }))
}

/// The renewal task of a held lease, stopped when the apply is done.
struct LeaseRenewal(tokio::task::JoinHandle<()>);

impl Drop for LeaseRenewal {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A startup apply's lines, logged with the package they are about.
struct Log {
    package: String,
}

impl Reporter for Log {
    fn out(&self, line: &str) {
        tracing::info!(package = %self.package, "{line}");
    }

    fn err(&self, line: &str) {
        match line.strip_prefix("error: ") {
            Some(line) => tracing::error!(package = %self.package, "{line}"),
            None => tracing::warn!(
                package = %self.package,
                "{}",
                line.strip_prefix("warning: ").unwrap_or(line)
            ),
        }
    }
}
