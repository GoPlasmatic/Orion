//! Governed ONNX models: the manifest that describes one, the artifact that
//! holds its bytes, the admission that proves a node can run it, and the
//! runtime abstraction an inference goes through.
//!
//! Sits beside `plugin`, and names neither `server` nor `bootstrap` — nor
//! `plugin`: the two subsystems share their digest and signature primitives
//! through [`crate::crypto`] and nothing else. It holds the node's handle
//! the way `engine::functions::channel_call` does — the `model_infer`
//! handler loads the serving generation once per call — and the generation
//! holds the model set back, which is the same pair of edges `engine` and
//! `runtime` already share.
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
//! - [`runtimes`]: the `ModelRuntime` / `LoadedModel` traits, the
//!   `LoadBinding` a load is given — everything a session is a function of
//!   besides the bytes and the device — the names and devices this build
//!   knows, the name-keyed registry, and `tract`, the one implementation.
//! - [`node`]: what one node holds for all of the above — the store, the
//!   admission queue, the loaded-session cache, the inference slots and the
//!   name its verdicts carry — built at boot when `models.enabled` and
//!   absent otherwise.
//! - [`loader`]: the model half of a generation — every active row that can
//!   serve here, its adapters and result compiled on that generation's
//!   engine, and the reasons any row did not load.
//! - [`cache`]: the sessions resident in a runtime, process-wide,
//!   single-flight per key and bounded by `models.max_loaded_bytes`.
//! - [`handler`]: the `model_infer` task function, and the load path it
//!   shares with the preload — over a [`handler::ModelSource`] that is the
//!   serving generation on a node and a fixed manifest set offline, and an
//!   [`handler::InferenceHost`] that says where a cold load gets its bytes.
//! - [`offline`]: the manifest set `dry-run` and `orion-server test` run
//!   against — compiled on the calling engine, bytes read from the file
//!   beside each manifest, no admission.
//! - [`limits`] and [`error`]: the effective ceilings for one model and the
//!   categories an inference can fail in.

pub mod admission;
pub mod artifact;
pub mod cache;
pub mod error;
pub mod handler;
pub mod limits;
pub mod loader;
pub mod manifest;
pub mod node;
pub mod offline;
pub mod onnx;
pub mod runtimes;

pub use admission::{
    AdmissionDeps, AdmissionJob, AdmissionOutcome, AdmissionQueue, AdmissionState, Stats,
    admission_json, admit, check_boundary,
};
pub use artifact::{ArtifactRef, ArtifactStore, FetchError, HeadInfo};
pub use cache::{CacheKey, LoadedCache};
pub use error::{Category, Failure};
pub use handler::{ArtifactSource, InferenceHost, ModelInferHandler, ModelSource, load_model};
pub use limits::Limits;
pub use loader::{ManifestEntry, ModelEntry, ModelLoadIssue, ModelSet, literal_references};
pub use manifest::{ABI, ArtifactReference, InputDecl, Manifest, OutputDecl, is_model_manifest};
pub use node::{ModelsRuntime, node_name};
pub use offline::{LocalArtifacts, OfflineModels};
pub use onnx::{GraphStats, read_stats};
pub use runtimes::{
    BoundInput, LoadBinding, LoadError, LoadedModel, ModelRuntime, ModelRuntimes, RunError,
    TractRuntime,
};

/// The `c4-tiny` fixture every model test loads — the graph, its manifest
/// and what `build.py` says about them — so the reader, the runtime and
/// the admission tests agree on one spelling.
#[cfg(test)]
pub(crate) mod fixture {
    pub const ONNX: &[u8] = include_bytes!("../../tests/fixtures/models/c4-tiny/c4-tiny.onnx");
    pub const MANIFEST: &str = include_str!("../../tests/fixtures/models/c4-tiny/model.json");

    /// The `two-out` fixture: one graph with two outputs of the same dtype
    /// and shape, and the two manifests that declare them in either order —
    /// the pair that shares an artifact digest and must not share a session.
    pub const TWO_OUT_ONNX: &[u8] =
        include_bytes!("../../tests/fixtures/models/two-out/two-out.onnx");
    pub const TWO_OUT_A: &str = include_str!("../../tests/fixtures/models/two-out/order-a.json");
    pub const TWO_OUT_B: &str = include_str!("../../tests/fixtures/models/two-out/order-b.json");

    /// The `weights` fixture: one network — a single `Gemm` over a [1, 4]
    /// input — written three ways, its 15 weights carried as initializers,
    /// as `Constant` node attributes, and as the `value_floats` /
    /// `value_ints` lists. The three that compute the same function and
    /// must all be counted.
    pub const AS_INIT_ONNX: &[u8] =
        include_bytes!("../../tests/fixtures/models/weights/as-init.onnx");
    pub const AS_CONST_ONNX: &[u8] =
        include_bytes!("../../tests/fixtures/models/weights/as-const.onnx");
    pub const AS_LIST_ONNX: &[u8] =
        include_bytes!("../../tests/fixtures/models/weights/as-list.onnx");

    /// The `dynamic` fixture: `y = x * 2` over an `[N, 3]` input, the axis
    /// symbolic in the ONNX file itself, so the graph genuinely takes any
    /// N and the manifest is not merely claiming one.
    pub const DYNAMIC_ONNX: &[u8] =
        include_bytes!("../../tests/fixtures/models/dynamic/scale.onnx");
    pub const DYNAMIC_MANIFEST: &str =
        include_str!("../../tests/fixtures/models/dynamic/model.json");

    pub fn dynamic() -> super::Manifest {
        super::Manifest::parse(DYNAMIC_MANIFEST).expect("the dynamic manifest is valid")
    }

    pub fn manifest() -> super::Manifest {
        super::Manifest::parse(MANIFEST).expect("the fixture manifest is valid")
    }

    /// The two `two-out` manifests, `order-a` first.
    pub fn two_out() -> (super::Manifest, super::Manifest) {
        (
            super::Manifest::parse(TWO_OUT_A).expect("the order-a manifest is valid"),
            super::Manifest::parse(TWO_OUT_B).expect("the order-b manifest is valid"),
        )
    }
}
