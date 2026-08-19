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

/// T36: the tests above prove absence works; this proves presence does —
/// BSON→JSON decoding of real documents (ObjectId `_id`, strings, ints,
/// nested documents) and filter selectivity, which is what `mongo_read`
/// exists to do and what nothing asserted before (the suite only ever read
/// empty collections).
#[tokio::test]
#[ignore]
async fn test_mongo_read_decodes_seeded_documents() {
    let app = common::test_app().await;

    let h = common::backends::start(Backend::Mongo, "mongo-seeded").await;
    common::create_connector(&app, h.connector_json()).await;

    // Seed through the same driver orion links.
    let client = mongodb::Client::with_uri_str(&h.connection_string)
        .await
        .expect("mongo client");
    let coll = client
        .database("orion_test")
        .collection::<mongodb::bson::Document>("seeded_items");
    coll.insert_many([
        mongodb::bson::doc! {"sku": "A-1", "qty": 3_i32, "meta": {"tier": "gold"}},
        mongodb::bson::doc! {"sku": "B-2", "qty": 7_i32, "meta": {"tier": "silver"}},
    ])
    .await
    .expect("seed documents");

    common::create_and_activate_channel(
        &app,
        "mongo-seeded-ch",
        common::workflow_with_tasks(
            "MongoReadSeeded",
            json!([
                {
                    "id": "t1",
                    "name": "Read seeded documents",
                    "function": {
                        "name": "mongo_read",
                        "input": {
                            "connector": "mongo-seeded",
                            "database": "orion_test",
                            "collection": "seeded_items",
                            "filter": {"sku": "A-1"},
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
            "/api/v1/data/mongo-seeded-ch",
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
    assert_eq!(
        items.len(),
        1,
        "the filter must select exactly the matching document: {items:?}"
    );
    let item = &items[0];
    assert_eq!(item["sku"], "A-1");
    assert_eq!(item["qty"], 3);
    assert_eq!(item["meta"]["tier"], "gold");
    assert!(
        !item["_id"].is_null(),
        "_id must survive BSON→JSON decoding: {item:?}"
    );
}

// ---------------------------------------------------------------------------
// #263: mongo_write / mongo_aggregate / extended-JSON values
// ---------------------------------------------------------------------------

/// The write twin, end to end: insert a *nested* document (the exact shape
/// v1.0.x could not write at all), read it back, target it by the `_id` the
/// read returned (canonical `{"$oid": …}` extended JSON, folded through a
/// `{"var": ..}` node), and delete it — with a delete-gated connector proving
/// the per-op gates hold on the way.
#[tokio::test]
#[ignore]
async fn test_mongo_write_crud_round_trip_with_nested_documents() {
    let app = common::test_app().await;

    let h = common::backends::start(Backend::Mongo, "mw-crud").await;
    common::create_connector(&app, h.connector_json()).await;

    // A second connector at the same server with `delete` disabled.
    let mut gated = h.connector_json();
    gated["id"] = json!("mw-gated");
    gated["name"] = json!("mw-gated");
    gated["config"]["operations"] = json!({ "delete": false });
    common::create_connector(&app, gated).await;

    let task = |id: &str, name: &str, input: serde_json::Value| {
        json!({ "id": id, "name": name, "function": { "name": "mongo_write", "input": input } })
    };

    // Insert: nested arrays and objects pass through, and `$date` becomes a
    // typed BSON date.
    common::create_and_activate_channel(
        &app,
        "mw-insert",
        common::workflow_with_tasks(
            "MwInsert",
            json!([
                { "id": "p", "name": "Parse", "function": { "name": "parse_json",
                    "input": {"source": "payload", "target": "req"} } },
                task("t1", "Insert nested", json!({
                    "connector": "mw-crud",
                    "database": "orion_test",
                    "collection": "meetings",
                    "op": "insert_one",
                    "document": {
                        "topic": { "var": "data.req.topic" },
                        "payload": { "var": "data.req.payload" },
                        "starts_at": { "$date": "2026-01-15T09:00:00Z" },
                        "deleted": false
                    },
                    "output": "data.res"
                })),
            ]),
        ),
    )
    .await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/mw-insert",
            Some(json!({"data": {
                "topic": "quarterly",
                "payload": { "object": { "participants": [ {"name": "Ada"}, {"name": "Bob"} ] } }
            }})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["res"]["status"], "ok", "{body}");
    assert_eq!(body["data"]["res"]["inserted"], 1, "{body}");
    let id = body["data"]["res"]["ids"][0].clone();
    assert!(
        id.get("$oid").is_some(),
        "a generated ObjectId serializes as canonical extended JSON: {id}"
    );

    // Read it back: the nested shape survived, and the typed date comes back
    // in a `$date` spelling (typed in the store, not a string).
    common::create_and_activate_channel(
        &app,
        "mw-read",
        common::workflow_with_tasks(
            "MwRead",
            json!([{
                "id": "t1", "name": "Read", "function": { "name": "mongo_read", "input": {
                    "connector": "mw-crud",
                    "database": "orion_test",
                    "collection": "meetings",
                    "filter": { "topic": "quarterly" },
                    "output": "data.items"
                } }
            }]),
        ),
    )
    .await;
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/mw-read",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    let items = body["data"]["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["payload"]["object"]["participants"][1]["name"], "Bob");
    assert!(
        items[0]["starts_at"].get("$date").is_some(),
        "a $date input must be stored typed, not as a string: {:?}",
        items[0]["starts_at"]
    );

    // Update, targeting the document by the `_id` the read returned — the
    // canonical wrapper object flows through a var node into the filter.
    common::create_and_activate_channel(
        &app,
        "mw-update",
        common::workflow_with_tasks(
            "MwUpdate",
            json!([
                { "id": "p", "name": "Parse", "function": { "name": "parse_json",
                    "input": {"source": "payload", "target": "req"} } },
                task("t1", "Update by id", json!({
                    "connector": "mw-crud",
                    "database": "orion_test",
                    "collection": "meetings",
                    "op": "update_one",
                    "filter": { "_id": { "var": "data.req.id" } },
                    "update": { "$set": { "deleted": true },
                                "$push": { "audit": { "at": { "$date": "2026-01-16T00:00:00Z" }, "who": "orion" } } },
                    "output": "data.res"
                })),
            ]),
        ),
    )
    .await;
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/mw-update",
            Some(json!({"data": { "id": id }})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["res"]["matched"], 1, "the $oid round-trip must target the document: {body}");
    assert_eq!(body["data"]["res"]["modified"], 1, "{body}");

    // The delete gate holds: the gated connector refuses, the open one deletes.
    for (connector, channel, expect_ok) in
        [("mw-gated", "mw-del-gated", false), ("mw-crud", "mw-del", true)]
    {
        common::create_and_activate_channel(
            &app,
            channel,
            common::workflow_with_tasks(
                &format!("MwDelete-{connector}"),
                json!([task("t1", "Delete", json!({
                    "connector": connector,
                    "database": "orion_test",
                    "collection": "meetings",
                    "op": "delete_many",
                    "filter": { "topic": "quarterly" },
                    "output": "data.res"
                }))]),
            ),
        )
        .await;
        let resp = app
            .clone()
            .oneshot(common::json_request(
                "POST",
                &format!("/api/v1/data/{channel}"),
                Some(json!({"data": {}})),
            ))
            .await
            .unwrap();
        if expect_ok {
            assert_eq!(resp.status(), StatusCode::OK);
            let body = common::body_json(resp).await;
            assert_eq!(body["data"]["res"]["deleted"], 1, "{body}");
        } else {
            assert!(
                resp.status().is_client_error(),
                "a delete-gated connector must refuse, got {}",
                resp.status()
            );
        }
    }
}

/// F28 semantics on the raw surface: an ordered `insert_many` that fails
/// mid-batch names applied / failed / never-attempted and does not abort the
/// workflow; upsert inserts on miss and reports the id.
#[tokio::test]
#[ignore]
async fn test_mongo_write_partial_batch_and_upsert() {
    let app = common::test_app().await;

    let h = common::backends::start(Backend::Mongo, "mw-bulk").await;
    common::create_connector(&app, h.connector_json()).await;

    // Duplicate explicit _id mid-batch: 0 lands, 1 fails, 2 never attempted.
    common::create_and_activate_channel(
        &app,
        "mw-bulk-ch",
        common::workflow_with_tasks(
            "MwBulk",
            json!([{
                "id": "t1", "name": "Bulk", "function": { "name": "mongo_write", "input": {
                    "connector": "mw-bulk",
                    "database": "orion_test",
                    "collection": "bulk_items",
                    "op": "insert_many",
                    "documents": [
                        { "_id": "dup", "n": 0 },
                        { "_id": "dup", "n": 1 },
                        { "_id": "other", "n": 2 }
                    ],
                    "output": "data.res"
                } }
            }]),
        ),
    )
    .await;
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/mw-bulk-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "a partial write continues the workflow, got {}",
        resp.status()
    );
    let body = common::body_json(resp).await;
    let res = &body["data"]["res"];
    assert_eq!(res["status"], "partial", "{res}");
    assert_eq!(res["inserted"], 1, "{res}");
    assert_eq!(res["failed"], 1, "{res}");
    assert_eq!(res["skipped"], 1, "index 2 was never attempted: {res}");
    assert_eq!(res["items"][1]["error"]["code"], 11000, "{res}");

    // Upsert on a miss inserts and names the new id.
    common::create_and_activate_channel(
        &app,
        "mw-upsert-ch",
        common::workflow_with_tasks(
            "MwUpsert",
            json!([{
                "id": "t1", "name": "Upsert", "function": { "name": "mongo_write", "input": {
                    "connector": "mw-bulk",
                    "database": "orion_test",
                    "collection": "bulk_items",
                    "op": "update_one",
                    "filter": { "slug": "missing" },
                    "update": { "$set": { "seen": 1 } },
                    "upsert": true,
                    "output": "data.res"
                } }
            }]),
        ),
    )
    .await;
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/mw-upsert-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["res"]["matched"], 0, "{body}");
    assert!(
        !body["data"]["res"]["upserted_id"].is_null(),
        "an upsert on a miss reports the inserted id: {body}"
    );
}

/// The aggregation surface: a group pipeline computes, the stage allowlist
/// refuses an unknown stage by name at request time, a `$merge` pipeline is
/// refused without the connector opt-in — and the dialect's `$date` wrapper
/// matches typed dates through the portable `data_query`.
#[tokio::test]
#[ignore]
async fn test_mongo_aggregate_and_dialect_tagged_values() {
    let app = common::test_app().await;

    let h = common::backends::start(Backend::Mongo, "agg").await;
    common::create_connector(&app, h.connector_json()).await;

    // Seed typed documents through the driver orion links.
    let client = mongodb::Client::with_uri_str(&h.connection_string)
        .await
        .expect("mongo client");
    let coll = client
        .database("orion_test")
        .collection::<mongodb::bson::Document>("recordings");
    coll.insert_many([
        mongodb::bson::doc! {"meeting": "m1", "quality": "hd",
            "at": mongodb::bson::DateTime::from_millis(1_760_000_000_000)},
        mongodb::bson::doc! {"meeting": "m1", "quality": "hd",
            "at": mongodb::bson::DateTime::from_millis(1_770_000_000_000)},
        mongodb::bson::doc! {"meeting": "m1", "quality": "sd",
            "at": mongodb::bson::DateTime::from_millis(1_780_000_000_000)},
    ])
    .await
    .expect("seed");

    // A group pipeline — the surface find() cannot reach.
    common::create_and_activate_channel(
        &app,
        "agg-group",
        common::workflow_with_tasks(
            "AggGroup",
            json!([{
                "id": "t1", "name": "Group", "function": { "name": "mongo_aggregate", "input": {
                    "connector": "agg",
                    "database": "orion_test",
                    "collection": "recordings",
                    "pipeline": [
                        { "$match": { "meeting": "m1" } },
                        { "$group": { "_id": "$quality", "n": { "$sum": 1 } } },
                        { "$sort": { "n": -1 } }
                    ],
                    "output": "data.by_quality"
                } }
            }]),
        ),
    )
    .await;
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/agg-group",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    let rows = body["data"]["by_quality"].as_array().expect("rows");
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0]["_id"], "hd");
    assert_eq!(rows[0]["n"], 2);
    assert_eq!(rows[1]["_id"], "sd");

    // An unknown stage and an un-opted-in $merge both refuse at request time.
    for (channel, stage) in [
        ("agg-unknown", json!({ "$collStats": {} })),
        ("agg-merge", json!({ "$merge": { "into": "summary" } })),
    ] {
        common::create_and_activate_channel(
            &app,
            channel,
            common::workflow_with_tasks(
                &format!("Agg-{channel}"),
                json!([
                    { "id": "p", "name": "Parse", "function": { "name": "parse_json",
                        "input": {"source": "payload", "target": "req"} } },
                    {
                        "id": "t1", "name": "Agg", "function": { "name": "mongo_aggregate", "input": {
                            "connector": "agg",
                            "database": "orion_test",
                            "collection": "recordings",
                            // The stage arrives via the message, proving the
                            // allowlist judges the *resolved* pipeline —
                            // fail-closed against smuggled stages.
                            "pipeline": [ { "$match": {} }, { "var": "data.req.stage" } ],
                            "output": "data.out"
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
                &format!("/api/v1/data/{channel}"),
                Some(json!({"data": { "stage": stage }})),
            ))
            .await
            .unwrap();
        assert!(
            resp.status().is_client_error(),
            "stage {stage} must be refused at request time, got {}",
            resp.status()
        );
    }

    // Portable dialect: a `$date` filter value compares against typed BSON
    // dates instead of silently matching nothing.
    common::create_and_activate_channel(
        &app,
        "agg-dialect",
        common::workflow_with_tasks(
            "AggDialect",
            json!([{
                "id": "t1", "name": "Query", "function": { "name": "data_query", "input": {
                    "connector": "agg",
                    "database": "orion_test",
                    "schema": { "unmapped": "identity" },
                    "query": {
                        "source": "recordings",
                        "filter": { ">": [ { "field": "at" },
                            { "$date": "2025-10-15T00:00:00Z" } ] }
                    },
                    "output": "data.rows"
                } }
            }]),
        ),
    )
    .await;
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/agg-dialect",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    let rows = body["data"]["rows"].as_array().expect("rows");
    assert_eq!(
        rows.len(),
        2,
        "the typed comparison must select the two later recordings: {rows:?}"
    );

    // mongo_read's additive options: sort + limit + projection.
    common::create_and_activate_channel(
        &app,
        "agg-readopts",
        common::workflow_with_tasks(
            "AggReadOpts",
            json!([{
                "id": "t1", "name": "Read", "function": { "name": "mongo_read", "input": {
                    "connector": "agg",
                    "database": "orion_test",
                    "collection": "recordings",
                    "sort": { "at": -1 },
                    "limit": 2,
                    "projection": { "quality": 1, "_id": 0 },
                    "output": "data.items"
                } }
            }]),
        ),
    )
    .await;
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/agg-readopts",
            Some(json!({"data": {}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    let items = body["data"]["items"].as_array().expect("items");
    assert_eq!(items.len(), 2, "limit applies: {items:?}");
    assert_eq!(items[0]["quality"], "sd", "newest first: {items:?}");
    assert!(
        items[0].get("_id").is_none(),
        "projection excludes _id: {items:?}"
    );
}
