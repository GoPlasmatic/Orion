//! Workflow definitions: what `POST /api/v2/admin/definitions` refuses and
//! accepts for a workflow, and what `POST /api/v2/admin/definitions/test`
//! runs. Upload strict-parses and compiles a workflow on the node's engine
//! and refuses anything that would quarantine it, so every "refused at
//! authoring time" rule here is a `400` naming the problem at its path
//! under `definition`.

use crate::common;

use crate::common::deploy;
use crate::common::{body_json, json_request};
use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

/// Upload a v1-shaped workflow body as a workflow definition, answering the
/// status and body as they are.
async fn upload_workflow(app: &axum::Router, workflow: Value) -> (StatusCode, Value) {
    deploy::try_upload(app, deploy::workflow_definition(&workflow)).await
}

/// The stored definition a digest names, as `GET /definitions/{digest}`
/// serves it.
async fn read_definition(app: &axum::Router, digest: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/api/v2/admin/definitions/{digest}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

// ============================================================
// Engine Status
// ============================================================

// ============================================================

#[tokio::test]
async fn test_engine_status_with_loaded_workflows() {
    let app = common::test_app().await;

    common::create_and_activate_channel(
        &app,
        "status-ch",
        common::simple_log_workflow("Status Check Workflow"),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v2/admin/engine/status", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["data"]["workflows_count"].as_i64().unwrap() >= 1);
    assert!(body["data"]["active_workflows"].as_i64().unwrap() >= 1);
    assert!(body["data"].get("channels").is_some());
    assert!(body["data"].get("version").is_some());
    assert!(body["data"].get("uptime_seconds").is_some());
    let channels = body["data"]["channels"].as_array().unwrap();
    assert!(channels.iter().any(|c| c == "status-ch"));
}

// ============================================================
// Task conditions
// ============================================================

#[tokio::test]
async fn a_task_condition_is_accepted_at_upload() {
    let app = common::test_app().await;

    let (status, body) = upload_workflow(
        &app,
        json!({
            "name": "Condition Test",
            "condition": true,
            "tasks": [{
                "id": "t1",
                "name": "Task with condition",
                "condition": {">": [{"var": "data.amount"}, 100]},
                "function": {"name": "log", "input": {"message": "test"}}
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// I188: upload compiles a condition on the serving engine, in templating
/// mode, where a multi-key object is a template emitting that object rather
/// than an invalid expression. A bare engine refused one ("Invalid JSONLogic
/// in … condition") that the serving engine compiles and runs.
#[tokio::test]
async fn upload_compiles_a_condition_as_the_serving_engine_does() {
    let app = common::test_app().await;

    let (status, body) = upload_workflow(
        &app,
        json!({
            "name": "Templated Condition",
            "condition": {"a": {"var": "data.amount"}, "b": 2},
            "tasks": [{
                "id": "t1",
                "name": "Task with a templated condition",
                "condition": {"!!": [{"a": 1, "b": 2}]},
                "function": {"name": "log", "input": {"message": "test"}}
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// I182, as refined: a Defect dataflow-rs 4 reports at load and does not
/// quarantine on, a `PAYLOAD_VAR` above all, does not refuse an upload. The
/// workflow deploys and its channel serves.
#[tokio::test]
async fn a_payload_var_defect_does_not_refuse_the_upload_and_the_channel_serves() {
    let app = common::test_app().await;
    let workflow = json!({
        "name": "Payload Read",
        "condition": true,
        "tasks": [{
            "id": "t1",
            "name": "Copy",
            "function": {"name": "map", "input": {"mappings": [
                {"path": "data.copied", "logic": {"var": "payload.x"}}
            ]}}
        }]
    });

    let (status, body) = upload_workflow(&app, workflow.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    common::create_and_activate_channel(&app, "payload-var-ch", workflow).await;
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/payload-var-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{}", body_json(resp).await);
}

#[tokio::test]
async fn test_http_call_format_axes_validated_at_upload() {
    let app = common::test_app().await;

    // Known values and a well-shaped static form body — scalars, an array of
    // scalars, a conditionally-null entry — are accepted.
    let (status, body) = upload_workflow(
        &app,
        json!({
            "name": "OAuth Token Call",
            "condition": true,
            "tasks": [{
                "id": "t1",
                "name": "Token",
                "function": {
                    "name": "http_call",
                    "input": {
                        "connector": "oauth",
                        "method": "POST",
                        "path": "/token",
                        "body_format": "form",
                        "body": {
                            "grant_type": "client_credentials",
                            "scope": ["read", "write"],
                            "audience": null
                        },
                        "response_format": "text",
                        "output": "temp_data.token"
                    }
                }
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Unknown format values are refused at upload — an authoring-time 400
    // naming both fields, never a request-time surprise.
    let (status, body) = upload_workflow(
        &app,
        json!({
            "name": "Bad Formats",
            "condition": true,
            "tasks": [{
                "id": "t1",
                "name": "Call",
                "function": {
                    "name": "http_call",
                    "input": {
                        "connector": "oauth",
                        "body_format": "multipart",
                        "response_format": "base64"
                    }
                }
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let details = body["error"]["details"].to_string();
    assert!(details.contains("body_format"), "{details}");
    assert!(details.contains("response_format"), "{details}");

    // A static body whose shape contradicts the format is caught the same way.
    let (status, body) = upload_workflow(
        &app,
        json!({
            "name": "Nested Form Body",
            "condition": true,
            "tasks": [{
                "id": "t1",
                "name": "Call",
                "function": {
                    "name": "http_call",
                    "input": {
                        "connector": "oauth",
                        "body_format": "form",
                        "body": {"metadata": {"nested": true}}
                    }
                }
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let details = body["error"]["details"].to_string();
    assert!(details.contains("metadata"), "{details}");
}

#[tokio::test]
async fn test_crypto_op_envelope_validated_at_upload() {
    let app = common::test_app().await;

    // The Zoom CRC shape from #259 — a valid hmac task is accepted.
    let (status, body) = upload_workflow(
        &app,
        json!({
            "name": "Zoom CRC",
            "condition": true,
            "tasks": [{
                "id": "t1",
                "name": "Sign",
                "function": {
                    "name": "crypto",
                    "input": {
                        "op": "hmac",
                        "algorithm": "sha256",
                        "key": "env://ZOOM_WEBHOOK_SECRET",
                        "data": {"var": "data.payload.plainToken"},
                        "encoding": "hex",
                        "output": "temp_data.encrypted_token"
                    }
                }
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // The capability table is enforced at upload: fast-hash-for-passwords,
    // a MAC without a key, an out-of-bounds cost, and a field that does not
    // apply to the op are each a named 400, never a request-time surprise.
    for (input, expected_field) in [
        (
            json!({"op": "password_hash", "algorithm": "sha256", "password": "x"}),
            "algorithm",
        ),
        (json!({"op": "hmac", "data": "x"}), "key"),
        (
            json!({"op": "password_hash", "password": "x", "params": {"cost": 42}}),
            "params",
        ),
        (json!({"op": "hash", "data": "x", "key": "why"}), "key"),
    ] {
        let (status, body) = upload_workflow(
            &app,
            json!({
                "name": "Bad Crypto",
                "condition": true,
                "tasks": [{
                    "id": "t1",
                    "name": "Bad",
                    "function": {"name": "crypto", "input": input}
                }]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "expected 400 for {input}");
        let details = body["error"]["details"].to_string();
        assert!(
            details.contains(expected_field),
            "{input} should have reported on `{expected_field}`, got {details}"
        );
    }
}

// ============================================================
// Running a definition once (POST /definitions/test)
// ============================================================

fn log_workflow(name: &str) -> Value {
    json!({
        "name": name,
        "condition": true,
        "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "dry run"}}}]
    })
}

#[tokio::test]
async fn test_workflow_test_with_metadata() {
    let app = common::test_app().await;

    let (status, body) = deploy::test_run(
        &app,
        &log_workflow("Dry Run Workflow"),
        json!({"key": "value"}),
        Some(json!({"source": "test-suite"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("matched").is_some(), "{body}");
    assert!(body.get("trace").is_some(), "{body}");
    assert!(body.get("output").is_some(), "{body}");
    assert!(body.get("errors").is_some(), "{body}");
}

/// `POST /definitions/test` must judge a workflow on state an ingress can
/// actually produce: caller-supplied `_orion_errors` is cleared, and
/// `headers` are lowercased and masked, as they are at ingress. Otherwise a
/// workflow could pass its test on state no request can create. The two
/// offline surfaces (`dry-run`, the CLI's `test`) go through the same
/// `prepare_offline_metadata`, so an offline pass means what a production
/// pass means.
#[tokio::test]
async fn workflow_test_normalizes_metadata_the_way_an_ingress_does() {
    let app = common::test_app().await;
    let workflow = json!({
        "name": "Echo Metadata",
        "condition": true,
        "tasks": [{
            "id": "t1",
            "name": "Echo",
            "function": { "name": "map", "input": { "mappings": [
                { "path": "data.hdr", "logic": { "var": "metadata.headers.deviceid" } },
                { "path": "data.auth_hdr", "logic": { "var": "metadata.headers.authorization" } },
                { "path": "data.errs", "logic": { "var": "metadata._orion_errors" } }
            ] } }
        }]
    });

    let (status, body) = deploy::test_run(
        &app,
        &workflow,
        json!({}),
        Some(json!({
            // axum yields lowercase header names, so a case writing
            // this would match here and miss in production.
            "headers": { "DeviceId": "abc", "Authorization": "Bearer sk-live-xyz" },
            // Engine-owned; the ingress clears it unconditionally, so a
            // caller cannot pre-seed failures a workflow branches on.
            "_orion_errors": [{ "message": "pre-seeded" }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let out = &body["output"];

    assert_eq!(out["hdr"], "abc", "header keys are lowercased: {body}");
    assert_ne!(
        out["auth_hdr"], "Bearer sk-live-xyz",
        "a credential header must be masked, as it is at ingress: {body}"
    );
    assert!(
        out["errs"].is_null(),
        "_orion_errors is engine-owned and cleared: {body}"
    );
}

/// The shape checks come with the normalization, so this surface refuses the
/// same impossible metadata the offline ones do rather than running on it.
#[tokio::test]
async fn workflow_test_refuses_metadata_no_ingress_could_produce() {
    let app = common::test_app().await;
    let workflow = log_workflow("Refuse");

    for (label, metadata) in [
        (
            "headers not an object of strings",
            json!({ "headers": { "a": 1 } }),
        ),
        ("vars not an object", json!({ "vars": "nope" })),
        ("channel not a string", json!({ "channel": 7 })),
        // The ingress builds `auth` as `{"claims": …}` and nothing else.
        (
            "auth carrying more than claims",
            json!({ "auth": { "sub": "u1" } }),
        ),
        // #307: stamped by the sign-in guard once the grant is verified.
        ("oauth not an object", json!({ "oauth": "token" })),
    ] {
        let (status, body) = deploy::test_run(&app, &workflow, json!({}), Some(metadata)).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{label} must be refused: {body}"
        );
    }
}

#[tokio::test]
async fn test_workflow_test_with_non_object_data() {
    let app = common::test_app().await;

    let (status, body) = deploy::test_run(
        &app,
        &log_workflow("Non Object Data"),
        json!("just a string"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

// ============================================================
// Definition round trip
// ============================================================

/// A workflow's description, priority, condition, tags and `on_failure`
/// are stored as uploaded: the definition read back by its digest carries
/// each of them.
#[tokio::test]
async fn a_workflow_definition_reads_back_with_description_priority_and_tags() {
    let app = common::test_app().await;

    let digest = deploy::upload(
        &app,
        deploy::workflow_definition(&json!({
            "name": "Described Workflow",
            "description": "This is a test workflow with a description",
            "priority": 5,
            "condition": {"==": [1, 1]},
            "tasks": [{"id": "t1", "name": "Log", "function": {"name": "log", "input": {"message": "test"}}}],
            "tags": ["production", "important"],
            "continue_on_error": true
        })),
    )
    .await;

    let stored = read_definition(&app, &digest).await;
    assert_eq!(stored["kind"], "workflow", "{stored}");
    let def = &stored["definition"];
    assert_eq!(
        def["description"], "This is a test workflow with a description",
        "{stored}"
    );
    assert_eq!(def["priority"], 5, "{stored}");
    assert_eq!(def["tags"], json!(["production", "important"]), "{stored}");
    assert_eq!(def["on_failure"], "continue", "{stored}");
}

// ============================================================
// R5: workflows that cannot run are refused, not deferred
// ============================================================

#[tokio::test]
async fn test_upload_rejects_unknown_function() {
    let app = common::test_app().await;

    // Accepting it would only defer the failure to its first request (R5).
    let body = deploy::assert_upload_refused(
        &app,
        deploy::workflow_definition(&json!({
            "name": "unknown-fn-wf",
            "tasks": [{
                "id": "t1",
                "name": "typo",
                "function": { "name": "http_calll", "input": { "connector": "c", "output": "data.x" } }
            }]
        })),
        "definition.tasks[0].function.name",
    )
    .await;
    assert!(
        body.to_string().contains("http_calll"),
        "error must name the unknown function, got {body}"
    );
}

/// `enrich` is refused at upload, because Orion registers no handler for it.
///
/// It is a dataflow-rs built-in *name* but not a self-contained one: it
/// deserializes into a typed built-in variant — so `Engine::new` accepts it and
/// the custom-input check skips it, since it never becomes
/// `FunctionConfig::Custom` — and then dispatches to a handler registered under
/// the same name. Nothing registers one.
///
/// A hand-copied name list is the wrong gate: `enrich` reads as a name "the
/// engine runs", yet such a workflow would serve cleanly and then fail every
/// single request with `FunctionNotFound`, forever. dataflow-rs publishes the
/// distinction as `BuiltinKind`, so the gate asks whether this engine can
/// run the task rather than whether the name is spelled correctly.
#[tokio::test]
async fn a_workflow_using_the_handlerless_enrich_builtin_is_refused() {
    let app = common::test_app().await;
    let (status, body) = upload_workflow(
        &app,
        json!({
            "name": "enrich-wf",
            "tasks": [{
                "id": "t1",
                "name": "Enrich",
                "function": { "name": "enrich", "input": {} }
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.to_string().contains("enrich"),
        "the refusal must name the function, got: {body}"
    );
}

/// …while a self-contained built-in with no Orion handler is still accepted.
/// `filter` needs no registration, so refusing it would turn the gate into a
/// blanket "Orion handlers only" rule.
#[tokio::test]
async fn a_workflow_using_a_self_contained_builtin_is_accepted() {
    let app = common::test_app().await;
    let (status, body) = upload_workflow(
        &app,
        json!({
            "name": "filter-wf",
            "tasks": [{
                "id": "t1",
                "name": "Filter",
                "function": { "name": "filter", "input": {"condition": true} }
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body = {body}");
}

// ============================================================
// Task identity is mandatory, and refused at authoring time
// ============================================================

/// Build a one-task workflow, letting the caller break the task's identity.
fn workflow_with_tasks(id: &str, tasks: serde_json::Value) -> serde_json::Value {
    json!({"workflow_id": id, "name": "Identity", "condition": true, "tasks": tasks})
}

/// A task with no `id` is refused at upload, not discovered at first request.
///
/// Accepted, such a workflow would activate cleanly and then fail at engine
/// load — quarantining its channel and answering 503 to every request with
/// "missing field `id`". Same class as the `enrich` false-accept above.
#[tokio::test]
async fn a_task_without_an_id_is_refused_at_upload() {
    let app = common::test_app().await;
    let (status, body) = upload_workflow(
        &app,
        workflow_with_tasks(
            "no-task-id",
            json!([{"name": "Log", "function": {"name": "log", "input": {"message": "hi"}}}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body = {body}");
    assert!(
        body.to_string().contains("tasks[0].id"),
        "the error must point at the offending task, got: {body}"
    );
}

/// A task's `name` is optional, as it is to the engine: a task without one is
/// accepted at upload.
#[tokio::test]
async fn a_task_without_a_name_is_accepted_at_upload() {
    let app = common::test_app().await;
    let (status, body) = upload_workflow(
        &app,
        workflow_with_tasks(
            "no-task-name",
            json!([{"id": "t1", "function": {"name": "log", "input": {"message": "hi"}}}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body = {body}");
}

/// …and so is an *empty* `name`, because the engine accepts it.
///
/// This is the boundary between the two rules. Orion tracks dataflow-rs's
/// parsing rules rather than tightening them, so that "Orion accepts it" and
/// "the engine can load it" remain the same statement. `""` is accepted and
/// the workflow loads and runs; an empty name is unhelpful in a log, but that
/// is an authoring preference, not a defect, and refusing it would be Orion
/// inventing a rule the engine does not have. `id` is the one deliberate
/// exception — see below.
#[tokio::test]
async fn a_task_with_an_empty_name_is_accepted_and_serves_traffic() {
    let app = common::test_app().await;

    // Not just "upload says 201": the point of the parity rule is that what
    // Orion accepts, the engine loads. Drive it all the way to a served
    // request, so this fails if the rule ever drifts in either direction.
    common::create_and_activate_channel(
        &app,
        "empty-name-ch",
        json!({
            "workflow_id": "empty-task-name",
            "name": "Empty Task Name",
            "condition": true,
            "tasks": [{
                "id": "t1",
                "name": "",
                "function": {"name": "map", "input": {"mappings": [{"path": "data.ok", "logic": true}]}}
            }]
        }),
    )
    .await;

    let resp = app
        .oneshot(json_request(
            "POST",
            "/api/v2/data/empty-name-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "an empty task name must load and run, not quarantine the channel"
    );
    let body = body_json(resp).await;
    assert_eq!(body["data"]["ok"], true, "body = {body}");
}

/// Duplicate task ids are the worst of the three: `Workflow::validate()` runs
/// inside the engine's compile, so a repeated id fails the whole engine build
/// rather than one channel. It is not contained by the per-channel
/// quarantine, which is exactly why it cannot be left to load time.
#[tokio::test]
async fn duplicate_task_ids_are_refused_at_upload() {
    let app = common::test_app().await;
    let (status, body) = upload_workflow(
        &app,
        workflow_with_tasks(
            "dup-task-ids",
            json!([
                {"id": "t1", "name": "A", "function": {"name": "log", "input": {"message": "a"}}},
                {"id": "t1", "name": "B", "function": {"name": "log", "input": {"message": "b"}}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body = {body}");
    let rendered = body.to_string();
    assert!(
        rendered.contains("tasks[1].id") && rendered.contains("DUPLICATE_TASK_ID"),
        "the error must name the second occurrence, got: {body}"
    );
}

/// A near miss of a known key is a `LIKELY_TYPO` Defect, which the loader
/// quarantines, so upload refuses it: an accepted workflow always serves.
#[tokio::test]
async fn a_likely_typo_is_refused_at_upload() {
    let app = common::test_app().await;
    let (status, body) = upload_workflow(
        &app,
        workflow_with_tasks(
            "typo-key",
            json!([
                {"id": "t1", "name": "A", "conditon": false,
                 "function": {"name": "log", "input": {"message": "a"}}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body = {body}");
    let rendered = body.to_string();
    assert!(
        rendered.contains("tasks[0].conditon") && rendered.contains("UNKNOWN_FIELD"),
        "the error must name the typo'd key, got: {body}"
    );
}

/// A blank `id`, unlike a blank `name`, is refused — the one place Orion is
/// deliberately stricter than the parse. `""` deserializes fine, but two of
/// them collide on `Workflow::validate()`'s uniqueness check, and even one
/// writes an empty `task_id` into every trace step, audit entry and metric
/// label, which makes the identifier useless for the thing it exists to do.
#[tokio::test]
async fn a_blank_task_id_is_refused_at_upload() {
    let app = common::test_app().await;
    let (status, body) = upload_workflow(
        &app,
        workflow_with_tasks(
            "blank-task-id",
            json!([{"id": "  ", "name": "Log", "function": {"name": "log", "input": {"message": "hi"}}}]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body = {body}");
    assert!(body.to_string().contains("tasks[0].id"), "body = {body}");
}

/// The whole point: a well-formed workflow is untouched by any of this.
#[tokio::test]
async fn a_workflow_with_proper_task_identity_is_accepted() {
    let app = common::test_app().await;
    let (status, body) = upload_workflow(
        &app,
        workflow_with_tasks(
            "good-ids",
            json!([
                {"id": "fetch", "name": "Fetch", "function": {"name": "log", "input": {"message": "a"}}},
                {"id": "emit",  "name": "Emit",  "function": {"name": "log", "input": {"message": "b"}}}
            ]),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body = {body}");
}

// ============================================================
// Workflow loop (dataflow-rs 3.3)
// ============================================================

fn looping_workflow(name: &str, loop_config: serde_json::Value) -> serde_json::Value {
    json!({
        "name": name,
        "condition": { "<": [{ "var": "temp_data.i" }, { "var": "data.count" }] },
        "loop": loop_config,
        "tasks": [{
            "id": "note",
            "name": "Note the sweep",
            "function": { "name": "log", "input": { "message": "sweep" } }
        }]
    })
}

/// A `loop` survives upload and read-back, and the definition read back is
/// the same content: uploading it again answers the same digest.
#[tokio::test]
async fn a_workflow_loop_round_trips_through_its_definition() {
    let app = common::test_app().await;

    let digest = deploy::upload(
        &app,
        deploy::workflow_definition(&looping_workflow(
            "Looping",
            json!({ "counter": "i", "max": 500 }),
        )),
    )
    .await;
    let stored = read_definition(&app, &digest).await;
    assert_eq!(stored["definition"]["loop"]["counter"], "i", "{stored}");
    assert_eq!(stored["definition"]["loop"]["max"], 500, "{stored}");

    let again = deploy::upload(&app, stored.clone()).await;
    assert_eq!(
        again, digest,
        "the definition read back must be the content uploaded"
    );
}

/// A workflow with no `loop` reads back without one, and its digest does not
/// depend on how the absence is spelled: the read-back form (which writes the
/// absent loop as `null`) uploads to the digest the loop-less upload answered.
#[tokio::test]
async fn a_workflow_without_a_loop_is_unchanged_by_the_feature() {
    let app = common::test_app().await;

    let digest = deploy::upload(
        &app,
        deploy::workflow_definition(&common::workflow_with_priority("No Loop", 0)),
    )
    .await;
    let stored = read_definition(&app, &digest).await;
    assert!(
        stored["definition"]["loop"].is_null(),
        "no loop was uploaded, so none may be read back: {stored}"
    );
    let again = deploy::upload(&app, stored.clone()).await;
    assert_eq!(again, digest, "an absent loop must not move the digest");
}

/// Each rule mirrors `LoopConfig::validate` in dataflow-rs, which otherwise
/// only fires at engine build — where the failure is a refused generation
/// rather than a 400 on one upload.
///
/// A `max` above `engine.max_loop_iterations` is not among them: upload
/// accepts it.
#[tokio::test]
async fn an_invalid_loop_is_refused_at_upload() {
    let app = common::test_app().await;

    let cases = [
        (json!({ "counter": "i" }), "max"),
        (json!({ "max": 10, "increment": 0 }), "increment"),
        (json!({ "max": 5, "init": 5 }), "max"),
        (json!({ "max": 10, "counter": "" }), "counter"),
        (json!({ "max": 10, "counter": "a..b" }), "counter"),
    ];

    for (loop_config, expected_field) in cases {
        let (status, body) =
            upload_workflow(&app, looping_workflow("Bad Loop", loop_config.clone())).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "expected 400 for loop {loop_config}: {body}"
        );
        let details = body["error"]["details"].to_string();
        assert!(
            details.contains(expected_field),
            "loop {loop_config} should have reported on `{expected_field}`, got {details}"
        );
    }
}

/// A loop's `setup` steps share the body's step-id namespace, and the engine
/// checks the two together, so a setup step reusing a body step's id is
/// refused at upload; a distinct one is accepted.
#[tokio::test]
async fn a_setup_step_id_colliding_with_the_body_is_refused_at_upload() {
    let app = common::test_app().await;
    let setup = |id: &str| {
        json!({ "max": 2, "setup": [{
            "id": id, "name": "Setup",
            "function": { "name": "log", "input": { "message": "setup" } }
        }]})
    };

    // The body's `note` is the setup step's id too.
    let (status, body) =
        upload_workflow(&app, looping_workflow("Colliding Loop", setup("note"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) =
        upload_workflow(&app, looping_workflow("Setup Loop", setup("prime"))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// #351: a loop's `setup` steps are steps. A check that walks `tasks` alone
/// validates a setup step by nothing; upload walks both, so an unknown
/// function in setup is refused at its own coordinate.
#[tokio::test]
async fn a_loops_setup_steps_are_validated() {
    let app = common::test_app().await;

    deploy::assert_upload_refused(
        &app,
        deploy::workflow_definition(&looping_workflow(
            "Unknown In Setup",
            json!({ "max": 2, "setup": [{
                "id": "prime", "name": "Prime",
                "function": { "name": "nosuch_fn", "input": {} }
            }]}),
        )),
        "definition.loop.setup[0].function.name",
    )
    .await;
}

/// The one that proves the feature rather than the plumbing: a looping
/// workflow, executed, must run its task list once per sweep and stop where
/// the break says. Upload acceptance would still pass if the `loop` key were
/// dropped on its way to the engine.
///
/// The break is a `filter` with `on_reject: "halt"` rather than a workflow
/// condition, and that is not a stylistic choice: `data` starts empty, so a
/// workflow-level condition indexing `data.*` is false on sweep 0 and the loop
/// never starts. Inside the body, `parse_json` has already run.
#[tokio::test]
async fn a_loop_executes_one_sweep_per_iteration() {
    let app = common::test_app().await;
    let workflow = json!({
        "name": "Loop Exec",
        "condition": true,
        "loop": { "counter": "i", "max": 50 },
        "tasks": [
            {
                "id": "parse",
                "name": "Parse the payload",
                "function": {
                    "name": "parse_json",
                    "input": { "source": "payload", "target": "req" }
                }
            },
            {
                "id": "more",
                "name": "Stop once every item is done",
                "function": {
                    "name": "filter",
                    "input": {
                        "condition": {
                            "<": [{ "var": "temp_data.i" }, { "var": "data.req.count" }]
                        },
                        "on_reject": "halt"
                    }
                }
            },
            {
                "id": "sweep",
                "name": "One sweep",
                "function": { "name": "log", "input": { "message": "sweep" } }
            }
        ]
    });

    // `max` is 50, so the filter is what stops this at 3.
    let (status, body) = deploy::test_run(&app, &workflow, json!({ "count": 3 }), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let steps = body["trace"]["steps"]
        .as_array()
        .unwrap_or_else(|| panic!("trace.steps should be an array: {body}"));
    let sweeps = steps
        .iter()
        .filter(|s| s["task_id"] == "sweep" && s["result"] != "skipped")
        .count();
    assert_eq!(
        sweeps, 3,
        "expected one executed step per sweep, stopped by the filter, got {sweeps} in {:#}",
        body["trace"]
    );
}

// ============================================================
// Task groups (dataflow-rs 3.6)
// ============================================================

/// Upload must agree with the engine about what a task is: a group read as a
/// task would be reported as missing `name` and `function.name`, refusing a
/// workflow the engine runs.
#[tokio::test]
async fn a_workflow_whose_tasks_are_grouped_is_accepted() {
    let app = common::test_app().await;

    let (status, body) = upload_workflow(
        &app,
        json!({
            "name": "Grouped",
            "condition": true,
            "tasks": [
                {
                    "id": "seed",
                    "name": "Seed",
                    "function": {"name": "map", "input": {"mappings": [
                        {"path": "data.amount", "logic": 500}
                    ]}}
                },
                {
                    "id": "guard",
                    "condition": {">": [{"var": "data.amount"}, 100]},
                    "tasks": [{
                        "id": "inner",
                        "name": "Inner",
                        "function": {"name": "log", "input": {"message": "big"}}
                    }]
                }
            ]
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "grouped workflow refused: {body}"
    );
}

/// The other half: a broken task *inside* a group is still checked, and its
/// refusal names the path that addresses it rather than a top-level index.
#[tokio::test]
async fn a_broken_task_inside_a_group_is_refused_at_its_path() {
    let app = common::test_app().await;

    deploy::assert_upload_refused(
        &app,
        deploy::workflow_definition(&json!({
            "name": "Grouped",
            "condition": true,
            "tasks": [{
                "id": "guard",
                "condition": true,
                "tasks": [{"id": "inner", "name": "Inner"}]
            }]
        })),
        "definition.tasks[0].tasks[0].function.name",
    )
    .await;
}
