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
use orion::plugin::{ABI, Limits, LoadError, Manifest, PluginFunctionHandler, WasmRuntime};

const COMPONENT: &[u8] = include_bytes!("../fixtures/plugins/fixture.wasm");
const MANIFEST: &str = include_str!("../fixtures/plugins/fixture.toml");

/// Every handler the fixture exports, built under `config`.
struct Fixture {
    runtime: Arc<WasmRuntime>,
    handlers: HashMap<String, PluginFunctionHandler>,
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
    let handlers = entries
        .iter()
        .map(|e| {
            let handler = PluginFunctionHandler::new(
                Arc::new(e.clone()),
                loaded.clone(),
                runtime.clone(),
                limits,
            );
            (e.name.clone(), handler)
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

/// Run one task through a real dataflow engine with `handler` registered
/// under its name; returns the message's final `data`, or the first task
/// error's text.
async fn run(handler: &PluginFunctionHandler, input: Value, data: Value) -> Result<Value, String> {
    let name = handler.name().to_string();
    let mut fns: HashMap<String, dataflow_rs::BoxedFunctionHandler> = HashMap::new();
    fns.insert(name.clone(), Box::new(handler.clone()));
    let workflow: dataflow_rs::Workflow = serde_json::from_value(json!({
        "id": "w", "name": "w", "condition": true,
        "tasks": [{"id": "t", "name": "t", "function": {"name": name, "input": input}}]
    }))
    .map_err(|e| e.to_string())?;
    let engine = dataflow_rs::Engine::new(vec![workflow], fns).map_err(|e| e.to_string())?;
    let mut message = dataflow_rs::Message::from_value(&json!({}));
    if !data.is_null() {
        dataflow_rs::engine::utils::set_nested_value(
            &mut message.context,
            "data",
            dataflow_rs::datavalue::OwnedDataValue::from(&data),
        );
    }
    engine
        .process_message(&mut message)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(err) = message.errors().first() {
        return Err(format!("{err:?}"));
    }
    Ok(message.data().into())
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
    let wit = include_str!("../../wit/orion-plugin.wit");
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
    assert!(err.contains("names no 'output'"), "{err}");
}

#[tokio::test]
async fn the_input_is_validated_against_the_entry_before_wasm() {
    let f = fixture(&config());
    // A required field missing from the authored input: refused when the
    // engine is built (`parse_input_with`), before any message.
    let err = run(
        &f.handlers["test.fixture.identity"],
        json!({"output": "data.x"}),
        json!({}),
    )
    .await
    .expect_err("a required field is missing");
    assert!(err.contains("REQUIRED"), "{err}");
    assert!(
        err.contains("test.fixture.identity"),
        "the error names the function: {err}"
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
/// `Engine::new` naming the function rather than failing the first message.
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
    assert!(err.contains("UNKNOWN_FIELD"), "{err}");
    assert!(
        err.contains("REQUIRED"),
        "and the declared one is missing: {err}"
    );
    assert!(err.contains("test.fixture.upper"), "{err}");

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
async fn a_function_at_its_concurrency_limit_refuses_a_permit_at_the_deadline() {
    let f = fixture(&PluginsConfig {
        max_concurrency_per_function: 1,
        max_timeout_ms: 500,
        ..config()
    });
    let ticker = tick(f.runtime.clone());
    let spin = f.handlers["test.fixture.spin"].clone();
    // Two spinners queue on one permit (FIFO), so the third caller's wait
    // outlives its deadline: the first holds the permit for ~500 ms, the
    // second takes it next.
    let a = tokio::spawn({
        let h = spin.clone();
        async move { run(&h, json!({"output": "data.a"}), json!({})).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let b = tokio::spawn({
        let h = spin.clone();
        async move { run(&h, json!({"output": "data.b"}), json!({})).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let c = run(&spin, json!({"output": "data.c"}), json!({})).await;
    let c = c.expect_err("the third caller finds no permit within its deadline");
    assert!(c.contains("no concurrency permit"), "{c}");
    for spinner in [a, b] {
        let err = spinner
            .await
            .expect("joins")
            .expect_err("each spinner times out");
        assert!(err.contains("exceeded its deadline"), "{err}");
    }
    ticker.abort();
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
