//! Workflow provider packages end to end: scaffolded, uploaded, admitted,
//! bound to their dependencies by a connection, called from a deployed
//! workflow through `connector_call`, revoked, and served again after a
//! restart — and the hermetic rules refusing what a package may not do.

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common::deploy::{self, Upsert};
use crate::common::{self, ScratchDir, body_json, json_request};

async fn send(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request(method, uri, body))
        .await
        .expect("the router answers");
    let status = resp.status();
    (status, body_json(resp).await)
}

/// The scaffolded `workflow` package, its entry workflow replaced by `tasks`
/// and its manifest declaring one dependency, `cache`, on the built-in
/// memory cache with `cache.set` and `cache.get`.
fn package_files(tasks: Value, extra_files: &[(&str, Value)]) -> serde_json::Map<String, Value> {
    let pkg = orion::test_support::scaffold_provider_package(
        "acme",
        "acme.recall",
        "1.0.0",
        orion_extension_api::ImplementationKind::Workflow,
    );
    let mut files = serde_json::Map::new();
    for (path, bytes) in pkg.files {
        files.insert(
            path,
            Value::String(String::from_utf8(bytes).expect("scaffold files are text")),
        );
    }
    let mut manifest: Value =
        serde_json::from_str(files["connector.json"].as_str().unwrap()).unwrap();
    manifest["dependencies"] = json!([{
        "kind": "provider",
        "alias": "cache",
        "id": "org.orion.memory-cache",
        "requires": ">=2.0.0",
        "operations": ["cache.set", "cache.get"],
    }]);
    let mut workflows = vec![json!("workflows/entry.json")];
    for (path, _) in extra_files {
        workflows.push(json!(path));
    }
    manifest["implementation"]["workflows"] = Value::Array(workflows);
    files.insert("connector.json".into(), Value::String(manifest.to_string()));
    files.insert(
        "workflows/entry.json".into(),
        Value::String(json!({"id": "entry", "name": "entry", "tasks": tasks}).to_string()),
    );
    for (path, doc) in extra_files {
        files.insert((*path).into(), Value::String(doc.to_string()));
    }
    files
}

/// The entry workflow of `item.get`: remember the id through the bound
/// cache, read it back, and hash it — a dependency call and a host function.
fn recall_tasks() -> Value {
    json!([
        {"id": "remember", "name": "remember", "function": {"name": "connector_call", "input": {
            "connection": "cache", "operation": "cache.set",
            "input": {"key": "last", "value": {"var": "data.id"}},
            "output": "data.stored"}}},
        {"id": "recall", "name": "recall", "function": {"name": "connector_call", "input": {
            "connection": "cache", "operation": "cache.get",
            "input": {"key": "last"}, "output": "data.recalled"}}},
        {"id": "digest", "name": "digest", "function": {"name": "crypto", "input": {
            "op": "hash", "algorithm": "sha256", "data": {"var": "data.id"},
            "output": "data.digest"}}},
    ])
}

async fn upload(app: &Router, files: serde_json::Map<String, Value>) -> (StatusCode, Value) {
    send(
        app,
        "POST",
        "/api/v2/admin/connector-packages",
        Some(json!({"files": files, "admit": true})),
    )
    .await
}

fn detail_paths(body: &Value) -> Vec<String> {
    deploy::refused_paths(body)
}

/// Upload and admit the recall package, create the cache connection and the
/// `crm` connection over the package bound to it, and deploy a channel whose
/// workflow calls `crm`. Answers the package digest.
async fn deploy_recall(app: &Router) -> String {
    let (status, body) = upload(app, package_files(recall_tasks(), &[])).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["admission"]["revoked"], false, "{body}");
    let digest = body["package"]["digest"].as_str().unwrap().to_string();

    let (_, _, cache_digest) = deploy::create_connection(
        app,
        &json!({"name": "cache-main", "connector_type": "cache", "config": {"backend": "memory"}}),
    )
    .await;

    // Every declared alias must be bound.
    let (status, refused) = send(
        app,
        "POST",
        "/api/v2/admin/connections/crm/revisions",
        Some(json!({
            "revision": 1, "provider_digest": digest,
            "public_config": {"endpoint": "https://crm.example"},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        detail_paths(&refused).contains(&"bindings.cache".to_string()),
        "{refused}"
    );

    let (status, created) = send(
        app,
        "POST",
        "/api/v2/admin/connections/crm/revisions",
        Some(json!({
            "revision": 1, "provider_digest": digest,
            "public_config": {"endpoint": "https://crm.example"},
            "bindings": {"cache": "cache-main"},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["connection"]["bindings"]["cache"], "cache-main");
    let crm_digest = created["connection"]["configuration_digest"]
        .as_str()
        .unwrap()
        .to_string();

    let workflow = deploy::upload(
        app,
        json!({"kind": "workflow", "definition": {"name": "front", "tasks": [
            {"id": "call", "function": {"name": "connector_call", "input": {
                "connection": "crm", "kind": "workflow", "operation": "item.get",
                "input": {"id": {"var": "data.id"}}, "output": "data.item"}}}
        ]}}),
    )
    .await;
    let channel = deploy::upload(
        app,
        json!({"kind": "channel", "definition": {
            "name": "front-ch", "protocol": "rest", "methods": ["POST"],
            "route_pattern": "/front-ch"}}),
    )
    .await;
    let connection = |id: &str, digest: &str, requires: Vec<(&'static str, String)>| Upsert {
        kind: "connection",
        id: id.to_string(),
        digest: digest.to_string(),
        revision: Some(1),
        requires,
    };

    // The package connection's member must require what it binds.
    let (base, members) = deploy::merged(
        app,
        vec![
            connection("cache-main", &cache_digest, vec![]),
            connection("crm", &crm_digest, vec![]),
        ],
    )
    .await;
    let (status, refused) = apply(app, base, members, &digest).await;
    assert_ne!(status, StatusCode::ACCEPTED, "{refused}");
    assert!(
        refused.to_string().contains("does not require"),
        "the refusal names the missing requirement: {refused}"
    );

    let (base, members) = deploy::merged(
        app,
        vec![
            connection("cache-main", &cache_digest, vec![]),
            connection(
                "crm",
                &crm_digest,
                vec![("connection", "cache-main".into())],
            ),
            Upsert {
                kind: "workflow",
                id: "front".into(),
                digest: workflow,
                revision: None,
                requires: vec![("connection", "crm".into())],
            },
            Upsert {
                kind: "channel",
                id: "front-ch".into(),
                digest: channel,
                revision: None,
                requires: vec![("workflow", "front".into())],
            },
        ],
    )
    .await;
    let (status, body) = apply(app, base, members, &digest).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["evaluation"]["converged"], true, "{body}");
    digest
}

/// Apply `members` over `base` with the package `digest` declared as the
/// manifest's provider, as a connection over an uploaded package requires.
async fn apply(app: &Router, base: u64, members: Vec<Value>, digest: &str) -> (StatusCode, Value) {
    static KEYS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let key = format!(
        "workflow-package-{}",
        KEYS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    send(
        app,
        "POST",
        "/api/v2/admin/deployments",
        Some(
            crate::common::deploy::reviewed(
                app,
                json!({
                    "idempotency_key": key,
                    "base_revision": base,
                    "manifest": {
                        "schema_version": 2,
                        "providers": [{
                            "publisher": "acme",
                            "provider_id": "acme.recall",
                            "version": "1.0.0",
                            "digest": digest,
                        }],
                        "members": members,
                    },
                    "reason": "workflow package test",
                }),
            )
            .await,
        ),
    )
    .await
}

async fn call_front(app: &Router) -> (StatusCode, Value) {
    send(
        app,
        "POST",
        "/api/v2/data/front-ch",
        Some(json!({"data": {"id": "42"}})),
    )
    .await
}

fn assert_recalled(status: StatusCode, body: &Value) {
    assert_eq!(status, StatusCode::OK, "{body}");
    let item = &body["data"]["item"];
    assert_eq!(item["id"], "42", "{body}");
    assert_eq!(item["stored"]["stored"], true, "{body}");
    assert_eq!(item["recalled"]["value"], "42", "{body}");
    assert_eq!(
        item["digest"], "73475cb40a568e8da8a045ced110137e159f890ac4da883b6b17dc651b3a8049",
        "{body}"
    );
}

async fn workflow_providers(app: &Router) -> Vec<String> {
    let (_, caps) = send(app, "GET", "/api/v2/admin/runtime-capabilities", None).await;
    caps["provider_backends"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["kind"] == "workflow")
        .filter_map(|b| b["driver_id"].as_str().map(str::to_string))
        .collect()
}

/// The round trip: the package's operation runs through the deployment's
/// workflow, calls its bound dependency and a host function, and answers
/// with its `data`. A restart rebuilds the backend from the store and serves
/// it again; revoking the package unregisters it and the call is refused.
#[tokio::test]
async fn a_workflow_package_runs_survives_a_restart_and_stops_when_revoked() {
    let dir = ScratchDir::new("workflow_package");
    let config = || {
        let mut config = orion::config::AppConfig::default();
        config.storage.url = dir.url();
        config.trace_storage.mode = orion::config::TraceStorageMode::Sync;
        config
    };

    let digest = {
        let app = orion::server::build_router(common::test_state_with_config(config()).await);
        let digest = deploy_recall(&app).await;
        assert!(workflow_providers(&app).await.contains(&digest));
        let (status, body) = call_front(&app).await;
        assert_recalled(status, &body);
        digest
    };

    // A restart: a fresh node over the same database.
    let app = orion::server::build_router(common::test_state_with_config(config()).await);
    assert!(
        workflow_providers(&app).await.contains(&digest),
        "the admitted package is registered again at boot"
    );
    let (status, body) = call_front(&app).await;
    assert_recalled(status, &body);

    let (status, body) = send(
        &app,
        "POST",
        &format!("/api/v2/admin/connector-packages/{digest}/revocations"),
        Some(json!({"reason": "test"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(!workflow_providers(&app).await.contains(&digest));
    let (status, body) = call_front(&app).await;
    assert_ne!(
        status,
        StatusCode::OK,
        "a revoked package is not served: {body}"
    );
}

/// An upload case: the entry workflow's tasks, extra workflow files, and the
/// path the refusal must name.
type RefusalCase = (Value, Vec<(&'static str, Value)>, &'static str);

/// What a package workflow may not do is refused at upload, naming the file
/// and the task: a host function that reaches the deployment by name, a
/// secret, a helper workflow, an undeclared dependency.
#[tokio::test]
async fn the_hermetic_rules_refuse_at_upload() {
    let app = common::test_app().await;
    let cases: Vec<RefusalCase> = vec![
        (
            json!([{"id": "egress", "name": "egress", "function": {"name": "http_call",
                "input": {"connector": "orders-api", "method": "GET"}}}]),
            vec![],
            "workflows/entry.json:tasks[0].function.name",
        ),
        (
            json!([{"id": "peek", "name": "peek", "function": {"name": "db_read",
                "input": {"connector": "orders-db", "query": "SELECT 1"}}}]),
            vec![],
            "workflows/entry.json:tasks[0].function.name",
        ),
        (
            json!([{"id": "mac", "name": "mac", "function": {"name": "crypto", "input": {
                "op": "hmac", "algorithm": "sha256", "data": "x",
                "key": {"secret": "partner_hmac"}, "output": "data.mac"}}}]),
            vec![],
            "workflows/entry.json:tasks[0].function.input.key",
        ),
        (
            json!([{"id": "other", "name": "other", "function": {"name": "connector_call",
                "input": {"connection": "orders-db", "operation": "query"}}}]),
            vec![],
            "workflows/entry.json:tasks[0].function.input.connection",
        ),
        (
            recall_tasks(),
            vec![(
                "workflows/helper.json",
                json!({"id": "helper", "name": "helper", "tasks": recall_tasks()}),
            )],
            "workflows/helper.json",
        ),
    ];
    for (tasks, extra, path) in cases {
        let (status, body) = upload(&app, package_files(tasks, &extra)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        assert!(
            detail_paths(&body).iter().any(|p| p == path),
            "the refusal names {path}: {body}"
        );
    }
    let (_, listed) = send(&app, "GET", "/api/v2/admin/connector-packages", None).await;
    assert_eq!(listed["count"], 0, "nothing refused was stored: {listed}");
}
