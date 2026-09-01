//! A1 (Tier-1 ergonomics): schema-validated function input contracts.
//!
//! Verifies that:
//!   - workflow create rejects tasks with missing/typed-wrong function inputs
//!     and returns field-pathed details via A3's envelope
//!   - the POST /workflows/validate endpoint reports the same issues in its
//!     `errors[]` array
//!   - GET /api/v1/admin/functions returns the registered schema list

use crate::common::{body_json, json_request, test_app};
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn create_workflow_with_missing_function_input_field_returns_field_pathed_details() {
    let app = test_app().await;
    // db_read requires connector + query. Omit connector.
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "broken-db-read",
                "tasks": [{
                    "id": "t1",
                    "name": "read",
                    "function": {
                        "name": "db_read",
                        "input": { "query": "SELECT 1" }
                    }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let details = body["error"]["details"]
        .as_array()
        .expect("expected field-pathed details");
    assert!(
        details
            .iter()
            .any(|d| d["path"] == "tasks[0].function.input.connector" && d["code"] == "REQUIRED"),
        "details should report missing connector field, got {body:?}"
    );
}

#[tokio::test]
async fn create_workflow_with_wrong_input_type_returns_type_mismatch_with_got() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "wrong-types",
                "tasks": [{
                    "id": "t1",
                    "name": "read",
                    "function": {
                        "name": "cache_read",
                        "input": { "connector": 42, "key": "k" }
                    }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let details = body["error"]["details"].as_array().unwrap();
    let mismatch = details
        .iter()
        .find(|d| d["path"] == "tasks[0].function.input.connector")
        .expect("expected TYPE_MISMATCH on connector");
    assert_eq!(mismatch["code"], "TYPE_MISMATCH");
    assert_eq!(mismatch["expected"], "string");
    assert_eq!(mismatch["got"], 42);
}

#[tokio::test]
async fn create_workflow_with_valid_inputs_succeeds() {
    let app = test_app().await;
    // Use cache_read with all required fields — must NOT trip the schema check.
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "happy-path",
                "tasks": [{
                    "id": "t1",
                    "name": "read",
                    "function": {
                        "name": "cache_read",
                        "input": { "connector": "my-cache", "key": "k" }
                    }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn validate_endpoint_returns_schema_errors_in_errors_array() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows/validate",
            Some(json!({
                "name": "test-validate",
                "tasks": [{
                    "id": "t1",
                    "name": "broken",
                    "function": { "name": "mongo_read", "input": {} }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["valid"], false);
    let errs = body["data"]["errors"].as_array().unwrap();
    let paths: Vec<&str> = errs
        .iter()
        .map(|e| e["field"].as_str().unwrap_or(""))
        .collect();
    assert!(paths.contains(&"tasks[0].function.input.connector"));
    assert!(paths.contains(&"tasks[0].function.input.database"));
    assert!(paths.contains(&"tasks[0].function.input.collection"));
}

/// The endpoint is a catalogue of every name a workflow may use, not just the
/// ones Orion input-validates. It served 18 of 27 until #288 — omitting `map`,
/// `filter`, `parse_json` and the rest, which are the most-used functions
/// there are, so anything completing from it offered the connector functions
/// and none of the ones people type.
/// The catalogue lists exactly what the serving engine will dispatch.
///
/// #288 was this drift: the endpoint served the schema registry, so it listed
/// the 18 functions Orion input-validates and omitted `map`, `filter`,
/// `parse_json` and the rest — the most-used names, missing from every
/// completion source built on it. The fix added an `ENGINE_BUILTINS` table,
/// which is a second list that can drift the same way.
///
/// dataflow-rs 3.7 makes the question answerable rather than mirrored:
/// `dispatchable_functions()` reports what the *live engine* will actually run
/// — self-contained built-ins, plus every name with a registered handler, with
/// alternative spellings grouped as aliases. Asserting set equality against
/// the running engine catches both directions: a new engine built-in Orion
/// never catalogued, and a catalogued name nothing can dispatch.
///
/// Spellings are compared as one set — canonical names and aliases together —
/// deliberately. Upstream's canonical spelling of the validation function is
/// `validate` with `validation` as its alias; Orion's catalogue presents them
/// the other way round, because that is the spelling its docs and stored
/// workflows use. Which one is canonical is a presentation choice; that both
/// are offered, and that nothing else is, is the contract.
#[tokio::test]
async fn the_catalogue_matches_what_the_engine_can_dispatch() {
    use std::collections::BTreeSet;

    let state = crate::common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    let dispatchable: BTreeSet<String> = state
        .engine
        .load()
        .dispatchable_functions()
        .flat_map(|f| {
            std::iter::once(f.name.to_string())
                .chain(f.aliases.iter().map(|a| a.to_string()))
                .collect::<Vec<_>>()
        })
        .collect();

    let resp = app
        .oneshot(json_request("GET", "/api/v1/admin/functions", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let data = body["data"].as_array().expect("data must be an array");

    let catalogued: BTreeSet<String> = data
        .iter()
        .flat_map(|f| {
            let name = f["name"]
                .as_str()
                .expect("every entry has a name")
                .to_string();
            let aliases = f["aliases"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            std::iter::once(name).chain(aliases)
        })
        .collect();

    assert_eq!(
        catalogued.difference(&dispatchable).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "the catalogue offers names the engine cannot dispatch — a workflow \
         using one would be accepted and then fail at its first request"
    );
    assert_eq!(
        dispatchable.difference(&catalogued).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "the engine dispatches names the catalogue does not offer — this is \
         exactly the #288 gap, where the most-used functions were missing from \
         every completion source built on this endpoint"
    );
}

/// The create-time gate agrees with the runtime.
///
/// `is_known_function` decides whether a workflow may *name* a function, and
/// it has to answer without an engine — validation runs before one exists, and
/// `CUSTOM_HANDLER_FUNCTIONS` is Orion's declaration of what it registers.
/// dataflow-rs 3.7's `can_dispatch` answers the same question against a real
/// registry, so the two can finally be checked against each other instead of
/// only against themselves.
///
/// Both directions matter and each has bitten before. A name create accepts
/// that nothing dispatches is F54's `enrich`: it activated cleanly and failed
/// every request with `FunctionNotFound`. A name the engine dispatches that
/// create rejects is a handler someone registered and forgot to declare, so a
/// workflow using it is refused for no reason the author can act on.
#[tokio::test]
async fn the_create_time_gate_agrees_with_the_running_engine() {
    use std::collections::BTreeSet;

    let state = crate::common::test_state_with_config(orion::config::AppConfig::default()).await;
    let engine = state.engine.load();

    for name in orion::engine::known_functions() {
        assert!(
            engine.can_dispatch(name),
            "create accepts '{name}', but the serving engine would fail every \
             message that reaches it with FunctionNotFound"
        );
    }

    // The other way: aliases are excluded because `known_functions` yields the
    // canonical spelling, which is the one a `BUILTIN_FUNCTION_NAMES` entry
    // carries.
    let declared: BTreeSet<&str> = orion::engine::known_functions().collect();
    for f in engine.dispatchable_functions() {
        assert!(
            declared.contains(f.name),
            "the engine dispatches '{}', but create would refuse a workflow \
             naming it",
            f.name
        );
    }
}

#[tokio::test]
async fn list_functions_serves_every_valid_name() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request("GET", "/api/v1/admin/functions", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let data = body["data"].as_array().expect("data must be an array");
    let names: Vec<&str> = data.iter().filter_map(|f| f["name"].as_str()).collect();

    // An engine built-in declares no schema, and says so by omission rather
    // than by a null — a consumer branches on presence.
    let map = data.iter().find(|f| f["name"] == "map").expect("map");
    assert_eq!(map["source"], "engine");
    assert!(
        map.get("input_fields").is_none(),
        "an engine built-in must omit input_fields, not null it: {map}"
    );
    assert_eq!(map["category"], "data");

    // The alias rides on its function rather than becoming a second entry.
    assert!(
        !names.contains(&"validate"),
        "an alias must not be catalogued as its own function"
    );
    let validation = data
        .iter()
        .find(|f| f["name"] == "validation")
        .expect("validation");
    assert_eq!(validation["aliases"][0], "validate");

    // An Orion handler is unchanged: source `orion`, schema present.
    let cache_read = data
        .iter()
        .find(|f| f["name"] == "cache_read")
        .expect("cache_read");
    assert_eq!(cache_read["source"], "orion");
    assert!(cache_read["input_fields"].is_array());
    assert!(
        cache_read.get("aliases").is_none(),
        "an empty alias list must be omitted"
    );
}

#[tokio::test]
async fn list_functions_returns_registry_with_schemas() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request("GET", "/api/v1/admin/functions", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let data = body["data"].as_array().expect("data must be an array");
    let names: Vec<&str> = data.iter().filter_map(|f| f["name"].as_str()).collect();
    assert!(names.contains(&"cache_read"));
    assert!(names.contains(&"db_read"));
    assert!(names.contains(&"channel_call"));

    // Spot-check one entry's input field schema.
    let cache_read = data
        .iter()
        .find(|f| f["name"] == "cache_read")
        .expect("cache_read must be present");
    let fields = cache_read["input_fields"].as_array().unwrap();
    let connector = fields
        .iter()
        .find(|f| f["name"] == "connector")
        .expect("connector field must be present");
    assert_eq!(connector["required"], true);
    assert_eq!(connector["kind"], "string");
}

#[tokio::test]
async fn channel_call_without_channel_or_logic_is_rejected() {
    let app = test_app().await;
    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "broken-channel-call",
                "tasks": [{
                    "id": "t1",
                    "name": "call",
                    "function": { "name": "channel_call", "input": {} }
                }]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    let details = body["error"]["details"].as_array().unwrap();
    assert!(
        details
            .iter()
            .any(|d| d["path"] == "tasks[0].function.input.channel" && d["code"] == "REQUIRED"),
        "should report channel_call needs a channel, got {body:?}"
    );
}
