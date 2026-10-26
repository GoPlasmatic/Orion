use super::*;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use dataflow_rs::function::{RetrySafety, testing};
use dataflow_rs::program::Severity;
use dataflow_rs::run::Instant as RunInstant;
use serde_json::json;

use crate::config::PluginsConfig;
use crate::engine::PluginBinding;
use crate::engine::functions::twin::{self, Seen};
use crate::engine::kit::test_exprs;
use crate::manifest::{ABI, Manifest};

const COMPONENT: &[u8] = include_bytes!("../../orion-server/tests/fixtures/plugins/fixture.wasm");
const MANIFEST: &str = include_str!("../../orion-server/tests/fixtures/plugins/fixture.toml");

/// The fixture's `identity` under a manifest of every position and kind
/// a field can take. The component dispatches on the function's last
/// label, so any field table serves; `identity` hands back the input it
/// was given, which shows what reached the guest.
const RICH: &str = r#"
abi = "orion:plugin@1.0.0"
name = "test.fixture"
version = "0.0.0"

[[functions]]
name = "test.fixture.identity"
description = "Return the input unchanged"

[[functions.input_fields]]
name = "message"
kind = "any"
resolvable = true

[[functions.input_fields]]
name = "label"
kind = "string"

[[functions.input_fields]]
name = "count"
kind = "number"
template_at = true

[[functions.input_fields]]
name = "greeting"
kind = "string"
template_at = true

[[functions.input_fields]]
name = "mode"
kind = "string"

[[functions.input_fields]]
name = "text"
kind = "string"
resolvable = true
"#;

fn config() -> PluginsConfig {
    PluginsConfig {
        enabled: true,
        max_memory_bytes: 16 * 1024 * 1024,
        max_timeout_ms: 300,
        max_live_instances: 64,
        max_concurrency_per_function: 64,
        // High enough that the deadline, not fuel, stops a spinner; the
        // fuel category has a case of its own.
        fuel_backstop: 1_000_000_000_000,
        ..PluginsConfig::default()
    }
}

/// The fixture component, compiled once for the binary on a sandbox
/// sized by [`config`]. Each test's functions run on it under the limits
/// its own config gives them (`Limits::effective`).
static SANDBOX: LazyLock<(Arc<WasmRuntime>, Arc<LoadedComponent>)> = LazyLock::new(|| {
    let runtime = WasmRuntime::new(&config()).expect("the sandbox builds");
    let loaded = runtime.load_blocking(COMPONENT).expect("the fixture loads");
    (runtime, loaded)
});

/// One function: its handler, and the engine function over it.
#[derive(Clone)]
struct Both {
    handler: PluginFunctionHandler,
    function: PluginFunction,
}

/// Every function `manifest` declares, over the fixture component, under
/// `config`'s limits, by name: built as the loader builds them.
fn functions_of(manifest: &str, config: &PluginsConfig) -> HashMap<String, Both> {
    let (runtime, loaded) = &*SANDBOX;
    let manifest = Manifest::parse(manifest).expect("the manifest parses");
    let binding = PluginBinding {
        id: manifest.name.clone(),
        version: 1,
        digest: loaded.digest.clone(),
        abi: ABI.to_string(),
    };
    let limits = Limits::effective(config, &manifest.name);
    manifest
        .entries(&binding)
        .into_iter()
        .zip(&manifest.functions)
        .map(|(entry, decl)| {
            let handler = PluginFunctionHandler::new(
                Arc::new(entry),
                loaded.clone(),
                runtime.clone(),
                limits,
            );
            let function = PluginFunction::new(handler.clone(), decl.runtime_fields());
            (function.name().to_string(), Both { handler, function })
        })
        .collect()
}

fn fixture() -> HashMap<String, Both> {
    functions_of(MANIFEST, &config())
}

/// `f` called with `input` over a message whose `data` is `data`.
async fn run(f: &Both, input: Value, data: Value) -> Seen {
    twin::v4(f.function.name(), f.function.clone(), input, data).await
}

/// `f` called, its issues and writes kept.
async fn v4_call(f: &PluginFunction, input: Value, data: Value) -> testing::Called {
    testing::call(f.clone(), input)
        .exprs(test_exprs())
        .name(f.name())
        .message(dataflow_rs::Message::from_parts(
            OwnedDataValue::Null,
            OwnedDataValue::from(&data),
            OwnedDataValue::Null,
        ))
        .await
}

/// The Rejected issues `input` is refused with at load, as
/// (path, code); the call must not have run.
async fn v4_refusal(f: &PluginFunction, input: Value) -> Vec<(String, IssueCode)> {
    let called = v4_call(f, input, json!({})).await;
    assert!(
        called
            .outcome
            .as_ref()
            .is_err_and(|e| *e.kind() == ErrorKind::Compile),
        "refused at load: {:?} {:?}",
        called.outcome,
        called.issues
    );
    called
        .issues
        .iter()
        .filter(|i| i.severity == Severity::Rejected)
        .map(|i| (i.path.to_string(), i.code))
        .collect()
}

/// One runtime field as a test compares it: name, kind (`FieldKind` has
/// no `PartialEq`, so its `Debug`), required, default, recorded.
fn shape(fields: &[RuntimeField]) -> Vec<(String, String, bool, Option<String>, bool)> {
    fields
        .iter()
        .map(|f| {
            (
                f.name.to_string(),
                format!("{:?}", f.kind),
                f.required,
                f.default.as_deref().map(str::to_string),
                f.records,
            )
        })
        .collect()
}

/// The fixture component's manifest, field by field (I183): each
/// function's runtime fields are its declared fields in the positions
/// 1.12 executed them in, then the implicit `output`, defaulting to the
/// declared root and required where there is none. The spec says what
/// the registry entry says, and every function registers beside the
/// built-ins, its catalogue fields the runtime fields.
#[test]
fn the_fixture_manifest_maps_field_by_field() {
    let f = fixture();
    let row = |name: &str, kind: &str, required: bool, default: Option<&str>, records: bool| {
        (
            name.to_string(),
            kind.to_string(),
            required,
            default.map(str::to_string),
            records,
        )
    };
    let at_root = [
        row("message", "Interpolated", true, None, false),
        row("output", "Output", false, Some("\"data\""), true),
    ];
    let output_only = [row("output", "Output", true, None, true)];
    let expected: Vec<(&str, Vec<_>)> = vec![
        ("test.fixture.identity", at_root.to_vec()),
        ("test.fixture.wrap", at_root.to_vec()),
        ("test.fixture.trap", output_only.to_vec()),
        ("test.fixture.alloc-forever", output_only.to_vec()),
        ("test.fixture.spin", output_only.to_vec()),
        (
            "test.fixture.upper",
            vec![
                row("text", "Expr(String)", true, None, true),
                row("output", "Output", true, None, true),
            ],
        ),
        (
            "test.fixture.big-output",
            vec![
                row("size", "Expr(Number)", false, None, true),
                row("output", "Output", true, None, true),
            ],
        ),
        ("test.fixture.bad-json", output_only.to_vec()),
        ("test.fixture.bad-code", output_only.to_vec()),
        ("test.fixture.caller-input-error", output_only.to_vec()),
        ("test.fixture.internal-error", output_only.to_vec()),
    ];
    assert_eq!(f.len(), expected.len());
    for (name, fields) in expected {
        let both = &f[name];
        assert_eq!(shape(both.function.fields()), fields, "{name}");
        let spec = dataflow_rs::Function::spec(&both.function);
        assert_eq!(spec.description, both.handler.entry.description, "{name}");
        assert_eq!(spec.category, "transform", "{name}");
        assert_eq!(spec.retry, RetrySafety::Idempotent, "{name}: pure");
        assert!(spec.references.is_empty(), "{name}");
        assert_eq!(
            shape(spec.runtime_fields.as_deref().expect("runtime fields")),
            fields,
            "{name}"
        );
    }

    let mut registry = dataflow_rs::Registry::new(test_exprs());
    let refused = crate::engine::register_plugin_functions(
        &mut registry,
        f.values().map(|both| both.function.clone()),
    );
    assert!(refused.is_empty(), "{refused:?}");
    let hosts: Vec<_> = registry.catalogue().filter(|info| !info.builtin).collect();
    assert_eq!(hosts.len(), 11);
    assert!(
        registry
            .catalogue()
            .any(|info| info.builtin && info.name == "map")
    );
    for info in hosts {
        assert_eq!(
            shape(info.fields),
            shape(f[info.name].function.fields()),
            "{}",
            info.name
        );
    }
}

/// What a call of `inner` reads: each field `read_fields` gave it, and
/// whether the value was lent from the config.
struct Lending {
    inner: PluginFunction,
    lent: Arc<Mutex<Vec<(String, bool)>>>,
}

impl dataflow_rs::Function for Lending {
    type Config = Templated;

    fn spec(&self) -> FunctionSpec {
        dataflow_rs::Function::spec(&self.inner)
    }

    fn prepare(&self, cfg: &mut Templated, p: &Prepare<'_>) -> Result<(), Vec<Issue>> {
        dataflow_rs::Function::prepare(&self.inner, cfg, p)
    }

    async fn call(&self, ctx: &mut Ctx<'_>, cfg: &Templated) -> dataflow_rs::Result<Outcome> {
        {
            let read = self.inner.read_fields(ctx, cfg).expect("every field reads");
            let mut lent = self.lent.lock().expect("not poisoned");
            for (name, value) in read {
                lent.push((name.to_string(), matches!(value, Cow::Borrowed(_))));
            }
        }
        dataflow_rs::Function::call(&self.inner, ctx, cfg).await
    }
}

/// I131: the guest's fields are read through `Templated::get`, so a
/// literal and a template that folded to a constant are lent from the
/// config, and a computed template and an interpolated field are
/// resolved per call. `identity` hands back what it received, which is
/// what 1.12 handed it.
#[tokio::test]
async fn identity_reads_its_fields_through_templated_get_lending_literals_and_constants() {
    let rich = functions_of(RICH, &config());
    let identity = &rich["test.fixture.identity"];
    let input = json!({
        "message": {"var": "data.raw"},
        "label": "plain",
        "count": {"+": [1, 2]},
        "greeting": {"cat": ["hello ", {"var": "data.who"}]},
        "output": "data.echoed",
    });
    let data = json!({
        "raw": {"mti": "0200", "fields": [1, 2, 3], "ok": true, "n": 1.5},
        "who": "world",
    });
    let lent = Arc::new(Mutex::new(Vec::new()));
    let called = testing::call(
        Lending {
            inner: identity.function.clone(),
            lent: Arc::clone(&lent),
        },
        input.clone(),
    )
    .exprs(test_exprs())
    .name(identity.function.name())
    .message(dataflow_rs::Message::from_parts(
        OwnedDataValue::Null,
        OwnedDataValue::from(&data),
        OwnedDataValue::Null,
    ))
    .await;
    assert!(called.outcome.is_ok(), "{:?}", called.outcome);
    assert_eq!(
        *lent.lock().expect("not poisoned"),
        [
            ("message".to_string(), false),
            ("label".to_string(), true),
            ("count".to_string(), true),
            ("greeting".to_string(), false),
        ]
    );
    let echoed = json!({
        "message": {"mti": "0200", "fields": [1, 2, 3], "ok": true, "n": 1.5},
        "label": "plain",
        "count": 3,
        "greeting": "hello world",
    });
    assert_eq!(
        Value::from(called.message.context().data())["echoed"],
        echoed
    );
    let v4 = run(identity, input, data).await;
    twin::pinned(&v4, "identity_reads_its_fields");
    assert_eq!(v4.expect("identity succeeds")["echoed"], echoed);
}

/// Each call the integration suite makes of a well-behaved fixture
/// function writes what 1.12 wrote: the round trip, the default root, an evaluated and a literal
/// template, and a result under the response limit.
#[tokio::test]
async fn a_call_writes_what_1_12_wrote() {
    let f = fixture();
    for (name, input, data) in [
        (
            "test.fixture.identity",
            json!({"message": {"var": "data.raw"}, "output": "data.echoed"}),
            json!({"raw": {"mti": "0200", "fields": [1, 2, 3], "ok": true, "n": 1.5}}),
        ),
        (
            "test.fixture.identity",
            json!({
                "message": [{"var": "data.n"}, {"$oid": "x"}, {"var": ["data.gone", 7]}],
                "output": "data.out.echo"
            }),
            json!({"n": -2, "keep": "this"}),
        ),
        (
            "test.fixture.wrap",
            json!({"message": "hello"}),
            json!({"before": 1}),
        ),
        (
            "test.fixture.upper",
            json!({"text": {"cat": ["hello ", {"var": "data.who"}]}, "output": "data.up"}),
            json!({"who": "world"}),
        ),
        (
            "test.fixture.upper",
            json!({"text": "plain", "output": "data.up"}),
            json!({}),
        ),
        (
            "test.fixture.big-output",
            json!({"size": {"var": "data.n"}, "output": "data.x"}),
            json!({"n": 12}),
        ),
        (
            "test.fixture.big-output",
            json!({"size": 1000, "output": "data.x"}),
            json!({}),
        ),
    ] {
        let v4 = run(&f[name], input.clone(), data).await;
        assert!(v4.is_ok(), "{name} {input}: {v4:?}");
        twin::pinned(&v4, format!("call/{name}/{input}"));
    }
}

/// Each refusal the integration suite pins at call time, with 1.12's
/// code, text, retryability and detail: a value that
/// resolved to the wrong kind (the schema run again over the resolved
/// input), a template that did not evaluate, the guest's own errors,
/// and the sandbox's.
#[tokio::test]
async fn a_refusal_at_call_is_1_12s() {
    let f = fixture();
    let out = json!({"output": "data.x"});
    for (name, input, data, needle) in [
        (
            "test.fixture.big-output",
            json!({"size": {"var": "data.size"}, "output": "data.x"}),
            json!({"size": "not a number"}),
            "expected number for 'size' (TYPE_MISMATCH)",
        ),
        (
            "test.fixture.upper",
            json!({"text": {"var": "data.missing"}, "output": "data.x"}),
            json!({}),
            "expected string for 'text' (TYPE_MISMATCH)",
        ),
        (
            "test.fixture.upper",
            json!({"text": {"throw": {"var": "data.why"}}, "output": "data.x"}),
            json!({"why": "boom"}),
            "'text' did not evaluate: ",
        ),
        // An object in a template position may be an operator call, so
        // it is admitted at load; this one is not, so it evaluates
        // to itself, an object where the table wants a string.
        (
            "test.fixture.upper",
            json!({"text": {"no_such_operator": [{"var": "data.n"}]}, "output": "data.x"}),
            json!({"n": 1}),
            "expected string for 'text' (TYPE_MISMATCH)",
        ),
        (
            "test.fixture.caller-input-error",
            out.clone(),
            json!({}),
            "BAD_MESSAGE: the message is not ISO 8583",
        ),
        (
            "test.fixture.internal-error",
            out.clone(),
            json!({}),
            "BOOM: the guest failed with a newline",
        ),
        (
            "test.fixture.bad-code",
            out.clone(),
            json!({}),
            "ABI grammar",
        ),
        (
            "test.fixture.trap",
            out.clone(),
            json!({"keep": true}),
            "the plugin trapped",
        ),
        (
            "test.fixture.alloc-forever",
            out.clone(),
            json!({}),
            "exceeded its memory limit",
        ),
        (
            "test.fixture.spin",
            out.clone(),
            json!({}),
            "exceeded its deadline",
        ),
        ("test.fixture.bad-json", out.clone(), json!({}), "not JSON"),
    ] {
        let v4 = run(&f[name], input.clone(), data).await;
        twin::pinned(&v4, format!("refusal/{name}/{input}"));
        let failure = v4.as_ref().expect_err(name);
        assert!(
            failure.message.contains(needle),
            "{name} {input}: {failure:?}"
        );
        assert!(
            failure.message.contains(name),
            "the error names the function: {failure:?}"
        );
    }
}

/// A template crossing `engine.ops_budget` is refused as 1.12 refused
/// it: the caller's class, so `VALIDATION_ERROR` and not
/// `BUDGET_EXCEEDED`, with datalogic's text under the field's name.
#[tokio::test]
async fn a_template_over_the_ops_budget_is_refused_as_it_was() {
    let f = fixture();
    let upper = &f["test.fixture.upper"];
    let input = json!({
        "text": {"cat": ["hello ", {"var": "data.who"}, " and ", {"var": "data.who"}]},
        "output": "data.up"
    });
    let data = json!({"who": "world"});
    let secrets = crate::engine::ResolvedSecrets::empty();

    let called = testing::call(upper.function.clone(), input)
        .exprs(crate::engine::operators::orion_exprs(&secrets, 2).expect("the engine builds"))
        .name(upper.function.name())
        .message(dataflow_rs::Message::from_parts(
            OwnedDataValue::Null,
            OwnedDataValue::from(&data),
            OwnedDataValue::Null,
        ))
        .await;
    let seen = twin::seen(&called);
    twin::pinned(&seen, "over_the_ops_budget");
    let failure = seen.expect_err("over the budget");
    assert_eq!(failure.code, "VALIDATION_ERROR");
    assert!(
        failure.message.contains("'text' did not evaluate: "),
        "{failure:?}"
    );
    assert!(failure.message.contains("budget exceeded"), "{failure:?}");
}

/// The size and fuel ceilings, under a plugin's own limits, refuse as
/// they did.
#[tokio::test]
async fn the_limits_refuse_as_they_did() {
    let f = functions_of(
        MANIFEST,
        &PluginsConfig {
            max_response_bytes: 64 * 1024,
            max_request_bytes: 64,
            fuel_backstop: 10_000_000,
            max_timeout_ms: 5_000,
            ..config()
        },
    );
    for (name, input, needle) in [
        (
            "test.fixture.big-output",
            json!({"size": 200_000, "output": "data.x"}),
            "over the 65536 byte limit",
        ),
        (
            "test.fixture.identity",
            json!({"message": "x".repeat(200), "output": "data.x"}),
            "over the 64 byte limit",
        ),
        (
            "test.fixture.spin",
            json!({"output": "data.x"}),
            "exhausted its fuel backstop",
        ),
    ] {
        let v4 = run(&f[name], input, json!({})).await;
        twin::pinned(&v4, format!("limits/{name}"));
        let failure = v4.as_ref().expect_err(name);
        assert!(failure.message.contains(needle), "{name}: {failure:?}");
    }
}

/// The permits are the function's: every clone holds its handler's
/// semaphore, so a call waits for a permit another holds, and is refused
/// with 1.12's text when none frees within the ceiling.
#[tokio::test]
async fn every_clone_of_a_function_shares_one_permit() {
    let f = functions_of(
        MANIFEST,
        &PluginsConfig {
            max_concurrency_per_function: 1,
            max_timeout_ms: 100,
            ..config()
        },
    );
    let identity = &f["test.fixture.identity"];
    assert!(Arc::ptr_eq(
        &identity.handler.permits,
        &identity.function.handler.permits
    ));
    let input = json!({"message": 1, "output": "data.x"});
    let held = Arc::clone(&identity.handler.permits)
        .acquire_owned()
        .await
        .expect("the permit is free");
    let v4 = run(identity, input.clone(), json!({})).await;
    twin::pinned(&v4, "permit/held");
    let failure = v4.as_ref().expect_err("no permit frees");
    assert!(
        failure
            .message
            .contains("no concurrency permit within 100ms (1 already running)"),
        "{failure:?}"
    );
    drop(held);
    let v4 = run(identity, input, json!({})).await;
    assert!(v4.is_ok(), "{v4:?}");
    twin::pinned(&v4, "permit/freed");
}

/// A run deadline before the plugin's own ceiling ends the call
/// `TimedOut`, the engine's race, with nothing written; the ceiling's
/// own refusal is 1.12's (above).
#[tokio::test]
async fn a_run_deadline_first_ends_the_call_timed_out_with_nothing_written() {
    let f = fixture();
    let called = testing::call(
        f["test.fixture.spin"].function.clone(),
        json!({"output": "data.x"}),
    )
    .exprs(test_exprs())
    .name("test.fixture.spin")
    .deadline(RunInstant::now() + Duration::from_millis(50))
    .await;
    // The run's own `TimedOut`, not the ceiling's `TIMEOUT_ERROR`.
    let e = called.outcome.expect_err("the run ends first");
    assert_eq!(*e.kind(), ErrorKind::TimedOut);
    assert!(called.writes.is_empty());
}

/// What 1.12 refused when its engine was built (`parse_input_with` over
/// the entry's table) is refused at load: a required field missing, an
/// unknown one, a literal or a template scalar of the wrong kind, an `output` that is not written as
/// a string, a `resolvable` field that is neither its kind nor one `var`
/// node, and an input that is not an object.
#[tokio::test]
async fn what_1_12_refused_at_load_is_refused_at_load() {
    let f = fixture();
    let rich = functions_of(RICH, &config());
    let identity = &f["test.fixture.identity"];
    let upper = &f["test.fixture.upper"];
    let any = &rich["test.fixture.identity"];
    for (both, input, v4_at) in [
        (identity, json!({"output": "data.x"}), "message"),
        (
            upper,
            json!({"txt": "x", "text": "y", "output": "data.x"}),
            "txt",
        ),
        (upper, json!({"text": 5, "output": "data.x"}), "text"),
        (upper, json!({"text": null, "output": "data.x"}), "text"),
        (
            identity,
            json!({"message": 1, "output": {"var": "data.where"}}),
            "output",
        ),
        (
            identity,
            json!({"message": 1, "output": {"cat": ["data.", "x"]}}),
            "output",
        ),
        (identity, json!({"message": 1, "output": 5}), "output"),
        (any, json!({"label": 5, "output": "data.x"}), "label"),
        (any, json!({"text": 5, "output": "data.x"}), "text"),
        (any, json!({"text": null, "output": "data.x"}), "text"),
        (
            any,
            json!({"text": {"var": "data.t", "or": "x"}, "output": "data.x"}),
            "text",
        ),
        (any, json!(["not", "an", "object"]), ""),
    ] {
        let issues = v4_refusal(&both.function, input.clone()).await;
        assert!(
            issues.iter().any(|(at, _)| at == v4_at),
            "{input}: {issues:?}"
        );
    }
    // And the kind 1.12 admitted, a `var` node in a `resolvable` field,
    // loads.
    let called = v4_call(
        &any.function,
        json!({"text": {"var": "data.t"}, "output": "data.x"}),
        json!({"t": "x"}),
    )
    .await;
    assert!(called.outcome.is_ok(), "{:?}", called.issues);
}

/// The named breaks (I183): what 1.12 loaded and then failed on every
/// call is refused at load. A task naming no `output` where the
/// function declares no default root (the field is required); an empty
/// or blank `output`; a template whose constant is not of its kind,
/// which 1.12 admitted as a possible operator call and refused at every
/// call once it evaluated.
#[tokio::test]
async fn what_1_12_failed_on_every_call_is_refused_at_load() {
    let f = fixture();
    let identity = &f["test.fixture.identity"];
    let upper = &f["test.fixture.upper"];
    for (both, input, v4_at) in [
        (&f["test.fixture.trap"], json!({}), "output"),
        (identity, json!({"message": 1, "output": ""}), "output"),
        (identity, json!({"message": 1, "output": "  "}), "output"),
        (upper, json!({"text": ["a"], "output": "data.x"}), "text"),
    ] {
        let issues = v4_refusal(&both.function, input.clone()).await;
        assert!(
            issues.iter().any(|(at, _)| at == v4_at),
            "{input}: {issues:?}"
        );
    }
    // O3's ruled class: a string naming no context root, which 1.12
    // wrote where no response shows, is no path.
    let input = json!({"message": 1, "output": "result"});
    assert_eq!(
        v4_refusal(&identity.function, input).await,
        [("output".to_string(), IssueCode::InvalidPath)]
    );
}

/// A plugin never sees key material. A `{"secret": …}` read in a
/// template field, which 1.12 refused at authoring and at load evaluated
/// into the guest's input, is refused at load, recorded as the guest's
/// result may be (`SECRET_IN_MESSAGE_WRITE`); in an interpolated field
/// it is data, refused as such (`SECRET_IN_DOCUMENT`). The authoring
/// refusal is `validation`'s, pinned by
/// `a_plugin_function_refuses_a_secret_node_in_every_field`.
#[tokio::test]
async fn a_secret_read_in_a_plugins_input_is_refused_at_load() {
    let rich = functions_of(RICH, &config());
    let any = &rich["test.fixture.identity"];
    for (input, code) in [
        (
            json!({"greeting": {"secret": "api_key"}, "output": "data.x"}),
            IssueCode::SecretInMessageWrite,
        ),
        (
            json!({"greeting": {"cat": ["k=", {"secret": "api_key"}]}, "output": "data.x"}),
            IssueCode::SecretInMessageWrite,
        ),
        (
            json!({"message": {"k": {"secret": "api_key"}}, "output": "data.x"}),
            IssueCode::SecretInDocument,
        ),
    ] {
        let issues = v4_refusal(&any.function, input.clone()).await;
        assert!(
            issues.iter().any(|(_, c)| *c == code),
            "{input}: {issues:?}"
        );
    }
}

/// Two values 1.12 refused at load (`TYPE_MISMATCH`) that load now, pinned
/// for the report's concern: a `null` in an optional literal field reads
/// as unset (I53), so the guest receives no such field; and a `null`
/// output keeps its default root.
#[tokio::test]
async fn a_null_that_1_12_refused_at_load_loads_as_unset() {
    let rich = functions_of(RICH, &config());
    let any = &rich["test.fixture.identity"];
    let input = json!({"mode": null, "label": "x", "output": "data.echoed"});
    let called = v4_call(&any.function, input, json!({})).await;
    assert!(called.outcome.is_ok(), "{:?}", called.issues);
    assert_eq!(
        Value::from(called.message.context().data())["echoed"],
        json!({"label": "x"})
    );

    let identity = &fixture()["test.fixture.identity"];
    let input = json!({"message": 1, "output": null});
    let called = v4_call(&identity.function, input, json!({"before": 1})).await;
    assert!(called.outcome.is_ok(), "{:?}", called.issues);
    assert_eq!(
        Value::from(called.message.context().data()),
        json!({"message": 1})
    );
}

/// I185 for a result that may be an object and may not: an `output` at
/// a literal context root, the default root included, draws an Advisory
/// at load, and a result that is not an object fails the call there
/// (break 21), where 1.12 replaced the root with it. Below a root,
/// nothing is said.
#[tokio::test]
async fn a_root_output_is_advised_at_load_and_refuses_a_result_that_is_not_an_object() {
    let f = fixture();
    let advisories = |called: &testing::Called| -> Vec<(String, IssueCode, Severity)> {
        called
            .issues
            .iter()
            .map(|i| (i.path.to_string(), i.code, i.severity))
            .collect()
    };
    let advised = [(
        "output".to_string(),
        IssueCode::InputParse,
        Severity::Advisory,
    )];

    let big = &f["test.fixture.big-output"];
    let input = json!({"size": 3, "output": "data"});
    let called = v4_call(&big.function, input, json!({"before": 1})).await;
    assert_eq!(advisories(&called), advised);
    let e = called.outcome.expect_err("a root takes only an object");
    assert_eq!(*e.kind(), ErrorKind::Config);
    assert!(
        e.message().contains("is a root and takes only an object"),
        "{e}"
    );
    assert!(called.writes.is_empty());

    // The default root: advised, and an object written there as before.
    let wrap = &f["test.fixture.wrap"];
    let called = v4_call(&wrap.function, json!({"message": "hello"}), json!({})).await;
    assert_eq!(advisories(&called), advised);
    assert!(called.outcome.is_ok());

    let called = v4_call(
        &wrap.function,
        json!({"message": "hello", "output": "temp_data.wrapped"}),
        json!({}),
    )
    .await;
    assert!(called.issues.is_empty(), "{:?}", called.issues);
}

/// I43: a name reserved for a built-in is refused where
/// `Registry::register` would panic on it, and the built-in keeps it;
/// every other name registers.
#[test]
fn registration_refuses_a_reserved_name_where_register_would_panic() {
    let f = fixture();
    let identity = &f["test.fixture.identity"];
    let renamed = |name: &str| {
        let mut entry = (*identity.handler.entry).clone();
        entry.name = name.to_string();
        let handler = PluginFunctionHandler::new(
            Arc::new(entry),
            identity.handler.loaded.clone(),
            identity.handler.runtime.clone(),
            identity.handler.limits,
        );
        PluginFunction::new(handler, identity.function.fields().to_vec())
    };
    let mut registry = dataflow_rs::Registry::new(test_exprs());
    let refused = crate::engine::register_plugin_functions(
        &mut registry,
        [
            renamed("map"),
            identity.function.clone(),
            renamed("validate"),
            renamed("parse_json"),
        ],
    );
    assert_eq!(refused, ["map", "validate", "parse_json"]);
    let hosts: Vec<&str> = registry
        .catalogue()
        .filter(|info| !info.builtin)
        .map(|info| info.name)
        .collect();
    assert_eq!(hosts, ["test.fixture.identity"]);
    assert!(
        registry
            .catalogue()
            .any(|info| info.builtin && info.name == "map")
    );

    for name in dataflow_rs::def::BUILTIN_FUNCTIONS {
        assert!(crate::engine::is_reserved_function_name(name), "{name}");
    }
    assert!(crate::engine::is_reserved_function_name("validate"));
    assert!(!crate::engine::is_reserved_function_name(
        "test.fixture.identity"
    ));
    // What the refusal stands in front of.
    let mut bare = dataflow_rs::Registry::new(test_exprs());
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        bare.register("map", renamed("map"));
    }));
    assert!(panicked.is_err(), "register panics on a built-in's name");
}
