use std::sync::Mutex;

use dataflow_rs::function::testing;
use dataflow_rs::observe::{Event, Interest, Observer};
use dataflow_rs::program::Severity;
use dataflow_rs::run::EngineConfig;
use serde_json::json;

use super::*;
use crate::engine::functions::channel_call::*;
use crate::engine::functions::twin::{self, Failure, Seen};
use crate::engine::kit::{self};
use crate::runtime::RuntimeHandle;
use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::function::FunctionConfig;
use dataflow_rs::program::IssueCode;
use dataflow_rs::run::{AuditMode, CancelToken, RunStatus};
use dataflow_rs::{Ctx, Function, Message, Outcome, RunOptions};
use std::time::Duration;

/// `engine.max_channel_call_depth`'s default.
const MAX_DEPTH: u32 = 10;

// -- The world: one generation and its engine --

/// One channel of a [`World`]: its row's config, the tasks of the one
/// workflow serving it (none for a channel no workflow serves), and
/// whether it is a cron channel.
#[derive(Clone)]
struct Chan {
    name: &'static str,
    config: Value,
    tasks: Option<Value>,
    condition: Value,
    cron: bool,
}

/// A channel whose workflow runs `tasks`.
fn chan(name: &'static str, tasks: Value) -> Chan {
    Chan {
        name,
        config: json!({}),
        tasks: Some(tasks),
        condition: json!(true),
        cron: false,
    }
}

impl Chan {
    fn config(mut self, config: Value) -> Self {
        self.config = config;
        self
    }

    fn condition(mut self, condition: Value) -> Self {
        self.condition = condition;
        self
    }

    fn unserved(mut self) -> Self {
        self.tasks = None;
        self
    }

    fn cron(mut self) -> Self {
        self.cron = true;
        self
    }

    fn named(mut self, name: &'static str) -> Self {
        self.name = name;
        self
    }

    fn row(&self) -> crate::storage::models::Channel {
        let now = chrono::Utc::now().naive_utc();
        crate::storage::models::Channel {
            tags_json: "[]".to_string(),
            channel_id: format!("ch_{}", self.name),
            version: 1,
            name: self.name.to_string(),
            description: None,
            channel_type: if self.cron { "async" } else { "sync" }.to_string(),
            protocol: if self.cron { "cron" } else { "http" }.to_string(),
            methods_json: None,
            route_pattern: None,
            topic: None,
            consumer_group: None,
            transport_config_json: if self.cron {
                json!({"schedule": "0 15 2 * * *"}).to_string()
            } else {
                "{}".to_string()
            },
            workflow_id: Some(self.name.to_string()),
            config_json: self.config.to_string(),
            status: "active".to_string(),
            priority: 0,
            created_at: now,
            updated_at: now,
        }
    }

    /// The workflow serving it.
    fn workflow(&self) -> Option<Value> {
        self.tasks.as_ref().map(|tasks| {
            json!({
                "id": self.name, "name": self.name, "channel": self.name,
                "condition": self.condition, "tasks": tasks,
            })
        })
    }
}

/// A task calling `channel_call` with `input`.
fn call(input: Value) -> Value {
    json!([{"id": "t", "name": "t", "function": {"name": NAME, "input": input}}])
}

/// Every run the engine finished, in order: its channel and how it
/// ended.
#[derive(Default)]
struct Runs {
    started: Mutex<Vec<(String, String)>>,
    finished: Mutex<Vec<(String, String)>>,
}

impl Observer for Runs {
    fn interest(&self) -> Interest {
        Interest::NONE
    }

    fn on(&self, event: &Event<'_>) {
        match event {
            Event::RunStarted {
                message_id,
                channel,
                ..
            } => self
                .started
                .lock()
                .expect("the runs lock")
                .push((message_id.to_string(), (*channel).unwrap_or("").to_string())),
            Event::RunFinished {
                message_id, status, ..
            } => {
                let channel = self
                    .started
                    .lock()
                    .expect("the runs lock")
                    .iter()
                    .find(|(id, _)| id == message_id)
                    .map(|(_, channel)| channel.clone())
                    .unwrap_or_default();
                let status = match status {
                    RunStatus::Completed => "completed".to_string(),
                    RunStatus::CompletedWithErrors => "completed_with_errors".to_string(),
                    RunStatus::Stopped(e) => format!("stopped {}", e.code()),
                    RunStatus::TimedOut => "timed_out".to_string(),
                    RunStatus::Cancelled => "cancelled".to_string(),
                    _ => "other".to_string(),
                };
                self.finished
                    .lock()
                    .expect("the runs lock")
                    .push((channel, status));
            }
            _ => {}
        }
    }
}

impl Runs {
    /// Every finished run, as `(channel, status)`.
    fn finished(&self) -> Vec<(String, String)> {
        self.finished.lock().expect("the runs lock").clone()
    }

    /// The channels of every run started.
    fn channels(&self) -> Vec<String> {
        self.started
            .lock()
            .expect("the runs lock")
            .iter()
            .map(|(_, channel)| channel.clone())
            .collect()
    }
}

/// Waits as long as `ms` says (an hour by default): a target that is
/// still running when a limit fires.
#[derive(Debug, FunctionConfig)]
struct WaitConfig {
    #[config(default = 3_600_000)]
    ms: u64,
}

struct Wait;

impl Function for Wait {
    type Config = WaitConfig;

    async fn call(&self, _: &mut Ctx<'_>, cfg: &WaitConfig) -> dataflow_rs::Result<Outcome> {
        tokio::time::sleep(Duration::from_millis(cfg.ms)).await;
        Ok(Outcome::Ok)
    }
}

/// Fails with a connection error, `IO_ERROR` and retryable: a code that
/// is not `FUNCTION_ERROR`, so passing it on shows.
#[derive(Debug, FunctionConfig)]
struct NoConfig {}

struct Fail;

impl Function for Fail {
    type Config = NoConfig;

    async fn call(&self, _: &mut Ctx<'_>, _: &NoConfig) -> dataflow_rs::Result<Outcome> {
        Err(HandlerError::new(ErrorClass::Connector, "the backend is down").into())
    }
}

/// Cancels the token it holds, then waits: a parent's token tripped
/// while its child runs. 1.12 had no token.
struct Trip(CancelToken);

impl Function for Trip {
    type Config = NoConfig;

    async fn call(&self, _: &mut Ctx<'_>, _: &NoConfig) -> dataflow_rs::Result<Outcome> {
        self.0.cancel();
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok(Outcome::Ok)
    }
}

/// One generation of a node serving `chans`, published on its handle:
/// the channel estate and an engine built from the same channels, with
/// `channel_call` itself registered, so calls chain.
struct World {
    runtime: Arc<RuntimeHandle>,
    v4: dataflow_rs::Engine,
    runs: Arc<Runs>,
    default_timeout_ms: u64,
}

/// The world's knobs: the engine default timeout, the channels
/// quarantined, the token `trip` cancels, and whether the generation
/// has an engine at all.
struct Knobs {
    default_timeout_ms: u64,
    quarantined: Vec<&'static str>,
    trip: CancelToken,
    engine: bool,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            default_timeout_ms: 30_000,
            quarantined: Vec::new(),
            trip: CancelToken::new(),
            engine: true,
        }
    }
}

async fn world(chans: &[Chan]) -> World {
    world_with(chans, Knobs::default()).await
}

async fn world_with(chans: &[Chan], knobs: Knobs) -> World {
    let exprs = kit::test_exprs();
    let client = reqwest::Client::new();
    let rows: Vec<_> = chans.iter().map(Chan::row).collect();
    let issues = knobs
        .quarantined
        .iter()
        .map(|name| crate::channel::ChannelLoadIssue {
            channel: name.to_string(),
            reason: "its workflow did not build".to_string(),
        })
        .collect();
    let connectors = crate::connector::ConnectorRegistry::new(Default::default());
    let channels = crate::channel::ChannelLoader::new()
        .build(
            &crate::channel::ChannelSnapshot::empty(),
            &rows,
            crate::channel::ReloadDeps {
                vars: None,
                connector_registry: &connectors,
                cache_pool: &crate::connector::cache_backend::CachePool::new(60, 128),
                exprs: &exprs,
                jwks: &Arc::new(crate::jwt::jwks::JwksCache::new(client.clone(), false)),
                http_client: &client,
                allow_private_token_urls: false,
                instance_oauth_providers: &std::collections::BTreeMap::new(),
                oidc_discovery: &Arc::new(crate::channel::oidc_discovery::DiscoveryCache::new(
                    client.clone(),
                    false,
                )),
                global_trace_storage: &crate::config::TraceStorageConfig::default(),
                cron_enabled: true,
                channel_drivers: &crate::extensions::ChannelDriverRegistry::community(),
            },
            issues,
        )
        .await;
    let channels = Arc::new(channels);
    // Without an engine the node stays on its boot generation, which
    // holds the estate and nothing to run it.
    let runtime = Arc::new(RuntimeHandle::new(
        Arc::clone(&channels),
        crate::engine::FunctionRegistry::builtin().clone(),
    ));

    let workflows: Vec<Value> = chans.iter().filter_map(Chan::workflow).collect();

    let runs = Arc::new(Runs::default());
    let mut registry = dataflow_rs::Registry::new(exprs.clone());
    registry.register(
        NAME,
        ChannelCallFunction {
            host: Arc::new(GenerationChannels {
                runtime: runtime.clone(),
                exprs: exprs.clone(),
            }),
            max_call_depth: MAX_DEPTH,
            default_timeout_ms: knobs.default_timeout_ms,
        },
    );
    registry.register("wait", Wait);
    registry.register("fail", Fail);
    registry.register("trip", Trip(knobs.trip));
    let config = EngineConfig::default()
        .error_context(crate::engine::ERROR_CONTEXT.clone())
        .observer(runs.clone());
    let mut builder = dataflow_rs::Engine::builder()
        .registry(registry)
        .config(config);
    for workflow in &workflows {
        builder = builder.workflow_json(workflow);
    }
    let v4 = builder.build().expect("the v4 engine builds");

    if knobs.engine {
        runtime.publish(
            v4.clone(),
            channels,
            crate::engine::FunctionRegistry::builtin().clone(),
            Arc::new(crate::plugin::PluginSet::empty()),
        );
    }
    World {
        runtime,
        v4,
        runs,
        default_timeout_ms: knobs.default_timeout_ms,
    }
}

/// The record a task `task` that failed with `failure` carries.
fn record_of(task: Option<&str>, failure: &Failure) -> String {
    format!("Task {} error: {}", task.unwrap_or("?"), failure.message)
}

impl World {
    /// `channel` run over `payload` and `metadata`, as `execute_admitted`
    /// builds the message, under `options` (the channel set here): the run and the message.
    async fn v4_run(
        &self,
        channel: &str,
        payload: Value,
        metadata: Value,
        options: RunOptions<'_>,
    ) -> (dataflow_rs::Run, Message) {
        let mut message = Message::from_parts(
            OwnedDataValue::from(&payload),
            OwnedDataValue::Null,
            OwnedDataValue::from(&metadata),
        );
        let options = options
            .channel(channel)
            .capture_changes(false)
            .audit(AuditMode::Off);
        let run = self.v4.run_with(&mut message, &options).await;
        (run, message)
    }

    /// [`Self::v4_run`] with no limits, as [`Seen`]: `data`, or the
    /// failure that stopped the run, its record checked.
    async fn v4(&self, channel: &str, payload: Value, metadata: Value) -> Seen {
        let (run, message) = self
            .v4_run(channel, payload, metadata, RunOptions::default())
            .await;
        seen(&run, &message)
    }
}

/// A run as [`Seen`]: `data` when every workflow finished, error records
/// or not, as 1.12 read a run that returned.
fn seen(run: &dataflow_rs::Run, message: &Message) -> Seen {
    match run.status() {
        RunStatus::Completed | RunStatus::CompletedWithErrors => {
            Ok(Value::from(message.context().data()))
        }
        RunStatus::Stopped(e) => {
            let failure = twin::failure(e);
            let record = message.errors().last().expect("the failure is recorded");
            assert_eq!(
                record.message,
                record_of(record.task_id.as_deref(), &failure)
            );
            assert_eq!(record.code, failure.code);
            Err(failure)
        }
        other => unreachable!("the run ended {other:?}"),
    }
}

/// The run in a world of its own, so a guard's state (a rate limit's
/// tokens) is not shared with any other: what it saw.
async fn answer(chans: &[Chan], channel: &str, payload: Value, metadata: Value) -> Seen {
    world(chans).await.v4(channel, payload, metadata).await
}

/// A target that greets the payload's `name`, as `channel_call_test`'s
/// basic target does.
fn greeter() -> Chan {
    chan(
        "target",
        json!([
            {"id": "t0", "name": "t0", "function": {"name": "parse_json",
                "input": {"source": "payload", "target": "input"}}},
            {"id": "t1", "name": "t1", "function": {"name": "map", "input": {"mappings": [
                {"path": "data.greeting",
                 "logic": {"cat": ["Hello, ", {"var": "data.input.name"}]}}
            ]}}}
        ]),
    )
}

// -- The config --

/// The named load refusals (O11a's CHANGELOG): `channel` missing
/// (I183), and an unknown key (break 2a), which 1.12's `deny_unknown:
/// false` accepted and ignored. 1.12's aliases are unknown keys: no
/// alias is read.
#[test]
fn a_missing_channel_and_an_unknown_key_are_refused_at_load() {
    let rejected = |input: Value| -> Vec<(String, IssueCode)> {
        testing::config_with::<ChannelCallConfig>(&kit::test_exprs(), input)
            .expect_err("refused")
            .into_iter()
            .filter(|i| i.severity == Severity::Rejected)
            .map(|i| (i.path.to_string(), i.code))
            .collect()
    };
    assert_eq!(
        rejected(json!({})),
        [("channel".to_string(), IssueCode::InputParse)]
    );
    let unknown = rejected(json!({"channel": "orders", "chanel_timeout": 5}));
    assert_eq!(unknown.len(), 1, "{unknown:?}");
    assert_eq!(unknown[0].0, "chanel_timeout");
    let alias = rejected(json!({"channel_logic": "orders"}));
    assert!(
        alias.iter().any(|(path, _)| path == "channel_logic")
            && alias.iter().any(|(path, _)| path == "channel"),
        "{alias:?}"
    );
}

/// A literal `output` that names no path is refused at load: one that
/// is not a string, which 1.12 refused at authoring (`TYPE_MISMATCH`),
/// and a string naming no context root, `""` included (O3's ruled class),
/// which 1.12 wrote to a key no response shows. Named breaks for O11a.
#[test]
fn a_literal_output_naming_no_path_is_refused_at_load() {
    let authoring = crate::engine::FunctionRegistry::builtin().validate_input(
        NAME,
        &json!({"channel": "a", "output": 7}),
        "tasks[0]",
    );
    assert!(
        authoring.iter().any(|e| e.code == "TYPE_MISMATCH"),
        "{authoring:?}"
    );
    for (output, code) in [
        (json!(7), IssueCode::InputParse),
        (json!(""), IssueCode::InvalidPath),
        (json!("result"), IssueCode::InvalidPath),
    ] {
        let issues = testing::config_with::<ChannelCallConfig>(
            &kit::test_exprs(),
            json!({"channel": "a", "output": output}),
        )
        .expect_err("refused at load");
        assert_eq!(
            issues
                .iter()
                .map(|i| (i.path.to_string(), i.code))
                .collect::<Vec<_>>(),
            [("output".to_string(), code)],
            "{output}"
        );
    }
}

// -- The call --

/// The basic call, `channel_call_test`'s first: the child runs on the
/// generation's engine, on the target channel, and what it left in
/// `data` is written at `output`, as 1.12 wrote it.
#[tokio::test]
async fn the_child_runs_on_the_generations_engine_and_its_data_is_written_at_output() {
    let chans = [
        greeter(),
        chan(
            "caller",
            call(json!({"channel": "target", "output": "data.result"})),
        ),
    ];
    let v4 = answer(&chans, "caller", json!({"name": "World"}), json!({})).await;
    twin::pinned(&v4, "basic");
    assert_eq!(
        v4.expect("the call answers")["result"]["greeting"],
        "Hello, World"
    );

    let world = world(&chans).await;
    world
        .v4("caller", json!({"name": "World"}), json!({}))
        .await
        .expect("answers");
    // Both runs reported to the engine's observer: the child ran on the
    // generation's engine.
    assert!(world.runtime.load().id > 0);
    assert_eq!(world.runs.channels(), ["caller", "target"]);
}

/// Without an engine there is no workflow to run the target: the call
/// is refused as 1.12 refused a target nothing serves, and nothing runs.
#[tokio::test]
async fn a_generation_without_an_engine_runs_no_child() {
    let chans = [
        greeter(),
        chan("caller", call(json!({"channel": "target"}))),
    ];
    let world = world_with(
        &chans,
        Knobs {
            engine: false,
            ..Knobs::default()
        },
    )
    .await;
    let failure = world
        .v4("caller", json!({}), json!({}))
        .await
        .expect_err("refused");
    let expected: dataflow_rs::Error = not_found("target").into();
    assert_eq!(failure.message, expected.to_string());
    assert_eq!(failure.code, "FUNCTION_ERROR");
    assert_eq!(world.runs.channels(), ["caller"], "no child ran");
}

/// `data` is what is sent, and without it the caller's payload is
/// forwarded; the output is the default `data`, or computed; every field
/// read from the message, as 1.12 read it.
#[tokio::test]
async fn the_fields_are_read_as_1_12_read_them() {
    let echo = chan(
        "echo",
        json!([{"id": "p", "name": "p", "function": {"name": "parse_json",
                "input": {"source": "payload", "target": "got"}}}]),
    );
    for input in [
        json!({"channel": "echo"}),
        json!({"channel": "echo", "output": "data.r"}),
        json!({"channel": "echo", "data": {"var": "data.parsed.inner"}, "output": "data.r"}),
        json!({"channel": "echo", "data": {"cat": ["a", "b"]}, "output": "temp_data.r"}),
        json!({"channel": {"cat": ["ec", "ho"]}, "output": {"cat": ["data.", "x"]}}),
        json!({"channel": "echo", "output": "data.r", "timeout_ms": 5000}),
        json!({"channel": "echo", "output": "data.r", "timeout_ms": {"var": "data.parsed.ms"}}),
        json!({"channel": "echo", "output": "data.r", "timeout_ms": 2.5}),
    ] {
        let caller = chan(
            "caller",
            json!([
                {"id": "p", "name": "p", "function": {"name": "parse_json",
                    "input": {"source": "payload", "target": "parsed"}}},
                {"id": "t", "name": "t", "function": {"name": NAME, "input": input.clone()}}
            ]),
        );
        let chans = [echo.clone(), caller];
        let payload = json!({"inner": {"k": 1}, "ms": 5000});
        let v4 = answer(&chans, "caller", payload, json!({})).await;
        twin::pinned(&v4, format!("fields/{input}"));
        assert!(v4.is_ok(), "{input}: {v4:?}");
    }
}

/// The call position as 1.12 advanced it: the child's metadata holds the
/// depth plus one, the chain with the target on the end, `channel`
/// naming the target, and the rest of the caller's metadata, its
/// headers included.
#[tokio::test]
async fn the_child_metadata_carries_the_call_position_as_1_12s_did() {
    let probe = chan(
        "probe",
        json!([{"id": "m", "name": "m", "function": {"name": "map", "input": {"mappings": [
            {"path": "data.depth", "logic": {"var": "metadata._orion_call_depth"}},
            {"path": "data.chain", "logic": {"var": "metadata._orion_call_chain"}},
            {"path": "data.channel", "logic": {"var": "metadata.channel"}},
            {"path": "data.header", "logic": {"var": "metadata.headers.x-trace"}},
            {"path": "data.vars", "logic": {"var": "metadata.vars"}}
        ]}}}]),
    );
    let chans = [
        probe,
        chan(
            "caller",
            call(json!({"channel": "probe", "output": "data.seen"})),
        ),
    ];
    for metadata in [
        json!({}),
        json!({"headers": {"x-trace": "abc"}, "vars": {"region": "eu"}}),
        json!({"_orion_call_depth": 2, "_orion_call_chain": ["edge", "mid"]}),
        json!({"_orion_call_depth": "two", "_orion_call_chain": "edge"}),
    ] {
        let v4 = answer(&chans, "caller", json!({}), metadata.clone()).await;
        twin::pinned(&v4, format!("metadata/{metadata}"));
        assert!(v4.is_ok(), "{metadata}");
    }
    let v4 = answer(
        &chans,
        "caller",
        json!({}),
        json!({"_orion_call_depth": 2, "_orion_call_chain": ["edge"]}),
    )
    .await;
    assert_eq!(
        v4.expect("answers")["seen"],
        json!({"depth": 3, "chain": ["edge", "probe"], "channel": "probe"})
    );
}

/// The recursion guards refuse at the call they fire in, in 1.12's
/// words but as a loop: a self-call, a chain that comes back, and a
/// depth past the maximum, each on the first hop (an injected position,
/// as `channel_call_test` injects one).
#[tokio::test]
async fn the_recursion_guards_refuse_as_a_loop() {
    let chans = [
        chan("self", call(json!({"channel": "self"}))),
        chan("caller", call(json!({"channel": "target"}))),
        greeter(),
    ];
    for (channel, metadata) in [
        ("caller", json!({"_orion_call_chain": ["edge", "target"]})),
        ("caller", json!({"_orion_call_depth": MAX_DEPTH})),
        (
            "caller",
            json!({"_orion_call_depth": MAX_DEPTH + 5, "_orion_call_chain": ["a"]}),
        ),
    ] {
        let v4 = answer(&chans, channel, json!({}), metadata.clone()).await;
        twin::pinned(&v4, format!("recursion/{channel}/{metadata}"));
        let refused = v4.expect_err("refused");
        assert_eq!(refused.code, crate::errors::kind::CHANNEL_LOOP);
        assert!(!refused.retryable);
    }
}

/// A refusal one hop down is a stopped child, so it is passed on with
/// its own code and retryability (I184): a self-call is refused in the
/// child as a loop, which the caller records under 1.12's prefix, where
/// 1.12 rewrapped it as `FUNCTION_ERROR`.
#[tokio::test]
async fn a_cycle_found_in_the_child_is_passed_on_with_its_code() {
    let chans = [chan("self", call(json!({"channel": "self"})))];
    let v4 = answer(&chans, "self", json!({}), json!({}))
        .await
        .expect_err("refused");
    // The child's refusal, as the child's run stopped with it: the
    // same call at the child's position.
    let inner = world(&chans)
        .await
        .v4(
            "self",
            json!({}),
            json!({"_orion_call_depth": 1, "_orion_call_chain": ["self"]}),
        )
        .await
        .expect_err("the child refuses the cycle");
    assert_eq!(inner.code, crate::errors::kind::CHANNEL_LOOP);
    assert_eq!(
        v4,
        Failure {
            code: inner.code.clone(),
            message: format!("channel_call to 'self' failed: {}", inner.message),
            retryable: inner.retryable,
            detail: inner.detail.clone(),
        }
    );
}

/// A target the registry cannot serve is refused as 1.12 refused it,
/// before the target is guarded or run: one not found (not a channel,
/// or not active), one quarantined, and a cron channel.
#[tokio::test]
async fn a_target_the_estate_cannot_serve_is_refused_as_1_12_refused_it() {
    let chans = [
        chan(
            "caller",
            json!([
                {"id": "p", "name": "p", "function": {"name": "parse_json",
                    "input": {"source": "payload", "target": "to"}}},
                {"id": "t", "name": "t", "function": {"name": NAME,
                    "input": {"channel": {"var": "data.to"}}}}
            ]),
        ),
        greeter().cron().named("nightly"),
        greeter().named("broken"),
    ];
    let knobs = || Knobs {
        quarantined: vec!["broken"],
        ..Knobs::default()
    };
    for to in ["nowhere", "nightly", "broken"] {
        let world = world_with(&chans, knobs()).await;
        let v4 = world.v4("caller", json!(to), json!({})).await;
        twin::pinned(&v4, format!("unserviceable/{to}"));
        assert_eq!(v4.expect_err("refused").code, "FUNCTION_ERROR", "{to}");
        assert_eq!(world.runs.channels(), ["caller"], "{to}: nothing ran");
    }
}

/// Zero matches are read from the run. A target whose workflows were
/// considered and all declined answers as 1.12's did, with the child's
/// empty `data`; a target the engine has no workflow for at all is
/// refused as not found, where 1.12, which read zero matches from the
/// registry, would have written that empty `data` (a generation built
/// from one set of rows has no such target).
#[tokio::test]
async fn zero_matches_are_read_from_the_run() {
    let declines = greeter().condition(json!(false));
    let chans = [
        declines,
        chan(
            "caller",
            call(json!({"channel": "target", "output": "data.r"})),
        ),
    ];
    let v4 = answer(&chans, "caller", json!({"name": "x"}), json!({})).await;
    twin::pinned(&v4, "zero_matches/declined");
    assert_eq!(v4.expect("answers"), json!({"r": {}}));

    let chans = [
        greeter().unserved(),
        chan(
            "caller",
            call(json!({"channel": "target", "output": "data.r"})),
        ),
    ];
    let world = world(&chans).await;
    let failure = world
        .v4("caller", json!({}), json!({}))
        .await
        .expect_err("refused");
    let expected: dataflow_rs::Error = not_found("target").into();
    assert_eq!(failure.code, "FUNCTION_ERROR");
    assert_eq!(failure.message, expected.to_string());
    assert_eq!(world.runs.channels(), ["caller", "target"]);
}

/// The target's guards admit or refuse as 1.12's did: its
/// `validation_logic` sees the call's data and the child's metadata,
/// a rejection is the caller's `VALIDATION_ERROR`, and a rate limit's
/// refusal keeps its status (`CHANNEL_RATE_LIMITED`, retryable).
#[tokio::test]
async fn the_targets_guards_admit_and_refuse_as_1_12s_did() {
    let guarded = greeter().named("guarded").config(json!({
        "validation_logic": {"and": [
            {"==": [{"var": "data.name"}, "Ada"]},
            {"==": [{"var": "metadata.channel"}, "guarded"]},
            {"==": [{"var": "metadata._orion_call_depth"}, 1]}
        ]}
    }));
    let limited = greeter()
        .named("limited")
        .config(json!({"rate_limit": {"requests_per_second": 1, "burst": 1}}));
    let caller = chan(
        "caller",
        json!([
            {"id": "a", "name": "a", "function": {"name": NAME,
                "input": {"channel": {"var": "data.in.to"}, "data": {"var": "data.in.send"},
                          "output": "data.first"}}},
            {"id": "b", "name": "b", "function": {"name": NAME,
                "input": {"channel": {"var": "data.in.to"}, "data": {"var": "data.in.send"},
                          "output": "data.second"}}}
        ]),
    );
    let caller = Chan {
        tasks: caller.tasks.map(|tasks| {
            let mut all = vec![
                json!({"id": "p", "name": "p", "function": {"name": "parse_json",
                    "input": {"source": "payload", "target": "in"}}}),
            ];
            all.extend(tasks.as_array().cloned().unwrap_or_default());
            Value::Array(all)
        }),
        ..caller
    };
    let chans = [guarded, limited, caller];
    for (payload, code) in [
        (json!({"to": "guarded", "send": {"name": "Ada"}}), None),
        (
            json!({"to": "guarded", "send": {"name": "Bob"}}),
            Some("VALIDATION_ERROR"),
        ),
        (
            json!({"to": "limited", "send": {"name": "Ada"}}),
            Some(crate::errors::kind::CHANNEL_RATE_LIMITED),
        ),
    ] {
        let v4 = answer(&chans, "caller", payload.clone(), json!({})).await;
        twin::pinned(&v4, format!("guards/{payload}"));
        match code {
            None => {
                let data = v4.expect("admitted");
                assert_eq!(data["first"]["greeting"], "Hello, Ada");
                assert_eq!(data["second"]["greeting"], "Hello, Ada");
            }
            Some(code) => assert_eq!(v4.expect_err("refused").code, code, "{payload}"),
        }
    }
    let v4 = answer(
        &chans,
        "caller",
        json!({"to": "limited", "send": {"name": "Ada"}}),
        json!({}),
    )
    .await;
    assert!(
        v4.expect_err("the second call is over the limit").retryable,
        "a rate limit's refusal is retryable"
    );
}

/// What the message makes of `channel`, `data` and `timeout_ms` fails
/// as 1.12 failed it: a name that is not a string, an empty one, an
/// expression that throws, and a timeout that is not a number.
#[tokio::test]
async fn computed_fields_fail_as_1_12s_did() {
    for input in [
        json!({"channel": {"var": "data.in.n"}}),
        json!({"channel": {"var": "data.in.empty"}}),
        json!({"channel": {"var": "data.in.missing"}}),
        json!({"channel": {"throw": "boom"}}),
        json!({"channel": "target", "data": {"throw": "boom"}}),
        json!({"channel": "target", "timeout_ms": "soon"}),
        json!({"channel": "target", "timeout_ms": -1}),
        json!({"channel": "target", "timeout_ms": {"var": "data.in.empty"}}),
    ] {
        let caller = chan(
            "caller",
            json!([
                {"id": "p", "name": "p", "function": {"name": "parse_json",
                    "input": {"source": "payload", "target": "in"}}},
                {"id": "t", "name": "t", "function": {"name": NAME, "input": input.clone()}}
            ]),
        );
        let chans = [greeter(), caller];
        let v4 = answer(&chans, "caller", json!({"n": 7, "empty": ""}), json!({})).await;
        twin::pinned(&v4, format!("computed_fields/{input}"));
        assert!(v4.is_err(), "{input}");
    }
}

/// A computed `output` is resolved before the target is guarded or run
/// (O3's ruling): one naming no path is refused and the target never
/// runs. 1.12 ran the target and wrote at a key no response shows.
#[tokio::test]
async fn a_computed_output_naming_no_path_runs_nothing() {
    for output in [
        json!({"var": "data.in.n"}),
        json!({"var": "data.in.nowhere"}),
        json!({"var": "data.in.where"}),
    ] {
        let caller = chan(
            "caller",
            json!([
                {"id": "p", "name": "p", "function": {"name": "parse_json",
                    "input": {"source": "payload", "target": "in"}}},
                {"id": "t", "name": "t", "function": {"name": NAME,
                    "input": {"channel": "target", "output": output.clone()}}}
            ]),
        );
        let world = world(&[greeter(), caller]).await;
        let failure = world
            .v4("caller", json!({"n": 7, "where": "result"}), json!({}))
            .await
            .expect_err("refused");
        assert_eq!(failure.code, "VALIDATION_ERROR", "{output}");
        twin::pinned(&Err(failure), format!("computed_output/{output}"));
        assert_eq!(
            world.runs.channels(),
            ["caller"],
            "{output}: the target never ran"
        );
    }
}

/// A target that completes with error records fails the call as 1.12's
/// `WorkflowErrors` did: `FUNCTION_ERROR`, the records summarised.
#[tokio::test]
async fn a_target_that_completes_with_errors_fails_the_call_as_1_12s_did() {
    let failing = chan(
        "failing",
        json!([{"id": "boom", "name": "boom", "continue_on_error": true,
                    "function": {"name": "fail", "input": {}}}]),
    );
    let chans = [failing, chan("caller", call(json!({"channel": "failing"})))];
    let v4 = answer(&chans, "caller", json!({}), json!({})).await;
    twin::pinned(&v4, "completes_with_errors");
    assert_eq!(v4.expect_err("failed").code, "FUNCTION_ERROR");
}

/// I184's named change: a target whose run stopped passes its error on,
/// with its code and its retryability, under 1.12's "failed" prefix.
/// 1.12 rewrapped it as a `FUNCTION_ERROR`, not retryable.
#[tokio::test]
async fn a_stopped_child_passes_its_error_on_with_its_code_and_retryability() {
    let failing = chan(
        "failing",
        json!([{"id": "boom", "name": "boom", "function": {"name": "fail", "input": {}}}]),
    );
    let chans = [failing, chan("caller", call(json!({"channel": "failing"})))];
    let world = world(&chans).await;
    let inner = world
        .v4("failing", json!({}), json!({}))
        .await
        .expect_err("the target stops");
    assert_eq!((inner.code.as_str(), inner.retryable), ("IO_ERROR", true));

    let v4 = answer(&chans, "caller", json!({}), json!({}))
        .await
        .expect_err("failed");
    assert_eq!(
        v4,
        Failure {
            code: inner.code.clone(),
            message: format!("channel_call to 'failing' failed: {}", inner.message),
            retryable: inner.retryable,
            detail: inner.detail.clone(),
        }
    );
}

/// Break 20: the child's run resets its own error context, so a child
/// sees none of its caller's failure records, as 1.12's
/// `clear_error_context` made sure; the caller's records stay its own.
#[tokio::test]
async fn the_childs_run_resets_its_own_error_context() {
    let probe = chan(
        "probe",
        json!([{"id": "m", "name": "m", "function": {"name": "map", "input": {"mappings": [
            {"path": "data.inherited",
             "logic": {"if": [{"var": "metadata._orion_errors"}, "inherited", "none"]}}
        ]}}}]),
    );
    let caller = chan(
        "caller",
        json!([
            {"id": "boom", "name": "boom", "continue_on_error": true,
             "function": {"name": "fail", "input": {}}},
            {"id": "t", "name": "t", "function": {"name": NAME,
                "input": {"channel": "probe", "output": "temp_data.seen"}}},
            {"id": "m", "name": "m", "function": {"name": "map", "input": {"mappings": [
                {"path": "data.seen", "logic": {"var": "temp_data.seen"}},
                {"path": "data.own", "logic": {"var": "metadata._orion_errors.0.task_id"}}
            ]}}}
        ]),
    );
    let world = world(&[probe, caller]).await;
    let (run, message) = world
        .v4_run(
            "caller",
            json!({}),
            json!({"_orion_errors": [{"task_id": "forged"}]}),
            RunOptions::default(),
        )
        .await;
    assert!(
        matches!(run.status(), RunStatus::CompletedWithErrors),
        "{run:?}"
    );
    assert_eq!(
        Value::from(message.context().data()),
        json!({"seen": {"inherited": "none"}, "own": "boom"})
    );
}

/// The target's guards see none of the caller's error records, as 1.12
/// cleared them before the guards (#280): a caller with a recorded
/// failure under `continue` calls a target whose `validation_logic`
/// rejects any message carrying one, and is admitted.
#[tokio::test]
async fn the_targets_guards_see_none_of_the_callers_error_records() {
    let strict = greeter().named("strict").config(json!({
        "validation_logic": {"!": [{"var": "metadata._orion_errors"}]}
    }));
    let caller = chan(
        "caller",
        json!([
            {"id": "boom", "name": "boom", "continue_on_error": true,
             "function": {"name": "fail", "input": {}}},
            {"id": "seen", "name": "seen", "function": {"name": "map", "input": {"mappings": [
                {"path": "data.own", "logic": {"var": "metadata._orion_errors.0.task_id"}}
            ]}}},
            {"id": "t", "name": "t", "function": {"name": NAME,
                "input": {"channel": "strict", "output": "data.r"}}}
        ]),
    );
    let chans = [strict, caller];
    let v4 = answer(&chans, "caller", json!({"name": "Ada"}), json!({})).await;
    twin::pinned(&v4, "guards_see_no_records");
    let data = v4.expect("admitted");
    assert_eq!(data["own"], "boom", "the caller holds its own record");
    assert_eq!(data["r"]["greeting"], "Hello, Ada");
}

/// O25: the payload, the metadata and the result cross as values, with
/// no JSON between them, so a datetime in the caller's payload or
/// metadata reaches the target, and comes back, as a datetime. 1.12's
/// JSON bridge turned each into a string.
#[tokio::test]
async fn values_cross_the_call_with_no_json_round_trip() {
    let datetime = || {
        OwnedDataValue::DateTime(
            dataflow_rs::datavalue::DataDateTime::parse("2024-01-15T12:30:45Z")
                .expect("a datetime"),
        )
    };
    let target = chan(
        "target",
        json!([
            {"id": "p", "name": "p", "function": {"name": "parse_json",
                "input": {"source": "payload", "target": "sent"}}},
            {"id": "m", "name": "m", "function": {"name": "map", "input": {"mappings": [
                {"path": "data.meta", "logic": {"var": "metadata.t"}}
            ]}}}
        ]),
    );
    let world = world(&[
        target,
        chan(
            "caller",
            call(json!({"channel": "target", "output": "data.r"})),
        ),
    ])
    .await;
    let payload = datetime();
    let metadata = OwnedDataValue::Object(vec![("t".to_string(), datetime())]);
    let mut message = Message::from_parts(payload, OwnedDataValue::Null, metadata);
    let run = world
        .v4
        .run_with(&mut message, &RunOptions::default().channel("caller"))
        .await;
    assert!(matches!(run.status(), RunStatus::Completed), "{run:?}");
    let r = message
        .context()
        .get(&"data.r".parse().expect("a path"))
        .expect("the result is written");
    assert_eq!(r.get("sent"), Some(&datetime()));
    assert_eq!(r.get("meta"), Some(&datetime()));
    assert!(matches!(r.get("sent"), Some(OwnedDataValue::DateTime(_))));
}

// -- The run's limits, and the call's own --

/// A world whose caller calls `target`, which waits, with the call's
/// own timeout `timeout_ms` when set.
async fn waiting(timeout_ms: Option<u64>, knobs: Knobs) -> World {
    let mut input = json!({"channel": "target"});
    if let Some(ms) = timeout_ms {
        input["timeout_ms"] = json!(ms);
    }
    let target = chan(
        "target",
        json!([{"id": "w", "name": "w", "function": {"name": "wait", "input": {}}}]),
    );
    world_with(&[target, chan("caller", call(input))], knobs).await
}

/// The run's deadline coming first, or with the call's own (a tie is
/// the run's): the child reports `TimedOut` with its run, and the
/// parent run ends `TimedOut` too, its task recording the run's
/// deadline, not a failure of the target.
#[tokio::test(start_paused = true)]
async fn the_runs_deadline_during_the_child_ends_the_run_timed_out() {
    for (own, run) in [(10_000, 100), (100, 100), (30_000, 250)] {
        let world = waiting(Some(own), Knobs::default()).await;
        let deadline = tokio::time::Instant::now().into_std() + Duration::from_millis(run);
        let (report, message) = world
            .v4_run(
                "caller",
                json!({}),
                json!({}),
                RunOptions::default().deadline(deadline),
            )
            .await;
        assert!(
            matches!(report.status(), RunStatus::TimedOut),
            "{own}/{run}: {report:?}"
        );
        assert_eq!(
            world.runs.finished(),
            [
                ("target".to_string(), "timed_out".to_string()),
                ("caller".to_string(), "timed_out".to_string()),
            ],
            "{own}/{run}"
        );
        let record = message.errors().last().expect("the call is recorded");
        assert_eq!(record.code, "TIMEOUT_ERROR");
        assert_eq!(
            record.message,
            "Task t error: Timeout error: the run's deadline passed during `channel_call`"
        );
        assert!(
            message
                .context()
                .data()
                .as_object()
                .is_some_and(<[_]>::is_empty)
        );
    }
}

/// The run's token tripped while the child runs trips the child's
/// (a child token): the child reports `Cancelled`, and the parent run
/// ends `Cancelled` too.
#[tokio::test(start_paused = true)]
async fn the_runs_token_tripped_during_the_child_ends_the_run_cancelled() {
    let token = CancelToken::new();
    let target = chan(
        "target",
        json!([{"id": "trip", "name": "trip", "function": {"name": "trip", "input": {}}}]),
    );
    let world = world_with(
        &[target, chan("caller", call(json!({"channel": "target"})))],
        Knobs {
            trip: token.clone(),
            ..Knobs::default()
        },
    )
    .await;
    let far = tokio::time::Instant::now().into_std() + Duration::from_secs(60);
    let (report, message) = world
        .v4_run(
            "caller",
            json!({}),
            json!({}),
            RunOptions::default().cancel(&token).deadline(far),
        )
        .await;
    assert!(
        matches!(report.status(), RunStatus::Cancelled),
        "{report:?}"
    );
    assert_eq!(
        world.runs.finished(),
        [
            ("target".to_string(), "cancelled".to_string()),
            ("caller".to_string(), "cancelled".to_string()),
        ]
    );
    let record = message.errors().last().expect("the call is recorded");
    assert_eq!(record.code, "CANCELLED");
    assert_eq!(
        record.message,
        "Task t error: the run was cancelled during `channel_call`"
    );
}

/// A child that is handed no token of its own when the run has none,
/// and the run's token, untripped, leaves the call alone.
#[tokio::test(start_paused = true)]
async fn an_untripped_token_and_a_far_deadline_change_nothing() {
    let chans = [
        greeter(),
        chan(
            "caller",
            call(json!({"channel": "target", "output": "data.r"})),
        ),
    ];
    let world = world(&chans).await;
    let token = CancelToken::new();
    let far = tokio::time::Instant::now().into_std() + Duration::from_secs(60);
    let (report, message) = world
        .v4_run(
            "caller",
            json!({"name": "Ada"}),
            json!({}),
            RunOptions::default().cancel(&token).deadline(far),
        )
        .await;
    assert_eq!(
        seen(&report, &message).expect("answers")["r"]["greeting"],
        "Hello, Ada"
    );
}

/// The call's own bound coming first (the task's `timeout_ms`, else
/// the target's, else the engine default), strictly before the run's
/// deadline or with none: the child reports `TimedOut`, and the call
/// fails with 1.12's `TIMEOUT_ERROR`, retryable, in 1.12's words, which
/// stops the parent run as any failure of its task does.
#[tokio::test(start_paused = true)]
async fn the_calls_own_timeout_first_fails_the_call_with_1_12s_timeout() {
    let cases: [(Option<u64>, Value, u64, Option<u64>); 4] = [
        (Some(100), json!({}), 30_000, None),
        (Some(100), json!({}), 30_000, Some(5_000)),
        (None, json!({"timeout_ms": 150}), 30_000, Some(5_000)),
        (None, json!({}), 200, None),
    ];
    for (task, config, default, run) in cases {
        let knobs = || Knobs {
            default_timeout_ms: default,
            ..Knobs::default()
        };
        let mut input = json!({"channel": "target"});
        if let Some(ms) = task {
            input["timeout_ms"] = json!(ms);
        }
        let target = chan(
            "target",
            json!([{"id": "w", "name": "w", "function": {"name": "wait", "input": {}}}]),
        )
        .config(config.clone());
        let chans = [target, chan("caller", call(input))];
        let world = world_with(&chans, knobs()).await;
        assert_eq!(world.default_timeout_ms, default);
        let mut options = RunOptions::default();
        if let Some(ms) = run {
            options = options
                .deadline(tokio::time::Instant::now().into_std() + Duration::from_millis(ms));
        }
        let (report, message) = world.v4_run("caller", json!({}), json!({}), options).await;
        let v4 = seen(&report, &message);
        twin::pinned(
            &v4,
            format!("own_timeout/{task:?}/{config}/{default}/{run:?}"),
        );
        let failure = v4.expect_err("the call timed out");
        assert_eq!(failure.code, "TIMEOUT_ERROR");
        assert!(failure.retryable);
        assert_eq!(
            world.runs.finished(),
            [
                ("target".to_string(), "timed_out".to_string()),
                ("caller".to_string(), "stopped TIMEOUT_ERROR".to_string()),
            ]
        );
    }
}

/// The run's token tripped from another task while the child runs: the
/// child, holding a child of that token, reports `Cancelled`, the call
/// passes its error on, and the engine ends the parent run `Cancelled`
/// on its own token, recording the token's text on the call's task.
#[tokio::test(start_paused = true)]
async fn the_runs_token_tripped_from_another_task_ends_the_run_cancelled() {
    let world = waiting(None, Knobs::default()).await;
    let token = CancelToken::new();
    let canceller = {
        let token = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            token.cancel();
        })
    };
    let (report, message) = world
        .v4_run(
            "caller",
            json!({}),
            json!({}),
            RunOptions::default().cancel(&token),
        )
        .await;
    canceller.await.expect("the canceller ran");
    assert!(
        matches!(report.status(), RunStatus::Cancelled),
        "{report:?}"
    );
    assert_eq!(
        world.runs.finished(),
        [
            ("target".to_string(), "cancelled".to_string()),
            ("caller".to_string(), "cancelled".to_string()),
        ]
    );
    let record = message.errors().last().expect("the call is recorded");
    assert_eq!(record.code, "CANCELLED");
    assert_eq!(
        record.message,
        "Task t error: the run was cancelled during `channel_call`"
    );
}

/// A chain of calls, `caller` to `mid` to `target`: the run's deadline
/// passing in the grandchild ends every run on the chain `TimedOut`,
/// innermost first, and the run's token tripped there ends every one
/// `Cancelled`: each call hands the limit down and passes its child's
/// error on, and the engine ends each run on its own limit, since the
/// error comes after it (dataflow-rs `daaf8c0`).
#[tokio::test(start_paused = true)]
async fn the_runs_limits_end_every_run_on_a_chain() {
    for trip in [false, true] {
        let token = CancelToken::new();
        let last = if trip { "trip" } else { "wait" };
        let chans = [
            chan(
                "target",
                json!([{"id": "w", "name": "w", "function": {"name": last, "input": {}}}]),
            ),
            chan("mid", call(json!({"channel": "target"}))),
            chan("caller", call(json!({"channel": "mid"}))),
        ];
        let world = world_with(
            &chans,
            Knobs {
                trip: token.clone(),
                ..Knobs::default()
            },
        )
        .await;
        let deadline = tokio::time::Instant::now().into_std() + Duration::from_millis(100);
        let (report, _) = world
            .v4_run(
                "caller",
                json!({}),
                json!({}),
                RunOptions::default().deadline(deadline).cancel(&token),
            )
            .await;
        let status = if trip { "cancelled" } else { "timed_out" };
        assert!(
            matches!(
                (report.status(), trip),
                (RunStatus::Cancelled, true) | (RunStatus::TimedOut, false)
            ),
            "{report:?}"
        );
        assert_eq!(
            world.runs.finished(),
            ["target", "mid", "caller"].map(|c| (c.to_string(), status.to_string())),
        );
    }
}

/// A call's own timeout in the middle of a chain is that call's
/// failure, not the run's: `mid`'s call to `target` times out on its
/// own 100 ms, well inside the run's deadline, so `mid` stops with 1.12's
/// `TIMEOUT_ERROR`, and `caller`'s call passes that on with its code
/// and retryability (I184), where 1.12 rewrapped it as a
/// `FUNCTION_ERROR`.
#[tokio::test(start_paused = true)]
async fn a_nested_calls_own_timeout_is_passed_on_with_its_code() {
    let chans = [
        chan(
            "target",
            json!([{"id": "w", "name": "w", "function": {"name": "wait", "input": {}}}]),
        ),
        chan("mid", call(json!({"channel": "target", "timeout_ms": 100}))),
        chan("caller", call(json!({"channel": "mid"}))),
    ];
    let world = world(&chans).await;
    let deadline = tokio::time::Instant::now().into_std() + Duration::from_secs(10);
    let (report, message) = world
        .v4_run(
            "caller",
            json!({}),
            json!({}),
            RunOptions::default().deadline(deadline),
        )
        .await;
    let v4 = seen(&report, &message).expect_err("the call failed");
    let inner: dataflow_rs::Error = timed_out("target", 100).into();
    assert_eq!(
        v4,
        Failure {
            code: "TIMEOUT_ERROR".to_string(),
            message: format!("channel_call to 'mid' failed: {inner}"),
            retryable: true,
            detail: None,
        }
    );
    assert_eq!(
        world.runs.finished(),
        [
            ("target".to_string(), "timed_out".to_string()),
            ("mid".to_string(), "stopped TIMEOUT_ERROR".to_string()),
            ("caller".to_string(), "stopped TIMEOUT_ERROR".to_string()),
        ]
    );
}

#[tokio::test]
async fn the_child_inherits_the_runs_accountant_without_resetting_budgets() {
    let chans = [
        greeter(),
        chan(
            "caller",
            call(json!({"channel": "target", "output": "data.result"})),
        ),
    ];
    let w = world(&chans).await;

    // 1. Unbounded SharedRunAccountant counts tasks and expression ops across
    //    both parent (`caller`: 1 task) and child (`target`: 2 tasks = 3 total).
    let acct = crate::engine::SharedRunAccountant::unbounded();
    let (run, msg) = w
        .v4_run(
            "caller",
            json!({"name": "World"}),
            json!({}),
            acct.apply(RunOptions::default()),
        )
        .await;
    assert_eq!(
        seen(&run, &msg).expect("succeeds")["result"]["greeting"],
        "Hello, World"
    );
    let snap = acct.snapshot();
    assert_eq!(
        snap.tasks_dispatched, 3,
        "parent (1 task) + child (2 tasks) share the same accountant"
    );
    assert!(
        snap.expr_ops_charged > 0,
        "child expression ops are reported to the shared accountant"
    );
    assert_eq!(snap.in_flight, 0);

    // 2. A root task ceiling of 2 allows the parent task (`channel_call`) and
    //    the child's first task (`t0`), then stops before the child's second
    //    task (`t1`) can run.
    let capped = crate::engine::SharedRunAccountant::new(crate::engine::RunBudgetLimits {
        max_tasks: Some(2),
        ..Default::default()
    });
    let (capped_run, capped_msg) = w
        .v4_run(
            "caller",
            json!({"name": "World"}),
            json!({}),
            capped.apply(RunOptions::default()),
        )
        .await;
    let err = seen(&capped_run, &capped_msg).expect_err("child task exceeds shared task ceiling");
    assert_eq!(err.code, "BUDGET_EXCEEDED");
    assert_eq!(capped.snapshot().tasks_dispatched, 3);
}
