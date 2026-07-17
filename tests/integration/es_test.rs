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
