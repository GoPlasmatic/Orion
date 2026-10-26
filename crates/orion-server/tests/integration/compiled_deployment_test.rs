//! What `orion-cli compile` writes is what a server stores: a compiled
//! deployment directory, uploaded through the admin API of an in-process
//! node. The compile runs the `orion-cli` binary beside this crate's own,
//! which `cargo test --workspace` builds; without one these skip.

use crate::common::{self, ScratchDir};

/// A set with one of each thing a deployment carries: a workflow on v1's
/// `continue_on_error` calling a connection through `connector_call` and
/// another through `http_call`, the channel serving it, an HTTP connector
/// whose token is a secret reference, and a cache connector.
fn deployment_set(label: &str) -> ScratchDir {
    let scratch = ScratchDir::new(label);
    let dir = scratch.path();
    std::fs::write(
        dir.join("conn-api.json"),
        r#"{ "name": "partner-api", "connector_type": "http", "tags": ["team:payments"],
             "config": { "type": "http", "url": "https://api.example.com",
                         "auth": { "type": "bearer", "token": "env://PARTNER_TOKEN" } } }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("conn-cache.json"),
        r#"{ "name": "local-cache", "connector_type": "cache",
             "config": { "type": "cache", "backend": "memory" } }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("wf.json"),
        r#"{ "workflow_id": "remember", "name": "Remember", "continue_on_error": true,
             "tasks": [
               { "id": "set", "name": "Set", "function": { "name": "connector_call", "input": {
                   "connection": "local-cache", "target_id": "org.orion.memory-cache",
                   "operation": "cache.set", "input": { "key": "k", "value": "v" },
                   "output": "data.set" } } },
               { "id": "ping", "name": "Ping", "function": { "name": "http_call", "input": {
                   "connector": "partner-api", "method": "GET", "output": "data.ping" } } },
               { "id": "done", "name": "Done", "function": { "name": "map", "input": {
                   "mappings": [ { "path": "data.done", "logic": true } ] } } } ] }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("ch.json"),
        r#"{ "channel_id": "remember-api", "name": "remember-api", "channel_type": "sync",
             "protocol": "rest", "methods": ["POST"], "route_pattern": "/remember",
             "workflow_id": "remember" }"#,
    )
    .unwrap();
    scratch
}

fn compile_deployment(
    cli: &std::path::Path,
    dir: &std::path::Path,
    out: &std::path::Path,
) -> (bool, String) {
    let out = std::process::Command::new(cli)
        .args([
            "compile",
            dir.to_str().unwrap(),
            "--format",
            "deployment",
            "-o",
            out.to_str().unwrap(),
        ])
        .output()
        .expect("run orion-cli");
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), combined)
}

fn read_json(path: &std::path::Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// An OAuth2 http connector compiles and deploys: its `token_url` is an
/// endpoint, public, while the client credentials and the refresh-token
/// seed are secret references that become locators.
#[tokio::test]
async fn an_oauth2_connection_compiles_and_creates() {
    use tower::ServiceExt as _;
    let Some(cli) = common::orion_cli_bin() else {
        return;
    };
    let scratch = deployment_set("compile-deployment-oauth2");
    let dir = scratch.path();
    std::fs::write(
        dir.join("conn-crm.json"),
        r#"{ "name": "crm", "connector_type": "http",
             "config": { "type": "http", "url": "https://api.example.com",
               "auth": { "type": "oauth2", "grant": "refresh_token",
                 "token_url": "https://idp.example.com/oauth2/token",
                 "client_id": "env://CRM_CLIENT_ID",
                 "client_secret": "env://CRM_CLIENT_SECRET",
                 "refresh_token": "env://CRM_REFRESH_TOKEN_SEED",
                 "scopes": ["api.read"] } } }"#,
    )
    .unwrap();
    let out = dir.join("dist");
    let (ok, report) = compile_deployment(&cli, dir, &out);
    assert!(ok, "{report}");
    let mut crm = read_json(&out.join("connections/crm.json"));
    assert_eq!(
        crm["secret_locators"],
        serde_json::json!({
            "auth.client_id": "env://CRM_CLIENT_ID",
            "auth.client_secret": "env://CRM_CLIENT_SECRET",
            "auth.refresh_token": "env://CRM_REFRESH_TOKEN_SEED",
        })
    );
    assert_eq!(
        crm["public_config"]["auth"]["token_url"],
        "https://idp.example.com/oauth2/token"
    );

    let app = common::test_app().await;
    let obj = crm.as_object_mut().unwrap();
    obj.remove("driver");
    obj.remove("connection_id");
    obj.insert("revision".into(), serde_json::json!(1));
    let resp = app
        .oneshot(common::json_request(
            "POST",
            "/api/v2/admin/connections/crm/revisions",
            Some(crm),
        ))
        .await
        .unwrap();
    let status = resp.status();
    let body = common::body_json(resp).await;
    assert_eq!(status, 201, "{body}");
}

/// What compile writes is what a server stores: every definition uploads to
/// the digest its file is named by, every connection creates as written,
/// and a manifest over them, revisions assigned, plans.
#[tokio::test]
async fn a_compiled_deployment_uploads_to_its_own_digests_and_plans() {
    use tower::ServiceExt as _;
    let Some(cli) = common::orion_cli_bin() else {
        return;
    };
    let scratch = deployment_set("compile-deployment-upload");
    let out = scratch.path().join("dist");
    let (ok, report) = compile_deployment(&cli, scratch.path(), &out);
    assert!(ok, "{report}");

    let app = common::test_app().await;
    let send = |method: &'static str, uri: String, body: serde_json::Value| {
        let app = app.clone();
        async move {
            let resp = app
                .oneshot(common::json_request(method, &uri, Some(body)))
                .await
                .unwrap();
            let status = resp.status();
            (status, common::body_json(resp).await)
        }
    };

    let deployment = read_json(&out.join("deployment.json"));
    let mut members = Vec::new();
    for member in deployment["members"].as_array().unwrap() {
        let id = member["id"].as_str().unwrap();
        let requires: Vec<serde_json::Value> = member["requires"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| serde_json::json!({"kind": r["kind"], "id": r["id"], "revision": 1}))
            .collect();
        let digest = if member["kind"] == "connection" {
            let mut connection = read_json(&out.join(format!("connections/{id}.json")));
            let obj = connection.as_object_mut().unwrap();
            obj.remove("driver");
            obj.remove("connection_id");
            obj.insert("revision".into(), serde_json::json!(1));
            let (status, body) = send(
                "POST",
                format!("/api/v2/admin/connections/{id}/revisions"),
                connection,
            )
            .await;
            assert_eq!(status, 201, "{id}: {body}");
            body["connection"]["configuration_digest"]
                .as_str()
                .unwrap()
                .to_string()
        } else {
            let named = member["definition"].as_str().unwrap();
            let file = out.join(format!(
                "definitions/{}.json",
                named.trim_start_matches("sha256:")
            ));
            let (status, body) =
                send("POST", "/api/v2/admin/definitions".into(), read_json(&file)).await;
            assert_eq!(status, 201, "{id}: {body}");
            assert_eq!(
                body["digest"], named,
                "{id}: the server's digest is the file's"
            );
            named.to_string()
        };
        members.push(serde_json::json!({
            "entity": {"kind": member["kind"], "id": id, "revision": 1},
            "definition_digest": digest,
            "requires": requires,
        }));
    }
    let (status, plan) = send(
        "POST",
        "/api/v2/admin/deployment-plans".into(),
        serde_json::json!({"base_revision": 0, "manifest": {
            "schema_version": 2, "providers": [], "members": members}}),
    )
    .await;
    assert_eq!(status, 200, "{plan}");
    assert_eq!(plan["added_entities"].as_array().map(Vec::len), Some(4));
}
