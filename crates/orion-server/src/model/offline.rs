//! What an offline run — `dry-run`, `orion-server test` — resolves
//! `model_infer` against: the manifests a `--model-dir` holds, each with its
//! artifact on disk beside it.
//!
//! Two things differ from a node, and only two. The **set** has no rows
//! behind it: it is compiled from the manifests themselves, on the engine
//! that is running the workflow, the first time a task asks — a `datalogic`
//! program is bound to the engine that compiled it, and a dry run has
//! exactly one, which it does not have in hand until a task runs. And a
//! **cold load** reads the file the manifest's `artifact` names rather than
//! fetching through a storage connector. Everything else — the adapters, the
//! limits, the permits, the result expression, every refusal — is the
//! handler's, unchanged.
//!
//! No admission runs. A node verifies a digest claim and probes a graph
//! before it will serve it; offline there is no claim to verify (the digest
//! is computed from the file, never declared) and the probe is what the run
//! itself is. The bytes on disk are trusted as the author's own. A graph
//! that does not load, or does not produce what its manifest declares,
//! still fails the task the way it would on a node — `unavailable` or `run`
//! — because the runtime is the same one.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use dataflow_rs::datalogic_rs as datalogic;

use super::handler::ArtifactSource;
use super::loader::{ManifestEntry, ModelEntry, ModelSet};
use super::runtimes::LoadError;
use crate::config::ModelsConfig;

/// The manifests one offline run serves, and the set they compile into.
///
/// One per built engine: the set is compiled on whichever engine first asks
/// and kept for that engine's run, so sharing an instance between two
/// engines would evaluate one engine's programs on the other. The parts
/// worth sharing across runs — the runtimes, the resident sessions, the
/// digests and stats read off the files — live in the
/// [`InferenceHost`](super::handler::InferenceHost) and the catalog that
/// built this, not here.
pub struct OfflineModels {
    manifests: Vec<ManifestEntry>,
    config: Arc<ModelsConfig>,
    set: OnceLock<Arc<ModelSet>>,
}

impl OfflineModels {
    pub fn new(manifests: Vec<ManifestEntry>, config: Arc<ModelsConfig>) -> Self {
        Self {
            manifests,
            config,
            set: OnceLock::new(),
        }
    }

    /// The set, compiled on `datalogic` the first time and reused after.
    pub fn set_on(&self, datalogic: &datalogic::Engine) -> Arc<ModelSet> {
        self.set
            .get_or_init(|| {
                Arc::new(ModelSet::from_manifests(
                    self.manifests.iter().cloned(),
                    &self.config,
                    datalogic,
                ))
            })
            .clone()
    }

    /// The model ids this run can serve, in manifest order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.manifests.iter().map(|m| m.manifest.name.as_str())
    }

    /// Where each model's bytes are: the artifact source a cold load reads.
    pub fn artifacts(&self) -> LocalArtifacts {
        LocalArtifacts {
            paths: self
                .manifests
                .iter()
                .map(|m| (m.manifest.name.clone(), m.artifact_path.clone()))
                .collect(),
        }
    }
}

/// The bytes of each model, by id, from the file beside its manifest.
pub struct LocalArtifacts {
    paths: HashMap<String, PathBuf>,
}

#[async_trait]
impl ArtifactSource for LocalArtifacts {
    async fn bytes(&self, entry: &ModelEntry) -> Result<Vec<u8>, LoadError> {
        let Some(path) = self.paths.get(&entry.id) else {
            return Err(LoadError::new(
                "artifact",
                format!("no artifact on disk for model '{}'", entry.id),
            ));
        };
        tokio::fs::read(path)
            .await
            .map_err(|e| LoadError::new("artifact", format!("reading '{}': {e}", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::fixture;

    fn entry(path: PathBuf) -> ManifestEntry {
        ManifestEntry {
            manifest: fixture::manifest(),
            artifact_path: path,
            digest: crate::crypto::sha256_digest(fixture::ONNX),
            stats: None,
        }
    }

    /// The set compiles once, on the engine that asks first, and the
    /// artifact source reads the file the manifest sits beside.
    #[tokio::test]
    async fn the_set_compiles_once_and_the_bytes_come_from_the_file() {
        let dir = std::env::temp_dir().join(format!("orion-offline-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("c4-tiny.onnx");
        std::fs::write(&path, fixture::ONNX).expect("write");

        let offline = OfflineModels::new(
            vec![entry(path.clone())],
            Arc::new(ModelsConfig {
                enabled: true,
                ..ModelsConfig::default()
            }),
        );
        assert_eq!(offline.ids().collect::<Vec<_>>(), ["ada.c4-tiny"]);
        let engine = crate::engine::operators::add_to_datalogic(
            datalogic::Engine::builder()
                .with_templating(true)
                .with_template_key_escape('$'),
        )
        .build();
        let first = offline.set_on(&engine);
        let second = offline.set_on(&engine);
        assert!(Arc::ptr_eq(&first, &second), "compiled once");
        let compiled = first.get("ada.c4-tiny").expect("compiled");

        let bytes = offline
            .artifacts()
            .bytes(compiled)
            .await
            .expect("the file is read");
        assert_eq!(bytes, fixture::ONNX);

        // A model the source has no file for is an `artifact` stage failure,
        // and so is a file that has gone.
        std::fs::remove_file(&path).expect("remove");
        let err = offline
            .artifacts()
            .bytes(compiled)
            .await
            .expect_err("the file is gone");
        assert_eq!(err.stage, "artifact");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
