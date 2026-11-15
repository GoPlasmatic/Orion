use super::*;
use serde_json::json;

#[test]
fn unknown_function_returns_no_errors() {
    // Functions without registered schemas pass through — keeps the door
    // open for ad-hoc dataflow-rs functions that haven't been catalogued.
    let errs = validate_input("nope", &json!({}), "tasks[0]");
    assert!(errs.is_empty());
}

#[test]
fn cache_read_missing_connector_is_required_error() {
    let errs = validate_input("cache_read", &json!({"key": "k"}), "tasks[0]");
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].path, "tasks[0].function.input.connector");
    assert_eq!(errs[0].code, "REQUIRED");
}

#[test]
fn cache_read_full_input_validates() {
    let errs = validate_input(
        "cache_read",
        &json!({"connector": "c", "key": "k", "output": "data.out"}),
        "tasks[0]",
    );
    assert!(errs.is_empty(), "{:?}", errs);
}

#[test]
fn type_mismatch_reports_expected_and_got() {
    let errs = validate_input(
        "cache_read",
        &json!({"connector": 42, "key": "k"}),
        "tasks[1]",
    );
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].code, "TYPE_MISMATCH");
    assert_eq!(errs[0].path, "tasks[1].function.input.connector");
    assert_eq!(errs[0].expected.as_ref().expect("test"), &json!("string"));
    assert_eq!(errs[0].got.as_ref().expect("test"), &json!(42));
}

/// A computed connector is refused once with the reason a static name is
/// required, even when the ordinary kind check also sees a non-string.
#[test]
fn a_computed_connector_is_refused_with_the_reason() {
    let errs = validate_input(
        "http_call",
        &json!({"connector": {"var": "data.which"}}),
        "tasks[0]",
    );
    let connector: Vec<_> = errs
        .iter()
        .filter(|e| e.path == "tasks[0].function.input.connector")
        .collect();
    assert_eq!(connector.len(), 1, "{errs:?}");
    assert_eq!(connector[0].code, "TYPE_MISMATCH");
    assert!(connector[0].message.contains("literal connector name"));
}

/// The other parameters of the same function stay computable — the limit is
/// the connector, not the config.
#[test]
fn the_other_http_call_parameters_stay_computable() {
    let errs = validate_input(
        "http_call",
        &json!({
            "connector": "api",
            "path": {"cat": ["/o/", {"var": "data.id"}]},
            "timeout_ms": {"var": "data.t"},
            "headers": {"X": {"var": "data.h"}}
        }),
        "tasks[0]",
    );
    assert!(errs.is_empty(), "{errs:?}");
}

#[test]
fn non_object_input_emits_single_type_error() {
    let errs = validate_input("cache_read", &json!("not an object"), "tasks[0]");
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].path, "tasks[0].function.input");
    assert_eq!(errs[0].code, "TYPE_MISMATCH");
}

#[test]
fn data_query_collects_all_missing_required_at_once() {
    let errs = validate_input("data_query", &json!({}), "tasks[0]");
    let paths: Vec<&str> = errs.iter().map(|e| e.path.as_str()).collect();
    assert!(paths.contains(&"tasks[0].function.input.connector"));
    assert!(paths.contains(&"tasks[0].function.input.query"));
}

/// One field carries both the literal and the computed spelling, so "name a
/// target" is the field's own `required` rather than a cross-field rule
/// over a pair — and the error points at the field instead of the input.
#[test]
fn channel_call_needs_a_channel() {
    let errs = validate_input("channel_call", &json!({}), "tasks[0]");
    assert!(errs.iter().any(|e| e.code == "REQUIRED"
        && e.path == "tasks[0].function.input.channel"
        && e.message.contains("channel_call")));
}

/// The pre-1.0 spelling names nothing, so the target is still missing.
#[test]
fn the_pre_1_0_channel_logic_spelling_names_no_target() {
    let errs = validate_input(
        "channel_call",
        &json!({"channel_logic": {"var": "data.target"}}),
        "tasks[0]",
    );
    assert!(
        errs.iter()
            .any(|e| e.code == "REQUIRED" && e.path == "tasks[0].function.input.channel"),
        "{errs:?}"
    );
}

/// A computed channel is an expression, so its kind describes what it must
/// evaluate to and the authored object is not checked against it.
#[test]
fn a_computed_channel_is_not_type_checked_against_string() {
    let errs = validate_input(
        "channel_call",
        &json!({"channel": {"cat": ["orders-", {"var": "data.region"}]}}),
        "tasks[0]",
    );
    assert!(errs.is_empty(), "{errs:?}");
}

/// A non-string scalar still is: it is unambiguously itself in JSONLogic,
/// so it is a channel name that is not a string.
#[test]
fn a_scalar_channel_of_the_wrong_type_is_still_caught() {
    let errs = validate_input("channel_call", &json!({"channel": 7}), "tasks[0]");
    assert!(
        errs.iter()
            .any(|e| e.code == "TYPE_MISMATCH" && e.path.ends_with(".channel")),
        "{errs:?}"
    );
}

#[test]
fn channel_call_with_static_channel_is_ok() {
    let errs = validate_input(
        "channel_call",
        &json!({"channel": "downstream"}),
        "tasks[0]",
    );
    assert!(errs.is_empty(), "{:?}", errs);
}

#[test]
fn channel_call_with_dynamic_logic_is_ok() {
    let errs = validate_input(
        "channel_call",
        &json!({"channel": {"var": "data.target"}}),
        "tasks[0]",
    );
    assert!(errs.is_empty(), "{:?}", errs);
}

#[test]
fn http_call_unknown_format_values_are_authoring_time_errors() {
    let errs = validate_input(
        "http_call",
        &json!({"connector": "c", "body_format": "multipart", "response_format": "base64"}),
        "tasks[0]",
    );
    assert_eq!(errs.len(), 2, "{errs:?}");
    assert_eq!(errs[0].path, "tasks[0].function.input.body_format");
    assert_eq!(errs[0].code, "INVALID");
    assert_eq!(errs[1].path, "tasks[0].function.input.response_format");
    assert_eq!(errs[1].code, "INVALID");
}

#[test]
fn http_call_known_format_values_validate() {
    let errs = validate_input(
        "http_call",
        &json!({
            "connector": "c",
            "method": "POST",
            "body_format": "form",
            // Scalars, an array of scalars, a null, and a bracket-path
            // key — the full supported form surface.
            "body": {
                "grant_type": "refresh_token",
                "retries": 3,
                "to": ["+15551111111", "+15552222222"],
                "optional": null,
                "metadata[order_id]": "6735",
            },
            "response_format": "text",
            "output": "temp_data.token",
        }),
        "tasks[0]",
    );
    assert!(errs.is_empty(), "{errs:?}");
}

#[test]
fn http_call_static_body_is_shape_checked_against_the_format() {
    // A nested value under 'form' is caught at authoring time by the same
    // encoder the request path runs.
    let errs = validate_input(
        "http_call",
        &json!({"connector": "c", "body_format": "form", "body": {"bad": {"nested": 1}}}),
        "tasks[0]",
    );
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].path, "tasks[0].function.input.body");
    assert_eq!(errs[0].code, "INVALID");
    assert!(errs[0].message.contains("'bad'"), "{}", errs[0].message);

    // 'text' requires a string body.
    let errs = validate_input(
        "http_call",
        &json!({"connector": "c", "body_format": "text", "body": {"a": 1}}),
        "tasks[0]",
    );
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].code, "INVALID");

    // A computed body only exists per message — nothing to check here.
    let errs = validate_input(
        "http_call",
        &json!({"connector": "c", "body_format": "form", "body": {"var": "data.form"}}),
        "tasks[0]",
    );
    assert!(errs.is_empty(), "{errs:?}");
}

#[test]
fn registry_is_non_empty_and_contains_all_known_connector_functions() {
    let names: Vec<&str> = schema::registry().iter().map(|s| s.name).collect();
    assert!(names.contains(&"cache_read"));
    assert!(names.contains(&"cache_write"));
    assert!(names.contains(&"db_read"));
    assert!(names.contains(&"db_write"));
    assert!(names.contains(&"channel_call"));
    assert!(names.contains(&"http_call"));
}
