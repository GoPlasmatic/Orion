use crate::common;
use crate::common::backends::Backend;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Elasticsearch integration test for data_query.
//
// Spins up an ephemeral Elasticsearch testcontainer (like data_roundtrip_test),
// so it needs Docker but no manual setup.
// Run with: cargo test --test integration -- --ignored es_test
// ---------------------------------------------------------------------------

/// data_query against an ES connector: the portable filter renders to an ES
/// query DSL body (filter context) and is POSTed to `{url}/{index}/_search`.
/// Seeds a few docs via the ES REST API, then asserts the filter result.
#[tokio::test]
#[ignore]
async fn test_data_query_es_search() {
    let h = common::backends::start(Backend::Es, "dq-es").await;
    let es = &h.connection_string;
    let index = "orion_dq_users";
    let http = reqwest::Client::new();

    // Seed documents into the fresh container (refresh so they're searchable).
    // Dynamic mapping is fine here: `age` maps to long, and the `terms` filter
    // matches the single lowercase token "active".
    for (id, name, age, status) in [
        ("u1", "Alice", 15, "active"),
        ("u2", "Bob", 30, "active"),
        ("u3", "Carol", 40, "inactive"),
    ] {
        http.post(format!("{es}/{index}/_doc/{id}?refresh=wait_for"))
            .json(&json!({ "name": name, "age": age, "status": status }))
            .send()
            .await
            .expect("seed doc");
    }

    let app = common::test_app().await;
    common::create_connector(&app, h.connector_json()).await;

    common::create_and_activate_channel(
        &app,
        "ch-dq-es",
        common::workflow_with_tasks(
            "DataQueryEs",
            json!([
                {
                    "id": "t_q", "name": "query",
                    "function": { "name": "data_query", "input": {
                        "connector": h.connector_name,
                        // F24: the dialect rejects undeclared names, so this
                        // pre-schema test asks for pass-through explicitly —
                        // the one line a 0.x task adds.
                        "schema": { "unmapped": "identity" },
                        "query": {
                            "source": index,
                            "filter": { "and": [
                                { ">": [{ "field": "age" }, 18] },
                                { "in": [{ "field": "status" }, ["active"]] }
                            ] },
                            "sort": [{ "age": "asc" }]
                        },
                        "output": "data.result"
                    } }
                }
            ]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/ch-dq-es",
            Some(json!({ "data": {} })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok", "body = {body}");

    // age > 18 AND status == active → only Bob(30).
    let rows = body["data"]["result"]
        .as_array()
        .expect("result should be an array");
    assert_eq!(rows.len(), 1, "body = {body}");
    assert_eq!(rows[0]["name"], "Bob");
}

/// The document key survives a read.
///
/// Elasticsearch keeps `_id` outside `_source`, and a search returns only
/// `_source` — so a schema declaring the `id` → `_id` rename (the spelling the
/// write path requires, and the one the parity table documents) used to project
/// `"_source": ["_id"]` and hand back `{}` for every hit. Not an error, an empty
/// object. That made insert-then-update-by-id the one pattern ES could not
/// express, because the id could be written but never read.
#[tokio::test]
#[ignore]
async fn test_data_query_es_returns_the_document_id() {
    let h = common::backends::start(Backend::Es, "dq-es-id").await;
    let es = &h.connection_string;
    let index = "orion_dq_id_users";
    let http = reqwest::Client::new();

    for (id, name, age) in [("u1", "Alice", 30), ("u2", "Bob", 40)] {
        http.post(format!("{es}/{index}/_doc/{id}?refresh=wait_for"))
            .json(&json!({ "name": name, "age": age }))
            .send()
            .await
            .expect("seed doc");
    }

    let app = common::test_app().await;
    common::create_connector(&app, h.connector_json()).await;
    common::create_and_activate_channel(
        &app,
        "ch-dq-es-id",
        common::workflow_with_tasks(
            "DataQueryEsId",
            json!([
                {
                    "id": "t_q", "name": "query",
                    "function": { "name": "data_query", "input": {
                        "connector": h.connector_name,
                        // The declared rename is what makes `_id` reachable at
                        // all — there is no implicit `id` → `_id` mapping.
                        "schema": { "entities": { index: { "columns": {
                            "id": { "name": "_id" },
                            "name": {},
                            "age": {}
                        } } } },
                        "query": {
                            "source": index,
                            "fields": ["id", "name"],
                            // Ordered on a numeric field: sorting an analysed
                            // `text` field needs fielddata and is an ES error.
                            "sort": [{ "age": "asc" }]
                        },
                        "output": "data.result"
                    } }
                }
            ]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/ch-dq-es-id",
            Some(json!({ "data": {} })),
        ))
        .await
        .unwrap();
    let status = resp.status();
    let body = common::body_json(resp).await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    let rows = body["data"]["result"]
        .as_array()
        .expect("result should be an array");
    assert_eq!(rows.len(), 2, "body = {body}");
    // Physical names, as every other backend's rows carry today.
    assert_eq!(rows[0]["_id"], "u1", "body = {body}");
    assert_eq!(rows[0]["name"], "Alice", "body = {body}");
    assert_eq!(rows[1]["_id"], "u2", "body = {body}");
}

/// A query that names no `_id` is unchanged: whole `_source` documents, with no
/// key added. The guard matters — adding `_id` unconditionally would change the
/// shape of every ES result that works today.
#[tokio::test]
#[ignore]
async fn test_data_query_es_without_the_id_projection_is_unchanged() {
    let h = common::backends::start(Backend::Es, "dq-es-noid").await;
    let es = &h.connection_string;
    let index = "orion_dq_noid_users";
    let http = reqwest::Client::new();

    http.post(format!("{es}/{index}/_doc/u1?refresh=wait_for"))
        .json(&json!({ "name": "Alice" }))
        .send()
        .await
        .expect("seed doc");

    let app = common::test_app().await;
    common::create_connector(&app, h.connector_json()).await;
    common::create_and_activate_channel(
        &app,
        "ch-dq-es-noid",
        common::workflow_with_tasks(
            "DataQueryEsNoId",
            json!([
                {
                    "id": "t_q", "name": "query",
                    "function": { "name": "data_query", "input": {
                        "connector": h.connector_name,
                        "schema": { "unmapped": "identity" },
                        "query": { "source": index },
                        "output": "data.result"
                    } }
                }
            ]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/ch-dq-es-noid",
            Some(json!({ "data": {} })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    let rows = body["data"]["result"]
        .as_array()
        .expect("result should be an array");
    assert_eq!(rows.len(), 1, "body = {body}");
    assert_eq!(rows[0]["name"], "Alice", "body = {body}");
    assert!(
        rows[0].get("_id").is_none(),
        "an unprojected `_id` must not appear: body = {body}"
    );
}
