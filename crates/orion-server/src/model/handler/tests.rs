use super::*;
use crate::model::ModelSet;
use crate::model::fixture;
use serde_json::json;

fn models_runtime(config: &ModelsConfig) -> Arc<ModelsRuntime> {
    Arc::new(ModelsRuntime::new(config, "n".to_string()).expect("creates the dir"))
}

fn config() -> ModelsConfig {
    ModelsConfig {
        enabled: true,
        cache_dir: std::env::temp_dir()
            .join(format!("orion-model-handler-{}", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned(),
        ..ModelsConfig::default()
    }
}

fn function(models: Option<Arc<ModelsRuntime>>, config: ModelsConfig) -> ModelInferFunction {
    let config = Arc::new(config);
    ModelInferFunction {
        source: ModelSource::Node(crate::runtime::generation::test_handle()),
        host: models.map(|m| {
            Arc::new(InferenceHost::node(
                &m,
                config.clone(),
                Arc::new(ConnectorRegistry::new(Default::default())),
                reqwest::Client::new(),
            ))
        }),
        config,
    }
}

async fn refused(function: ModelInferFunction, input: Value) -> String {
    twin::v4(NAME, function, input, json!({"board": [1]}))
        .await
        .expect_err("the task must fail")
        .message
}

/// An engine with no workflows, for a generation that only carries a set.
fn empty_engine() -> dataflow_rs::Engine {
    dataflow_rs::Engine::builder()
        .build()
        .expect("an empty engine builds")
}

/// Every refusal before a model runs names the handler and what the
/// caller can act on: the node without the runtime, an id the
/// generation does not serve, the id's own load issue, the fields the
/// task must carry. (A task with no `model` is refused at load; see
/// `refusals_are_1_12s`.)
#[tokio::test]
async fn refusals_name_the_handler_and_the_reason() {
    let config = config();
    let good = json!({"model": "ada.c4-tiny", "input": {"var": ""}});

    let err = refused(function(None, config.clone()), good.clone()).await;
    assert!(
        err.contains("model_infer: models are disabled on this node"),
        "{err}"
    );

    let models = models_runtime(&config);
    let err = refused(function(Some(models.clone()), config.clone()), good.clone()).await;
    assert!(
        err.contains("model 'ada.c4-tiny' is unavailable on this node")
            && err.contains("unknown or not active"),
        "{err}"
    );

    let err = refused(
        function(Some(models.clone()), config.clone()),
        json!({"model": 7, "input": {"var": ""}}),
    )
    .await;
    assert!(err.contains("'model' must evaluate to a model id"), "{err}");

    // A generation carrying the model's load issue hands the reason on.
    let h = function(Some(models.clone()), config.clone());
    let ModelSource::Node(runtime) = &h.source else {
        unreachable!("built on a node source")
    };
    let runtime = runtime.clone();
    let generation = runtime.load();
    let now = chrono::Utc::now().naive_utc();
    let row = crate::storage::models::Model {
        model_id: "ada.c4-tiny".to_string(),
        version: 1,
        status: "active".to_string(),
        digest: "sha256:abc".to_string(),
        manifest_json: fixture::MANIFEST.to_string(),
        artifact_json: json!({"connector": "bucket", "key": "k", "digest": "sha256:abc"})
            .to_string(),
        admission_json: json!({"state": "failed", "stage": "digest", "reason": "mismatch"})
            .to_string(),
        stats_json: None,
        tags_json: "[]".to_string(),
        signature: None,
        created_at: now,
        updated_at: now,
    };
    let set = ModelSet::load_active(&[row], &config, true, &crate::engine::kit::test_exprs());
    runtime.publish(
        empty_engine(),
        generation.channels.clone(),
        generation.functions.clone(),
        generation.plugins.clone(),
        Arc::new(set),
    );
    let err = refused(h, good).await;
    assert!(
        err.contains(
            "unavailable on this node: admission: admission is 'failed' at stage \
                          'digest': mismatch"
        ),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&config.cache_dir);
}

/// Cold loads across distinct callers are bounded by
/// `models.max_concurrent_loads`: with one slot the fetch-and-parse that
/// holds an artifact in memory never overlaps; unbounded, it does.
#[tokio::test]
async fn cold_loads_are_bounded_by_the_load_semaphore() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Records how many loads hold the fetch-and-parse slot at once.
    struct Counting {
        live: AtomicUsize,
        peak: AtomicUsize,
    }
    #[async_trait]
    impl ArtifactSource for Counting {
        async fn bytes(&self, _entry: &ModelEntry) -> Result<Vec<u8>, LoadError> {
            let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(live, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            self.live.fetch_sub(1, Ordering::SeqCst);
            Ok(vec![0u8; 4])
        }
    }

    struct Loaded;
    impl LoadedModel for Loaded {
        fn digest(&self) -> &str {
            "sha256:fake"
        }
        fn resident_bytes(&self) -> usize {
            1
        }
        fn run(
            &self,
            _inputs: Vec<OwnedDataTensor>,
        ) -> Result<Vec<OwnedDataTensor>, super::super::runtimes::RunError> {
            Ok(Vec::new())
        }
    }
    struct Stub;
    impl ModelRuntime for Stub {
        fn name(&self) -> &'static str {
            "tract"
        }
        fn devices(&self) -> &'static [&'static str] {
            &["cpu"]
        }
        fn formats(&self) -> &'static [&'static str] {
            &["onnx"]
        }
        fn load(
            &self,
            _bytes: &[u8],
            _binding: &super::super::runtimes::LoadBinding,
            _device: &str,
        ) -> Result<Arc<dyn LoadedModel>, LoadError> {
            Ok(Arc::new(Loaded))
        }
    }

    // One compiled entry, reused: fetch_and_load does not dedup (the cache
    // single-flight does), so N concurrent calls each take a load slot —
    // which is what we are measuring.
    let cfg = config();
    let set = ModelSet::from_manifests(
        [super::super::loader::ManifestEntry {
            manifest: fixture::manifest(),
            artifact_path: std::path::PathBuf::from("x.onnx"),
            digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                .to_string(),
            stats: None,
        }],
        &cfg,
        &crate::engine::kit::test_exprs(),
    );
    let id = set.ids().next().expect("one entry compiled").to_string();
    let entry = set.get(&id).expect("the entry").clone();

    let peak_with = |slots: Option<usize>| {
        let entry = entry.clone();
        let cfg = cfg.clone();
        async move {
            let counting = Arc::new(Counting {
                live: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
            });
            let host = Arc::new(InferenceHost {
                runtimes: Arc::new(ModelRuntimes::builtin(&cfg)),
                loaded: Arc::new(LoadedCache::new(1 << 30)),
                inference_permits: Arc::new(Semaphore::new(1)),
                inference_slots: 1,
                load_permits: slots.map(|n| Arc::new(Semaphore::new(n))),
                artifacts: counting.clone(),
            });
            let runtime: Arc<dyn ModelRuntime> = Arc::new(Stub);
            let mut tasks = Vec::new();
            for _ in 0..4 {
                let host = host.clone();
                let entry = entry.clone();
                let runtime = runtime.clone();
                tasks.push(tokio::spawn(async move {
                    fetch_and_load(&host, &entry, runtime, "cpu").await
                }));
            }
            for task in tasks {
                task.await.expect("join").expect("the load succeeds");
            }
            counting.peak.load(Ordering::SeqCst)
        }
    };

    assert_eq!(
        peak_with(Some(1)).await,
        1,
        "one slot serialises cold loads"
    );
    assert!(
        peak_with(None).await >= 2,
        "unbounded, distinct cold loads overlap"
    );
}

/// The pieces of a message: a shape as `dtype[dims]`, a value's kind.
#[test]
fn messages_spell_shapes_and_kinds() {
    assert_eq!(dims(&[1, 2, 6, 7]), "1,2,6,7");
    assert_eq!(dims::<usize>(&[]), "");
    // A declared shape prints the same way, a named axis by its name,
    // so a refusal can put the declaration beside what arrived.
    assert_eq!(dims(&crate::model::manifest::fixed_shape(&[1, 7])), "1,7");
    assert_eq!(
        dims(&[
            crate::model::manifest::Dim::Fixed(1),
            crate::model::manifest::Dim::Named("H".to_string()),
        ]),
        "1,H"
    );
    assert_eq!(kind_of(&OwnedDataValue::Null), "null");
    assert_eq!(kind_of(&OwnedDataValue::from(&json!([1]))), "a list");
    assert_eq!(
        kind_of(&OwnedDataValue::from(&json!({"a": 1}))),
        "an object"
    );
    assert_eq!(kind_of(&OwnedDataValue::from(&json!("s"))), "a string");
}

// -- `ModelInferFunction`, against 1.12's records --

use crate::engine::ResolvedSecrets;
use crate::engine::functions::twin::{self, Seen};
use crate::model::loader::ManifestEntry;
use dataflow_rs::function::testing;
use dataflow_rs::program::{IssueCode, Severity};

/// The fixture's graph, after a pause: a cold load as slow as a test
/// needs.
struct Graph(Duration);

#[async_trait]
impl ArtifactSource for Graph {
    async fn bytes(&self, _entry: &ModelEntry) -> Result<Vec<u8>, LoadError> {
        tokio::time::sleep(self.0).await;
        Ok(fixture::ONNX.to_vec())
    }
}

/// What one call runs over: the `c4-tiny` fixture's set, compiled on
/// `exprs`, and a host of the call's own, whose cold load takes `delay`
/// and which holds `slots` inference slots.
struct Rig {
    config: Arc<ModelsConfig>,
    exprs: ExprEngine,
    delay: Duration,
    slots: usize,
}

impl Rig {
    fn new() -> Self {
        Self {
            config: Arc::new(ModelsConfig {
                enabled: true,
                ..ModelsConfig::default()
            }),
            exprs: kit::test_exprs(),
            delay: Duration::ZERO,
            slots: 1,
        }
    }

    fn source(&self) -> ModelSource {
        ModelSource::Offline(Arc::new(OfflineModels::new(
            vec![ManifestEntry {
                manifest: fixture::manifest(),
                artifact_path: std::path::PathBuf::from("c4-tiny.onnx"),
                digest: crate::crypto::sha256_digest(fixture::ONNX),
                stats: None,
            }],
            self.config.clone(),
            self.exprs.clone(),
        )))
    }

    fn host(&self) -> Arc<InferenceHost> {
        Arc::new(InferenceHost {
            runtimes: Arc::new(ModelRuntimes::builtin(&self.config)),
            loaded: Arc::new(LoadedCache::new(self.config.max_loaded_bytes)),
            inference_permits: Arc::new(Semaphore::new(self.slots)),
            inference_slots: self.slots,
            load_permits: None,
            artifacts: Arc::new(Graph(self.delay)),
        })
    }

    fn function(&self) -> ModelInferFunction {
        ModelInferFunction {
            source: self.source(),
            host: Some(self.host()),
            config: self.config.clone(),
        }
    }
}

struct BlockingWork {
    gate: std::sync::Mutex<bool>,
    wake: std::sync::Condvar,
    started: tokio::sync::mpsc::UnboundedSender<()>,
}

impl BlockingWork {
    fn block(&self) {
        self.started.send(()).expect("test watches started work");
        let guard = self.gate.lock().expect("gate");
        let _wait = self
            .wake
            .wait_timeout_while(guard, Duration::from_secs(5), |released| !*released)
            .expect("wait");
    }

    fn release(&self) {
        *self.gate.lock().expect("gate") = true;
        self.wake.notify_all();
    }
}

impl LoadedModel for BlockingWork {
    fn digest(&self) -> &str {
        "test"
    }
    fn resident_bytes(&self) -> usize {
        1
    }
    fn run(
        &self,
        _: Vec<OwnedDataTensor>,
    ) -> Result<Vec<OwnedDataTensor>, super::super::runtimes::RunError> {
        self.block();
        Ok(Vec::new())
    }
}

struct BlockingRuntime(Arc<BlockingWork>);

impl ModelRuntime for BlockingRuntime {
    fn name(&self) -> &'static str {
        "tract"
    }
    fn devices(&self) -> &'static [&'static str] {
        &["cpu"]
    }
    fn formats(&self) -> &'static [&'static str] {
        &["onnx"]
    }
    fn load(
        &self,
        _: &[u8],
        _: &super::super::runtimes::LoadBinding,
        _: &str,
    ) -> Result<Arc<dyn LoadedModel>, LoadError> {
        self.0.block();
        Ok(self.0.clone())
    }
}

fn blocking_work() -> (Arc<BlockingWork>, tokio::sync::mpsc::UnboundedReceiver<()>) {
    let (started, receiver) = tokio::sync::mpsc::unbounded_channel();
    (
        Arc::new(BlockingWork {
            gate: std::sync::Mutex::new(false),
            wake: std::sync::Condvar::new(),
            started,
        }),
        receiver,
    )
}

/// The limit counts real blocking jobs, including ones whose callers
/// have timed out or gone away. Both permit families must stay held.
#[tokio::test]
async fn inference_capacity_survives_timeout_and_caller_cancellation() {
    for cancel in [false, true] {
        let rig = Rig::new();
        let source = match rig.source() {
            ModelSource::Offline(source) => source,
            _ => unreachable!(),
        };
        let host = rig.host();
        let entry = source.set().get("ada.c4-tiny").expect("entry").clone();
        let (work, mut started) = blocking_work();
        let model: Arc<dyn LoadedModel> = work.clone();
        host.loaded
            .get_or_load(
                CacheKey {
                    digest: entry.digest.clone(),
                    runtime: "tract",
                    device: "cpu".to_string(),
                    binding: entry.binding.fingerprint().to_string(),
                },
                || async { Ok(model) },
            )
            .await
            .expect("cached");
        let call = |timeout_ms| {
            testing::call(
                ModelInferFunction {
                    source: ModelSource::Offline(source.clone()),
                    host: Some(host.clone()),
                    config: rig.config.clone(),
                },
                infer(json!({"timeout_ms": timeout_ms})),
            )
            .exprs(rig.exprs.clone())
            .name(NAME)
            .message(message(&data()))
        };
        let first = tokio::spawn(call(if cancel { 10_000 } else { 100 }).into_future());
        tokio::time::timeout(Duration::from_secs(2), started.recv())
            .await
            .expect("job starts")
            .expect("started");
        if cancel {
            first.abort();
            assert!(matches!(first.await, Err(error) if error.is_cancelled()));
        } else {
            assert!(first.await.expect("joined").outcome.is_err());
        }
        let held_global = host.inference_permits.available_permits();
        let held_own = entry.permits.available_permits();
        let second = call(20).await;
        let no_second_job = started.try_recv().is_err();
        work.release();
        let _released_slot = tokio::time::timeout(
            Duration::from_secs(2),
            host.inference_permits.clone().acquire_owned(),
        )
        .await
        .expect("real completion returns capacity")
        .expect("open");
        assert_eq!(held_global, 0, "timed-out work keeps global capacity");
        assert_eq!(
            held_own,
            entry.limits.max_concurrency as usize - 1,
            "timed-out work keeps its model permit"
        );
        assert!(
            second.outcome.is_err(),
            "second caller cannot bypass the limit"
        );
        assert!(no_second_job, "only one blocking job may start");
    }
}

#[tokio::test]
async fn cold_load_capacity_survives_caller_cancellation() {
    let rig = Rig::new();
    let entry = rig
        .source()
        .set()
        .get("ada.c4-tiny")
        .expect("entry")
        .clone();
    let mut host = rig.host();
    Arc::get_mut(&mut host).expect("unique host").load_permits = Some(Arc::new(Semaphore::new(1)));
    let slots = host.load_permits.as_ref().expect("bounded").clone();
    let (work, mut started) = blocking_work();
    let runtime: Arc<dyn ModelRuntime> = Arc::new(BlockingRuntime(work.clone()));
    let task_host = host.clone();
    let task_entry = entry.clone();
    let task_runtime = runtime.clone();
    let first =
        tokio::spawn(
            async move { fetch_and_load(&task_host, &task_entry, task_runtime, "cpu").await },
        );
    tokio::time::timeout(Duration::from_secs(2), started.recv())
        .await
        .expect("load starts")
        .expect("started");
    first.abort();
    assert!(matches!(first.await, Err(error) if error.is_cancelled()));
    let held = slots.available_permits();
    let second = tokio::time::timeout(
        Duration::from_millis(20),
        fetch_and_load(&host, &entry, runtime, "cpu"),
    )
    .await;
    let no_second_job = started.try_recv().is_err();
    work.release();
    let _released_slot = tokio::time::timeout(Duration::from_secs(2), slots.acquire())
        .await
        .expect("load finishes")
        .expect("open");
    assert_eq!(held, 0, "a running parse retains its cold-load slot");
    assert!(second.is_err(), "second load waits for real completion");
    assert!(no_second_job);
}

/// A `[1, 2, 6, 7]` board, a few stones placed, as the fixture's
/// adapter reads it at `data.board`; the id a computed `model` reads;
/// the columns a `select` gathers.
fn data() -> Value {
    let mut planes = vec![vec![vec![0.0f32; 7]; 6]; 2];
    planes[0][5][3] = 1.0;
    planes[1][5][2] = 1.0;
    planes[0][4][3] = 1.0;
    json!({"board": [planes], "which": "ada.c4-tiny", "cols": [3, 5]})
}

/// A message whose `data` is `data`.
fn message(data: &Value) -> dataflow_rs::Message {
    dataflow_rs::Message::from_parts(
        OwnedDataValue::Null,
        OwnedDataValue::from(data),
        OwnedDataValue::Null,
    )
}

/// `input` with `extra`'s members set on it.
fn with(mut input: Value, extra: Value) -> Value {
    if let (Some(into), Some(from)) = (input.as_object_mut(), extra.as_object()) {
        into.extend(from.clone());
    }
    input
}

/// The fixture, over the whole context, its result at `data.result`.
fn infer(extra: Value) -> Value {
    with(
        json!({"model": "ada.c4-tiny", "input": {"var": ""}, "output": "data.result"}),
        extra,
    )
}

/// `seen` with the stats' two timings, which no two calls share, read
/// as numbers and set aside.
fn steady(seen: Seen) -> Seen {
    seen.map(|mut data| {
        if let Some(stats) = data.get_mut("stats").and_then(Value::as_object_mut) {
            for timing in ["queued_ms", "inference_ms"] {
                assert!(stats.get(timing).is_some_and(Value::is_number), "{timing}");
                stats.insert(timing.to_string(), Value::Null);
            }
        }
        data
    })
}

/// One task over a fresh host of `rig`'s, so its load is cold.
async fn run(rig: &Rig, input: Value) -> Seen {
    steady(twin::v4(NAME, rig.function(), input, data()).await)
}

/// One call of `rig`'s function over [`data`], on Orion's
/// expressions, under the run deadline `deadline` from now.
async fn call_v4(rig: &Rig, input: Value, deadline: Option<Duration>) -> testing::Called {
    let call = testing::call(rig.function(), input)
        .exprs(kit::test_exprs())
        .name(NAME)
        .message(message(&data()));
    match deadline {
        Some(after) => {
            call.deadline(dataflow_rs::run::Instant::now() + after)
                .await
        }
        None => call.await,
    }
}

/// The fixture runs for real and writes what 1.12 wrote: the manifest's
/// result, the outputs `raw`, a `select`'s reduction with
/// operands from the message, and the stats beside the result.
#[tokio::test]
async fn an_inference_writes_what_1_12_wrote() {
    let rig = Rig::new();
    for input in [
        infer(json!({"stats_output": "data.stats"})),
        // A computed model, a runtime named and a computed deadline.
        infer(json!({
            "model": {"var": "data.which"},
            "runtime": "tract",
            "timeout_ms": {"+": [4000, 1000]}
        })),
        infer(json!({"raw": true})),
        infer(json!({"select": {"to_list": [{"gather": [
            {"reshape": [{"var": "policy"}, [7]]},
            {"var": "input.data.cols"},
            0
        ]}]}})),
        infer(json!({
            "select": {"argmax": [{"var": "policy"}, 1]},
            "stats_output": "data.stats"
        })),
    ] {
        let v4 = run(&rig, input.clone()).await;
        twin::pinned(&v4, format!("inference/{input}"));
        let data = v4.expect("the inference ran");
        assert!(
            data.get("result").is_some_and(|r| !r.is_null()),
            "{input}: {data}"
        );
    }
}

/// `stats_output` is written beside `output`, the primary, after it, as
/// 1.12 wrote it: the call's account, `ops` and `peak_ops` included.
#[tokio::test]
async fn stats_output_is_written_beside_output() {
    let called = call_v4(
        &Rig::new(),
        json!({"model": "ada.c4-tiny", "input": {"var": ""}, "stats_output": "data.stats"}),
        None,
    )
    .await;
    assert_eq!(called.outcome.as_ref().ok(), Some(&Outcome::Ok));
    let paths: Vec<String> = called.writes.iter().map(|(p, _)| p.to_string()).collect();
    assert_eq!(paths, ["temp_data.inference", "data.stats"]);
    let result = Value::from(called.message.context().temp_data());
    assert_eq!(
        result["inference"]["policy"][0].as_array().map(Vec::len),
        Some(7),
        "{result}"
    );
    let stats = &Value::from(called.message.context().data())["stats"];
    let keys: Vec<&str> = stats
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "id",
            "version",
            "digest",
            "runtime",
            "device",
            "parameters",
            "artifact_bytes",
            "ops",
            "peak_ops",
            "queued_ms",
            "inference_ms",
            "cold_load"
        ]
    );
    assert_eq!(stats["id"], "ada.c4-tiny");
    assert_eq!(stats["runtime"], "tract");
    assert_eq!(stats["cold_load"], true);
    let (ops, peak) = (stats["ops"].as_u64(), stats["peak_ops"].as_u64());
    assert!(
        matches!((ops, peak), (Some(ops), Some(peak)) if peak > 0 && ops >= peak),
        "{stats}"
    );
}

/// Every refusal before and after a model runs, with 1.12's code, text
/// and retryability, the generation's load issue included. A missing
/// `model` is refused at load (I183), where 1.12 refused each call.
#[tokio::test]
async fn refusals_are_1_12s() {
    let rig = Rig::new();
    let disabled = ModelInferFunction {
        source: rig.source(),
        host: None,
        config: rig.config.clone(),
    };
    let v4 = twin::v4(NAME, disabled, infer(json!({})), data()).await;
    twin::pinned(&v4, "refusals/disabled");
    assert!(
        v4.expect_err("disabled")
            .message
            .contains("model_infer: models are disabled on this node")
    );

    for input in [
        infer(json!({"model": 7})),
        infer(json!({"model": "ada.nope"})),
        infer(json!({"model": {"var": "data.nothing"}})),
        infer(json!({"input": {"var": "data.nothing"}})),
        infer(json!({"input": {"data": {"board": [[1, 2]]}}})),
        infer(json!({"timeout_ms": {"-": [5, 5]}})),
        infer(json!({"select": {"reshape": [{"var": "policy"}, [5]]}})),
    ] {
        let v4 = run(&rig, input.clone()).await;
        twin::pinned(&v4, format!("refusals/{input}"));
        assert!(v4.is_err(), "{input}");
    }

    // A generation carrying the model's load issue hands the reason on.
    let config = config();
    let runtime = crate::runtime::generation::test_handle();
    let generation = runtime.load();
    let now = chrono::Utc::now().naive_utc();
    let row = crate::storage::models::Model {
        model_id: "ada.c4-tiny".to_string(),
        version: 1,
        status: "active".to_string(),
        digest: "sha256:abc".to_string(),
        manifest_json: fixture::MANIFEST.to_string(),
        artifact_json: json!({"connector": "bucket", "key": "k", "digest": "sha256:abc"})
            .to_string(),
        admission_json: json!({"state": "failed", "stage": "digest", "reason": "mismatch"})
            .to_string(),
        stats_json: None,
        tags_json: "[]".to_string(),
        signature: None,
        created_at: now,
        updated_at: now,
    };
    runtime.publish(
        empty_engine(),
        generation.channels.clone(),
        generation.functions.clone(),
        generation.plugins.clone(),
        Arc::new(ModelSet::load_active(
            &[row],
            &config,
            true,
            &kit::test_exprs(),
        )),
    );
    let models = models_runtime(&config);
    let node = |runtime: &Arc<RuntimeHandle>| {
        let config = Arc::new(config.clone());
        let host = Some(Arc::new(InferenceHost::node(
            &models,
            config.clone(),
            Arc::new(ConnectorRegistry::new(Default::default())),
            reqwest::Client::new(),
        )));
        ModelInferFunction {
            source: ModelSource::Node(runtime.clone()),
            host,
            config,
        }
    };
    let v4 = twin::v4(NAME, node(&runtime), infer(json!({})), data()).await;
    twin::pinned(&v4, "refusals/load_issue");
    assert!(v4.expect_err("not served").message.contains(
        "unavailable on this node: admission: admission is 'failed' at stage 'digest': \
                 mismatch"
    ));
    let _ = std::fs::remove_dir_all(&config.cache_dir);

    // A task with no `model` never loads.
    let called = call_v4(&rig, json!({"input": {"var": ""}}), None).await;
    assert_eq!(
        *called.outcome.expect_err("refused at load").kind(),
        ErrorKind::Compile
    );
    assert!(
        called
            .issues
            .iter()
            .any(|i| i.path.to_string() == "model" && i.severity == Severity::Rejected),
        "{:?}",
        called.issues
    );
}

/// An `engine.ops_budget` crossed in an adapter is `ErrorKind::Budget`,
/// where 1.12 matched the error's `BudgetExceeded` tag: 1.12's code, text
/// and retryability, under the adapter's name.
#[tokio::test]
async fn a_budget_crossed_in_an_adapter_is_errorkind_budget() {
    let mut rig = Rig::new();
    rig.exprs =
        crate::engine::operators::orion_exprs(&ResolvedSecrets::empty(), 20).expect("builds");
    let v4 = run(&rig, infer(json!({}))).await;
    twin::pinned(&v4, "budget/adapter");
    let failure = v4.expect_err("over the ceiling");
    assert_eq!(failure.code, "BUDGET_EXCEEDED");
    assert!(
        failure
            .message
            .starts_with("model_infer: adapter for input 'board' failed: ")
            && failure.message.contains("budget of 20"),
        "{}",
        failure.message
    );
    assert!(!failure.retryable);
    let called = call_v4(&rig, infer(json!({})), None).await;
    assert_eq!(
        *called.outcome.expect_err("over the ceiling").kind(),
        ErrorKind::Budget
    );
    assert!(called.writes.is_empty());
}

/// A ceiling crossed in a field of the task's own keeps its kind under
/// the field's name.
#[tokio::test]
async fn a_budget_crossed_in_a_field_keeps_its_kind() {
    let rig = Rig::new();
    let input = infer(json!({
        "input": {"+": [{"+": [{"+": [{"+": [1, {"var": "data.cols.0"}]}, 1]}, 1]}, 1]}
    }));
    let called = testing::call(rig.function(), input)
        .exprs(crate::engine::operators::orion_exprs(&ResolvedSecrets::empty(), 3).expect("builds"))
        .name(NAME)
        .message(message(&data()))
        .await;
    let v4 = twin::seen(&called);
    twin::pinned(&v4, "budget/field");
    let failure = v4.expect_err("over the ceiling");
    assert_eq!(failure.code, "BUDGET_EXCEEDED");
    assert!(
        failure.message.starts_with("model_infer: 'input': "),
        "{}",
        failure.message
    );
    assert_eq!(
        *called.outcome.expect_err("over the ceiling").kind(),
        ErrorKind::Budget
    );
}

/// The model's own deadline first — `timeout_ms`, or the node's ceiling
/// when it is unset — is 1.12's, whatever the run's deadline after it:
/// a cold load cut short, `TIMEOUT_ERROR`, retryable, in 1.12's words,
/// and nothing written.
#[tokio::test]
async fn the_models_own_deadline_first_fails_as_1_12_did() {
    let mut slow = Rig::new();
    slow.delay = Duration::from_secs(5);
    let mut capped = Rig::new();
    capped.delay = Duration::from_secs(5);
    capped.config = Arc::new(ModelsConfig {
        enabled: true,
        max_timeout_ms: 100,
        ..ModelsConfig::default()
    });
    for (rig, input) in [
        (&slow, infer(json!({"timeout_ms": 100}))),
        (&capped, infer(json!({}))),
    ] {
        let called = call_v4(rig, input.clone(), Some(Duration::from_secs(60))).await;
        let v4 = twin::seen(&called);
        let failure = v4.expect_err("timed out");
        assert_eq!(failure.code, "TIMEOUT_ERROR");
        assert_eq!(
            failure.message,
            "Timeout error: model_infer: the inference exceeded its deadline of 100ms"
        );
        assert!(failure.retryable);
        assert!(called.writes.is_empty());
    }
}

/// The run's deadline first: the call arms no timer of the model's,
/// and the engine ends it `TimedOut` at the run's deadline, writing
/// nothing, as 1.12's channel timeout ended the run.
#[tokio::test]
async fn the_runs_deadline_first_ends_the_call_timed_out() {
    let mut rig = Rig::new();
    rig.delay = Duration::from_secs(5);
    let started = std::time::Instant::now();
    let called = call_v4(
        &rig,
        infer(json!({"timeout_ms": 10_000})),
        Some(Duration::from_millis(150)),
    )
    .await;
    let error = called.outcome.expect_err("the run's deadline passed");
    assert_eq!(*error.kind(), ErrorKind::TimedOut);
    assert_eq!(
        error.message(),
        "the run's deadline passed during `model_infer`"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert!(called.writes.is_empty());
}

/// A call waiting for an inference slot waits under the same rule: the
/// model's deadline first is 1.12's `permit` refusal; the run's first
/// ends the call `TimedOut`.
#[tokio::test]
async fn a_full_slot_waits_under_the_earlier_deadline() {
    let mut rig = Rig::new();
    rig.slots = 0;
    let input = infer(json!({"timeout_ms": 150}));
    let called = call_v4(&rig, input, Some(Duration::from_secs(60))).await;
    let failure = twin::seen(&called).expect_err("no slot");
    assert_eq!(failure.code, "VALIDATION_ERROR");
    assert_eq!(
        failure.message,
        "Validation error: model_infer: no inference slot freed up before the deadline \
             (models.max_concurrent_inferences)"
    );

    let called = call_v4(
        &rig,
        infer(json!({"timeout_ms": 10_000})),
        Some(Duration::from_millis(150)),
    )
    .await;
    assert_eq!(
        *called.outcome.expect_err("the run's deadline").kind(),
        ErrorKind::TimedOut
    );
    assert!(called.writes.is_empty());
}

/// I185: a literal context root at `output`, the default excepted, is an
/// Advisory, since the result may not be an object: the manifest's
/// object result is written there, and a `select`'s number fails the
/// call (break 21). `raw` writes an object, so it draws none.
#[tokio::test]
async fn a_literal_root_output_is_an_advisory_unless_raw() {
    let rig = Rig::new();
    let argmax = json!({"argmax": [{"var": "policy"}, 1]});
    for (input, advisory, written) in [
        (infer(json!({"output": "data"})), true, true),
        (
            infer(json!({"output": "temp_data", "select": argmax})),
            true,
            false,
        ),
        (infer(json!({"output": "data", "raw": true})), false, true),
        (
            json!({"model": "ada.c4-tiny", "input": {"var": ""}}),
            false,
            true,
        ),
        (infer(json!({"select": argmax})), false, true),
    ] {
        let called = call_v4(&rig, input.clone(), None).await;
        let advisories: Vec<(String, IssueCode)> = called
            .issues
            .iter()
            .filter(|i| i.severity == Severity::Advisory)
            .map(|i| (i.path.to_string(), i.code))
            .collect();
        if advisory {
            assert_eq!(
                advisories,
                [("output".to_string(), IssueCode::InputParse)],
                "{input}"
            );
        } else {
            assert!(advisories.is_empty(), "{input}: {advisories:?}");
        }
        assert_eq!(
            called.outcome.is_ok(),
            written,
            "{input}: {:?}",
            called.outcome
        );
    }
}
