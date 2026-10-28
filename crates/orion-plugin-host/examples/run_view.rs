//! Reproducible v4 run-view workload; driven by tests/benchmark/run-view.py.
//! Compile/instantiate setup and message construction are outside the window.
#[allow(unsafe_code, reason = "benchmark-only counting adapter over System")]
#[path = "run_view_support/allocation.rs"]
mod allocation;

use std::sync::Arc;
use std::time::Instant;

use dataflow_rs::datavalue::OwnedDataValue;
use dataflow_rs::observe::TraceOptions;
use dataflow_rs::run::{AuditMode, RunOptions};
use orion_common::config::PluginsConfig;
use orion_engine::PluginBinding;
use orion_plugin_host::{
    ABI, Limits, Manifest, PluginFunction, PluginFunctionHandler, WasmRuntime,
};
use serde_json::{Value, json};

#[global_allocator]
static ALLOCATOR: allocation::Meter = allocation::Meter;

fn workflow(kind: &str, steps: usize, concurrency: usize) -> Value {
    if kind == "loop" {
        json!({"id":"loop", "condition":{"<":[{"var":"temp_data.i"}, steps]},
            "loop":{"counter":"i", "max":steps}, "tasks":[
            {"id":"template", "function":{"name":"test.fixture.upper", "input":{
                "text":{"cat":["event-", {"var":"temp_data.i"}]}, "output":"temp_data.echo"
            }}},
            {"id":"grow", "function":{"name":"map", "input":{"mappings":[{
                "path":"data.replay", "logic":{"var":"temp_data.echo.TEXT"}, "mode":"append"
            }]}}}
        ]})
    } else {
        assert_eq!(kind, "fan");
        json!({"id":"fan", "tasks":[{
            "id":"echo", "for_each":{
                "over":{"var":"data.items"}, "as":"item", "max_concurrency":concurrency,
                "collect":"temp_data.echo.message", "into":"data.results"
            }, "function":{"name":"test.fixture.identity", "input":{
                "message":{"var":"temp_data.item"}, "output":"temp_data.echo"
            }}
        }]})
    }
}

fn engine(kind: &str, steps: usize, concurrency: usize) -> dataflow_rs::Engine {
    let config = PluginsConfig {
        enabled: true,
        max_memory_bytes: 16 * 1024 * 1024,
        max_live_instances: 64,
        max_timeout_ms: 5000,
        ..PluginsConfig::default()
    };
    let runtime = WasmRuntime::new(&config).expect("sandbox");
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../orion-server/tests/fixtures/plugins");
    let loaded = runtime
        .load_blocking(&std::fs::read(fixtures.join("fixture.wasm")).expect("fixture bytes"))
        .expect("fixture component");
    let manifest = Manifest::parse(
        &std::fs::read_to_string(fixtures.join("fixture.toml")).expect("manifest text"),
    )
    .expect("fixture manifest");
    let binding = PluginBinding {
        id: manifest.name.clone(),
        version: 1,
        digest: loaded.digest.clone(),
        abi: ABI.into(),
    };
    let exprs = orion_engine::operators::orion_exprs(&orion_engine::ResolvedSecrets::empty(), 0)
        .expect("Orion expressions");
    let mut registry = dataflow_rs::Registry::new(exprs);
    for (entry, decl) in manifest
        .entries(&binding)
        .into_iter()
        .zip(&manifest.functions)
    {
        let handler = PluginFunctionHandler::new(
            Arc::new(entry),
            loaded.clone(),
            runtime.clone(),
            Limits::effective(&config, &manifest.name),
        );
        registry.register(
            &decl.name,
            PluginFunction::new(handler, decl.runtime_fields()),
        );
    }
    dataflow_rs::Engine::builder()
        .registry(registry)
        .workflow_json(&workflow(kind, steps, concurrency))
        .build()
        .expect("workload builds")
}

fn initial(nodes: usize, steps: usize) -> OwnedDataValue {
    // Many small objects expose tree-copy costs that a single large shared
    // string can hide. Vary nodes independently of items and result sizes.
    let unrelated: Vec<Value> = (0..nodes)
        .map(|i| json!({"id":i,"text":"x".repeat(64)}))
        .collect();
    OwnedDataValue::from(
        &json!({"unrelated":unrelated,"items":(0..steps).collect::<Vec<_>>(),"replay":[]}),
    )
}

fn verify(kind: &str, steps: usize, message: &dataflow_rs::Message) {
    assert!(message.errors().is_empty(), "{:?}", message.errors());
    let data = message.context().data();
    let result = data
        .get(if kind == "loop" { "replay" } else { "results" })
        .expect("result");
    let values = Value::from(result);
    if kind == "loop" {
        assert_eq!(
            values,
            json!((0..steps).map(|i| format!("EVENT-{i}")).collect::<Vec<_>>())
        );
    } else {
        assert_eq!(values, json!((0..steps).collect::<Vec<_>>()));
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert_eq!(
        args.len(),
        7,
        "usage: run_view loop|fan NODES STEPS CONCURRENCY ROUNDS TRACE(0|1)"
    );
    let kind = &args[1];
    let nodes: usize = args[2].parse().expect("nodes");
    let steps: usize = args[3].parse().expect("steps");
    let concurrency: usize = args[4].parse().expect("concurrency");
    let rounds: usize = args[5].parse().expect("rounds");
    let trace = args[6] == "1";
    assert!(steps > 0 && rounds > 0 && concurrency > 0);
    let engine = engine(kind, steps, concurrency);
    let initial = initial(nodes, steps);
    let input_bytes = initial.to_string().len();
    let mut opts = RunOptions::default()
        .audit(AuditMode::Off)
        .capture_changes(false);
    if trace {
        opts = opts.trace(TraceOptions::default());
    }
    let mut samples = Vec::with_capacity(rounds);
    for round in 0..=rounds {
        let mut message = dataflow_rs::Message::from_parts(
            OwnedDataValue::Null,
            initial.clone(),
            OwnedDataValue::Null,
        );
        // First run warms Wasmtime's pool and lazy runtime state.
        let window = allocation::Window::start();
        let started = Instant::now();
        let run = engine.run_with(&mut message, &opts).await;
        let elapsed = started.elapsed().as_nanos();
        let (allocations, bytes) = window.finish();
        verify(kind, steps, &message);
        assert_eq!(run.trace().is_some(), trace);
        std::hint::black_box(&run);
        if round > 0 {
            samples.push(
                json!({"elapsed_ns":elapsed,"allocations":allocations,"allocated_bytes":bytes}),
            );
        }
    }
    println!(
        "{}",
        json!({"kind":kind,"nodes":nodes,"steps":steps,"concurrency":concurrency,
        "trace":trace,"rounds":rounds,"input_json_bytes":input_bytes,"samples":samples})
    );
}
