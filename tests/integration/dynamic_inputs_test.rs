//! Message-derived inputs for the raw connector handlers (proposal F1).
//!
//! dataflow-rs precompiles a task's `input` once at engine build, so a handler
//! sees the literal workflow JSON. `db_read`, `db_write`, `cache_read`,
//! `cache_write`, and `mongo_read` used to read those fields raw, which meant a
//! cache key or bind parameter could only ever be a constant. They now fold
//! `{"var": ..}` nodes against the message context
//! (`connector_helpers::resolve_value`).
//!
//! Every test here drives a real workflow through the HTTP surface, so it
//! covers the precompilation behaviour that a unit test on the resolver cannot.

use crate::common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

/// A cache key built from the request body: two requests with different ids
/// must land on different entries. Under the old behaviour both wrote the
/// literal object `{"var": "data.req.id"}` as the key and the second read returned
/// the first caller's value.
#[tokio::test]
async fn cache_key_resolves_from_the_message() {
    let app = common::test_app().await;

    common::create_connector(&app, common::cache_connector_memory("dyn-cache")).await;

    common::create_and_activate_channel(
        &app,
        "dyn-cache-ch",
        common::workflow_with_tasks(
            "DynamicCacheKey",
            json!([
                {
                    "id": "t0",
                    "name": "Bring the request payload into data.req",
                    "function": {
                        "name": "parse_json",
                        "input": {"source": "payload", "target": "req"}
                    }
                },
                {
                    "id": "t1",
                    "name": "Write per-request entry",
                    "function": {
                        "name": "cache_write",
                        "input": {
                            "connector": "dyn-cache",
                            "key": {"var": "data.req.id"},
                            "value": {"var": "data.req.payload"}
                        }
                    }
                },
                {
                    "id": "t2",
                    "name": "Read it back",
                    "function": {
                        "name": "cache_read",
                        "input": {
                            "connector": "dyn-cache",
                            "key": {"var": "data.req.id"},
                            "output": "data.cached"
                        }
                    }
                }
            ]),
        ),
    )
    .await;

    for (id, payload) in [("alpha", "first"), ("beta", "second")] {
        let resp = app
            .clone()
            .oneshot(common::json_request(
                "POST",
                "/api/v1/data/dyn-cache-ch",
                Some(json!({"data": {"id": id, "payload": payload}})),
            ))
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = common::body_json(resp).await;
        assert_eq!(
            body["data"]["cached"], payload,
            "key {id} should read back its own value, got {body}"
        );
    }

    // And the first key is still intact after the second request wrote its own.
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/dyn-cache-ch",
            Some(json!({"data": {"id": "alpha", "payload": "first"}})),
        ))
        .await
        .expect("request");
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["cached"], "first");
}

/// A non-string key (`data.req.id` is a number) is coerced rather than rejected —
/// numeric ids are the common case for a per-entity cache key.
#[tokio::test]
async fn cache_key_accepts_a_numeric_message_value() {
    let app = common::test_app().await;

    common::create_connector(&app, common::cache_connector_memory("dyn-cache-num")).await;

    common::create_and_activate_channel(
        &app,
        "dyn-cache-num-ch",
        common::workflow_with_tasks(
            "NumericCacheKey",
            json!([
                {
                    "id": "t0",
                    "name": "Bring the request payload into data.req",
                    "function": {
                        "name": "parse_json",
                        "input": {"source": "payload", "target": "req"}
                    }
                },
                {
                    "id": "t1",
                    "name": "Write",
                    "function": {
                        "name": "cache_write",
                        "input": {
                            "connector": "dyn-cache-num",
                            "key": {"var": "data.req.id"},
                            "value": {"total": 7}
                        }
                    }
                },
                {
                    "id": "t2",
                    "name": "Read",
                    "function": {
                        "name": "cache_read",
                        "input": {
                            "connector": "dyn-cache-num",
                            "key": {"var": "data.req.id"},
                            "output": "data.cached"
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
            "/api/v1/data/dyn-cache-num-ch",
            Some(json!({"data": {"id": 4242}})),
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["cached"]["total"], 7);
}

/// `ttl_secs` is resolvable too, so a per-request expiry is expressible.
#[tokio::test]
async fn cache_ttl_resolves_from_the_message() {
    let app = common::test_app().await;

    common::create_connector(&app, common::cache_connector_memory("dyn-cache-ttl")).await;

    common::create_and_activate_channel(
        &app,
        "dyn-cache-ttl-ch",
        common::workflow_with_tasks(
            "DynamicTtl",
            json!([
                {
                    "id": "t0",
                    "name": "Bring the request payload into data.req",
                    "function": {
                        "name": "parse_json",
                        "input": {"source": "payload", "target": "req"}
                    }
                },
                {
                    "id": "t1",
                    "name": "Write with dynamic ttl",
                    "function": {
                        "name": "cache_write",
                        "input": {
                            "connector": "dyn-cache-ttl",
                            "key": {"var": "data.req.id"},
                            "value": "kept",
                            "ttl_secs": {"var": "data.req.ttl"}
                        }
                    }
                },
                {
                    "id": "t2",
                    "name": "Read",
                    "function": {
                        "name": "cache_read",
                        "input": {
                            "connector": "dyn-cache-ttl",
                            "key": {"var": "data.req.id"},
                            "output": "data.cached"
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
            "/api/v1/data/dyn-cache-ttl-ch",
            Some(json!({"data": {"id": "ttl-key", "ttl": 60}})),
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["cached"], "kept");
}

/// Bind parameters for the raw-SQL escape hatch come from the message, so a
/// `db_write` can persist request data and a `db_read` can look it up again.
#[tokio::test]
async fn sql_bind_params_resolve_from_the_message() {
    let app = common::test_app().await;

    common::create_connector(
        &app,
        common::db_connector_sqlite(
            "dyn-db",
            "sqlite:file:dynamic_inputs_sql_params?mode=memory&cache=shared",
        ),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "dyn-db-ch",
        common::workflow_with_tasks(
            "DynamicBindParams",
            json!([
                {
                    "id": "t0",
                    "name": "Bring the request payload into data.req",
                    "function": {
                        "name": "parse_json",
                        "input": {"source": "payload", "target": "req"}
                    }
                },
                {
                    "id": "t1",
                    "name": "Create table",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "dyn-db",
                            "query": "CREATE TABLE IF NOT EXISTS dyn_items (id TEXT PRIMARY KEY, name TEXT, qty INTEGER)",
                            "output": "data.created"
                        }
                    }
                },
                {
                    "id": "t2",
                    "name": "Insert from the request body",
                    "function": {
                        "name": "db_write",
                        "input": {
                            "connector": "dyn-db",
                            "query": "INSERT INTO dyn_items (id, name, qty) VALUES (?, ?, ?)",
                            "params": [
                                {"var": "data.req.id"},
                                {"var": "data.req.name"},
                                {"var": "data.req.qty"}
                            ],
                            "output": "data.inserted"
                        }
                    }
                },
                {
                    "id": "t3",
                    "name": "Read it back by the same id",
                    "function": {
                        "name": "db_read",
                        "input": {
                            "connector": "dyn-db",
                            "query": "SELECT id, name, qty FROM dyn_items WHERE id = ?",
                            "params": [{"var": "data.req.id"}],
                            "output": "data.rows"
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
            "/api/v1/data/dyn-db-ch",
            Some(json!({"data": {"id": "sku-77", "name": "Sprocket", "qty": 12}})),
        ))
        .await
        .expect("request");

    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["inserted"]["rows_affected"], 1);

    let rows = body["data"]["rows"]
        .as_array()
        .unwrap_or_else(|| panic!("rows should be an array, got {body}"));
    assert_eq!(rows.len(), 1, "got {body}");
    assert_eq!(rows[0]["id"], "sku-77");
    assert_eq!(rows[0]["name"], "Sprocket");
    assert_eq!(rows[0]["qty"], 12);
}

/// The JSONLogic two-argument `var` form supplies a fallback when the path is
/// absent from the message.
#[tokio::test]
async fn bind_params_support_the_var_default_form() {
    let app = common::test_app().await;

    common::create_connector(
        &app,
        common::db_connector_sqlite(
            "dyn-db-default",
            "sqlite:file:dynamic_inputs_var_default?mode=memory&cache=shared",
        ),
    )
    .await;

    common::create_and_activate_channel(
        &app,
        "dyn-db-default-ch",
        common::workflow_with_tasks(
            "VarDefault",
            json!([
                {
                    "id": "t0",
                    "name": "Bring the request payload into data.req",
                    "function": {
                        "name": "parse_json",
                        "input": {"source": "payload", "target": "req"}
                    }
                },
                {
                    "id": "t1",
                    "name": "Echo the bound value back",
                    "function": {
                        "name": "db_read",
                        "input": {
                            "connector": "dyn-db-default",
                            "query": "SELECT ? AS bound",
                            "params": [{"var": ["data.req.missing", "fallback"]}],
                            "output": "data.rows"
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
            "/api/v1/data/dyn-db-default-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request");

    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["rows"][0]["bound"], "fallback", "got {body}");
}

/// Constant inputs keep working unchanged — the resolver only rewrites
/// `{"var": ..}` nodes, so already-authored workflows are unaffected.
#[tokio::test]
async fn literal_inputs_are_unchanged() {
    let app = common::test_app().await;

    common::create_connector(&app, common::cache_connector_memory("dyn-literal")).await;

    common::create_and_activate_channel(
        &app,
        "dyn-literal-ch",
        common::workflow_with_tasks(
            "LiteralInputs",
            json!([
                {
                    "id": "t0",
                    "name": "Bring the request payload into data.req",
                    "function": {
                        "name": "parse_json",
                        "input": {"source": "payload", "target": "req"}
                    }
                },
                {
                    "id": "t1",
                    "name": "Write a constant",
                    "function": {
                        "name": "cache_write",
                        "input": {
                            "connector": "dyn-literal",
                            "key": "fixed-key",
                            "value": {"nested": {"deep": [1, 2, 3]}},
                            "ttl_secs": 30
                        }
                    }
                },
                {
                    "id": "t2",
                    "name": "Read the constant",
                    "function": {
                        "name": "cache_read",
                        "input": {
                            "connector": "dyn-literal",
                            "key": "fixed-key",
                            "output": "data.cached"
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
            "/api/v1/data/dyn-literal-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request");

    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["cached"]["nested"]["deep"], json!([1, 2, 3]));
}

/// Admin-time validation accepts a `{"var": ..}` node where the handler
/// resolves one, so a dynamic workflow can actually be created.
#[tokio::test]
async fn workflow_validation_accepts_var_nodes_on_resolvable_fields() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "dynamic-inputs",
                "tasks": [{
                    "id": "t1",
                    "name": "read",
                    "function": {
                        "name": "cache_read",
                        "input": {"connector": "c", "key": {"var": "data.req.id"}}
                    }
                }]
            })),
        ))
        .await
        .expect("request");

    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "body: {}",
        common::body_json(resp).await
    );
}

/// The connector name is privileged config, not request data. It stays literal,
/// and validation still rejects a `{"var": ..}` node there.
#[tokio::test]
async fn workflow_validation_rejects_var_nodes_on_literal_fields() {
    let app = common::test_app().await;

    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/admin/workflows",
            Some(json!({
                "name": "dynamic-connector",
                "tasks": [{
                    "id": "t1",
                    "name": "read",
                    "function": {
                        "name": "cache_read",
                        "input": {"connector": {"var": "data.req.which"}, "key": "k"}
                    }
                }]
            })),
        ))
        .await
        .expect("request");

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = common::body_json(resp).await;
    let details = body["error"]["details"]
        .as_array()
        .unwrap_or_else(|| panic!("expected details, got {body}"));
    assert!(
        details
            .iter()
            .any(|d| d["path"] == "tasks[0].function.input.connector"
                && d["code"] == "TYPE_MISMATCH"),
        "got {body}"
    );
}

/// mongo_read's filter is folded against the message, so a find() can select on
/// request data. Requires Docker; run with
/// `cargo test --test integration -- --ignored dynamic_inputs`.
#[tokio::test]
#[ignore]
async fn mongo_filter_resolves_from_the_message() {
    use crate::common::backends::Backend;

    let app = common::test_app().await;
    let h = common::backends::start(Backend::Mongo, "dyn-mongo").await;
    common::create_connector(&app, h.connector_json()).await;

    // Seeding lives on its own channel so the read channel can be driven more
    // than once without re-inserting a duplicate `_id`.
    common::create_and_activate_channel(
        &app,
        "dyn-mongo-seed-ch",
        common::workflow_with_tasks(
            "SeedMongoDoc",
            json!([
                {
                    "id": "t1",
                    "name": "Seed a document",
                    "function": {
                        "name": "data_write",
                        "input": {
                            "connector": "dyn-mongo",
                            "database": "orion_test",
                            // F24: the dialect rejects undeclared names, so
                            // this seed asks for pass-through explicitly —
                            // the one line a 0.x task adds.
                            "schema": { "unmapped": "identity" },
                            "op": "insert",
                            "target": "dyn_docs",
                            "values": {"id": "doc-1", "owner": "alice"},
                            "output": "data.seeded"
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
            "/api/v1/data/dyn-mongo-seed-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("seed request");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "seed failed: {}",
        common::body_json(resp).await
    );

    common::create_and_activate_channel(
        &app,
        "dyn-mongo-ch",
        common::workflow_with_tasks(
            "DynamicMongoFilter",
            json!([
                {
                    "id": "t0",
                    "name": "Bring the request payload into data.req",
                    "function": {
                        "name": "parse_json",
                        "input": {"source": "payload", "target": "req"}
                    }
                },
                {
                    "id": "t1",
                    "name": "Find by the requested owner",
                    "function": {
                        "name": "mongo_read",
                        "input": {
                            "connector": "dyn-mongo",
                            "database": "orion_test",
                            "collection": "dyn_docs",
                            "filter": {"owner": {"var": "data.req.owner"}},
                            "output": "data.found"
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
            "/api/v1/data/dyn-mongo-ch",
            Some(json!({"data": {"owner": "alice"}})),
        ))
        .await
        .expect("request");

    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    let found = body["data"]["found"]
        .as_array()
        .unwrap_or_else(|| panic!("found should be an array, got {body}"));
    assert_eq!(found.len(), 1, "got {body}");
    assert_eq!(found[0]["owner"], "alice");

    // A filter that resolves to a different owner must match nothing — proving
    // the document was selected by the message value, not by a constant.
    let resp = app
        .clone()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/data/dyn-mongo-ch",
            Some(json!({"data": {"owner": "bob"}})),
        ))
        .await
        .expect("request");
    let body = common::body_json(resp).await;
    assert_eq!(
        body["data"]["found"].as_array().map(|a| a.len()),
        Some(0),
        "got {body}"
    );
}
