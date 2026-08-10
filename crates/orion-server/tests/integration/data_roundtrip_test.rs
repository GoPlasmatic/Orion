//! Cross-backend round-trip tests for the portable `data_query` / `data_write`
//! dialects.
//!
//! One CRUD scenario (insert → filtered read → update → delete → upsert →
//! auto-increment insert) is written once in [`assert_crud_roundtrip`] and run
//! against every backend `data_write` supports. Each step writes to a distinct
//! output path so the whole round-trip runs in a single workflow / request, and
//! the final response is asserted step by step.
//!
//! SQLite runs in-memory and is *not* `#[ignore]` (it needs no Docker), so it
//! executes in the normal CI coverage job. Postgres / MySQL / MongoDB /
//! Elasticsearch use testcontainers and are `#[ignore]` so `cargo test` stays
//! Docker-free by default; the dedicated CI job runs them with `-- --ignored`.

use crate::common;
use crate::common::backends::{Backend, BackendHarness};
use crate::common::dsl;

use axum::http::StatusCode;
use serde_json::{Value, json};
use std::collections::HashMap;
use tower::ServiceExt;

/// A `data_write` task carrying the given envelope; the Mongo `database`
/// field and the backend's inline schema are layered onto the shared DSL
/// task shape.
fn dw(h: &BackendHarness, id: &str, mut envelope: Value, out: &str) -> Value {
    envelope["output"] = json!(out);
    if let Some(schema) = h.schema_json() {
        envelope["schema"] = schema;
    }
    dsl::dw(&h.connector_name, id, h.with_db(envelope))
}

/// A `data_query` read-back task. Unlike [`dsl::dq`] each read targets its own
/// output path, and the backend schema / Mongo `database` field are added.
fn dq(h: &BackendHarness, id: &str, query: Value, out: &str) -> Value {
    let mut input = h.with_db(json!({
        "connector": h.connector_name, "query": query, "output": out
    }));
    // F24: the dialect rejects undeclared names, so a backend with no schema of
    // its own still has to say it wants pass-through — the same one line a 0.x
    // workflow adds on upgrade. This harness is about cross-backend parity, not
    // the allowlist.
    input["schema"] = h
        .schema_json()
        .unwrap_or_else(|| json!({ "unmapped": "identity" }));
    json!({ "id": id, "name": id, "function": { "name": "data_query", "input": input } })
}

/// Collect a `name -> status` map from a read result array.
fn name_status(v: &Value) -> HashMap<String, String> {
    v.as_array()
        .expect("read result should be an array")
        .iter()
        .filter_map(|r| {
            Some((
                r["name"].as_str()?.to_string(),
                r["status"].as_str()?.to_string(),
            ))
        })
        .collect()
}

/// Collect the `name` column from a read result array.
fn names(v: &Value) -> Vec<String> {
    v.as_array()
        .expect("read result should be an array")
        .iter()
        .filter_map(|r| r["name"].as_str().map(str::to_string))
        .collect()
}

/// Drive the full CRUD round-trip against one backend and assert every step.
async fn assert_crud_roundtrip(backend: Backend) {
    let app = common::test_app().await;
    let name = format!("rt_{}", backend.label());
    let h = common::backends::start(backend, &name).await;
    h.prepare().await;
    common::create_connector(&app, h.connector_json()).await;

    // Build the round-trip as one ordered task pipeline. Each read goes to its
    // own output path so we can assert intermediate state from one response.
    let mut tasks: Vec<Value> = Vec::new();
    if let Some(sql) = h.ddl_users() {
        tasks.push(dsl::ddl(&h.connector_name, "ddl_users", &sql));
    }
    if let Some(sql) = h.ddl_items() {
        tasks.push(dsl::ddl(&h.connector_name, "ddl_items", &sql));
    }

    // 1. Insert three users.
    tasks.push(dw(
        &h,
        "ins",
        json!({
            "op": "insert", "target": "users",
            "values": [
                { "id": "u1", "name": "Alice", "age": 30, "status": "active" },
                { "id": "u2", "name": "Bob",   "age": 20, "status": "active" },
                { "id": "u3", "name": "Carol", "age": 40, "status": "active" }
            ]
        }),
        "data.w_insert",
    ));

    // 2. Filtered + sorted + projected read (age > 25, by age asc): Alice, Carol.
    tasks.push(dq(
        &h,
        "r_filter",
        json!({
            "source": "users",
            "filter": { ">": [{ "field": "age" }, 25] },
            "fields": ["name", "age"],
            "sort": [{ "age": "asc" }]
        }),
        "data.r_filter",
    ));

    // 3. Update a single row by id, then read all.
    tasks.push(dw(
        &h,
        "upd",
        json!({
            "op": "update", "target": "users",
            "set": { "status": "inactive" },
            "filter": { "==": [{ "field": "id" }, "u1"] }
        }),
        "data.w_update",
    ));
    tasks.push(dq(
        &h,
        "r_upd",
        json!({ "source": "users", "sort": [{ "name": "asc" }] }),
        "data.r_after_update",
    ));

    // 4. Delete a single row by id, then read all.
    tasks.push(dw(
        &h,
        "del",
        json!({
            "op": "delete", "target": "users",
            "filter": { "==": [{ "field": "id" }, "u2"] }
        }),
        "data.w_delete",
    ));
    tasks.push(dq(
        &h,
        "r_del",
        json!({ "source": "users", "sort": [{ "name": "asc" }] }),
        "data.r_after_delete",
    ));

    // 5. Upsert the existing u1 (conflict → update), then read all.
    tasks.push(dw(
        &h,
        "ups",
        json!({
            "op": "upsert", "target": "users",
            "values": { "id": "u1", "name": "Alice2", "age": 31, "status": "active" },
            "on_conflict": { "target": ["id"], "action": "update" }
        }),
        "data.w_upsert",
    ));
    tasks.push(dq(
        &h,
        "r_ups",
        json!({ "source": "users", "sort": [{ "name": "asc" }] }),
        "data.r_after_upsert",
    ));

    // 6. Auto-increment insert: RETURNING on SQLite/Postgres, last_insert_id on
    //    MySQL, generated ids on Mongo/ES.
    let item_env = if h.backend.supports_returning() {
        json!({ "op": "insert", "target": "items", "values": { "name": "Widget" }, "returning": ["id", "name"] })
    } else {
        json!({ "op": "insert", "target": "items", "values": { "name": "Widget" } })
    };
    tasks.push(dw(&h, "item", item_env, "data.w_item"));

    // Run the pipeline in a single sync request.
    let channel = format!("ch-rt-{}", backend.label());
    common::create_and_activate_channel(
        &app,
        &channel,
        common::workflow_with_tasks("roundtrip", json!(tasks)),
    )
    .await;
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            &format!("/api/v1/data/{channel}"),
            Some(json!({ "data": {} })),
        ))
        .await
        .unwrap();
    let status = resp.status();
    let body = common::body_json(resp).await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(body["status"], "ok", "body = {body}");

    // 1. Insert count.
    h.assert_insert_count(&body["data"]["w_insert"], 3);

    // 2. Filter/sort/projection: only Alice (30) and Carol (40), age-ascending.
    assert_eq!(
        names(&body["data"]["r_filter"]),
        vec!["Alice", "Carol"],
        "filter/sort: {body}"
    );

    // 3. Update: only Alice flipped to inactive.
    h.assert_update_count(&body["data"]["w_update"], 1);
    let after_update = name_status(&body["data"]["r_after_update"]);
    assert_eq!(
        after_update.get("Alice").map(String::as_str),
        Some("inactive"),
        "{body}"
    );
    assert_eq!(
        after_update.get("Bob").map(String::as_str),
        Some("active"),
        "{body}"
    );
    assert_eq!(
        after_update.get("Carol").map(String::as_str),
        Some("active"),
        "{body}"
    );

    // 4. Delete: Bob is gone, two rows remain.
    h.assert_delete_count(&body["data"]["w_delete"], 1);
    let after_delete = names(&body["data"]["r_after_delete"]);
    assert_eq!(after_delete.len(), 2, "after delete: {body}");
    assert!(
        !after_delete.contains(&"Bob".to_string()),
        "Bob should be deleted: {body}"
    );

    // 5. Upsert on the existing key updates in place (no new row).
    let after_upsert = names(&body["data"]["r_after_upsert"]);
    assert_eq!(after_upsert.len(), 2, "upsert must not add a row: {body}");
    assert!(
        after_upsert.contains(&"Alice2".to_string()),
        "upsert should rename Alice: {body}"
    );
    assert!(after_upsert.contains(&"Carol".to_string()), "{body}");

    // 6. Auto-increment insert result shape per backend.
    let w_item = &body["data"]["w_item"];
    if h.backend.supports_returning() {
        let ret = w_item["returning"].as_array().expect("returning array");
        assert_eq!(ret.len(), 1, "{w_item}");
        assert_eq!(ret[0]["name"], "Widget", "{w_item}");
        assert!(
            ret[0]["id"].as_i64().unwrap_or(0) >= 1,
            "generated id: {w_item}"
        );
    } else if h.backend.is_doc_store() {
        assert_eq!(w_item["inserted"].as_u64(), Some(1), "{w_item}");
        assert!(
            w_item["ids"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false),
            "doc-store generated id: {w_item}"
        );
    } else {
        // MySQL: no RETURNING; the auto-increment id is reported instead.
        assert!(
            w_item["last_insert_id"].as_i64().unwrap_or(0) >= 1,
            "mysql last_insert_id: {w_item}"
        );
    }
}

#[tokio::test]
async fn roundtrip_sqlite() {
    assert_crud_roundtrip(Backend::Sqlite).await;
}

#[tokio::test]
#[ignore]
async fn roundtrip_postgres() {
    assert_crud_roundtrip(Backend::Postgres).await;
}

#[tokio::test]
#[ignore]
async fn roundtrip_mysql() {
    assert_crud_roundtrip(Backend::Mysql).await;
}

#[tokio::test]
#[ignore]
async fn roundtrip_mongo() {
    assert_crud_roundtrip(Backend::Mongo).await;
}

#[tokio::test]
#[ignore]
async fn roundtrip_es() {
    assert_crud_roundtrip(Backend::Es).await;
}
