//! N3/N4: a channel whose config does not parse, or whose
//! `validation_logic` does not compile, must never serve with its guards
//! silently absent.
//!
//! A node serving a deployment states that guarantee at two points. A
//! definition that would not serve is refused at upload, naming the field.
//! A definition stored past upload (`common::store_definition`, which writes
//! past upload validation — how such a definition occurs in practice: an
//! older node's upload, or a later engine reading it differently) is caught
//! when the revision builds: a channel whose config does not load is
//! quarantined — absent from the route table and refused at every ingress —
//! while a workflow that cannot build fails its revision as a whole: the
//! deployment is refused before it commits, the head does not move, and the
//! previous generation keeps serving every channel. Nothing of the refused
//! revision is published, so nothing of it is quarantined.

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use crate::common::{self, body_json, json_request};

async fn serves_ok(app: &axum::Router, channel: &str) -> bool {
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/api/v2/data/{channel}"),
            Some(json!({"data": {"x": 1}})),
        ))
        .await
        .expect("data request");
    resp.status() == StatusCode::OK
}

/// A deployment adding `upserts` to the head, applied as it is: answering
/// the apply body, and asserting the node refused to commit a revision it
/// could not build, naming every one of `fragments`, while the head stays
/// where it was and the generation it served before keeps serving.
async fn assert_revision_fails_whole(
    state: &orion::runtime::state::AppState,
    app: &axum::Router,
    upserts: Vec<common::deploy::Upsert>,
    fragments: &[&str],
    neighbour: &str,
) -> serde_json::Value {
    let before = state.runtime.load().id;
    let (base, members) = common::deploy::merged(app, upserts).await;
    let (status, body) = common::deploy::apply(app, base, members).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "the revision is refused: {body}"
    );
    assert_eq!(
        body["error"]["code"], "DEPLOYMENT_WOULD_NOT_BUILD",
        "{body}"
    );
    assert_eq!(
        common::deploy::head(app).await.0,
        base,
        "a refused revision does not move the head"
    );
    let error = body["error"]["message"].as_str().unwrap_or_default();
    for fragment in fragments {
        assert!(
            error.contains(fragment),
            "expected {fragment:?} in the node's error: {body}"
        );
    }
    assert_eq!(
        state.runtime.load().id,
        before,
        "a revision that cannot build publishes nothing"
    );
    assert!(
        serves_ok(app, neighbour).await,
        "the previous generation keeps serving"
    );
    assert!(
        state.runtime.load().channels.quarantined().is_empty(),
        "nothing of the failed revision is quarantined, because nothing of it is published"
    );
    body
}

/// The v2 statement of a channel whose config would not load: its definition
/// is refused at upload, naming the field at `path` (under the request's
/// `definition`). A definition stored past upload is not asserted here: a
/// manifest build quarantines a channel whose config does not load rather
/// than failing its revision, which is not a guarantee to pin.
async fn assert_refused(config: serde_json::Value, bad_channel: &str, path: &str) {
    let app = common::test_app().await;
    let channel = json!({
        "name": bad_channel,
        "channel_type": "sync",
        "protocol": "http",
        "methods": ["POST"],
        "route_pattern": format!("/{bad_channel}"),
        "config": config,
    });
    let (status, body) =
        common::deploy::try_upload(&app, json!({"kind": "channel", "definition": channel})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let paths = common::deploy::refused_paths(&body);
    assert!(
        paths
            .iter()
            .any(|p| p.starts_with("definition.") && p.ends_with(path)),
        "the refusal names {path}: {body}"
    );
    assert_eq!(
        common::deploy::head(&app).await.0,
        0,
        "nothing was deployed"
    );
}

#[tokio::test]
async fn malformed_config_json_refuses_channel_load_on_single_node() {
    // A config that is not an object at all.
    assert_refused(json!("{ not json"), "bad-cfg-ch", "config").await;
}

#[tokio::test]
async fn wrongly_typed_config_field_refuses_channel_load() {
    // Valid JSON, wrong type — the proposal's example: one typo used to load
    // the channel with no rate limit, validation, dedup, or backpressure.
    assert_refused(
        json!({"rate_limit": {"requests_per_second": "100"}}),
        "typo-cfg-ch",
        "config",
    )
    .await;
}

#[tokio::test]
async fn uncompilable_validation_logic_refuses_channel_load() {
    // Valid JSON that the datalogic compiler rejects (multi-key object).
    // The channel must not serve unvalidated because its input contract
    // stopped compiling.
    assert_refused(
        json!({"validation_logic": {"==": [1, 1], "!=": [1, 2]}}),
        "bad-logic-ch",
        "config.validation_logic",
    )
    .await;
}

/// N5: a `rate_limit.key_logic` that no longer compiles used to fall back to
/// `client_ip`, silently re-dimensioning the limit — a per-API-key or
/// per-tenant limit became per-IP, so tenants behind one NAT shared a bucket
/// and one tenant could take N× its quota by rotating IPs. Same class as
/// N3/N4: refuse the channel rather than serve it with a different control
/// than the one configured.
#[tokio::test]
async fn uncompilable_rate_limit_key_logic_refuses_channel_load() {
    assert_refused(
        json!({"rate_limit": {"requests_per_second": 10, "burst": 10, "key_logic": {"==": [1, 1], "!=": [1, 2]}}}),
        "bad-key-logic-ch",
        "config.rate_limit.key_logic",
    )
    .await;
}

/// #275: a `key_logic` reading a header outside the key context is a warning
/// at load, **not** a quarantine. The distinction matters in both directions:
/// the operator must be told (the request-time answer is a 429 that looks like
/// ordinary throttling), but the path may be composed in ways the static check
/// cannot see, so refusing the channel on a static guess would be worse than
/// the defect it warns about.
#[tokio::test]
async fn an_unreachable_key_logic_header_warns_but_still_loads() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    common::create_and_activate_channel_with_config(
        &app,
        "warn-ch",
        common::simple_log_workflow("Warn WF"),
        json!({"rate_limit": {"requests_per_second": 10, "key_logic": {"var": "headers.deviceid"}}}),
    )
    .await;

    assert!(
        state
            .runtime
            .load()
            .channels
            .get_by_name("warn-ch")
            .is_some(),
        "an unreachable header is a warning, not a quarantine — the channel must load"
    );
    assert!(
        state
            .runtime
            .load()
            .channels
            .quarantine_reason("warn-ch")
            .is_none(),
        "the channel must not be quarantined for a statically-unreachable header"
    );
}

/// F35: the defect this change fixes. One channel with an unparseable
/// `config_json` used to fail *every* operation that triggers a reload —
/// activate, archive, delete, rollout — with a 500 `CONFIG_ERROR`, because the
/// registry reload was all-or-nothing and the admin handler turned a non-empty
/// issue list into a hard error. The instance became unmanageable through the
/// API until someone edited the row out of the database by hand.
///
/// A deployment that cannot build does not wedge the node either: the next
/// deployment, without the broken member, converges, and whatever it adds
/// serves — and so does the one after it, which takes that away again.
#[tokio::test]
async fn a_broken_channel_does_not_wedge_admin_mutations() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    let (_, keep_wf) = common::create_and_activate_channel(
        &app,
        "keep-ch",
        common::simple_log_workflow("Keep WF"),
    )
    .await;
    // A revision that cannot build: a channel whose workflow, stored past
    // upload, carries a typo'd key.
    let broken = common::store_definition(
        &state,
        common::deploy::workflow_definition(&json!({"name": "Wedge", "tasks": [
            {"id": "t1", "name": "log", "conditon": false,
             "function": {"name": "log", "input": {"message": "x"}}}]})),
    )
    .await;
    let wedge = common::deploy::upload(
        &app,
        common::deploy::channel_definition(&common::sync_http_channel("wedge-ch", "wedge-wf")).0,
    )
    .await;
    assert_revision_fails_whole(
        &state,
        &app,
        vec![
            common::deploy::Upsert {
                kind: "workflow",
                id: "wedge-wf".to_string(),
                digest: broken,
                revision: None,
                requires: Vec::new(),
            },
            common::deploy::Upsert {
                kind: "channel",
                id: "wedge-ch".to_string(),
                digest: wedge,
                revision: None,
                requires: vec![("workflow", "wedge-wf".to_string())],
            },
        ],
        &["wedge-ch", "LIKELY_TYPO"],
        "keep-ch",
    )
    .await;

    // The refused members never entered the head, so the next deployment —
    // a channel on the same workflow, the operation that used to 500 — goes
    // through on its own.
    common::deploy_channel(&app, common::sync_http_channel("new-ch", &keep_wf)).await;
    assert!(
        serves_ok(&app, "new-ch").await,
        "a channel deployed after a refused revision must serve"
    );

    // Taking it away again deploys too.
    common::remove_member(&app, "channel", "new-ch").await;
    assert!(!serves_ok(&app, "new-ch").await);
    assert!(serves_ok(&app, "keep-ch").await);
}

/// A quarantined channel's REST route must not resolve either — otherwise it
/// would shadow the path and 503 requests that a working channel could serve.
///
/// The channel is stored past upload with a REST route and a config that
/// does not load; its revision builds with the channel quarantined.
#[tokio::test]
async fn a_quarantined_channel_is_absent_from_the_route_table() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());
    let workflow =
        common::create_and_activate_workflow(&app, common::simple_log_workflow("Route WF")).await;

    let digest = common::store_definition(
        &state,
        json!({"kind": "channel", "definition": {
            "name": "route-ch",
            "channel_type": "sync",
            "protocol": "rest",
            "methods": ["GET"],
            "route_pattern": "/quarantined/{id}",
            "config": "{ not json",
        }}),
    )
    .await;
    let (base, members) = common::deploy::merged(
        &app,
        vec![common::deploy::Upsert {
            kind: "channel",
            id: "route-ch".to_string(),
            digest,
            revision: None,
            requires: vec![("workflow", workflow)],
        }],
    )
    .await;
    let (status, body) = common::deploy::apply(&app, base, members).await;
    assert_eq!(status, StatusCode::ACCEPTED, "the revision commits: {body}");
    assert!(
        quarantine_reason(&app, "route-ch").await.is_some(),
        "the channel is quarantined: {body}"
    );
    assert!(
        state
            .runtime
            .load()
            .channels
            .match_route("GET", "quarantined/42")
            .expect("valid path")
            .is_none(),
        "a quarantined channel must not own a route"
    );
}

// ============================================================
// F33: engine-build failures quarantine the channel
// ============================================================

/// Archiving a workflow out from under an active channel used to leave the
/// channel in the route table with no workflow behind it — requests got an
/// opaque engine error. Engine-build failures now feed the same quarantine
/// as config-load failures.
///
/// A deployment cannot take a workflow out from under its channel at all:
/// the manifest that drops it leaves the channel's requirement unmet, so it
/// is refused, naming both, and the channel keeps serving.
#[tokio::test]
async fn channel_whose_workflow_disappears_is_quarantined() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    let (_ch_name, wf_id) = common::create_and_activate_channel(
        &app,
        "f33-orphan",
        common::simple_log_workflow("F33 Orphan WF"),
    )
    .await;
    assert!(serves_ok(&app, "f33-orphan").await);

    let (base, mut members) = common::deploy::head(&app).await;
    members.retain(|m| !(m["entity"]["kind"] == "workflow" && m["entity"]["id"] == wf_id.as_str()));
    let (status, body) = common::deploy::apply(&app, base, members).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["error"]["code"], "MANIFEST_MISSING_REQUIREMENT",
        "{body}"
    );
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("f33-orphan") && message.contains(&wf_id),
        "the refusal names the channel and the workflow it requires: {message}"
    );

    assert_eq!(
        common::deploy::head(&app).await.0,
        base,
        "nothing was committed"
    );
    assert!(serves_ok(&app, "f33-orphan").await);
}

/// The reason a channel was quarantined, or `None` if it loaded.
async fn quarantine_reason(app: &axum::Router, channel: &str) -> Option<String> {
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/health", None))
        .await
        .unwrap();
    let body = body_json(resp).await;
    body["channels"]["quarantined"]
        .as_array()
        .expect("quarantined list")
        .iter()
        .find(|e| e["channel"] == channel)
        .map(|e| e["reason"].as_str().unwrap_or_default().to_string())
}

/// The v2 statement of a workflow the load refuses: the definition is
/// refused at upload, naming `path`; stored past upload and committed behind
/// `bad_channel`, the revision fails as a whole, naming every one of
/// `fragments`, and a
/// neighbour deployed before it keeps serving.
async fn assert_workflow_refused(
    workflow: serde_json::Value,
    bad_channel: &str,
    path: &str,
    fragments: &[&str],
) {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());
    common::create_and_activate_channel(&app, "neighbour", common::simple_log_workflow("Healthy"))
        .await;

    let definition = common::deploy::workflow_definition(&workflow);
    let (status, body) = common::deploy::try_upload(&app, definition.clone()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{bad_channel}: {body}");
    assert!(
        common::deploy::refused_paths(&body)
            .iter()
            .any(|p| p == path),
        "{bad_channel}: the refusal names {path}: {body}"
    );

    let workflow_id = format!("wf_{bad_channel}");
    let digest = common::store_definition(&state, definition).await;
    let channel = common::deploy::upload(
        &app,
        common::deploy::channel_definition(&common::sync_http_channel(bad_channel, &workflow_id)).0,
    )
    .await;
    assert_revision_fails_whole(
        &state,
        &app,
        vec![
            common::deploy::Upsert {
                kind: "workflow",
                id: workflow_id.clone(),
                digest,
                revision: None,
                requires: Vec::new(),
            },
            common::deploy::Upsert {
                kind: "channel",
                id: bad_channel.to_string(),
                digest: channel,
                revision: None,
                requires: vec![("workflow", workflow_id)],
            },
        ],
        fragments,
        "neighbour",
    )
    .await;
    assert!(
        !serves_ok(&app, bad_channel).await,
        "{bad_channel} was never published"
    );
}

/// `channel_logic`, 1.12's spelling of a computed `channel`, is no longer
/// read: a definition carrying only it has no `channel`, so it is refused at
/// upload naming the missing field, and stored past upload it fails its
/// revision, rather than running with no target.
#[tokio::test]
async fn channel_call_with_only_channel_logic_is_quarantined() {
    assert_workflow_refused(
        json!({"name": "dyn-fanout", "tasks": [{"id": "t1", "name": "fan",
            "function": {"name": "channel_call",
                "input": {"channel_logic": {"var": "target"}}}}]}),
        "dyn-fanout",
        "definition.tasks[0].function.input.channel",
        &["channel 'dyn-fanout'", "task 't1'"],
    )
    .await;
}

/// The other half: a typed field carrying the wrong type must be caught at
/// load and quarantine only its own channel, rather than failing the build for
/// every channel on the node.
///
/// Deployed, it is caught at upload; stored past upload, it fails its
/// revision — never the generation already serving.
#[tokio::test]
async fn channel_call_with_a_wrongly_typed_field_quarantines_only_its_channel() {
    assert_workflow_refused(
        json!({"name": "bad-typed-input", "tasks": [{"id": "t1", "name": "fan",
            "function": {"name": "channel_call", "input": {"channel": 7}}}]}),
        "bad-typed-input",
        "definition.tasks[0].function.input.channel",
        &["channel 'bad-typed-input'", "task 't1'"],
    )
    .await;
}

/// I182, as refined: a Defect quarantines its channel only where
/// dataflow-rs 4 would fail every message or the plan names it. A
/// `PAYLOAD_VAR` (a `var` rooted at `payload`, which reads nothing) is
/// neither: upload accepts it, and the channel deploys and serves, as on
/// 1.12.
#[tokio::test]
async fn a_payload_var_defect_loads_and_serves() {
    let state = common::test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());

    common::create_and_activate_channel(
        &app,
        "payload-var",
        json!({"name": "payload-var", "tasks": [{"id": "t1", "name": "m",
            "function": {"name": "map", "input": {"mappings": [
                {"path": "data.copied", "logic": {"var": "payload.x"}}]}}}]}),
    )
    .await;

    assert_eq!(quarantine_reason(&app, "payload-var").await, None);
    assert!(serves_ok(&app, "payload-var").await);
    assert!(
        state
            .runtime
            .load()
            .engine
            .program()
            .workflows()
            .iter()
            .any(|w| w.channel == "payload-var"),
        "dataflow-rs 4 loads it too"
    );
}

/// Break 21 (I185): a function whose result is never an object, written at
/// a literal context root, fails every message on dataflow-rs 4, so its
/// Defect refuses the workflow, the code and path in the reason. 1.12
/// loaded it and replaced `data` with the rows.
#[tokio::test]
async fn a_root_output_defect_quarantines_only_its_channel() {
    assert_workflow_refused(
        json!({"name": "rows-at-root", "tasks": [{"id": "t1", "name": "read",
            "function": {"name": "db_read", "input": {
                "connector": "db", "query": "SELECT 1", "output": "data"}}}]}),
        "rows-at-root",
        "definition.tasks[0].function.input.output",
        &["task 't1': INPUT_PARSE at tasks[0].function.input.output"],
    )
    .await;
}

/// O3's and O10's named load break: an `output` written as a string that
/// names no context root (`"result"`, `"response"`), where 1.12 wrote the
/// answer at a key no response shows, is `INVALID_PATH` at load. The v1
/// create path accepted it and the load quarantined the channel; upload
/// refuses it, and a revision carrying it past upload fails.
#[tokio::test]
async fn an_output_that_is_no_path_quarantines_its_channel() {
    let task = |function: &str, input: serde_json::Value| {
        json!({"name": "No Path", "condition": true, "tasks": [
            {"id": "t1", "name": "t1", "function": {"name": function, "input": input}}
        ]})
    };
    for (channel, spelled, workflow) in [
        (
            "cc-result",
            "result",
            task(
                "channel_call",
                json!({"channel": "nonexistent", "output": "result"}),
            ),
        ),
        (
            "http-response",
            "response",
            task(
                "http_call",
                json!({"connector": "no-path-api", "method": "POST", "path": "/x",
                       "output": "response"}),
            ),
        ),
    ] {
        assert_workflow_refused(
            workflow,
            channel,
            "definition.tasks[0].function.input.output",
            &[
                "task 't1': INVALID_PATH at tasks[0].function.input.output",
                &format!("path `{spelled}` must start at `data`"),
            ],
        )
        .await;
    }
}

/// Break 2: a near miss of a known key is a `LIKELY_TYPO` Defect, and it
/// refuses the workflow: `conditon` would otherwise run the task
/// unconditionally, which is what 1.12 did with it. Upload refuses it;
/// stored past upload, it fails its revision.
#[tokio::test]
async fn a_likely_typo_quarantines_its_channel() {
    assert_workflow_refused(
        json!({"name": "typo", "tasks": [{"id": "t1", "name": "log", "conditon": false,
            "function": {"name": "log", "input": {"message": "x"}}}]}),
        "typo",
        "definition.tasks[0].conditon",
        &["LIKELY_TYPO at tasks[0].conditon"],
    )
    .await;
}

/// A stored workflow whose `loop` the engine build refuses must not fail the
/// build for every channel on the node.
///
/// `Engine::check_workflow` does not run `Workflow::validate()`; only `build`
/// does, and it fails on the first refusal. With nothing screening it, a
/// duplicate step id across `loop.setup` and `tasks` failed every reload, and
/// at boot stopped the process. Deployed, it is refused at upload; stored
/// past upload, it fails its own revision and the serving one stays.
#[tokio::test]
async fn a_loop_the_engine_refuses_quarantines_only_its_channel() {
    let log = json!({"name": "log", "input": {"message": "x"}});
    // dataflow-rs 4's verdict decides which rows load, so each reason names
    // its issue's code and path (I182), where it was dataflow-rs 3's
    // `Workflow::validate` text ("Duplicate task id …").
    let cases = [
        // A setup step shares the body's id namespace.
        (
            "loop-dup-id",
            json!({"max": 2, "setup": [{"id": "t1", "name": "s", "function": log}]}),
            "tasks[0].id",
            "DUPLICATE_STEP_ID at loop.setup[0].id",
        ),
        // Two setup steps with one id.
        (
            "loop-dup-setup",
            json!({"max": 2, "setup": [
                {"id": "s", "name": "s", "function": log},
                {"id": "s", "name": "s", "function": log}]}),
            "loop.setup[1].id",
            "DUPLICATE_STEP_ID at loop.setup[1].id",
        ),
        // `as` names a `temp_data` slot and must be a plain key.
        (
            "loop-bad-as",
            json!({"max": 2, "over": {"var": "data.items"}, "as": "a..b"}),
            "loop.as",
            "at loop.as",
        ),
    ];
    for (name, loop_config, path, fragment) in cases {
        assert_workflow_refused(
            json!({"name": name, "tasks": [{"id": "t1", "name": "body", "function": log}],
                   "loop": loop_config}),
            name,
            &format!("definition.{path}"),
            &[fragment],
        )
        .await;
    }
}
