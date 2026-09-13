//! What one node holds for models: the artifact cache, the admission queue,
//! the runtimes it offers and the name it records its verdicts under.
//!
//! Built once at boot when `models.enabled`, and absent — `None` on the
//! application state — otherwise, which is how every route and gate learns
//! that this node admits and runs nothing. The receiving half of the queue
//! stays here until the admission worker starts and takes it: a node that
//! never starts the worker (the integration harness starts no background
//! tasks) keeps the channel open, so a registration still queues its job
//! rather than failing against a closed channel.

use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use super::admission::{AdmissionJob, AdmissionQueue, QUEUE_CAPACITY};
use super::artifact::ArtifactStore;
use super::runtimes::ModelRuntimes;
use crate::config::ModelsConfig;

pub struct ModelsRuntime {
    /// The digest-keyed disk cache every admission fills.
    pub store: Arc<ArtifactStore>,
    /// The sending half of the admission queue — what a route hands a job
    /// to.
    pub admissions: AdmissionQueue,
    /// The receiving half, until the worker takes it.
    receiver: Mutex<Option<mpsc::Receiver<AdmissionJob>>>,
    /// The name this node records on every verdict: `cluster.instance_id`
    /// when the operator set one, else the host's name.
    pub node: String,
    /// The runtimes `[models.runtimes]` enables here — what an admission
    /// probes on and a generation loads into, each on the device its entry
    /// names; which one a model gets is `[models.default_runtime]`'s row
    /// for the manifest's format.
    pub runtimes: Arc<ModelRuntimes>,
}

impl ModelsRuntime {
    /// Create the cache directory if it is missing, open the queue and
    /// build the runtimes the config enables.
    pub fn new(config: &ModelsConfig, node: String) -> Result<Self, String> {
        let cache_dir = Path::new(&config.cache_dir);
        std::fs::create_dir_all(cache_dir).map_err(|e| {
            format!(
                "models.cache_dir '{}' could not be created: {e}",
                config.cache_dir
            )
        })?;
        let (admissions, receiver) = AdmissionQueue::new();
        Ok(Self {
            store: Arc::new(ArtifactStore::new(cache_dir, config.max_cache_bytes)),
            admissions,
            receiver: Mutex::new(Some(receiver)),
            node,
            runtimes: Arc::new(ModelRuntimes::builtin(config)),
        })
    }

    /// The receiving half of the queue, once. `None` after the worker has
    /// taken it.
    pub fn take_receiver(&self) -> Option<mpsc::Receiver<AdmissionJob>> {
        self.receiver
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// How many jobs the queue holds before a registration is told to
    /// retry.
    pub fn queue_capacity(&self) -> usize {
        QUEUE_CAPACITY
    }
}

/// The name a node records on its verdicts: the configured cluster instance
/// id when there is one, otherwise the host's name, and `unknown` on a host
/// that will not say.
pub fn node_name(instance_id: &str) -> String {
    if !instance_id.trim().is_empty() {
        return instance_id.trim().to_string();
    }
    gethostname::gethostname()
        .into_string()
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configured_instance_id_names_the_node() {
        assert_eq!(node_name(" node-7 "), "node-7");
        assert!(!node_name("").is_empty());
    }

    #[test]
    fn the_receiver_is_taken_once() {
        let dir = std::env::temp_dir().join(format!("orion-models-node-{}", uuid::Uuid::new_v4()));
        let config = ModelsConfig {
            enabled: true,
            cache_dir: dir.to_string_lossy().into_owned(),
            ..ModelsConfig::default()
        };
        let runtime = ModelsRuntime::new(&config, "n".to_string()).expect("creates the dir");
        assert!(dir.is_dir());
        assert!(runtime.take_receiver().is_some());
        assert!(runtime.take_receiver().is_none());
        assert_eq!(runtime.queue_capacity(), QUEUE_CAPACITY);
        // The default config enables tract on cpu for onnx, and the node's
        // registry answers for it.
        assert_eq!(runtime.runtimes.names(), ["tract"]);
        let (tract, device) = runtime
            .runtimes
            .default_for(&config, "onnx")
            .expect("onnx has a default");
        assert_eq!(tract.name(), "tract");
        assert_eq!(device, "cpu");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
