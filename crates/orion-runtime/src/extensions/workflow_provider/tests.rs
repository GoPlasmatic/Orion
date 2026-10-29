#![expect(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use dataflow_rs::program::IssueCode;
use serde_json::{Value, json};

use super::*;
use crate::extensions::Digest;

fn provider() -> ProviderRef {
    ProviderRef {
        publisher: "acme".to_string(),
        provider_id: "acme.crm".into(),
        version: "1.0.0".to_string(),
        digest: Digest::parse(&format!("sha256:{}", "ab".repeat(32))).unwrap(),
    }
}

fn manifest(entries: Value, workflows: Value) -> Value {
    json!({
        "implementation": {"kind": "workflow", "entries": entries, "workflows": workflows},
        "dependencies": [
            {"kind": "provider", "alias": "cache", "id": "org.orion.memory-cache",
             "requires": ">=2", "operations": ["cache.get"]}
        ],
    })
}

fn files(docs: &[(&str, Value)]) -> BTreeMap<String, Vec<u8>> {
    docs.iter()
        .map(|(path, doc)| (path.to_string(), serde_json::to_vec(doc).unwrap()))
        .collect()
}

fn paths(problems: &[PackageProblem]) -> Vec<&str> {
    problems.iter().map(|p| p.path.as_str()).collect()
}

/// The allowlist, task by task: a host function that reaches the
/// deployment by name is refused naming the task, a secret node or
/// reference anywhere is refused, and `connector_call` is held to the
/// declared dependencies.
#[test]
fn a_package_workflow_reaches_only_the_hermetic_set() {
    let doc = json!({"id": "entry", "name": "entry", "tasks": [
        {"id": "ok-map", "name": "m", "function": {"name": "map", "input": {"mappings": [
            {"path": "data.x", "logic": 1}]}}},
        {"id": "ok-hash", "name": "h", "function": {"name": "crypto", "input": {
            "op": "hash", "algorithm": "sha256", "data": "x", "output": "data.h"}}},
        {"id": "egress", "name": "e", "function": {"name": "http_call", "input": {
            "connector": "orders-api", "method": "GET"}}},
        {"id": "peek", "name": "p", "function": {"name": "channel_call", "input": {
            "channel": "admin"}}},
        {"id": "keyed", "name": "k", "function": {"name": "crypto", "input": {
            "op": "hmac", "algorithm": "sha256", "data": "x",
            "key": {"secret": "partner"}, "output": "data.mac"}}},
        {"id": "env", "name": "v", "function": {"name": "map", "input": {"mappings": [
            {"path": "data.k", "logic": "env://AWS_SECRET_ACCESS_KEY"}]}}},
        {"id": "dep-ok", "name": "d", "function": {"name": "connector_call", "input": {
            "connection": "cache", "operation": "cache.get", "input": {"key": "k"}}}},
        {"id": "dep-op", "name": "d", "function": {"name": "connector_call", "input": {
            "connection": "cache", "operation": "cache.set"}}},
        {"id": "dep-alias", "name": "d", "function": {"name": "connector_call", "input": {
            "connection": "orders-db", "operation": "query", "target_id": "org.orion.sqlite"}}},
    ]});
    let problems = WorkflowPackage::from_files(
        provider(),
        &manifest(
            json!({"item.get": "entry"}),
            json!(["workflows/entry.json"]),
        ),
        &files(&[("workflows/entry.json", doc)]),
    )
    .expect_err("refused");
    let at = paths(&problems);
    for refused in [
        "workflows/entry.json:tasks[2].function.name",
        "workflows/entry.json:tasks[3].function.name",
        "workflows/entry.json:tasks[4].function.input.key",
        "workflows/entry.json:tasks[5].function.input.mappings[0].logic",
        "workflows/entry.json:tasks[7].function.input.operation",
        "workflows/entry.json:tasks[8].function.input.connection",
        "workflows/entry.json:tasks[8].function.input.target_id",
    ] {
        assert!(at.contains(&refused), "{refused} refused: {problems:#?}");
    }
    assert_eq!(at.len(), 7, "nothing else is refused: {problems:#?}");
    assert!(
        problems
            .iter()
            .any(|p| p.message.contains("task 'egress' calls `http_call`")),
        "{problems:#?}"
    );
}

/// One entry workflow per operation: a workflow no operation enters is a
/// helper and refused, as is a manifest declaring helpers.
#[test]
fn a_package_has_no_helpers() {
    let tasks = json!([{"id": "t", "name": "t", "function": {"name": "map", "input": {"mappings": [{"path": "data.x", "logic": 1}]}}}]);
    let entry = json!({"id": "entry", "name": "entry", "tasks": tasks});
    let helper = json!({"id": "helper", "name": "helper", "tasks": tasks});
    let mut m = manifest(
        json!({"item.get": "entry"}),
        json!(["workflows/entry.json", "workflows/helper.json"]),
    );
    m["implementation"]["helpers"] = json!(["helper"]);
    let problems = WorkflowPackage::from_files(
        provider(),
        &m,
        &files(&[
            ("workflows/entry.json", entry.clone()),
            ("workflows/helper.json", helper),
        ]),
    )
    .expect_err("refused");
    assert_eq!(
        paths(&problems),
        vec![
            "connector.json:implementation.helpers",
            "workflows/helper.json"
        ],
        "{problems:#?}"
    );

    let ok = WorkflowPackage::from_files(
        provider(),
        &manifest(
            json!({"item.get": "entry"}),
            json!(["workflows/entry.json"]),
        ),
        &files(&[("workflows/entry.json", entry)]),
    )
    .expect("one entry per operation is accepted");
    assert_eq!(ok.entries["item.get"].document["channel"], "item.get");
}

/// Defence in depth: even a document that skipped the package check
/// cannot call a deployment-reaching function or read a secret, because
/// the package's registry holds neither.
#[test]
fn the_package_registry_cannot_reach_the_deployment_or_its_secrets() {
    let host = WorkflowHost {
        providers: crate::extensions::CompositionBuilder::new(
            crate::extensions::EditionTier::Community,
        )
        .build_in_memory()
        .expect("composes")
        .providers()
        .clone(),
        jwks: Arc::new(crate::jwt::jwks::JwksCache::new(
            reqwest::Client::new(),
            false,
        )),
        ops_budget: 0,
    };
    let registry = package_registry(&host, &BTreeMap::new(), &crate::plugin::PluginSet::empty())
        .expect("the registry builds");
    let compiler = registry.compiler(dataflow_rs::program::CompileOptions::default());
    let compile = |task: Value| {
        crate::engine::compile_row(
            &compiler,
            &json!({"id": "w", "name": "w", "channel": "op", "tasks": [task]}),
        )
    };
    let egress = compile(
        json!({"id": "e", "name": "e", "function": {"name": "http_call",
        "input": {"connector": "orders-api", "method": "GET"}}}),
    )
    .err()
    .unwrap_or_default();
    assert!(!egress.is_empty(), "http_call is not registered");
    assert!(
        egress.iter().any(|i| i.code == IssueCode::UnknownFunction),
        "{egress:?}"
    );
    let secret = compile(
        json!({"id": "k", "name": "k", "function": {"name": "crypto",
        "input": {"op": "hmac", "algorithm": "sha256", "data": "x",
                  "key": {"secret": "partner_hmac"}, "output": "data.mac"}}}),
    );
    assert!(secret.is_err(), "no secret resolves on a package's engine");
    let pure = compile(
        json!({"id": "h", "name": "h", "function": {"name": "crypto",
        "input": {"op": "hash", "algorithm": "sha256", "data": "x", "output": "data.h"}}}),
    );
    assert!(pure.is_ok(), "{:?}", pure.err());
}
