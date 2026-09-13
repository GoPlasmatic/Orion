//! Governed ONNX models: the manifest that describes one, the artifact that
//! holds its bytes, the admission that proves a node can run it, and the
//! runtime abstraction an inference goes through.
//!
//! Sits beside `plugin` and below `runtime`, and names neither `server` nor
//! `bootstrap` — nor `plugin`: the two subsystems share their digest and
//! signature primitives through [`crate::crypto`] and nothing else. What a
//! generation carries (the loaded models, the compiled adapters) and the
//! `model_infer` task function that runs them are built on top of this
//! module; this module is the part that does not need a database or an
//! engine to exist.
//!
//! - [`manifest`]: the `orion:model@1.0.0` document — inputs, outputs, their
//!   dtypes and shapes, and the JSONLogic adapters that marshal a message
//!   into tensors and a result back out.
//! - [`artifact`]: the storage-side reference to a model's bytes, the signed
//!   fetch through a storage connector, and the digest-keyed disk cache.
//! - [`admission`]: the sequence a node runs before a model version may
//!   serve — trust, size, fetch, verify, parse, probe — as a pure function
//!   over its dependencies, plus the queue the admin API hands jobs to.
//! - [`onnx`]: the runtime-independent reader of a model's structure — the
//!   parameter and node counts, IR version, opset and boundary names an
//!   admission records, read from the protobuf so they never move with a
//!   runtime.
//! - [`runtimes`]: the `ModelRuntime` / `LoadedModel` traits, the names and
//!   devices this build knows, the name-keyed registry, and `tract`, the
//!   one implementation.
//! - [`node`]: what one node holds for all of the above — the store, the
//!   admission queue and the name its verdicts carry — built at boot when
//!   `models.enabled` and absent otherwise.
//! - [`limits`] and [`error`]: the effective ceilings for one model and the
//!   categories an inference can fail in.

pub mod admission;
pub mod artifact;
pub mod error;
pub mod limits;
pub mod manifest;
pub mod node;
pub mod onnx;
pub mod runtimes;

pub use admission::{
    AdmissionDeps, AdmissionJob, AdmissionOutcome, AdmissionQueue, AdmissionState, Stats,
    admission_json, admit,
};
pub use artifact::{ArtifactRef, ArtifactStore, FetchError, HeadInfo};
pub use error::{Category, Failure};
pub use limits::Limits;
pub use manifest::{ABI, InputDecl, Manifest, OutputDecl, is_model_manifest};
pub use node::{ModelsRuntime, node_name};
pub use onnx::{GraphStats, read_stats};
pub use runtimes::{LoadError, LoadedModel, ModelRuntime, ModelRuntimes, RunError, TractRuntime};

/// The `c4-tiny` fixture every model test loads — the graph, its manifest
/// and what `build.py` says about them — so the reader, the runtime and
/// the admission tests agree on one spelling.
#[cfg(test)]
pub(crate) mod fixture {
    pub const ONNX: &[u8] = include_bytes!("../../tests/fixtures/models/c4-tiny/c4-tiny.onnx");
    pub const MANIFEST: &str = include_str!("../../tests/fixtures/models/c4-tiny/model.json");

    pub fn manifest() -> super::Manifest {
        super::Manifest::parse(MANIFEST).expect("the fixture manifest is valid")
    }
}
