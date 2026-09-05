//! The cross-language ABI contract, as data: `tests/fixtures/plugins/abi-cases.json`.
//!
//! Every SDK's `identity` must satisfy these cases — Unicode, integers past
//! 2^53, nested nulls, empty input, and an error round-trip with its code and
//! class intact. The cases are JSON rather than Rust so a second SDK is held
//! to the same file, and the component under test is the committed fixture
//! unless `ORION_PLUGIN_FIXTURE` names another: the `plugin-sdk` CI job
//! rebuilds the fixture from source on the shipped SDK and points this suite
//! at the fresh bytes, which is what makes "supported SDK" mean something.

use serde::Deserialize;
use serde_json::Value;

use orion::config::PluginsConfig;
use orion::plugin::{Category, Limits, WasmRuntime};

#[derive(Deserialize)]
struct Case {
    name: String,
    /// The label under `test.fixture.` — every SDK's fixture exports the same
    /// set, so the cases name functions the same way.
    function: String,
    input: Value,
    #[serde(default)]
    expect: Option<Value>,
    #[serde(default)]
    expect_error: Option<ExpectedError>,
}

#[derive(Deserialize)]
struct ExpectedError {
    code: String,
    class: String,
}

/// A relative `ORION_PLUGIN_FIXTURE` is resolved against the **workspace
/// root**, not the process's working directory: cargo runs a test binary from
/// the package directory, so the repo-relative path a CI step (or anyone
/// standing at the repo root) writes would otherwise resolve under
/// `crates/orion-server/` and miss.
fn fixture_override(path: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(path);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}

fn component() -> Vec<u8> {
    match std::env::var("ORION_PLUGIN_FIXTURE") {
        Ok(path) => {
            let path = fixture_override(&path);
            std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
        }
        Err(_) => include_bytes!("../fixtures/plugins/fixture.wasm").to_vec(),
    }
}

#[tokio::test]
async fn every_abi_case_holds_for_the_fixture_component() {
    let cases: Vec<Case> =
        serde_json::from_str(include_str!("../fixtures/plugins/abi-cases.json")).expect("cases");
    assert!(cases.len() >= 5, "the case file has been emptied");

    let config = PluginsConfig {
        enabled: true,
        ..PluginsConfig::default()
    };
    let runtime = WasmRuntime::new(&config).expect("engine builds");
    let loaded = runtime
        .load_blocking(&component())
        .expect("component loads");
    let limits = Limits::effective(&config, "test.fixture");

    let mut failures = Vec::new();
    for case in &cases {
        let function = format!("test.fixture.{}", case.function);
        let input = serde_json::to_string(&case.input).expect("input json");
        let outcome = runtime.invoke(&loaded, &limits, &function, &input).await;
        match (&case.expect, &case.expect_error, outcome) {
            (Some(expected), None, Ok(text)) => {
                let actual: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        failures.push(format!("{}: returned non-JSON {text:?}: {e}", case.name));
                        continue;
                    }
                };
                if &actual != expected {
                    failures.push(format!("{}: expected {expected}, got {actual}", case.name));
                }
            }
            (Some(_), None, Err(invocation)) => failures.push(format!(
                "{}: expected a value, the call failed: {:?}",
                case.name,
                invocation.into_failure()
            )),
            (None, Some(expected), Err(invocation)) => {
                let failure = invocation.into_failure();
                let class = match failure.category {
                    Category::CallerInput => "caller-input",
                    Category::GuestError => "internal",
                    other => other.as_str(),
                };
                if class != expected.class || !failure.message.contains(&expected.code) {
                    failures.push(format!(
                        "{}: expected a {} error with code {}, got category {} and message {:?}",
                        case.name,
                        expected.class,
                        expected.code,
                        failure.category.as_str(),
                        failure.message
                    ));
                }
            }
            (None, Some(expected), Ok(text)) => failures.push(format!(
                "{}: expected a {} error, the call returned {text}",
                case.name, expected.class
            )),
            _ => failures.push(format!(
                "{}: a case names exactly one of `expect` and `expect_error`",
                case.name
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "ABI cases failed:\n  {}",
        failures.join("\n  ")
    );
}
