use crate::common;
use crate::common::backends::Backend;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// MongoDB integration tests
//
// These tests spin up an ephemeral MongoDB container via testcontainers, so
// they require Docker to be available. They are #[ignore] by default; run with:
//   cargo test --test integration -- --ignored mongodb_test
//
// The connector uses the "db" connector type; the mongo_read / data_query
// handlers extract the DbConnectorConfig to get the connection string, and the
// `database` field in each task input selects the Mongo database.
// ---------------------------------------------------------------------------

/// Read from an empty (or nonexistent) collection with an empty filter.
/// The mongo_read function should return an empty array rather than an error.
#[tokio::test]
#[ignore]
async fn test_mongo_read_returns_documents() {
    let app = common::test_app().await;

    let h = common::backends::start(Backend::Mongo, "mongo-read").await;
    common::create_connector(&app, h.connector_json()).await;

    common::create_and_activate_channel(
        &app,
        "mongo-read-ch",
        common::workflow_with_tasks(
            "MongoReadEmpty",
            json!([
                {
                    "id": "t1",
                    "name": "Read from empty collection",
                    "function": {
                        "name": "mongo_read",
                        "input": {
                            "connector": "mongo-read",
                            "database": "orion_test",
                            "collection": "empty_items",
                            "filter": {},
                            "output": "data.items"
                        }
                    }
                }
            ]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/mongo-read-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");

    let items = body["data"]["items"]
        .as_array()
        .expect("items should be an array");
    assert!(
        items.is_empty(),
        "expected empty array from nonexistent collection, got {:?}",
        items
    );
}

/// Read with a filter document on an empty collection. Verifies that the
/// mongo_read function executes without error even when no documents match.
#[tokio::test]
#[ignore]
async fn test_mongo_read_with_filter() {
    let app = common::test_app().await;

    let h = common::backends::start(Backend::Mongo, "mongo-filter").await;
    common::create_connector(&app, h.connector_json()).await;

    common::create_and_activate_channel(
        &app,
        "mongo-filter-ch",
        common::workflow_with_tasks(
            "MongoReadFilter",
            json!([
                {
                    "id": "t1",
                    "name": "Read with filter",
                    "function": {
                        "name": "mongo_read",
                        "input": {
                            "connector": "mongo-filter",
                            "database": "orion_test",
                            "collection": "filtered_items",
                            "filter": { "status": "active", "priority": { "$gte": 5 } },
                            "output": "data.results"
                        }
                    }
                }
            ]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/mongo-filter-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");

    let results = body["data"]["results"]
        .as_array()
        .expect("results should be an array");
    assert!(
        results.is_empty(),
        "expected empty array for filter on empty collection, got {:?}",
        results
    );
}

/// data_query against a Mongo connector: the portable filter renders to a
/// `$match` find. On an empty collection it returns an empty array, proving the
/// data_query → Mongo execution path end-to-end.
#[tokio::test]
#[ignore]
async fn test_data_query_mongo_find() {
    let app = common::test_app().await;

    let h = common::backends::start(Backend::Mongo, "dq-mongo").await;
    common::create_connector(&app, h.connector_json()).await;

    common::create_and_activate_channel(
        &app,
        "dq-mongo-ch",
        common::workflow_with_tasks(
            "DataQueryMongo",
            json!([
                {
                    "id": "t1",
                    "name": "portable query",
                    "function": {
                        "name": "data_query",
                        "input": {
                            "connector": "dq-mongo",
                            "database": "orion_test",
                            // F24: the dialect rejects undeclared names, so
                            // this pre-schema test asks for pass-through
                            // explicitly — the one line a 0.x task adds.
                            "schema": { "unmapped": "identity" },
                            "query": {
                                "source": "dq_empty_items",
                                "filter": { "and": [
                                    { ">": [{ "field": "age" }, 18] },
                                    { "in": [{ "field": "status" }, ["active"]] }
                                ] },
                                "sort": [{ "age": "asc" }],
                                "limit": 10
                            },
                            "output": "data.result"
                        }
                    }
                }
            ]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/dq-mongo-ch",
            Some(json!({ "data": {} })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["status"], "ok");
    let rows = body["data"]["result"]
        .as_array()
        .expect("result should be an array");
    assert!(rows.is_empty(), "expected empty array, got {rows:?}");
}
