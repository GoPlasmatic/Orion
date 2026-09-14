//! The model entity through the admin API: registration by reference,
//! admission on this node, the activation gate on the verdict, the
//! dependants walk, and the export/import round trip.
//!
//! Uses the fixture in `tests/fixtures/models/c4-tiny/` served by an
//! in-process bucket (see `common::models`). `test_app` starts no background
//! tasks, so admission is driven through `admit_now` — the same function the
//! worker calls per job — or through `POST /models/{id}/admit?wait=true`.

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common::models::{
    AS_CONST_ID, AS_CONST_MANIFEST, AS_CONST_ONNX, FIXTURE_ID, FIXTURE_KEY, FIXTURE_ONNX, admit,
    create_storage_connector, fixture_digest, harness, harness_serving, harness_with, manifest,
    models_config, register_fixture, registration, spawn_bucket,
};
use crate::common::{body_json, json_request, test_app_with_config};
use orion::model::AdmissionState;
use orion::server::state::AppState;
use orion::storage::repositories::workflows::CreateWorkflowRequest;

async fn send(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request(method, path, body))
        .await
        .expect("request");
    let status = resp.status();
    (status, body_json(resp).await)
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    send(app, "GET", path, None).await
}

async fn set_status(app: &axum::Router, id: &str, status: &str) -> (StatusCode, Value) {
    send(
        app,
        "PATCH",
        &format!("/api/v1/admin/models/{id}/status"),
        Some(json!({"status": status})),
    )
    .await
}

/// A workflow calling `model_infer` on `model` by its literal id, written
/// through the repository: the function arrives with the runtime, so the
/// create-time name gate on `POST /workflows` would refuse it today.
async fn active_workflow_naming(state: &AppState, name: &str, model: &str) -> String {
    let row = state
        .repos
        .workflows
        .create(&CreateWorkflowRequest {
            workflow_id: None,
            name: name.to_string(),
            description: None,
            priority: 0,
            condition: json!(true),
            tasks: json!([
                {"id": "parse", "name": "parse", "function": {"name": "parse_json",
                    "input": {"source": "payload", "target": "input"}}},
                {"id": "group", "tasks": [
                    {"id": "infer", "name": "infer", "function": {"name": "model_infer",
                        "input": {"model": model, "output": "data.policy"}}}
                ]}
            ]),
            tags: vec![],
            loop_config: None,
            continue_on_error: false,
        })
        .await
        .expect("create workflow");
    state
        .repos
        .workflows
        .activate(&row.workflow_id, 100)
        .await
        .expect("activate workflow");
    row.workflow_id
}

#[tokio::test]
async fn a_registration_is_checked_stored_pending_and_queued() {
    let h = harness().await;
    let model = register_fixture(&h.app).await;
    assert_eq!(model["model_id"], FIXTURE_ID);
    assert_eq!(model["version"], 1);
    assert_eq!(model["status"], "draft");
    assert_eq!(model["abi"], "orion:model@1.0.0");
    assert_eq!(model["model_version"], "0.1.0");
    assert_eq!(model["format"], "onnx");
    assert_eq!(model["digest"], fixture_digest());
    assert_eq!(model["inputs"], json!(["board"]));
    assert_eq!(model["outputs"], json!(["policy"]));
    assert_eq!(model["artifact"]["connector"], "bucket");
    assert_eq!(model["artifact"]["key"], FIXTURE_KEY);
    assert_eq!(model["artifact"]["digest"], fixture_digest());
    // The size the bucket reported, not one the author claimed.
    assert_eq!(model["artifact"]["size"], FIXTURE_ONNX.len() as u64);
    assert_eq!(model["admission"], json!({"state": "pending"}));
    assert!(model["stats"].is_null(), "{model}");
    assert_eq!(model["tags"], json!(["fixture"]));
    assert!(
        model["content_hash"]
            .as_str()
            .is_some_and(|h| h.starts_with("sha256:"))
    );
    assert!(
        model.get("health").is_none(),
        "the list shape carries no health"
    );
    // The registration HEADed the object and fetched nothing.
    assert_eq!(h.bucket.heads.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(h.bucket.gets.load(std::sync::atomic::Ordering::SeqCst), 0);

    // The single read adds this node's view.
    let (status, body) = get(&h.app, &format!("/api/v1/admin/models/{FIXTURE_ID}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["health"]["state"], "pending");

    // A second registration of the same id conflicts.
    let (status, body) = send(
        &h.app,
        "POST",
        "/api/v1/admin/models",
        Some(registration("bucket", &fixture_digest())),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
async fn admission_fetches_verifies_and_records_the_stats() {
    let h = harness().await;
    register_fixture(&h.app).await;

    let outcome = admit(&h.state, FIXTURE_ID).await;
    assert!(outcome.passed(), "{outcome:?}");
    let AdmissionState::Passed { stats } = &outcome.state else {
        unreachable!("passed")
    };
    assert_eq!(stats.artifact_bytes, FIXTURE_ONNX.len() as u64);
    // The graph numbers are what `build.py` printed; the probe ran on this
    // node's default runtime and device.
    assert_eq!(stats.parameters, 1479);
    assert_eq!(stats.nodes, 4);
    assert_eq!(stats.opset, 17);
    assert_eq!(stats.ir_version, 9);
    assert_eq!(stats.runtime, "tract");
    assert_eq!(stats.device, "cpu");
    assert!(stats.probe_ms > 0.0, "{stats:?}");
    assert_eq!(h.bucket.gets.load(std::sync::atomic::Ordering::SeqCst), 1);

    let (status, body) = get(&h.app, &format!("/api/v1/admin/models/{FIXTURE_ID}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let model = &body["data"];
    assert_eq!(model["admission"]["state"], "passed");
    assert!(model["admission"]["node"].is_string(), "{model}");
    assert!(model["admission"]["at"].is_string(), "{model}");
    assert!(model["admission"].get("stage").is_none(), "{model}");
    assert_eq!(model["stats"]["artifact_bytes"], 6171);
    assert_eq!(model["stats"]["parameters"], 1479);
    assert_eq!(model["stats"]["nodes"], 4);
    assert_eq!(model["stats"]["opset"], 17);
    assert_eq!(model["stats"]["ir_version"], 9);
    assert_eq!(model["stats"]["runtime"], "tract");
    assert_eq!(model["stats"]["device"], "cpu");
    assert!(
        model["stats"]["probe_ms"]
            .as_f64()
            .is_some_and(|ms| ms > 0.0),
        "{model}"
    );
    assert_eq!(model["digest"], fixture_digest());
    // Passed, but a draft: not what this node serves.
    assert_eq!(model["health"]["state"], "inactive");

    // The cache holds the verified bytes under the digest.
    let models = h.state.models.as_ref().expect("enabled");
    assert_eq!(models.store.cached_bytes(), FIXTURE_ONNX.len() as u64);
    let path = models.store.path_for(&fixture_digest());
    assert_eq!(std::fs::read(&path).expect("cached"), FIXTURE_ONNX);
    let _ = std::fs::remove_dir_all(models.store.cache_dir());
}

#[tokio::test]
async fn a_wrong_digest_fails_admission_at_the_digest_stage() {
    let h = harness().await;
    let claimed = orion::crypto::sha256_digest(b"not the bytes the bucket serves");
    let (status, body) = send(
        &h.app,
        "POST",
        "/api/v1/admin/models",
        Some(registration("bucket", &claimed)),
    )
    .await;
    // The HEAD cannot tell: the object exists. Admission can.
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    let outcome = admit(&h.state, FIXTURE_ID).await;
    assert!(
        matches!(&outcome.state, AdmissionState::Failed { stage: "digest", reason }
            if reason.contains(&claimed) && reason.contains(&fixture_digest())),
        "{outcome:?}"
    );
    let (_, body) = get(&h.app, &format!("/api/v1/admin/models/{FIXTURE_ID}")).await;
    assert_eq!(body["data"]["admission"]["state"], "failed");
    assert_eq!(body["data"]["admission"]["stage"], "digest");
    assert!(body["data"]["stats"].is_null());
    assert_eq!(body["data"]["health"]["state"], "rejected");
    assert!(
        body["data"]["health"]["reason"]
            .as_str()
            .is_some_and(|r| r.starts_with("digest:")),
        "{body}"
    );
    // Nothing was kept.
    let models = h.state.models.as_ref().expect("enabled");
    assert_eq!(models.store.cached_bytes(), 0);
    let _ = std::fs::remove_dir_all(models.store.cache_dir());
}

/// The row after a failed admission: the verdict names the stage, the
/// stats stay `null`, and the model is not one this node serves.
async fn assert_failed_at(app: &axum::Router, stage: &str) {
    let (_, body) = get(app, &format!("/api/v1/admin/models/{FIXTURE_ID}")).await;
    assert_eq!(body["data"]["admission"]["state"], "failed", "{body}");
    assert_eq!(body["data"]["admission"]["stage"], stage, "{body}");
    assert!(body["data"]["stats"].is_null(), "{body}");
    assert_eq!(body["data"]["health"]["state"], "rejected", "{body}");
}

#[tokio::test]
async fn a_manifest_input_the_graph_lacks_fails_admission_at_parse() {
    let h = harness().await;
    let mut body = registration("bucket", &fixture_digest());
    body["manifest"]["inputs"][0]["name"] = json!("boards");
    let (status, resp) = send(&h.app, "POST", "/api/v1/admin/models", Some(body)).await;
    // Registration cannot tell: the graph is not read until admission.
    assert_eq!(status, StatusCode::ACCEPTED, "{resp}");

    let outcome = admit(&h.state, FIXTURE_ID).await;
    assert!(
        matches!(&outcome.state, AdmissionState::Failed { stage: "parse", reason }
            if reason.contains("'boards'") && reason.contains("'board'")),
        "{outcome:?}"
    );
    assert_failed_at(&h.app, "parse").await;
    let models = h.state.models.as_ref().expect("enabled");
    let _ = std::fs::remove_dir_all(models.store.cache_dir());
}

#[tokio::test]
async fn a_parameter_count_over_the_ceiling_fails_admission_at_parse() {
    let mut config = models_config(true);
    config.models.max_parameters = 1000;
    let h = harness_with(config).await;
    register_fixture(&h.app).await;

    let outcome = admit(&h.state, FIXTURE_ID).await;
    assert!(
        matches!(&outcome.state, AdmissionState::Failed { stage: "parse", reason }
            if reason.contains("1479") && reason.contains("models.max_parameters (1000)")),
        "{outcome:?}"
    );
    assert_failed_at(&h.app, "parse").await;
    let models = h.state.models.as_ref().expect("enabled");
    let _ = std::fs::remove_dir_all(models.store.cache_dir());
}

/// Weights carried as `Constant` node attributes are measured like any
/// other, and the ceiling applies to them (#325).
///
/// `as-const.onnx` is the `weights` fixture's second encoding: the same
/// single `Gemm` as `as-init.onnx` over the same fifteen numbers, moved out
/// of the graph's initializers and into its nodes' attributes. It computes
/// the same function and answers identically. While the reader counted only
/// the initializers it reported zero, so a graph of any size admitted under
/// any `models.max_parameters` — the one graph-shape ceiling admission has
/// — by being re-exported.
#[tokio::test]
async fn weights_carried_in_attributes_are_counted_and_the_ceiling_holds() {
    let digest = orion::crypto::sha256_digest(AS_CONST_ONNX);
    let registration = json!({
        "manifest": serde_json::from_str::<Value>(AS_CONST_MANIFEST).expect("manifest parses"),
        "artifact": { "connector": "bucket", "key": FIXTURE_KEY, "digest": digest },
    });

    // What the node measures, with nothing in the way.
    let h = harness_serving(AS_CONST_ONNX.to_vec(), models_config(true)).await;
    let (status, body) = send(
        &h.app,
        "POST",
        "/api/v1/admin/models",
        Some(registration.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let outcome = admit(&h.state, AS_CONST_ID).await;
    let AdmissionState::Passed { stats } = &outcome.state else {
        panic!("{outcome:?}")
    };
    assert_eq!(stats.parameters, 15);
    // The rewrite shows where it actually costs something: two more nodes.
    assert_eq!(stats.nodes, 3);
    let models = h.state.models.as_ref().expect("enabled");
    let _ = std::fs::remove_dir_all(models.store.cache_dir());

    // And a ceiling under that count refuses it, where it used to admit at
    // zero whatever the ceiling was.
    let mut config = models_config(true);
    config.models.max_parameters = 10;
    let h = harness_serving(AS_CONST_ONNX.to_vec(), config).await;
    let (status, body) = send(&h.app, "POST", "/api/v1/admin/models", Some(registration)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let outcome = admit(&h.state, AS_CONST_ID).await;
    assert!(
        matches!(&outcome.state, AdmissionState::Failed { stage: "parse", reason }
            if reason.contains("15 parameters") && reason.contains("models.max_parameters (10)")),
        "{outcome:?}"
    );
    let models = h.state.models.as_ref().expect("enabled");
    let _ = std::fs::remove_dir_all(models.store.cache_dir());
}

#[tokio::test]
async fn an_output_shape_the_graph_does_not_produce_fails_admission_at_probe() {
    let h = harness().await;
    let mut body = registration("bucket", &fixture_digest());
    body["manifest"]["outputs"][0]["shape"] = json!([1, 8]);
    let (status, resp) = send(&h.app, "POST", "/api/v1/admin/models", Some(body)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{resp}");

    let outcome = admit(&h.state, FIXTURE_ID).await;
    assert!(
        matches!(&outcome.state, AdmissionState::Failed { stage: "probe", reason }
            if reason.contains("'policy'")
                && reason.contains("f32[1, 7]")
                && reason.contains("f32[1, 8]")),
        "{outcome:?}"
    );
    assert_failed_at(&h.app, "probe").await;
    let models = h.state.models.as_ref().expect("enabled");
    let _ = std::fs::remove_dir_all(models.store.cache_dir());
}

#[tokio::test]
async fn bytes_that_are_not_a_model_fail_admission_at_parse() {
    let h = harness().await;
    // One hundred bytes of text: a digest the registration can claim
    // honestly, and a first byte that is not a protobuf tag.
    let junk: Vec<u8> = b"not an onnx model "
        .iter()
        .copied()
        .cycle()
        .take(100)
        .collect();
    let bucket = spawn_bucket(junk.clone()).await;
    create_storage_connector(&h.app, "junk", bucket.addr).await;
    let (status, resp) = send(
        &h.app,
        "POST",
        "/api/v1/admin/models",
        Some(registration("junk", &orion::crypto::sha256_digest(&junk))),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{resp}");

    let outcome = admit(&h.state, FIXTURE_ID).await;
    assert!(
        matches!(&outcome.state, AdmissionState::Failed { stage: "parse", reason }
            if reason.contains("not an ONNX model")),
        "{outcome:?}"
    );
    assert_eq!(bucket.gets.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_failed_at(&h.app, "parse").await;
    let models = h.state.models.as_ref().expect("enabled");
    let _ = std::fs::remove_dir_all(models.store.cache_dir());
}

#[tokio::test]
async fn activation_waits_for_admission_to_pass() {
    let h = harness().await;
    register_fixture(&h.app).await;

    // Pending: refused, and the dry run says the same without writing.
    let (status, body) = set_status(&h.app, FIXTURE_ID, "active").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("'pending'")),
        "{body}"
    );
    let (status, body) = send(
        &h.app,
        "PATCH",
        &format!("/api/v1/admin/models/{FIXTURE_ID}/status?dry_run=true"),
        Some(json!({"status": "active"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["valid"], false, "{body}");
    assert!(
        body["data"]["errors"].to_string().contains("pending"),
        "{body}"
    );

    // Failed: refused naming the stage and the reason.
    let models = h.state.models.as_ref().expect("enabled");
    h.state
        .repos
        .models
        .set_admission(
            FIXTURE_ID,
            1,
            &json!({"state": "failed", "stage": "fetch", "reason": "GET answered HTTP 503",
                "node": models.node, "at": "2026-09-13T00:00:00"})
            .to_string(),
        )
        .await
        .expect("record");
    let (status, body) = set_status(&h.app, FIXTURE_ID, "active").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("'fetch'") && message.contains("HTTP 503"),
        "{body}"
    );

    // Passed: allowed, and the row is what this node serves.
    assert!(admit(&h.state, FIXTURE_ID).await.passed());
    let (status, body) = set_status(&h.app, FIXTURE_ID, "active").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["status"], "active");
    let (_, body) = get(&h.app, &format!("/api/v1/admin/models/{FIXTURE_ID}")).await;
    assert_eq!(body["data"]["health"]["state"], "admitted");

    // The admin lists select on the verdict.
    let (_, body) = get(&h.app, "/api/v1/admin/models?admission=passed").await;
    assert_eq!(body["total"], 1);
    let (_, body) = get(&h.app, "/api/v1/admin/models?admission=pending").await;
    assert_eq!(body["total"], 0);
    let _ = std::fs::remove_dir_all(models.store.cache_dir());
}

#[tokio::test]
async fn the_admit_route_reruns_admission_on_this_node() {
    let h = harness().await;
    register_fixture(&h.app).await;

    // Queued: the row is unchanged and the request is accepted.
    let (status, body) = send(
        &h.app,
        "POST",
        &format!("/api/v1/admin/models/{FIXTURE_ID}/admit"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["data"]["admission"]["state"], "pending");

    // Inline: the verdict is on the row when the response comes back.
    let (status, body) = send(
        &h.app,
        "POST",
        &format!("/api/v1/admin/models/{FIXTURE_ID}/admit?wait=true"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["admission"]["state"], "passed");
    assert_eq!(body["data"]["stats"]["artifact_bytes"], 6171);

    // Idempotent: again is a fresh verdict, served from the cache.
    let gets = h.bucket.gets.load(std::sync::atomic::Ordering::SeqCst);
    let (status, body) = send(
        &h.app,
        "POST",
        &format!("/api/v1/admin/models/{FIXTURE_ID}/admit?wait=true"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["admission"]["state"], "passed");
    assert_eq!(
        h.bucket.gets.load(std::sync::atomic::Ordering::SeqCst),
        gets
    );
    let models = h.state.models.as_ref().expect("enabled");
    let _ = std::fs::remove_dir_all(models.store.cache_dir());
}

#[tokio::test]
async fn a_connector_that_refuses_reads_is_refused_at_registration() {
    let h = harness().await;
    let mut gated = crate::common::models::storage_connector("gated", h.bucket.addr);
    gated["config"]["operations"] = json!({"presign_get": false});
    let (status, body) = send(&h.app, "POST", "/api/v1/admin/connectors", Some(gated)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = send(
        &h.app,
        "POST",
        "/api/v1/admin/models",
        Some(registration("gated", &fixture_digest())),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["details"][0]["path"], "artifact.connector");
    assert!(
        body["error"]["details"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("presign_get")),
        "{body}"
    );

    // A connector that is not a storage connector, and one that does not
    // exist, are refused the same way.
    let (status, body) = send(
        &h.app,
        "POST",
        "/api/v1/admin/connectors",
        Some(crate::common::db_connector("not-a-bucket")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    for connector in ["not-a-bucket", "absent"] {
        let (status, body) = send(
            &h.app,
            "POST",
            "/api/v1/admin/models",
            Some(registration(connector, &fixture_digest())),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{connector}: {body}");
        assert_eq!(body["error"]["details"][0]["path"], "artifact.connector");
    }
    // Nothing was written.
    let (_, body) = get(&h.app, "/api/v1/admin/models").await;
    assert_eq!(body["total"], 0);
}

#[tokio::test]
async fn a_node_without_the_runtime_refuses_every_write() {
    let app = test_app_with_config(models_config(false)).await;
    let (status, body) = send(
        &app,
        "POST",
        "/api/v1/admin/models",
        Some(registration("bucket", &fixture_digest())),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("models.enabled = false")),
        "{body}"
    );
    let (status, body) = send(
        &app,
        "POST",
        "/api/v1/admin/models/validate",
        Some(registration("bucket", &fixture_digest())),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["valid"], false);
    assert!(
        body["data"]["errors"]
            .to_string()
            .contains("models.enabled = false")
    );

    // The read side answers, and says so.
    let (status, body) = get(&app, "/api/v1/admin/models").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 0);
    let (_, body) = get(&app, "/health").await;
    assert_eq!(body["components"]["models"], "disabled");
    assert!(body.get("models").is_none(), "{body}");
}

#[tokio::test]
async fn validate_reports_the_manifest_and_the_object() {
    let h = harness().await;

    // A bad manifest: the errors name the field under `manifest.`.
    let mut bad = manifest();
    bad["inputs"] = json!([]);
    bad["outputs"][0]["dtype"] = json!("F32");
    let (status, body) = send(
        &h.app,
        "POST",
        "/api/v1/admin/models/validate",
        Some(json!({"manifest": bad, "artifact": {
            "connector": "bucket", "key": FIXTURE_KEY, "digest": fixture_digest()}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["valid"], false);
    let fields: Vec<&str> = body["data"]["errors"]
        .as_array()
        .expect("errors")
        .iter()
        .filter_map(|e| e["field"].as_str())
        .collect();
    assert_eq!(
        fields,
        ["manifest.inputs", "manifest.outputs[0].dtype"],
        "{body}"
    );
    assert!(body["data"].get("head").is_none(), "{body}");

    // An incomplete reference: every missing field at once.
    let (status, body) = send(
        &h.app,
        "POST",
        "/api/v1/admin/models/validate",
        Some(json!({"manifest": manifest(), "artifact": {"connector": "bucket"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["valid"], false);
    assert_eq!(body["data"]["errors"].as_array().map(Vec::len), Some(2));

    // A good registration: valid, with what the bucket said.
    let (status, body) = send(
        &h.app,
        "POST",
        "/api/v1/admin/models/validate",
        Some(registration("bucket", &fixture_digest())),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["valid"], true, "{body}");
    assert_eq!(body["data"]["head"]["size"], FIXTURE_ONNX.len() as u64);
    assert_eq!(body["data"]["head"]["etag"], "fixture-etag");
    // Nothing written, nothing fetched.
    let (_, body) = get(&h.app, "/api/v1/admin/models").await;
    assert_eq!(body["total"], 0);
    assert_eq!(h.bucket.gets.load(std::sync::atomic::Ordering::SeqCst), 0);

    // `POST /models` refuses exactly what `validate` refused.
    let (status, body) = send(
        &h.app,
        "POST",
        "/api/v1/admin/models",
        Some(
            json!({"model_id": "ada.other", "manifest": manifest(), "artifact": {
            "connector": "bucket", "key": FIXTURE_KEY, "digest": fixture_digest()}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["details"][0]["path"], "model_id");
}

#[tokio::test]
async fn an_update_keeps_the_verdict_unless_the_artifact_moved() {
    let h = harness().await;
    register_fixture(&h.app).await;
    assert!(admit(&h.state, FIXTURE_ID).await.passed());

    // Tags and the manifest's description change: the bytes did not.
    let mut described = manifest();
    described["description"] = json!("retrained on more games");
    let (status, body) = send(
        &h.app,
        "PUT",
        &format!("/api/v1/admin/models/{FIXTURE_ID}"),
        Some(json!({"manifest": described, "tags": ["fixture", "v2"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["admission"]["state"], "passed");
    assert_eq!(body["data"]["stats"]["artifact_bytes"], 6171);
    assert_eq!(body["data"]["tags"], json!(["fixture", "v2"]));
    assert_eq!(
        body["data"]["manifest"]["description"],
        "retrained on more games"
    );

    // The key changes: the verdict was about another object.
    let (status, body) = send(
        &h.app,
        "PUT",
        &format!("/api/v1/admin/models/{FIXTURE_ID}"),
        Some(
            json!({"artifact": {"connector": "bucket", "key": "models/c4-tiny-v2.onnx",
            "digest": fixture_digest()}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["artifact"]["key"], "models/c4-tiny-v2.onnx");
    assert_eq!(body["data"]["admission"], json!({"state": "pending"}));
    assert!(body["data"]["stats"].is_null());

    // A new version carries the verdict forward with the reference.
    assert!(admit(&h.state, FIXTURE_ID).await.passed());
    let (status, body) = set_status(&h.app, FIXTURE_ID, "active").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(
        &h.app,
        "POST",
        &format!("/api/v1/admin/models/{FIXTURE_ID}/versions"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["data"]["version"], 2);
    assert_eq!(body["data"]["status"], "draft");
    assert_eq!(body["data"]["admission"]["state"], "passed");
    assert_eq!(body["data"]["stats"]["artifact_bytes"], 6171);
    let (_, body) = get(
        &h.app,
        &format!("/api/v1/admin/models/{FIXTURE_ID}/versions"),
    )
    .await;
    assert_eq!(body["total"], 2);
    let models = h.state.models.as_ref().expect("enabled");
    let _ = std::fs::remove_dir_all(models.store.cache_dir());
}

#[tokio::test]
async fn dependants_are_listed_and_gate_archive_and_delete() {
    let h = harness().await;
    register_fixture(&h.app).await;
    assert!(admit(&h.state, FIXTURE_ID).await.passed());
    let (status, body) = set_status(&h.app, FIXTURE_ID, "active").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let user = active_workflow_naming(&h.state, "policy-picker", FIXTURE_ID).await;
    // A workflow naming another model, and one computing the id, are not
    // dependants.
    active_workflow_naming(&h.state, "other-picker", "ada.other").await;

    let (status, body) = get(
        &h.app,
        &format!("/api/v1/admin/models/{FIXTURE_ID}/dependencies"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["model_id"], FIXTURE_ID);
    assert_eq!(body["data"]["version"], 1);
    assert_eq!(body["data"]["dynamic_references_unlisted"], true);
    assert_eq!(
        body["data"]["workflows"],
        json!([{"workflow_id": user, "version": 1, "task_ids": ["infer"]}])
    );

    // Archive and delete are refused naming the workflow.
    let (status, body) = set_status(&h.app, FIXTURE_ID, "archived").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"].to_string().contains(&user),
        "{body}"
    );
    let (status, body) = send(
        &h.app,
        "DELETE",
        &format!("/api/v1/admin/models/{FIXTURE_ID}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = send(
        &h.app,
        "PATCH",
        &format!("/api/v1/admin/models/{FIXTURE_ID}/status?dry_run=true"),
        Some(json!({"status": "archived"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["valid"], false, "{body}");

    // Once the workflow is archived, both go through.
    h.state
        .repos
        .workflows
        .archive(&user)
        .await
        .expect("archive workflow");
    let (status, body) = set_status(&h.app, FIXTURE_ID, "archived").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["status"], "archived");
    let resp = h
        .app
        .clone()
        .oneshot(json_request(
            "DELETE",
            &format!("/api/v1/admin/models/{FIXTURE_ID}"),
            None,
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let (status, _) = get(&h.app, &format!("/api/v1/admin/models/{FIXTURE_ID}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let models = h.state.models.as_ref().expect("enabled");
    let _ = std::fs::remove_dir_all(models.store.cache_dir());
}

#[tokio::test]
async fn export_carries_the_reference_and_import_readmits() {
    let source = harness().await;
    register_fixture(&source.app).await;
    assert!(admit(&source.state, FIXTURE_ID).await.passed());

    let (status, body) = get(&source.app, "/api/v1/admin/models/export?tag=fixture").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["data"].as_array().expect("data");
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item["artifact"]["connector"], "bucket");
    assert_eq!(item["artifact"]["digest"], fixture_digest());
    assert!(
        item.get("component").is_none() && item.get("bytes").is_none(),
        "{item}"
    );
    assert!(
        item.to_string().len() < 4_000,
        "an export item is a reference, not the artifact: {} bytes",
        item.to_string().len()
    );
    let (_, body) = get(&source.app, "/api/v1/admin/models/export?tag=absent").await;
    assert!(body["data"].as_array().expect("data").is_empty());

    // A fresh instance with its own connector of the same name: the import
    // stores the reference, checks the object, and queues admission here.
    let target = harness().await;
    let (status, body) = send(
        &target.app,
        "POST",
        "/api/v1/admin/models/import",
        Some(json!(items)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["imported"], 1, "{body}");
    assert_eq!(body["data"]["failed"], 0, "{body}");
    let (_, body) = get(&target.app, &format!("/api/v1/admin/models/{FIXTURE_ID}")).await;
    assert_eq!(body["data"]["admission"], json!({"state": "pending"}));
    assert_eq!(body["data"]["content_hash"], item["content_hash"]);
    assert!(admit(&target.state, FIXTURE_ID).await.passed());
    assert_eq!(
        target.bucket.gets.load(std::sync::atomic::Ordering::SeqCst),
        1
    );

    // Re-importing identical content under `new_version` is a no-op.
    let (status, body) = send(
        &target.app,
        "POST",
        "/api/v1/admin/models/import?on_conflict=new_version",
        Some(json!(items)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["unchanged"], 1, "{body}");
    assert_eq!(body["data"]["imported"], 0, "{body}");
    for h in [&source, &target] {
        let models = h.state.models.as_ref().expect("enabled");
        let _ = std::fs::remove_dir_all(models.store.cache_dir());
    }
}

#[tokio::test]
async fn health_reports_the_model_node() {
    let h = harness().await;
    let (status, body) = get(&h.app, "/health").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["components"]["models"], "ok");
    assert_eq!(body["models"]["admission_queue_capacity"], 1024);
    assert_eq!(body["models"]["cache_bytes"], 0);
    assert!(body["models"]["node"].is_string(), "{body}");
}
