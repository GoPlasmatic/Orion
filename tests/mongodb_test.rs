mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// MongoDB integration tests
//
// These tests require a running MongoDB instance on localhost:27017.
// Start it with: docker compose -f docker-compose.test.yml up -d mongo
// Run with:      cargo test --test mongodb_test -- --ignored
// ---------------------------------------------------------------------------

/// Helper: create a MongoDB connector via the admin API.
/// MongoDB uses the "db" connector type; the mongo_read function extracts
/// the DbConnectorConfig to get the connection string.
fn mongo_connector(name: &str) -> serde_json::Value {
    json!({
        "id": name,
        "name": name,
        "connector_type": "db",
        "config": {
            "type": "db",
            "connection_string": "mongodb://localhost:27017",
            "driver": "mongodb",
            "max_connections": 2,
            "query_timeout_ms": 5000
        }
    })
}

/// Read from an empty (or nonexistent) collection with an empty filter.
/// The mongo_read function should return an empty array rather than an error.
#[tokio::test]
#[ignore]
async fn test_mongo_read_returns_documents() {
    let app = common::test_app().await;

    common::create_connector(&app, mongo_connector("mongo-read")).await;

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

    common::create_connector(&app, mongo_connector("mongo-filter")).await;

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
