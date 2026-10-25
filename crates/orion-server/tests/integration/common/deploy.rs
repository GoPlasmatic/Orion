//! Deploying test fixtures the way a node is deployed: definitions uploaded,
//! connections created, and a manifest applied over the current head.
//!
//! The shared helpers in [`super`] (`create_and_activate_channel`,
//! `create_connector`, …) keep their v1-shaped arguments, so a behaviour test
//! reads as it always did; this module turns each call into one deployment.
//! Every call reads the head manifest, merges its members by `(kind, id)` — a
//! member whose definition changed gets the next revision, a new one revision
//! 1 — re-resolves every `requires` to its target's current revision, and
//! applies the result. So a test that creates a connector, then a channel,
//! then another channel, ends up with one manifest holding all of them, as a
//! real deployment would.

use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{body_json, json_request};

/// One member to put in the next deployment.
pub struct Upsert {
    pub kind: &'static str,
    pub id: String,
    pub digest: String,
    /// A connection's revision is the one it was created with; every other
    /// kind's is assigned by the merge.
    pub revision: Option<u64>,
    /// `(kind, id)` of what the member depends on.
    pub requires: Vec<(&'static str, String)>,
}

async fn send(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request(method, uri, body))
        .await
        .expect("the router answers");
    let status = resp.status();
    (status, body_json(resp).await)
}

/// Upload `definition` (`{"kind": …, "definition": {…}}`), answering the
/// status and body as they are — for a test whose subject is the refusal.
pub async fn try_upload(app: &Router, definition: Value) -> (StatusCode, Value) {
    send(app, "POST", "/api/v2/admin/definitions", Some(definition)).await
}

/// Upload `definition` (`{"kind": …, "definition": {…}}`), answering its
/// digest. Panics on a refusal, naming it: a fixture that does not deploy is
/// a broken test.
pub async fn upload(app: &Router, definition: Value) -> String {
    let (status, body) = try_upload(app, definition.clone()).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "definition refused: {body}\n{definition}"
    );
    body["digest"].as_str().expect("a digest").to_string()
}

/// The `path` of every problem a refused upload names.
pub fn refused_paths(body: &Value) -> Vec<String> {
    body["error"]["details"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["path"].as_str().map(str::to_string))
        .collect()
}

/// Upload `definition`, asserting it is refused naming `path` (rooted at the
/// request's `definition`), answering the refusal — the v2 statement of a
/// definition the v1 load quarantined.
pub async fn assert_upload_refused(app: &Router, definition: Value, path: &str) -> Value {
    let (status, body) = try_upload(app, definition.clone()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}\n{definition}");
    assert!(
        refused_paths(&body).iter().any(|p| p == path),
        "the refusal names {path}: {body}"
    );
    body
}

/// The head revision and its members.
pub async fn head(app: &Router) -> (u64, Vec<Value>) {
    let (status, summary) = send(app, "GET", "/api/v2/admin/deployments", None).await;
    assert_eq!(status, StatusCode::OK, "{summary}");
    let head = summary["head_revision"].as_u64().unwrap_or(0);
    if head == 0 {
        return (0, Vec::new());
    }
    let (status, revision) = send(
        app,
        "GET",
        &format!("/api/v2/admin/deployments/{head}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{revision}");
    let members = revision["manifest"]["members"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    (head, members)
}

/// The definition digest the head's member `(kind, id)` names.
pub async fn member_digest(app: &Router, kind: &str, id: &str) -> String {
    let (_, members) = head(app).await;
    members
        .iter()
        .find(|m| m["entity"]["kind"] == kind && m["entity"]["id"] == id)
        .and_then(|m| m["definition_digest"].as_str())
        .unwrap_or_else(|| panic!("the head has no {kind} '{id}'"))
        .to_string()
}

/// The head's members with `upserts` merged in by `(kind, id)`, every
/// requirement resolved to its target's current revision, and the head
/// revision they are based on.
pub async fn merged(app: &Router, upserts: Vec<Upsert>) -> (u64, Vec<Value>) {
    let (base, mut members) = head(app).await;
    for upsert in upserts {
        let at = members.iter().position(|m| {
            m["entity"]["kind"] == upsert.kind && m["entity"]["id"] == upsert.id.as_str()
        });
        let revision = match (upsert.revision, at) {
            (Some(revision), _) => revision,
            (None, Some(i)) if members[i]["definition_digest"] == upsert.digest.as_str() => {
                members[i]["entity"]["revision"].as_u64().unwrap_or(1)
            }
            (None, Some(i)) => members[i]["entity"]["revision"].as_u64().unwrap_or(0) + 1,
            (None, None) => 1,
        };
        let member = json!({
            "entity": {"kind": upsert.kind, "id": upsert.id, "revision": revision},
            "definition_digest": upsert.digest,
            "requires": upsert
                .requires
                .iter()
                .map(|(kind, id)| json!({"kind": kind, "id": id, "revision": 1}))
                .collect::<Vec<_>>(),
        });
        match at {
            Some(i) => members[i] = member,
            None => members.push(member),
        }
    }
    resolve_requires(&mut members);
    (base, members)
}

/// Point every requirement at its target's current revision.
fn resolve_requires(members: &mut [Value]) {
    let current: Vec<(String, String, u64)> = members
        .iter()
        .map(|m| {
            (
                m["entity"]["kind"].as_str().unwrap_or_default().to_string(),
                m["entity"]["id"].as_str().unwrap_or_default().to_string(),
                m["entity"]["revision"].as_u64().unwrap_or(1),
            )
        })
        .collect();
    for member in members.iter_mut() {
        if let Some(requires) = member["requires"].as_array_mut() {
            for req in requires {
                if let Some((_, _, revision)) = current
                    .iter()
                    .find(|(kind, id, _)| req["kind"] == kind.as_str() && req["id"] == id.as_str())
                {
                    req["revision"] = json!(revision);
                }
            }
        }
    }
}

/// Apply `members` over `base`, answering the status and the body as they
/// are — for a test whose subject is what an apply answers.
pub async fn apply(app: &Router, base: u64, members: Vec<Value>) -> (StatusCode, Value) {
    static KEYS: AtomicU64 = AtomicU64::new(0);
    let key = format!("fixture-{}", KEYS.fetch_add(1, Ordering::Relaxed));
    send(
        app,
        "POST",
        "/api/v2/admin/deployments",
        Some(json!({
            "idempotency_key": key,
            "base_revision": base,
            "manifest": {"schema_version": 2, "providers": [], "members": members},
            "reason": "test fixture",
        })),
    )
    .await
}

/// Commit `members` over `base` straight into `state`'s deployment store,
/// past the apply route's build check — the head another node, or a binary
/// with other plugins or settings, could have committed — answering the
/// committed revision. Nothing is built.
pub async fn commit_unchecked(
    state: &orion::server::state::AppState,
    base: u64,
    members: Vec<Value>,
) -> u64 {
    static KEYS: AtomicU64 = AtomicU64::new(0);
    let manifest =
        serde_json::from_value(json!({"schema_version": 2, "providers": [], "members": members}))
            .expect("a manifest");
    let store = &state.composition.deployments;
    let plan = store
        .plan_candidate_in_db(&state.db_pool, base, manifest)
        .await
        .expect("the store plans the candidate");
    store
        .apply_candidate_in_db(
            &state.db_pool,
            "test",
            &format!("unchecked-{}", KEYS.fetch_add(1, Ordering::Relaxed)),
            &plan,
            "committed past the build check",
            state.composition.admission.as_ref(),
        )
        .await
        .expect("the store commits the candidate")
        .committed_revision
}

/// Panic unless an apply was accepted and this node converged on it.
fn assert_converged(status: StatusCode, body: &Value) {
    assert_eq!(status, StatusCode::ACCEPTED, "deployment refused: {body}");
    assert_eq!(
        body["evaluation"]["converged"], true,
        "the node did not converge on the fixture deployment: {body}"
    );
}

/// Apply the head plus `upserts`, answering the apply body. Panics unless the
/// node converged on it.
pub async fn deploy(app: &Router, upserts: Vec<Upsert>) -> Value {
    let (base, members) = merged(app, upserts).await;
    let (status, body) = apply(app, base, members).await;
    assert_converged(status, &body);
    body
}

/// Apply the head without the members `gone` names by `(kind, id)` — what
/// archiving or deleting them was in v1 — answering the apply body.
/// Requirements on them are left in place, so removing something another
/// member requires is refused, as it would be for any deployer; this panics
/// on that refusal.
pub async fn remove(app: &Router, gone: &[(&str, &str)]) -> Value {
    let (base, mut members) = head(app).await;
    for (kind, id) in gone {
        let before = members.len();
        members.retain(|m| !(m["entity"]["kind"] == *kind && m["entity"]["id"] == *id));
        assert_ne!(members.len(), before, "the head has no {kind} '{id}'");
    }
    let (status, body) = apply(app, base, members).await;
    assert_converged(status, &body);
    body
}

/// A fresh, unique workflow id, as a v1 create minted one per call.
pub fn workflow_id(name: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!(
        "{}-{}",
        slug.trim_matches('-'),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// The id a v1-shaped workflow body asks for (`workflow_id`, or its alias
/// `id`), else a fresh one minted from its name.
pub fn workflow_id_of(workflow: &Value) -> String {
    workflow["workflow_id"]
        .as_str()
        .or_else(|| workflow["id"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| workflow_id(workflow["name"].as_str().unwrap_or("wf")))
}

/// A v1-shaped workflow body as a workflow definition. Identity and
/// lifecycle keys leave; `continue_on_error` becomes `on_failure`. A key a
/// definition does not take panics, naming it.
pub fn workflow_definition(workflow: &Value) -> Value {
    let mut def = serde_json::Map::new();
    for (key, value) in workflow.as_object().expect("a workflow object") {
        match key.as_str() {
            "workflow_id" | "id" | "rollout_percentage" | "status" | "version" => {}
            "continue_on_error" => {
                if value == &json!(true) {
                    def.insert("on_failure".into(), json!("continue"));
                }
            }
            "name" | "description" | "priority" | "condition" | "tasks" | "tags" | "loop"
            | "on_failure" => {
                if !value.is_null() {
                    def.insert(key.clone(), value.clone());
                }
            }
            other => panic!("a workflow definition does not take '{other}': {workflow}"),
        }
    }
    json!({"kind": "workflow", "definition": def})
}

/// A v1-shaped channel body as a channel definition and the workflow it is
/// bound to.
pub fn channel_definition(channel: &Value) -> (Value, Option<String>) {
    let mut def = serde_json::Map::new();
    let mut workflow = None;
    for (key, value) in channel.as_object().expect("a channel object") {
        match key.as_str() {
            "workflow_id" => workflow = value.as_str().map(str::to_string),
            "channel_id" | "status" | "version" => {}
            "topic" | "consumer_group" if value.is_null() => {}
            "name" | "description" | "channel_type" | "protocol" | "methods" | "route_pattern"
            | "transport_config" | "config" | "priority" | "tags" => {
                if !value.is_null() {
                    def.insert(key.clone(), value.clone());
                }
            }
            other => panic!("a channel definition does not take '{other}': {channel}"),
        }
    }
    (json!({"kind": "channel", "definition": def}), workflow)
}

/// The built-in driver a v1 connector's type (and, for `db`, its `driver`
/// or connection string) names.
fn driver_of(connector_type: &str, config: &Value) -> &'static str {
    match connector_type {
        "http" => "org.orion.http",
        "cache" => "org.orion.memory-cache",
        "db" => {
            let named = config["driver"].as_str().unwrap_or_default();
            let url = config["connection_string"].as_str().unwrap_or_default();
            match named {
                "postgresql" => "org.orion.postgresql",
                "mysql" => "org.orion.mysql",
                "sqlite" => "org.orion.sqlite",
                _ if url.starts_with("postgres") => "org.orion.postgresql",
                _ if url.starts_with("mysql") || url.starts_with("mariadb") => "org.orion.mysql",
                _ if url.starts_with("sqlite") => "org.orion.sqlite",
                _ => panic!("cannot tell this db connector's driver: {config}"),
            }
        }
        other => panic!("no built-in driver serves a '{other}' connector"),
    }
}

/// Move every secret reference in `value` to `locators` at its dotted path.
fn split_secrets(value: &mut Value, path: &str, locators: &mut serde_json::Map<String, Value>) {
    let Value::Object(map) = value else {
        return;
    };
    let keys: Vec<String> = map.keys().cloned().collect();
    for key in keys {
        let at = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        let is_ref = map[&key]
            .as_str()
            .is_some_and(orion::connector::secrets::is_secret_reference);
        if is_ref {
            if let Some(reference) = map.remove(&key) {
                locators.insert(at, reference);
            }
        } else if let Some(child) = map.get_mut(&key) {
            split_secrets(child, &at, locators);
        }
    }
}

/// Create a connection revision from a v1-shaped connector body
/// (`{name, connector_type, config}`), answering `(id, revision, digest)`.
pub async fn create_connection(app: &Router, connector: &Value) -> (String, u64, String) {
    let id = connector["name"]
        .as_str()
        .expect("a connector name")
        .to_string();
    let connector_type = connector["connector_type"]
        .as_str()
        .or_else(|| connector["type"].as_str())
        .expect("a connector type");
    let mut config = connector["config"].clone();
    let driver = driver_of(connector_type, &config);
    if let Some(object) = config.as_object_mut() {
        object.remove("type");
        object.remove("driver");
    }
    let mut locators = serde_json::Map::new();
    split_secrets(&mut config, "", &mut locators);

    let (status, caps) = send(app, "GET", "/api/v2/admin/runtime-capabilities", None).await;
    assert_eq!(status, StatusCode::OK, "{caps}");
    let provider = caps["provider_backends"]
        .as_array()
        .and_then(|backends| backends.iter().find(|b| b["driver_id"] == driver))
        .and_then(|b| b["package_digest"].as_str())
        .unwrap_or_else(|| panic!("the node reports no built-in digest for {driver}"))
        .to_string();

    let (status, listed) = send(app, "GET", "/api/v2/admin/connections", None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let revision = listed["connections"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["connection_id"] == id.as_str())
        .filter_map(|c| c["revision"].as_u64())
        .max()
        .unwrap_or(0)
        + 1;
    let (status, created) = send(
        app,
        "POST",
        "/api/v2/admin/connections",
        Some(json!({
            "connection_id": id,
            "revision": revision,
            "provider_digest": provider,
            "public_config": config,
            "secret_locators": locators,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "connection refused: {created}");
    let digest = created["connection"]["configuration_digest"]
        .as_str()
        .expect("a digest")
        .to_string();
    (id, revision, digest)
}

/// Run a workflow once through `POST /definitions/test`, without deploying
/// it, answering the status and body as they are. `workflow` is a v1-shaped
/// body, as [`workflow_definition`] takes it; `metadata` is sent only when
/// given.
pub async fn test_run(
    app: &Router,
    workflow: &Value,
    data: Value,
    metadata: Option<Value>,
) -> (StatusCode, Value) {
    let mut body = json!({
        "definition": workflow_definition(workflow)["definition"].clone(),
        "data": data,
    });
    if let Some(metadata) = metadata {
        body["metadata"] = metadata;
    }
    send(app, "POST", "/api/v2/admin/definitions/test", Some(body)).await
}
