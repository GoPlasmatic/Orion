//! What this node's `[packages] apply` has done so far.
//!
//! Written by the startup apply (`package::boot`), read by `/readyz` and
//! `/health`. A holder of state and nothing else, so the probes can read it
//! without naming the layer that applies packages.
//!
//! Readiness is a *startup* condition: once every package is serving the
//! component stays `ok`. A package quarantined by a later reload degrades
//! `/health` the way any other load issue does, and does not take the node
//! out of rotation.

use std::sync::Mutex;

use serde::Serialize;

/// One configured package's progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BootState {
    /// Not reached yet.
    Pending,
    /// Being read, applied or verified.
    Applying,
    /// Applied by this node, and serving.
    Applied,
    /// Already the package's current version — by an earlier boot, or by a
    /// peer — and serving.
    AlreadyApplied,
    /// A later version of the package is current; left as it is, and what
    /// this version still holds is serving.
    Superseded,
    /// Refused, or not serving. The process exits.
    Failed,
}

impl BootState {
    /// Whether the package counts as serving for readiness.
    pub fn is_serving(self) -> bool {
        matches!(
            self,
            Self::Applied | Self::AlreadyApplied | Self::Superseded
        )
    }
}

/// One configured package, as `/health` lists it.
#[derive(Debug, Clone, Serialize)]
pub struct BootPackageStatus {
    /// The `[packages] apply` entry.
    pub file: String,
    /// From the artifact, once read; empty before.
    pub name: String,
    pub version: String,
    pub content_hash: String,
    pub state: BootState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Every configured package's progress, and the node's verdict.
pub struct BootPackages {
    entries: Mutex<Vec<BootPackageStatus>>,
    failure: Mutex<Option<String>>,
    failed: tokio::sync::watch::Sender<bool>,
}

impl BootPackages {
    /// One `Pending` entry per `[packages] apply` path.
    pub fn new(files: &[String]) -> Self {
        let entries = files
            .iter()
            .map(|file| BootPackageStatus {
                file: file.clone(),
                name: String::new(),
                version: String::new(),
                content_hash: String::new(),
                state: BootState::Pending,
                error: None,
            })
            .collect();
        Self {
            entries: Mutex::new(entries),
            failure: Mutex::new(None),
            failed: tokio::sync::watch::channel(false).0,
        }
    }

    /// None configured — no probe reports the component.
    pub fn is_empty(&self) -> bool {
        self.lock_entries().is_empty()
    }

    /// The readiness component: `None` when nothing is configured, else
    /// `failed`, `ok` once every package is serving, or `applying`.
    pub fn component(&self) -> Option<&'static str> {
        let entries = self.lock_entries();
        if entries.is_empty() {
            return None;
        }
        Some(if self.failure().is_some() {
            "failed"
        } else if entries.iter().all(|e| e.state.is_serving()) {
            "ok"
        } else {
            "applying"
        })
    }

    pub fn snapshot(&self) -> Vec<BootPackageStatus> {
        self.lock_entries().clone()
    }

    /// Edit entry `index`.
    pub fn update(&self, index: usize, edit: impl FnOnce(&mut BootPackageStatus)) {
        if let Some(entry) = self.lock_entries().get_mut(index) {
            edit(entry);
        }
    }

    /// Record that entry `index` failed with `error`, and wake whoever
    /// waits on [`Self::failed`]. The first failure is the node's.
    pub fn fail(&self, index: usize, error: String) {
        self.update(index, |entry| {
            entry.state = BootState::Failed;
            entry.error = Some(error.clone());
        });
        let mut failure = self.failure.lock().unwrap_or_else(|e| e.into_inner());
        if failure.is_none() {
            *failure = Some(error);
        }
        drop(failure);
        self.failed.send_replace(true);
    }

    /// Why the startup apply failed, once it has.
    pub fn failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Resolves once a package has failed; never, otherwise.
    pub async fn failed(&self) {
        let mut failed = self.failed.subscribe();
        // `wait_for` checks the current value first, so a failure recorded
        // before this call resolves at once. The sender lives as long as
        // `self`, so the error arm cannot happen while it is borrowed.
        let _ = failed.wait_for(|failed| *failed).await;
    }

    fn lock_entries(&self) -> std::sync::MutexGuard<'_, Vec<BootPackageStatus>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_configured_reports_no_component() {
        let status = BootPackages::new(&[]);
        assert!(status.is_empty());
        assert_eq!(status.component(), None);
    }

    #[tokio::test]
    async fn the_component_follows_the_entries_and_a_failure_wakes_the_waiter() {
        let status = BootPackages::new(&["a.json".to_string(), "b.json".to_string()]);
        assert_eq!(status.component(), Some("applying"));
        status.update(0, |e| e.state = BootState::Applied);
        assert_eq!(status.component(), Some("applying"));
        status.update(1, |e| e.state = BootState::Superseded);
        assert_eq!(status.component(), Some("ok"));

        let waiter = tokio::time::timeout(std::time::Duration::from_millis(50), status.failed());
        assert!(waiter.await.is_err(), "no failure, no wake-up");
        status.fail(1, "b.json: refused".to_string());
        status.fail(0, "a.json: later".to_string());
        tokio::time::timeout(std::time::Duration::from_secs(1), status.failed())
            .await
            .expect("a recorded failure resolves at once");
        assert_eq!(status.component(), Some("failed"));
        assert_eq!(status.failure().as_deref(), Some("b.json: refused"));
        assert_eq!(status.snapshot()[1].state, BootState::Failed);
    }
}
