//! The `FunctionRegistry` contracts the plugin work depends on, pinned before
//! a plugin can exist.
//!
//! The registry replaced three static tables and six free functions as the
//! one answer to "what may a workflow name, and what does it accept". Four
//! things have to stay true for that to be safe:
//!
//! - what it declares is what `build_custom_functions` registers, name for
//!   name — a handler without an entry is unusable for no visible reason, an
//!   entry without a handler fails every request;
//! - the catalogue route serves the *generation's* registry, so an entry a
//!   generation carries is what a tool sees;
//! - create-time validation reads the same registry, so a workflow is
//!   accepted against exactly the set the engine it will run on dispatches;
//! - `fmt` reads none of it. One style everywhere is its whole value, so an
//!   entry only a generation knows must not change how a file is laid out.

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use crate::common::{body_json, json_request, test_app, test_state_with_config};
use orion::engine::functions::schema::{FieldKind, RetrySafety, Source, WriteShape};
use orion::engine::{FieldSpec, FunctionEntry, FunctionRegistry, PluginBinding};

/// An entry the static tables cannot contain: what a loaded plugin would add.
fn plugin_entry(name: &str) -> FunctionEntry {
    FunctionEntry {
        name: name.to_string(),
        description: "Echo the message".to_string(),
        category: "transform".to_string(),
        source: Source::Plugin,
        aliases: Vec::new(),
        input_fields: Some(vec![
            FieldSpec {
                name: "message".to_string(),
                description: "The message to echo".to_string(),
                kind: FieldKind::String,
                required: true,
                resolvable: true,
                secret_at: &[],
                template_at: &[],
                alias: None,
            },
            // Implicit on every plugin function (the `OutputPath` contract);
            // the manifest conversion appends it, so the entry declares it.
            FieldSpec {
                name: "output".to_string(),
                description: "Where the result is written".to_string(),
                kind: FieldKind::String,
                required: false,
                resolvable: false,
                secret_at: &[],
                template_at: &[],
                alias: None,
            },
        ]),
        writes: WriteShape::OutputPath {
            default_root: Some("data"),
        },
        retry_safety: RetrySafety::Pure,
        deny_unknown: true,
        validate_static: None,
        connector: None,
        plugin: Some(PluginBinding {
            id: "acme.echo".to_string(),
            version: 1,
            digest: "sha256:0000".to_string(),
            abi: "orion:plugin@1.0.0".to_string(),
        }),
    }
}

/// The registry's Orion entries are exactly the handlers Orion constructs.
///
/// This is the pin the retired `CUSTOM_HANDLER_FUNCTIONS` constant cited by
/// name and never actually had: its doc comment named a test that did not
/// exist. The live-engine test in `function_schema_test` covers it
/// transitively; this one asks the handler map directly, so a handler added
/// to `build_custom_functions` without a schema row fails here with both
/// names in the message.
#[tokio::test]
async fn the_registry_declares_exactly_the_handlers_orion_registers() {
    use std::collections::BTreeSet;

    let state = test_state_with_config(orion::config::AppConfig::default()).await;
    let handlers = orion::engine::build_custom_functions(orion::runtime::handler_deps(&state));
    let registered: BTreeSet<&str> = handlers.keys().map(String::as_str).collect();
    let declared: BTreeSet<&str> = FunctionRegistry::builtin()
        .entries()
        .filter(|e| e.source == Source::Orion)
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(
        declared, registered,
        "the registry's Orion entries and build_custom_functions' keys must be the same set"
    );
    assert_eq!(declared.len(), 18);
}

/// `GET /admin/functions` is the generation's registry, serialised — and a
/// built-in entry's wire shape carries no `plugin` key.
#[tokio::test]
async fn the_catalogue_route_serves_the_generations_registry() {
    let state = test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/functions", None))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let served = body_json(resp).await["data"].clone();
    let expected = serde_json::to_value(state.runtime.load().functions.catalogue()).expect("json");
    assert_eq!(served, expected);

    const KEYS: [&str; 7] = [
        "name",
        "description",
        "category",
        "source",
        "aliases",
        "input_fields",
        "retry_safety",
    ];
    for entry in served.as_array().expect("array") {
        for key in entry.as_object().expect("object").keys() {
            assert!(
                KEYS.contains(&key.as_str()),
                "'{}' serves an unexpected key '{key}' — the wire shape is pinned by \
                 docs/openapi.json",
                entry["name"]
            );
        }
    }

    // A generation carrying a plugin entry serves it, with the plugin block
    // and nothing else changed.
    let generation = state.runtime.load();
    let extended = generation
        .functions
        .with_entries(vec![plugin_entry("acme.echo.identity")])
        .expect("extends");
    state.runtime.publish(
        generation.engine.clone(),
        generation.channels.clone(),
        Arc::new(extended),
    );
    let resp = app
        .oneshot(json_request("GET", "/api/v1/admin/functions", None))
        .await
        .expect("request");
    let served = body_json(resp).await["data"].clone();
    let entry = served
        .as_array()
        .expect("array")
        .iter()
        .find(|e| e["name"] == "acme.echo.identity")
        .expect("the plugin entry is served");
    assert_eq!(entry["source"], "plugin");
    assert_eq!(entry["plugin"]["id"], "acme.echo");
    assert_eq!(entry["plugin"]["digest"], "sha256:0000");
    assert_eq!(entry["retry_safety"]["kind"], "pure");
    assert_eq!(entry["input_fields"][0]["name"], "message");
}

/// Workflow creation is validated against the registry of the generation the
/// node is serving — not against the static tables.
///
/// The same request is refused with `UNKNOWN_FUNCTION` while no generation
/// knows the name, and accepted once one does. This is the property every
/// later phase builds on: a plugin's functions become nameable by publishing
/// a generation, and nothing else.
#[tokio::test]
async fn create_time_validation_reads_the_generations_registry() {
    let state = test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());
    let workflow = json!({
        "name": "Echo via plugin",
        "condition": true,
        "tasks": [{
            "id": "echo",
            "name": "echo",
            "function": {
                "name": "acme.echo.identity",
                "input": {"message": {"var": "data.raw"}, "output": "data.echoed"}
            }
        }]
    });

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(workflow.clone()),
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert!(
        body.to_string().contains("UNKNOWN_FUNCTION"),
        "an unregistered name is refused by code: {body}"
    );

    let generation = state.runtime.load();
    let extended = generation
        .functions
        .with_entries(vec![plugin_entry("acme.echo.identity")])
        .expect("extends");
    state.runtime.publish(
        generation.engine.clone(),
        generation.channels.clone(),
        Arc::new(extended),
    );

    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(workflow),
        ))
        .await
        .expect("request");
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "the same workflow is accepted once a generation's registry names the function"
    );

    // And the entry's own schema is what validates it: a field the entry does
    // not declare is refused, because the entry says `deny_unknown`.
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "Echo via plugin, misspelled",
                "condition": true,
                "tasks": [{
                    "id": "echo",
                    "name": "echo",
                    "function": {"name": "acme.echo.identity", "input": {"mesage": "x"}}
                }]
            })),
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await.to_string();
    assert!(body.contains("REQUIRED"), "{body}");
    assert!(body.contains("UNKNOWN_FIELD"), "{body}");
}

/// `fmt` never consults the registry: a function only a generation could
/// know keeps its author order, and a function the static tables know is
/// reordered — the same two inputs, formatted once, with nothing to pass.
#[test]
fn fmt_keeps_author_order_for_a_function_only_the_registry_could_know() {
    use orion::definitions::fmt::{Outcome, format_str};

    let format = |text: &str| match format_str(text, "wf.json").expect("formats") {
        Outcome::Unchanged => text.to_string(),
        Outcome::Changed(s) => s,
    };

    let plugin = format(
        r#"{"name":"p","tasks":[{"id":"t","name":"t","function":{"name":"acme.echo.identity","input":{"zeta":1,"alpha":2}}}]}"#,
    );
    let zeta = plugin.find("\"zeta\"").expect("zeta kept");
    let alpha = plugin.find("\"alpha\"").expect("alpha kept");
    assert!(
        zeta < alpha,
        "a function the static tables do not know keeps author order:\n{plugin}"
    );

    // The control: an Orion function's input is put in its field-table order
    // (`connector` before `key`), so the assertion above is about the name
    // and not about fmt leaving every input alone.
    let known = format(
        r#"{"name":"k","tasks":[{"id":"t","name":"t","function":{"name":"cache_read","input":{"key":"k","connector":"c"}}}]}"#,
    );
    let connector = known.find("\"connector\"").expect("connector kept");
    let key = known.find("\"key\"").expect("key kept");
    assert!(
        connector < key,
        "an Orion function's input is reordered to the field table:\n{known}"
    );
}

/// Loading the built-in registry is what interns its names for the metrics
/// observer, and the built-in set is closed: a name it does not hold interns
/// to nothing, which the observer collapses to one label.
#[test]
fn the_builtin_registry_is_the_static_tables_and_nothing_else() {
    let registry = FunctionRegistry::builtin();
    assert!(registry.entries().all(|e| e.source != Source::Plugin));
    assert!(registry.entries().all(|e| e.plugin.is_none()));
    let orion = registry
        .entries()
        .filter(|e| e.source == Source::Orion)
        .count();
    let engine = registry
        .entries()
        .filter(|e| e.source == Source::Engine)
        .count();
    assert_eq!((orion, engine), (18, 8));
    assert!(orion::engine::functions::registry::interned("acme.never.registered").is_none());
}

#[tokio::test]
async fn test_app_serves_the_builtin_catalogue() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request("GET", "/api/v1/admin/functions", None))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let names: Vec<String> = body_json(resp).await["data"]
        .as_array()
        .expect("array")
        .iter()
        .map(|e| e["name"].as_str().expect("name").to_string())
        .collect();
    let expected: Vec<String> = FunctionRegistry::builtin()
        .names()
        .map(str::to_string)
        .collect();
    assert_eq!(names, expected);
}
