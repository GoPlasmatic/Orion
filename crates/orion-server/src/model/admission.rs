//! Admission: what a node proves about a model version before it may serve
//! it, as a pure function over its dependencies.
//!
//! A model row is *stored* by the admin API and *admitted* by a node — the
//! two are separate because the second takes seconds and touches the
//! network. The sequence is: the signature over the claimed digest verifies
//! against `[models.trust]` (nothing to check when no key is configured);
//! the object exists and is within `max_artifact_bytes`; the bytes arrive,
//! hash to the claim and land in the cache; the graph reads as ONNX, names
//! every tensor the manifest declares, and is within `max_parameters`; and
//! the node's default runtime for the manifest's format
//! (`[models.default_runtime]`) loads it on its device and runs it — five
//! inferences over zero-filled inputs, the median within `max_probe_ms`,
//! the outputs of the dtype and shape the manifest declares. What the last
//! two stages learn is the row's [`Stats`].
//!
//! [`admit`] takes everything it needs as a parameter and returns what it
//! found; recording the outcome on the row is the worker's job, and the
//! worker is wired where the repositories are. [`AdmissionQueue`] is the
//! bounded channel the admin API hands jobs to and [`run_worker`] the loop
//! that drains it into whatever the caller does with a job.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use dataflow_rs::datavalue::OwnedDataTensor;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::artifact::{ArtifactRef, ArtifactStore};
use super::manifest::Manifest;
use super::onnx;
use super::runtimes::{LoadedModel, ModelRuntimes};
use crate::config::ModelsConfig;
use crate::connector::StorageConnectorConfig;

/// One model version to admit.
#[derive(Debug, Clone, PartialEq)]
pub struct AdmissionJob {
    pub model_id: String,
    pub version: i64,
    pub artifact: ArtifactRef,
    /// The Ed25519 signature over `artifact.digest`, when the row carries
    /// one.
    pub signature: Option<String>,
    /// The row's manifest: what the graph is checked against and what the
    /// probe feeds it.
    pub manifest: Manifest,
}

/// What [`admit`] needs. Borrowed, so a worker that admits many jobs shares
/// one store, one client and one config across them.
pub struct AdmissionDeps<'a> {
    pub store: &'a ArtifactStore,
    /// The storage connector `job.artifact.connector` resolved to.
    pub storage: &'a StorageConnectorConfig,
    pub client: &'a reqwest::Client,
    pub config: &'a ModelsConfig,
    /// This node's name, recorded with the outcome.
    pub node: &'a str,
    /// The runtimes this node offers. The probe asks them for the default
    /// for the manifest's format ([`ModelRuntimes::default_for`]), which
    /// answers with the runtime and the device the config puts it on.
    pub runtimes: &'a ModelRuntimes,
}

/// How many inferences the probe runs; `probe_ms` is their median, so one
/// cold first run does not decide a verdict.
pub const PROBE_RUNS: usize = 5;

/// What admission learned about the artifact. The wire shape of a model
/// row's `stats`.
///
/// `artifact_bytes` is the fetch stage's; `parameters`, `nodes`,
/// `ir_version` and `opset` are read from the protobuf by the parse stage
/// (`onnx::read_stats`), so they are the same on every node; `probe_ms`,
/// `runtime` and `device` are the probe stage's, on the node that admitted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub parameters: u64,
    pub nodes: u64,
    pub artifact_bytes: u64,
    /// The median wall time of [`PROBE_RUNS`] inferences, in milliseconds.
    pub probe_ms: f64,
    pub ir_version: i64,
    pub opset: i64,
    pub runtime: String,
    pub device: String,
}

impl Stats {
    /// What an offline reader learns without a probe: the graph's numbers
    /// and the file's size, `probe_ms` zero and no runtime or device — the
    /// stats `lint` prints and a `stats_output` reports in a dry run. A
    /// node's admission fills the rest.
    pub fn offline(graph: &onnx::GraphStats, artifact_bytes: u64) -> Self {
        Self {
            parameters: graph.parameters,
            nodes: graph.nodes,
            artifact_bytes,
            probe_ms: 0.0,
            ir_version: graph.ir_version,
            opset: graph.opset,
            runtime: String::new(),
            device: String::new(),
        }
    }
}

/// How an admission ended.
#[derive(Debug, Clone, PartialEq)]
pub enum AdmissionState {
    Passed {
        stats: Stats,
    },
    /// `signature`, `gate`, `head`, `size`, `fetch`, `digest`, `cache`,
    /// `parse`, `probe` — or the stage the sequence was in when
    /// `admission_timeout_secs` elapsed.
    Failed {
        stage: &'static str,
        reason: String,
    },
}

/// The result of one [`admit`].
#[derive(Debug, Clone, PartialEq)]
pub struct AdmissionOutcome {
    pub model_id: String,
    pub version: i64,
    pub state: AdmissionState,
    /// Where the verified bytes are, when the sequence got that far.
    pub artifact_path: Option<PathBuf>,
    pub elapsed: Duration,
}

impl AdmissionOutcome {
    pub fn passed(&self) -> bool {
        matches!(self.state, AdmissionState::Passed { .. })
    }
}

/// The wire shape of a model row's `admission`: what happened, when, where,
/// and — for a failure — at which stage and why.
///
/// `at` is naive UTC, like every other timestamp the API publishes — the
/// row DTO decodes it as one, so an offset here would fail every read of
/// the row.
pub fn admission_json(outcome: &AdmissionOutcome, node: &str, now: DateTime<Utc>) -> Value {
    let (state, stage, reason) = match &outcome.state {
        AdmissionState::Passed { .. } => ("passed", None, None),
        AdmissionState::Failed { stage, reason } => ("failed", Some(*stage), Some(reason.as_str())),
    };
    json!({
        "state": state,
        "at": now.naive_utc(),
        "node": node,
        "stage": stage,
        "reason": reason,
    })
}

/// Run the admission sequence for `job`, bounded by
/// `config.admission_timeout_secs`. Never fails as a `Result`: an admission
/// that could not complete is a [`AdmissionState::Failed`] naming the stage.
pub async fn admit(deps: &AdmissionDeps<'_>, job: &AdmissionJob) -> AdmissionOutcome {
    let started = Instant::now();
    let stage = Mutex::new("signature");
    let budget = Duration::from_secs(deps.config.admission_timeout_secs);
    let state = match tokio::time::timeout(budget, sequence(deps, job, &stage)).await {
        Ok(Ok((stats, path))) => (AdmissionState::Passed { stats }, Some(path)),
        Ok(Err((stage, reason))) => (AdmissionState::Failed { stage, reason }, None),
        Err(_elapsed) => {
            let stage = *stage.lock().unwrap_or_else(|e| e.into_inner());
            (
                AdmissionState::Failed {
                    stage,
                    reason: format!(
                        "admission exceeded models.admission_timeout_secs ({}) during {stage}",
                        deps.config.admission_timeout_secs
                    ),
                },
                None,
            )
        }
    };
    let elapsed = started.elapsed();
    match &state.0 {
        AdmissionState::Passed { .. } => {
            crate::metrics::record_model_admission("passed", None, elapsed.as_secs_f64());
        }
        AdmissionState::Failed { stage, .. } => {
            crate::metrics::record_model_admission("failed", Some(stage), elapsed.as_secs_f64());
        }
    }
    AdmissionOutcome {
        model_id: job.model_id.clone(),
        version: job.version,
        state: state.0,
        artifact_path: state.1,
        elapsed,
    }
}

/// The steps, each recording where it is in `stage` so a timeout can say.
async fn sequence(
    deps: &AdmissionDeps<'_>,
    job: &AdmissionJob,
    stage: &Mutex<&'static str>,
) -> Result<(Stats, PathBuf), (&'static str, String)> {
    let at = |s: &'static str| *stage.lock().unwrap_or_else(|e| e.into_inner()) = s;

    // The signature is over the claimed digest, so it is checked before the
    // bytes are fetched — a signature that fails needs no fetch to fail.
    at("signature");
    crate::crypto::ed25519::verify(
        &deps.config.trust.public_keys,
        &job.artifact.digest,
        job.signature.as_deref(),
    )
    .map_err(|reason| ("signature", format!("{reason} (models.trust.public_keys)")))?;

    at("head");
    let info = deps
        .store
        .head(deps.storage, deps.client, &job.artifact.key)
        .await
        .map_err(|e| (e.stage(), with_connector(&job.artifact, e.to_string())))?;
    at("size");
    if let Some(size) = info.size
        && size > deps.config.max_artifact_bytes as u64
    {
        return Err((
            "size",
            format!(
                "the object is {size} bytes, over models.max_artifact_bytes ({})",
                deps.config.max_artifact_bytes
            ),
        ));
    }

    at("fetch");
    let fetch_started = Instant::now();
    let fetched = deps
        .store
        .fetch(
            deps.storage,
            deps.client,
            &job.artifact,
            deps.config.max_artifact_bytes,
            Duration::from_secs(deps.config.fetch_timeout_secs),
        )
        .await;
    let fetch_secs = fetch_started.elapsed().as_secs_f64();
    let path = match fetched {
        Ok(path) => path,
        Err(e) => {
            crate::metrics::record_model_fetch(&job.model_id, "error", 0, fetch_secs);
            return Err((e.stage(), with_connector(&job.artifact, e.to_string())));
        }
    };
    let artifact_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    crate::metrics::record_model_fetch(&job.model_id, "ok", artifact_bytes, fetch_secs);

    // The parse reads the protobuf, never a runtime, so what it records is
    // the same on every node. Decoding a large artifact is CPU work, and
    // `Bytes` lets the blocking pool borrow the file without a copy.
    at("parse");
    let bytes = Bytes::from(tokio::fs::read(&path).await.map_err(|e| {
        (
            "parse",
            format!("the cached artifact could not be read: {e}"),
        )
    })?);
    let graph = {
        let bytes = bytes.clone();
        tokio::task::spawn_blocking(move || onnx::read_stats(&bytes))
            .await
            .map_err(|e| ("parse", format!("the parse did not complete: {e}")))?
            .map_err(|reason| ("parse", reason))?
    };
    check_boundary(&job.manifest, &graph).map_err(|reason| ("parse", reason))?;
    if deps.config.max_parameters != 0 && graph.parameters > deps.config.max_parameters {
        return Err((
            "parse",
            format!(
                "the graph has {} parameters, over models.max_parameters ({})",
                graph.parameters, deps.config.max_parameters
            ),
        ));
    }

    // The probe proves the default runtime for the manifest's format runs
    // this graph *here*: the load and every inference go through the same
    // `ModelRuntime` a generation will use, on the blocking pool because
    // both are synchronous. A selection that fails is a verdict about this
    // node's config, and the reason says which row.
    at("probe");
    let (runtime, device) = deps
        .runtimes
        .default_for(deps.config, &job.manifest.format)
        .map_err(|selection| ("probe", selection.to_string()))?;
    let inputs = job
        .manifest
        .zero_inputs()
        .map_err(|reason| ("probe", reason))?;
    let load_started = Instant::now();
    let loaded = {
        let runtime = runtime.clone();
        let bytes = bytes.clone();
        let manifest = job.manifest.clone();
        let device = device.to_string();
        tokio::task::spawn_blocking(move || runtime.load(&bytes, &manifest, &device)).await
    };
    let load_secs = load_started.elapsed().as_secs_f64();
    let outcome = match &loaded {
        Ok(Ok(_)) => "ok",
        _ => "error",
    };
    crate::metrics::record_model_load(
        &job.model_id,
        runtime.name(),
        outcome,
        "admission",
        load_secs,
    );
    let model = match loaded {
        Ok(Ok(model)) => model,
        Ok(Err(e)) => {
            return Err((
                "probe",
                format!(
                    "the {} runtime could not load the graph on '{device}': {e}",
                    runtime.name(),
                ),
            ));
        }
        Err(e) => return Err(("probe", format!("the load did not complete: {e}"))),
    };
    let (probe_ms, outputs) = tokio::task::spawn_blocking(move || probe_runs(&*model, inputs))
        .await
        .map_err(|e| ("probe", format!("the probe did not complete: {e}")))?
        .map_err(|reason| ("probe", reason))?;
    if probe_ms > deps.config.max_probe_ms as f64 {
        return Err((
            "probe",
            format!(
                "the probe inference took {probe_ms:.3} ms (median of {PROBE_RUNS}), over \
                 models.max_probe_ms ({})",
                deps.config.max_probe_ms
            ),
        ));
    }
    check_outputs(&job.manifest, &outputs).map_err(|reason| ("probe", reason))?;

    Ok((
        Stats {
            parameters: graph.parameters,
            nodes: graph.nodes,
            artifact_bytes,
            probe_ms,
            ir_version: graph.ir_version,
            opset: graph.opset,
            runtime: runtime.name().to_string(),
            device: device.to_string(),
        },
        path,
    ))
}

/// Every `declared` name must be one of the graph's; the reason lists the
/// graph's so a typo is a one-line fix.
/// Every input and output the manifest declares must be a tensor the graph
/// has, by name. The parse stage's rule, and `lint`'s over a manifest with
/// its artifact beside it — one function, so the offline report and the
/// admission verdict cannot disagree about a boundary.
pub fn check_boundary(manifest: &Manifest, graph: &onnx::GraphStats) -> Result<(), String> {
    check_names("input", manifest.input_names(), &graph.input_names)?;
    check_names("output", manifest.output_names(), &graph.output_names)
}

fn check_names<'a>(
    kind: &str,
    declared: impl Iterator<Item = &'a str>,
    graph: &[String],
) -> Result<(), String> {
    for name in declared {
        if !graph.iter().any(|g| g == name) {
            return Err(format!(
                "the manifest declares {kind} '{name}', which the graph does not have; the \
                 graph's {kind}s are: {}",
                graph
                    .iter()
                    .map(|n| format!("'{n}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    Ok(())
}

/// [`PROBE_RUNS`] inferences over `inputs`, timed one by one: the median in
/// milliseconds, and the last run's outputs.
fn probe_runs(
    model: &dyn LoadedModel,
    inputs: Vec<OwnedDataTensor>,
) -> Result<(f64, Vec<OwnedDataTensor>), String> {
    let mut times = Vec::with_capacity(PROBE_RUNS);
    let mut outputs = Vec::new();
    for _ in 0..PROBE_RUNS {
        let started = Instant::now();
        outputs = model
            .run(inputs.clone())
            .map_err(|e| format!("the probe inference failed: {e}"))?;
        times.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(f64::total_cmp);
    Ok((times[PROBE_RUNS / 2], outputs))
}

/// What came back must be what the manifest promised, output by output: the
/// result expression and every caller read the manifest, not the graph.
fn check_outputs(manifest: &Manifest, outputs: &[OwnedDataTensor]) -> Result<(), String> {
    if outputs.len() != manifest.outputs.len() {
        return Err(format!(
            "the probe produced {} outputs, the manifest declares {}",
            outputs.len(),
            manifest.outputs.len()
        ));
    }
    for (decl, tensor) in manifest.outputs.iter().zip(outputs) {
        if tensor.dtype().name() != decl.dtype || tensor.shape() != decl.shape.as_slice() {
            return Err(format!(
                "output '{}' is {}{:?} from the graph, but the manifest declares {}{:?}",
                decl.name,
                tensor.dtype().name(),
                tensor.shape(),
                decl.dtype,
                decl.shape
            ));
        }
    }
    Ok(())
}

fn with_connector(artifact: &ArtifactRef, message: String) -> String {
    format!(
        "connector '{}', key '{}': {message}",
        artifact.connector, artifact.key
    )
}

/// Jobs waiting for a worker. Bounded: an admin API that outruns the worker
/// is told so rather than growing a queue without limit.
pub const QUEUE_CAPACITY: usize = 1024;

/// The sending half of the admission queue, held by whoever creates model
/// rows.
#[derive(Clone)]
pub struct AdmissionQueue {
    tx: mpsc::Sender<AdmissionJob>,
}

/// The queue was full; the job is handed back so the caller can report it.
/// Boxed: the job carries an artifact reference and a manifest, and an
/// error type as large as its `Ok` counterpart is small makes every
/// `Result` pay for it.
#[derive(Debug)]
pub struct QueueFull(pub Box<AdmissionJob>);

impl std::fmt::Display for QueueFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the admission queue is full ({QUEUE_CAPACITY} jobs waiting); retry once the worker \
             has caught up"
        )
    }
}

impl AdmissionQueue {
    /// A queue of [`QUEUE_CAPACITY`], and the receiver a worker drains.
    pub fn new() -> (Self, mpsc::Receiver<AdmissionJob>) {
        Self::with_capacity(QUEUE_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> (Self, mpsc::Receiver<AdmissionJob>) {
        let (tx, rx) = mpsc::channel(capacity);
        (Self { tx }, rx)
    }

    /// Hand a job to the worker without waiting. A full queue — or a worker
    /// that is gone — returns the job.
    pub fn enqueue(&self, job: AdmissionJob) -> Result<(), QueueFull> {
        self.tx.try_send(job).map_err(|e| match e {
            mpsc::error::TrySendError::Full(job) | mpsc::error::TrySendError::Closed(job) => {
                QueueFull(Box::new(job))
            }
        })
    }
}

/// Drain `receiver`, handing each job to `handle`, until every sender is
/// dropped. What a job *does* — resolve its connector, call [`admit`], write
/// the outcome to the row — is the closure's, so this loop needs nothing it
/// cannot name.
pub async fn run_worker<F, Fut>(mut receiver: mpsc::Receiver<AdmissionJob>, mut handle: F)
where
    F: FnMut(AdmissionJob) -> Fut,
    Fut: Future<Output = ()>,
{
    while let Some(job) = receiver.recv().await {
        handle(job).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::artifact::tests::{
        artifact, spawn_bucket, spawn_bucket_with_delay, storage_config, temp_cache_dir,
    };
    use crate::model::fixture;

    fn config() -> ModelsConfig {
        ModelsConfig {
            enabled: true,
            cache_dir: "unused: the store is built directly".to_string(),
            ..ModelsConfig::default()
        }
    }

    /// The fixture, served as the bucket serves it and described as its
    /// manifest describes it.
    fn job(body: &[u8]) -> AdmissionJob {
        job_with(body, fixture::manifest())
    }

    fn job_with(body: &[u8], manifest: Manifest) -> AdmissionJob {
        AdmissionJob {
            model_id: "ada.c4-tiny".to_string(),
            version: 1,
            artifact: artifact(body),
            signature: None,
            manifest,
        }
    }

    /// Everything one `admit` borrows, so a test builds it once.
    struct Rig {
        storage: StorageConnectorConfig,
        client: reqwest::Client,
        dir: PathBuf,
        store: ArtifactStore,
        config: ModelsConfig,
        runtimes: ModelRuntimes,
    }

    impl Rig {
        fn new(addr: std::net::SocketAddr, config: ModelsConfig) -> Self {
            let dir = temp_cache_dir();
            Self {
                storage: storage_config(addr),
                client: reqwest::Client::new(),
                store: ArtifactStore::new(&dir, 1 << 20),
                dir,
                runtimes: ModelRuntimes::builtin(&config),
                config,
            }
        }

        fn deps(&self) -> AdmissionDeps<'_> {
            AdmissionDeps {
                store: &self.store,
                storage: &self.storage,
                client: &self.client,
                config: &self.config,
                node: "node-a",
                runtimes: &self.runtimes,
            }
        }

        /// A fresh store over the same directory, for a test that changed
        /// the config and wants the bytes fetched again.
        fn reset_store(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
            self.store = ArtifactStore::new(&self.dir, 1 << 20);
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn failure(outcome: &AdmissionOutcome) -> (&'static str, &str) {
        match &outcome.state {
            AdmissionState::Failed { stage, reason } => (stage, reason.as_str()),
            AdmissionState::Passed { .. } => unreachable!("expected a failure: {outcome:?}"),
        }
    }

    #[tokio::test]
    async fn a_verified_artifact_passes_with_its_stats() {
        let body = fixture::ONNX.to_vec();
        let bucket = spawn_bucket(body.clone(), None).await;
        let rig = Rig::new(bucket.addr, config());
        let outcome = admit(&rig.deps(), &job(&body)).await;
        assert!(outcome.passed(), "{outcome:?}");
        assert_eq!(outcome.model_id, "ada.c4-tiny");
        assert_eq!(outcome.version, 1);
        assert_eq!(
            outcome.artifact_path.as_deref(),
            Some(rig.store.path_for(&job(&body).artifact.digest).as_path())
        );
        let AdmissionState::Passed { stats } = &outcome.state else {
            unreachable!("passed")
        };
        // The graph numbers are `build.py`'s; the probe's are this node's.
        assert_eq!(stats.parameters, 1479);
        assert_eq!(stats.nodes, 4);
        assert_eq!(stats.ir_version, 9);
        assert_eq!(stats.opset, 17);
        assert_eq!(stats.artifact_bytes, body.len() as u64);
        assert_eq!(stats.runtime, "tract");
        assert_eq!(stats.device, "cpu");
        assert!(stats.probe_ms > 0.0, "{stats:?}");
        assert!(
            stats.probe_ms <= rig.config.max_probe_ms as f64,
            "{stats:?}"
        );
        // The wire shape a row stores, field for field.
        let stats_json = serde_json::to_value(stats).expect("serialises");
        let mut keys: Vec<&str> = stats_json
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "artifact_bytes",
                "device",
                "ir_version",
                "nodes",
                "opset",
                "parameters",
                "probe_ms",
                "runtime"
            ]
        );
        let now = Utc::now();
        assert_eq!(
            admission_json(&outcome, "node-a", now),
            json!({
                "state": "passed",
                "at": now.naive_utc(),
                "node": "node-a",
                "stage": null,
                "reason": null,
            })
        );
    }

    #[tokio::test]
    async fn each_stage_fails_by_name() {
        let body = fixture::ONNX.to_vec();
        let bucket = spawn_bucket(body.clone(), None).await;

        // signature: keys configured, none given, then a foreign one.
        let signer = crate::crypto::ed25519::SigningKey::generate();
        let mut config = config();
        config.trust.public_keys = vec![signer.public_key_base64()];
        let mut rig = Rig::new(bucket.addr, config);
        let outcome = admit(&rig.deps(), &job(&body)).await;
        let (stage, reason) = failure(&outcome);
        assert_eq!(stage, "signature");
        assert!(reason.contains("models.trust.public_keys"), "{reason}");
        assert!(outcome.artifact_path.is_none());
        assert_eq!(bucket.gets.load(std::sync::atomic::Ordering::SeqCst), 0);
        let mut signed = job(&body);
        signed.signature = Some(signer.sign(&signed.artifact.digest));
        assert!(
            admit(&rig.deps(), &signed).await.passed(),
            "a good signature passes"
        );

        // size: the HEAD's Content-Length is over the cap, so no GET happens.
        rig.reset_store();
        rig.config.trust.public_keys.clear();
        rig.config.max_artifact_bytes = 4;
        let gets_before = bucket.gets.load(std::sync::atomic::Ordering::SeqCst);
        let outcome = admit(&rig.deps(), &job(&body)).await;
        let (stage, reason) = failure(&outcome);
        assert_eq!(stage, "size");
        assert!(reason.contains("models.max_artifact_bytes (4)"), "{reason}");
        assert_eq!(
            bucket.gets.load(std::sync::atomic::Ordering::SeqCst),
            gets_before
        );
        rig.config.max_artifact_bytes = 1 << 20;

        // digest: the bucket serves other bytes than the row names.
        let outcome = admit(&rig.deps(), &job(b"what the row expected")).await;
        let (stage, reason) = failure(&outcome);
        assert_eq!(stage, "digest");
        assert!(reason.contains("connector 'bucket'"), "{reason}");
        assert!(reason.contains("nothing was kept"), "{reason}");

        // gate: the connector refuses reads.
        rig.storage.operations.presign_get = false;
        let outcome = admit(&rig.deps(), &job(&body)).await;
        assert_eq!(failure(&outcome).0, "gate");
        rig.storage.operations.presign_get = true;

        // head: the object is not there.
        let missing = spawn_bucket(body.clone(), Some(404)).await;
        rig.storage = storage_config(missing.addr);
        let outcome = admit(&rig.deps(), &job(&body)).await;
        let (stage, reason) = failure(&outcome);
        assert_eq!(stage, "head");
        assert!(reason.contains("404"), "{reason}");
        let now = Utc::now();
        let recorded = admission_json(&outcome, "n", now);
        assert_eq!(recorded["state"], "failed");
        assert_eq!(recorded["stage"], "head");
        assert_eq!(recorded["node"], "n");
        assert_eq!(recorded["at"], json!(now.naive_utc()));
        // The row DTO decodes it: the two must agree on the spelling.
        assert!(
            serde_json::from_value::<orion_api::dto::ModelAdmission>(recorded.clone())
                .is_ok_and(|a| a.at.is_some()),
            "{recorded}"
        );
        assert!(
            recorded["reason"]
                .as_str()
                .is_some_and(|r| r.contains("404"))
        );
    }

    /// The parse stage: bytes that are not a model, a manifest naming a
    /// tensor the graph lacks, and a parameter count over the ceiling —
    /// each before any runtime is asked.
    #[tokio::test]
    async fn the_parse_stage_reads_the_graph_against_the_manifest() {
        let junk = b"\x00\x01this is not a model".to_vec();
        let bucket = spawn_bucket(junk.clone(), None).await;
        let rig = Rig::new(bucket.addr, config());
        let outcome = admit(&rig.deps(), &job(&junk)).await;
        let (stage, reason) = failure(&outcome);
        assert_eq!(stage, "parse");
        assert!(reason.contains("not an ONNX model"), "{reason}");
        // The bytes were verified and kept — it is the graph that failed.
        assert!(rig.store.path_for(&job(&junk).artifact.digest).is_file());

        let body = fixture::ONNX.to_vec();
        let bucket = spawn_bucket(body.clone(), None).await;
        let mut rig = Rig::new(bucket.addr, config());

        let mut manifest = fixture::manifest();
        manifest.inputs[0].name = "boards".to_string();
        let outcome = admit(&rig.deps(), &job_with(&body, manifest)).await;
        let (stage, reason) = failure(&outcome);
        assert_eq!(stage, "parse");
        assert!(reason.contains("input 'boards'"), "{reason}");
        assert!(reason.contains("'board'"), "{reason}");

        let mut manifest = fixture::manifest();
        manifest.outputs[0].name = "logits".to_string();
        let outcome = admit(&rig.deps(), &job_with(&body, manifest)).await;
        let (stage, reason) = failure(&outcome);
        assert_eq!(stage, "parse");
        assert!(reason.contains("output 'logits'"), "{reason}");
        assert!(reason.contains("'policy'"), "{reason}");

        rig.config.max_parameters = 1000;
        let outcome = admit(&rig.deps(), &job(&body)).await;
        let (stage, reason) = failure(&outcome);
        assert_eq!(stage, "parse");
        assert!(reason.contains("1479"), "{reason}");
        assert!(reason.contains("models.max_parameters (1000)"), "{reason}");
        rig.config.max_parameters = 1479;
        assert!(
            admit(&rig.deps(), &job(&body)).await.passed(),
            "at the ceiling is within it"
        );
    }

    /// The probe stage: no runtime enabled, an output the graph does not
    /// produce as declared, and a probe over the time ceiling.
    #[tokio::test]
    async fn the_probe_stage_runs_the_graph_on_the_default_runtime() {
        let body = fixture::ONNX.to_vec();
        let bucket = spawn_bucket(body.clone(), None).await;
        let mut rig = Rig::new(bucket.addr, config());

        rig.runtimes = ModelRuntimes::empty();
        let outcome = admit(&rig.deps(), &job(&body)).await;
        let (stage, reason) = failure(&outcome);
        assert_eq!(stage, "probe");
        assert!(
            reason.contains("runtime 'tract' for format 'onnx' is not enabled on this node"),
            "{reason}"
        );
        rig.runtimes = ModelRuntimes::builtin(&rig.config);

        let mut manifest = fixture::manifest();
        manifest.outputs[0].shape = vec![1, 8];
        let outcome = admit(&rig.deps(), &job_with(&body, manifest)).await;
        let (stage, reason) = failure(&outcome);
        assert_eq!(stage, "probe");
        assert!(reason.contains("output 'policy'"), "{reason}");
        assert!(reason.contains("f32[1, 7]"), "{reason}");
        assert!(reason.contains("f32[1, 8]"), "{reason}");

        let mut manifest = fixture::manifest();
        manifest.outputs[0].dtype = "f64".to_string();
        let outcome = admit(&rig.deps(), &job_with(&body, manifest)).await;
        let (stage, reason) = failure(&outcome);
        assert_eq!(stage, "probe");
        assert!(reason.contains("f64[1, 7]"), "{reason}");

        // No inference takes zero time, so a zero ceiling — which the
        // config refuses, and a test may set — fails every probe by time.
        rig.config.max_probe_ms = 0;
        let outcome = admit(&rig.deps(), &job(&body)).await;
        let (stage, reason) = failure(&outcome);
        assert_eq!(stage, "probe");
        assert!(reason.contains("models.max_probe_ms (0)"), "{reason}");
        assert!(reason.contains("median of 5"), "{reason}");
    }

    /// The runtime is chosen by the manifest's format through the config's
    /// `[models.default_runtime]` table: a row pointing at a runtime the
    /// node has switched off, a row naming a runtime this build lacks, and
    /// a format with no row each fail at `probe` with the selection's own
    /// words — the config's, not the runtime's.
    #[tokio::test]
    async fn the_probe_fails_with_the_selection_message_when_no_runtime_serves_the_format() {
        let body = fixture::ONNX.to_vec();
        let bucket = spawn_bucket(body.clone(), None).await;

        // `default_runtime.onnx = "tract"` with tract disabled: `builtin`
        // registers nothing, and the reason says which row and why.
        let mut disabled = config();
        disabled.runtimes.get_mut("tract").expect("entry").enabled = false;
        let rig = Rig::new(bucket.addr, disabled);
        let outcome = admit(&rig.deps(), &job(&body)).await;
        let (stage, reason) = failure(&outcome);
        assert_eq!(stage, "probe");
        assert_eq!(
            reason,
            "runtime 'tract' for format 'onnx' is not enabled on this node \
             (models.runtimes.tract.enabled)"
        );
        // The bytes were fetched, verified and kept: the failure is the
        // node's, not the artifact's.
        assert!(rig.store.path_for(&job(&body).artifact.digest).is_file());

        // A row naming a runtime this build does not have.
        let mut unknown = config();
        unknown
            .default_runtime
            .insert("onnx".to_string(), "ort".to_string());
        let rig = Rig::new(bucket.addr, unknown);
        let outcome = admit(&rig.deps(), &job(&body)).await;
        let (stage, reason) = failure(&outcome);
        assert_eq!(stage, "probe");
        assert!(
            reason.contains("runtime 'ort' for format 'onnx' is unknown"),
            "{reason}"
        );

        // A manifest format with no row. (Registration refuses a format no
        // runtime serves; the admission job takes the manifest as given.)
        let rig = Rig::new(bucket.addr, config());
        let mut manifest = fixture::manifest();
        manifest.format = "nnef".to_string();
        let outcome = admit(&rig.deps(), &job_with(&body, manifest)).await;
        let (stage, reason) = failure(&outcome);
        assert_eq!(stage, "probe");
        assert_eq!(
            reason,
            "no default runtime for format 'nnef' (models.default_runtime.nnef is not set)"
        );
    }

    /// The budget applies to the whole sequence, and the failure names the
    /// stage it was in.
    #[tokio::test]
    async fn an_admission_over_its_budget_fails_in_the_stage_it_was_in() {
        let body = fixture::ONNX.to_vec();
        let bucket = spawn_bucket_with_delay(body.clone(), None, Duration::from_secs(5)).await;
        let mut config = config();
        config.admission_timeout_secs = 1;
        let rig = Rig::new(bucket.addr, config);
        let outcome = admit(&rig.deps(), &job(&body)).await;
        let (stage, reason) = failure(&outcome);
        assert_eq!(stage, "fetch");
        assert!(reason.contains("admission_timeout_secs (1)"), "{reason}");
        assert!(reason.contains("during fetch"), "{reason}");
        assert!(outcome.elapsed < Duration::from_secs(4));
    }

    #[tokio::test]
    async fn the_queue_is_bounded_and_the_worker_drains_it() {
        let (queue, rx) = AdmissionQueue::with_capacity(2);
        let body = b"x";
        queue.enqueue(job(body)).expect("one");
        queue.enqueue(job(body)).expect("two");
        let err = queue
            .enqueue(job(body))
            .expect_err("three is over capacity");
        assert_eq!(*err.0, job(body), "the job comes back");
        assert!(err.to_string().contains("full"), "{err}");

        let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = seen.clone();
        let worker = tokio::spawn(run_worker(rx, move |_job| {
            let counted = counted.clone();
            async move {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }));
        drop(queue);
        worker
            .await
            .expect("the worker ends when every sender is gone");
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
