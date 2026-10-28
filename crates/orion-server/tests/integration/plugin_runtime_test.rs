//! The plugin sandbox, exercised end to end through a real engine over the
//! fixture component in `tests/fixtures/plugins/`.
//!
//! One component, eleven functions, each doing what its name says: the
//! well-behaved ones prove the ABI round trip, the output contract and the
//! three ways a field reaches the guest (evaluated, folded, literal); the
//! rest prove that a guest which traps, spins, allocates without end or
//! returns the wrong shape cannot touch the task context and fails as the
//! category the design's error table names.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use orion::config::PluginsConfig;
use orion::engine::{FunctionEntry, FunctionRegistry, PluginBinding};
use orion::plugin::{
    ABI, Limits, LoadError, Manifest, PluginFunction, PluginFunctionHandler, WasmRuntime,
};

const COMPONENT: &[u8] = include_bytes!("../fixtures/plugins/fixture.wasm");
const MANIFEST: &str = include_str!("../fixtures/plugins/fixture.toml");

/// Every function the fixture exports, built under `config`.
struct Fixture {
    runtime: Arc<WasmRuntime>,
    handlers: HashMap<String, PluginFunction>,
    entries: Vec<FunctionEntry>,
}

fn fixture(config: &PluginsConfig) -> Fixture {
    let runtime = WasmRuntime::new(config).expect("engine builds");
    let loaded = runtime.load_blocking(COMPONENT).expect("fixture loads");
    let manifest = Manifest::parse(MANIFEST).expect("fixture manifest parses");
    let binding = PluginBinding {
        id: manifest.name.clone(),
        version: 1,
        digest: loaded.digest.clone(),
        abi: ABI.to_string(),
    };
    let entries = manifest.entries(&binding);
    let limits = Limits::effective(config, &manifest.name);
    // The loader's pairing: the manifest's functions in the order
    // `Manifest::entries` gives them, each compiled from its runtime fields.
    let handlers = entries
        .iter()
        .zip(&manifest.functions)
        .map(|(e, decl)| {
            let handler = PluginFunctionHandler::new(
                Arc::new(e.clone()),
                loaded.clone(),
                runtime.clone(),
                limits,
            );
            (
                e.name.clone(),
                PluginFunction::new(handler, decl.runtime_fields()),
            )
        })
        .collect();
    Fixture {
        runtime,
        handlers,
        entries,
    }
}

fn config() -> PluginsConfig {
    PluginsConfig {
        enabled: true,
        max_memory_bytes: 16 * 1024 * 1024,
        max_timeout_ms: 300,
        max_live_instances: 64,
        max_concurrency_per_function: 64,
        // High enough that the deadline, not fuel, is what stops a spinner
        // in these tests; the fuel category has a test of its own.
        fuel_backstop: 1_000_000_000_000,
        ..PluginsConfig::default()
    }
}

/// Run one task through a real dataflow engine with `function` registered
/// under its name; returns the message's final `data`, or the refusal or
/// first task error's text.
async fn run(function: &PluginFunction, input: Value, data: Value) -> Result<Value, String> {
    use dataflow_rs::datavalue::OwnedDataValue;
    let engine = plugin_engine(function, input)?;
    let data = OwnedDataValue::from(&if data.is_null() { json!({}) } else { data });
    let mut message = dataflow_rs::Message::from_parts(data.clone(), data, OwnedDataValue::Null);
    engine.run(&mut message).await;
    if let Some(err) = message.errors().first() {
        return Err(format!("{err:?}"));
    }
    Ok(Value::from(message.context().data()))
}

fn plugin_engine(function: &PluginFunction, input: Value) -> Result<dataflow_rs::Engine, String> {
    let name = function.name().to_string();
    let exprs = orion::engine::operators::orion_exprs(&orion::engine::ResolvedSecrets::empty(), 0)
        .map_err(|e| e.to_string())?;
    let mut registry = dataflow_rs::Registry::new(exprs);
    registry.register(&name, function.clone());
    dataflow_rs::Engine::builder()
        .registry(registry)
        .workflow_json(&json!({
            "id": "w",
            "tasks": [{"id": "t", "function": {"name": name, "input": input}}]
        }))
        .build()
        .map_err(|e| format!("{e:?}"))
}

/// Poll the call itself until it enters Wasm and yields. No spawned task or
/// sleep is needed to establish that cancellation happens during invocation.
async fn poll_until_live<F: std::future::Future>(
    mut call: std::pin::Pin<&mut F>,
    runtime: &WasmRuntime,
    expected: u64,
) {
    tokio::time::timeout(
        Duration::from_secs(5),
        std::future::poll_fn(|cx| {
            assert!(
                call.as_mut().poll(cx).is_pending(),
                "the guest keeps spinning"
            );
            if runtime.live_instances() == expected {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        }),
    )
    .await
    .expect("the invocation enters Wasm");
}

/// A ticker for the tests that need the epoch clock, at the runtime's tick.
fn tick(runtime: Arc<WasmRuntime>) -> tokio::task::AbortHandle {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(orion::plugin::EPOCH_TICK).await;
            runtime.increment_epoch();
        }
    })
    .abort_handle()
}

#[test]
fn the_world_imports_nothing() {
    // The security model is a property of the WIT, asserted on the file
    // rather than on a component: a world with an `import` is a different
    // ABI, whatever any component does with it.
    let wit = include_str!("../../../orion-plugin-host/wit/orion-plugin.wit");
    let world = wit.split("world plugin").nth(1).expect("world");
    assert!(
        !world.contains("import"),
        "the plugin world must import nothing:\n{world}"
    );
    assert!(world.contains("export functions;"));
}

#[test]
fn a_component_loads_once_per_digest_and_a_non_component_does_not() {
    let runtime = WasmRuntime::new(&config()).expect("engine");
    let a = runtime.load_blocking(COMPONENT).expect("loads");
    assert!(a.digest.starts_with("sha256:"));
    assert_eq!(a.size, COMPONENT.len());
    let b = runtime.load_blocking(COMPONENT).expect("loads again");
    assert!(
        Arc::ptr_eq(&a, &b),
        "the second load of one digest is the cache"
    );
    assert!(runtime.cached(&a.digest).is_some());

    match runtime.load_blocking(b"not a component") {
        Err(LoadError::Compile(_)) => {}
        other => panic!("garbage must fail to compile: {other:?}"),
    }
    let small = PluginsConfig {
        max_component_bytes: 16,
        ..config()
    };
    match WasmRuntime::new(&small)
        .expect("engine")
        .load_blocking(COMPONENT)
    {
        Err(LoadError::TooLarge { size, max: 16 }) => assert_eq!(size, COMPONENT.len()),
        other => panic!("an oversized component must be refused: {other:?}"),
    }
}

#[test]
fn retain_keeps_named_digests_and_a_slack_of_recent_ones() {
    use std::collections::HashSet;

    let runtime = WasmRuntime::new(&config()).expect("engine");
    let loaded = runtime.load_blocking(COMPONENT).expect("loads");
    let digest = loaded.digest.clone();
    assert!(runtime.cached(&digest).is_some());

    // A digest a live generation names is kept, whatever the slack.
    let keep: HashSet<&str> = [digest.as_str()].into_iter().collect();
    assert_eq!(runtime.retain(&keep, 0), 0);
    assert!(
        runtime.cached(&digest).is_some(),
        "a named digest survives a retain that names it"
    );

    // Not named, but within the slack (the upload-then-activate window): spared.
    assert_eq!(runtime.retain(&HashSet::new(), 8), 0);
    assert!(
        runtime.cached(&digest).is_some(),
        "the slack spares a recently compiled component no generation names"
    );

    // Not named and past the slack: evicted. This is the leak the sweep closes
    // — a draft, self-test failure, superseded version or one-off `validate`
    // that no active generation carries.
    assert_eq!(runtime.retain(&HashSet::new(), 0), 1);
    assert!(
        runtime.cached(&digest).is_none(),
        "an unnamed component past the slack is dropped"
    );
}

#[tokio::test]
async fn the_fixture_manifest_registers_beside_the_builtins() {
    let f = fixture(&config());
    let registry = FunctionRegistry::builtin()
        .with_entries(f.entries.clone())
        .expect("extends");
    assert!(registry.contains("test.fixture.identity"));
    assert!(registry.contains("map") && registry.contains("crypto"));
    let entry = registry.get("test.fixture.identity").expect("entry");
    assert_eq!(entry.plugin.as_ref().expect("bound").id, "test.fixture");
    assert_eq!(f.handlers.len(), 11);
}

#[tokio::test]
async fn identity_round_trips_a_resolved_input_to_the_named_output() {
    let f = fixture(&config());
    let data = run(
        &f.handlers["test.fixture.identity"],
        json!({"message": {"var": "data.raw"}, "output": "data.echoed"}),
        json!({"raw": {"mti": "0200", "fields": [1, 2, 3], "ok": true, "n": 1.5}}),
    )
    .await
    .expect("identity succeeds");
    // The guest receives the whole evaluated input object — that is the ABI —
    // with the resolvable field folded against the message.
    assert_eq!(
        data["echoed"],
        json!({"message": {"mti": "0200", "fields": [1, 2, 3], "ok": true, "n": 1.5}})
    );
    assert_eq!(
        data["raw"]["mti"], "0200",
        "the rest of the context is untouched"
    );
}

#[tokio::test]
async fn a_task_without_output_writes_at_the_functions_default_root() {
    let f = fixture(&config());
    let data = run(
        &f.handlers["test.fixture.wrap"],
        json!({"message": "hello"}),
        json!({"before": 1}),
    )
    .await
    .expect("wrap succeeds");
    // `wrap` declares `output_default_root = "data"`, so the result is the
    // whole data root.
    assert_eq!(data, json!({"wrapped": {"message": "hello"}, "len": 19}));

    // `trap` declares no default, so a task must name one.
    let err = run(&f.handlers["test.fixture.trap"], json!({}), json!({}))
        .await
        .expect_err("no output, no default");
    assert!(err.contains("missing field `output`"), "{err}");
}

#[tokio::test]
async fn the_input_is_validated_against_the_entry_before_wasm() {
    let f = fixture(&config());
    // A required field missing from the authored input: refused when the
    // engine is built, before any message, at the field.
    let err = run(
        &f.handlers["test.fixture.identity"],
        json!({"output": "data.x"}),
        json!({}),
    )
    .await
    .expect_err("a required field is missing");
    assert!(err.contains("missing field `message`"), "{err}");
    assert!(
        err.contains(r#"Key("input"), Key("message")"#),
        "the issue is at the field: {err}"
    );

    // A field that resolved to the wrong kind per message is refused too —
    // at execution, after the fold, and still before WASM.
    let err = run(
        &f.handlers["test.fixture.big-output"],
        json!({"size": {"var": "data.size"}, "output": "data.x"}),
        json!({"size": "not a number"}),
    )
    .await
    .expect_err("resolved to the wrong kind");
    assert!(err.contains("TYPE_MISMATCH"), "{err}");
}

/// A `template_at` field is a JSONLogic expression the engine compiles at
/// load and evaluates per message; the guest receives the result. `upper`
/// upper-cases the JSON text it is given, so the evaluated field comes back
/// visibly transformed.
#[tokio::test]
async fn a_template_field_is_evaluated_by_the_engine_before_the_guest_sees_it() {
    let f = fixture(&config());
    let data = run(
        &f.handlers["test.fixture.upper"],
        json!({
            "text": {"cat": ["hello ", {"var": "data.who"}]},
            "output": "data.up"
        }),
        json!({"who": "world"}),
    )
    .await
    .expect("upper succeeds");
    assert_eq!(data["up"], json!({"TEXT": "HELLO WORLD"}));

    // A literal in a template position folds to itself.
    let data = run(
        &f.handlers["test.fixture.upper"],
        json!({"text": "plain", "output": "data.up"}),
        json!({}),
    )
    .await
    .expect("literal template");
    assert_eq!(data["up"], json!({"TEXT": "PLAIN"}));
}

/// The kind check runs again over the *evaluated* value, with every field
/// read as literal: an expression that produced the wrong kind is refused
/// before WASM, where at create time an object in a template position was
/// admitted as an operator call.
#[tokio::test]
async fn a_template_that_evaluates_to_the_wrong_kind_is_refused_before_wasm() {
    let f = fixture(&config());
    let err = run(
        &f.handlers["test.fixture.big-output"],
        json!({"size": {"var": "data.size"}, "output": "data.x"}),
        json!({"size": "not a number"}),
    )
    .await
    .expect_err("evaluated to the wrong kind");
    assert!(err.contains("TYPE_MISMATCH"), "{err}");
    assert!(err.contains("test.fixture.big-output"), "{err}");
}

/// The receiver-taking hooks dataflow-rs 3.12 added: the authored input is
/// checked against *this registration's* table, and its template fields are
/// compiled, when the engine is built — so an input the schema refuses fails
/// the engine build at the field rather than failing the first message.
/// That is what lets the serving screen quarantine a workflow whose plugin
/// changed its schema underneath it.
#[tokio::test]
async fn the_authored_input_is_checked_against_the_registration_at_engine_build() {
    let f = fixture(&config());
    let err = run(
        &f.handlers["test.fixture.upper"],
        json!({"txt": "x", "output": "data.x"}),
        json!({}),
    )
    .await
    .expect_err("an undeclared field");
    assert!(
        err.contains("unknown key `txt`, did you mean `text`?"),
        "{err}"
    );
    assert!(
        err.contains("missing field `text`"),
        "and the declared one is missing: {err}"
    );

    // An object in a template position is admitted at build — it may be an
    // operator call — and judged per message by what it evaluates to. This
    // one is not an operator, so it evaluates to itself: an object where the
    // table wants a string, refused before the guest sees it.
    let err = run(
        &f.handlers["test.fixture.upper"],
        json!({"text": {"no_such_operator": [1]}, "output": "data.x"}),
        json!({}),
    )
    .await
    .expect_err("evaluated to an object");
    assert!(err.contains("TYPE_MISMATCH"), "{err}");
    assert!(err.contains("test.fixture.upper"), "{err}");
}

#[tokio::test]
async fn guest_errors_keep_their_code_and_lose_their_internals() {
    let f = fixture(&config());
    let err = run(
        &f.handlers["test.fixture.caller-input-error"],
        json!({"output": "data.x"}),
        json!({}),
    )
    .await
    .expect_err("caller input");
    assert!(
        err.contains("BAD_MESSAGE: the message is not ISO 8583"),
        "{err}"
    );

    let err = run(
        &f.handlers["test.fixture.internal-error"],
        json!({"output": "data.x"}),
        json!({}),
    )
    .await
    .expect_err("internal");
    assert!(
        err.contains("BOOM: the guest failed with a newline"),
        "{err}"
    );
    assert!(
        !err.contains('\n'),
        "control characters are stripped: {err:?}"
    );

    let err = run(
        &f.handlers["test.fixture.bad-code"],
        json!({"output": "data.x"}),
        json!({}),
    )
    .await
    .expect_err("bad code");
    assert!(err.contains("ABI grammar"), "{err}");
    assert!(
        !err.contains("lowercase code"),
        "the guest's string is not echoed: {err}"
    );
}

#[tokio::test]
async fn a_trap_writes_nothing_and_names_no_internals() {
    let f = fixture(&config());
    let err = run(
        &f.handlers["test.fixture.trap"],
        json!({"output": "data.x"}),
        json!({"keep": true}),
    )
    .await
    .expect_err("trap");
    assert!(err.contains("the plugin trapped"), "{err}");
    assert!(!err.to_lowercase().contains("unreachable"), "{err}");
}

#[tokio::test]
async fn a_guest_that_allocates_without_end_hits_the_memory_limit() {
    let f = fixture(&config());
    let err = run(
        &f.handlers["test.fixture.alloc-forever"],
        json!({"output": "data.x"}),
        json!({}),
    )
    .await
    .expect_err("memory");
    assert!(err.contains("exceeded its memory limit"), "{err}");
}

#[tokio::test]
async fn a_spinning_guest_is_stopped_by_the_epoch_deadline() {
    let f = fixture(&config());
    let ticker = tick(f.runtime.clone());
    let started = Instant::now();
    let err = run(
        &f.handlers["test.fixture.spin"],
        json!({"output": "data.x"}),
        json!({}),
    )
    .await
    .expect_err("timeout");
    ticker.abort();
    assert!(err.contains("exceeded its deadline"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "stopped near the 300 ms deadline, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_spinning_guest_is_stopped_by_the_wall_clock_when_no_ticker_runs() {
    // The belt to the epoch's brace: with nothing advancing the epoch, the
    // fuel-yield interval lets the wall-clock timeout cancel the call.
    let f = fixture(&config());
    let started = Instant::now();
    let err = run(
        &f.handlers["test.fixture.spin"],
        json!({"output": "data.x"}),
        json!({}),
    )
    .await
    .expect_err("timeout");
    assert!(err.contains("exceeded its deadline"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_spinning_guest_is_stopped_by_fuel_when_the_backstop_is_the_smaller_bound() {
    let f = fixture(&PluginsConfig {
        fuel_backstop: 10_000_000,
        max_timeout_ms: 5_000,
        ..config()
    });
    let started = Instant::now();
    let err = run(
        &f.handlers["test.fixture.spin"],
        json!({"output": "data.x"}),
        json!({}),
    )
    .await
    .expect_err("fuel");
    assert!(err.contains("exhausted its fuel backstop"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn oversized_results_and_non_json_results_are_refused() {
    let f = fixture(&PluginsConfig {
        max_response_bytes: 64 * 1024,
        ..config()
    });
    let err = run(
        &f.handlers["test.fixture.big-output"],
        json!({"size": 200_000, "output": "data.x"}),
        json!({}),
    )
    .await
    .expect_err("too big");
    assert!(err.contains("over the 65536 byte limit"), "{err}");

    let data = run(
        &f.handlers["test.fixture.big-output"],
        json!({"size": 1000, "output": "data.x"}),
        json!({}),
    )
    .await
    .expect("under the limit");
    assert_eq!(data["x"].as_str().map(str::len), Some(1000));

    let err = run(
        &f.handlers["test.fixture.bad-json"],
        json!({"output": "data.x"}),
        json!({}),
    )
    .await
    .expect_err("not json");
    assert!(err.contains("not JSON"), "{err}");
}

#[tokio::test]
async fn an_oversized_input_never_enters_wasm() {
    let f = fixture(&PluginsConfig {
        max_request_bytes: 64,
        ..config()
    });
    let err = run(
        &f.handlers["test.fixture.identity"],
        json!({"message": "x".repeat(200), "output": "data.x"}),
        json!({}),
    )
    .await
    .expect_err("too big");
    assert!(err.contains("over the 64 byte limit"), "{err}");
}

#[tokio::test]
async fn the_self_test_accepts_a_guest_error_and_refuses_a_trap() {
    let f = fixture(&config());
    let loaded = f.runtime.load_blocking(COMPONENT).expect("cached");
    let limits = Limits::effective(&config(), "test.fixture");
    f.runtime
        .self_test(
            &loaded,
            &limits,
            &[
                "test.fixture.identity",
                "test.fixture.caller-input-error",
                "test.fixture.bad-json",
            ],
        )
        .await
        .expect("a value or a guest error proves the export answers");
    let err = f
        .runtime
        .self_test(&loaded, &limits, &["test.fixture.trap"])
        .await
        .expect_err("a trap fails the self-test");
    match err {
        LoadError::SelfTest { function, reason } => {
            assert_eq!(function, "test.fixture.trap");
            assert!(reason.contains("trapped"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn no_instance_outlives_its_call() {
    let f = fixture(&config());
    assert_eq!(f.runtime.live_instances(), 0);
    run(
        &f.handlers["test.fixture.identity"],
        json!({"message": 1, "output": "data.x"}),
        json!({}),
    )
    .await
    .expect("ok");
    let _ = run(
        &f.handlers["test.fixture.trap"],
        json!({"output": "data.x"}),
        json!({}),
    )
    .await;
    assert_eq!(
        f.runtime.live_instances(),
        0,
        "a trapped store is dropped like any other"
    );
}

#[tokio::test]
async fn workflow_plugin_inputs_follow_the_v2_numeric_representation() {
    let f = fixture(&config());
    let identity = &f.handlers["test.fixture.identity"];
    for (input, expected) in [
        (json!(i64::MIN), json!(i64::MIN)),
        (
            json!(-9_007_199_254_740_993_i64),
            json!(-9_007_199_254_740_993_i64),
        ),
        (
            json!(9_007_199_254_740_993_i64),
            json!(9_007_199_254_740_993_i64),
        ),
        (json!(i64::MAX), json!(i64::MAX)),
        (
            json!(9_223_372_036_854_775_808_u64),
            json!(9_223_372_036_854_775_808.0),
        ),
        (
            json!(9_223_372_036_854_775_809_u64),
            json!(9_223_372_036_854_775_808.0),
        ),
        (json!(u64::MAX), json!(18_446_744_073_709_551_616.0)),
        (json!(0.5), json!(0.5)),
        (json!(u64::MAX.to_string()), json!(u64::MAX.to_string())),
    ] {
        for from_message in [false, true] {
            let message = if from_message {
                json!({"var": "data.value"})
            } else {
                input.clone()
            };
            let data = run(
                identity,
                json!({"message": message, "output": "data.result"}),
                json!({"value": input}),
            )
            .await
            .expect("the complete workflow invokes the identity guest");
            assert_eq!(
                data["result"]["message"], expected,
                "input={input}, from_message={from_message}"
            );
        }
    }
}

#[tokio::test]
async fn dropping_plugin_invocations_releases_each_live_instance() {
    let cfg = config();
    let runtime = WasmRuntime::new(&cfg).expect("engine");
    let loaded = runtime.load_blocking(COMPONENT).expect("fixture loads");
    let limits = Limits::effective(&cfg, "test.fixture");
    let mut first = Box::pin(runtime.invoke(&loaded, &limits, "test.fixture.spin", "{}"));
    let mut second = Box::pin(runtime.invoke(&loaded, &limits, "test.fixture.spin", "{}"));

    poll_until_live(first.as_mut(), &runtime, 1).await;
    poll_until_live(second.as_mut(), &runtime, 2).await;
    drop(first);
    assert_eq!(runtime.live_instances(), 1, "the other call remains live");
    drop(second);
    assert_eq!(runtime.live_instances(), 0, "both stores were dropped");

    let result = runtime
        .invoke(&loaded, &limits, "test.fixture.identity", "{\"message\":1}")
        .await
        .expect("another invocation succeeds");
    assert_eq!(result, "{\"message\":1}");
    assert_eq!(runtime.live_instances(), 0);
}

#[tokio::test]
async fn cancelling_plugin_runs_releases_the_instance_and_function_permit() {
    use dataflow_rs::datavalue::OwnedDataValue;
    use dataflow_rs::run::{CancelToken, RunOptions, RunStatus};

    let f = fixture(&PluginsConfig {
        max_concurrency_per_function: 1,
        ..config()
    });
    let engine = plugin_engine(
        &f.handlers["test.fixture.spin"],
        json!({"output": "data.x"}),
    )
    .expect("engine builds");

    // A dropped run and a signalled cancellation both release the sole permit.
    // Re-entering Wasm on the next iteration proves admission works again.
    for drop_run in [true, false, false] {
        let cancel = CancelToken::new();
        let opts = RunOptions::default().cancel(&cancel);
        let data = OwnedDataValue::from(&json!({}));
        let mut message =
            dataflow_rs::Message::from_parts(data.clone(), data, OwnedDataValue::Null);
        let mut running = Box::pin(engine.run_with(&mut message, &opts));
        poll_until_live(running.as_mut(), &f.runtime, 1).await;
        if drop_run {
            drop(running);
        } else {
            cancel.cancel();
            let result = running.await;
            assert!(matches!(result.status(), RunStatus::Cancelled));
        }
        assert_eq!(f.runtime.live_instances(), 0, "the run dropped its store");
        assert_eq!(Value::from(message.context().data()), json!({}));
    }
}

/// A compile is a job on the native work scheduler the runtime was given: it
/// waits for a slot, a caller that gets none in time is refused as
/// `Capacity` (the component was never compiled), and it compiles once one
/// frees, releasing it when done.
#[tokio::test]
async fn a_compile_waits_for_a_native_work_slot() {
    use orion::test_support::NativeWorkScheduler;

    let native = NativeWorkScheduler::new(1);
    let impatient =
        WasmRuntime::with_native_work(&config(), native.clone(), Duration::from_millis(100))
            .expect("engine");
    let held = native.try_acquire("held").expect("the one slot");
    match impatient.load(COMPONENT.to_vec()).await {
        Err(LoadError::Capacity(reason)) => assert!(
            reason.contains("NATIVE_QUEUE_DEADLINE_EXCEEDED"),
            "{reason}"
        ),
        other => panic!("a compile with no slot must be refused as capacity: {other:?}"),
    }
    assert!(
        impatient.cached(&WasmRuntime::digest(COMPONENT)).is_none(),
        "nothing compiled while the slot was held"
    );

    let patient = WasmRuntime::with_native_work(&config(), native.clone(), Duration::from_secs(60))
        .expect("engine");
    let waiting = {
        let patient = patient.clone();
        tokio::spawn(async move { patient.load(COMPONENT.to_vec()).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiting.is_finished(), "the compile waits for the slot");
    drop(held);
    let loaded = waiting
        .await
        .expect("the task joins")
        .expect("it compiles once the slot frees");
    assert_eq!(loaded.size, COMPONENT.len());
    // The job's thread holds its slot until it returns, a moment after it
    // has handed the caller its result.
    let released = Instant::now() + Duration::from_secs(5);
    while native.in_flight() > 0 && Instant::now() < released {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(native.in_flight(), 0, "the compile gave its slot back");
}

/// The node's scheduler is sized from `engine.native_work_max_in_flight` and
/// reported under `native_scheduler` in the runtime capabilities.
#[tokio::test]
async fn the_native_work_bound_comes_from_config() {
    use tower::ServiceExt;

    let mut config = orion::config::AppConfig::default();
    config.engine.native_work_max_in_flight = 3;
    let app = crate::common::test_app_with_config(config).await;
    let resp = app
        .oneshot(crate::common::json_request(
            "GET",
            "/api/v2/admin/runtime-capabilities",
            None,
        ))
        .await
        .expect("responds");
    let caps = crate::common::body_json(resp).await;
    assert_eq!(caps["native_scheduler"]["max_in_flight"], 3, "{caps}");
    assert_eq!(caps["native_scheduler"]["in_flight"], 0, "{caps}");
}
