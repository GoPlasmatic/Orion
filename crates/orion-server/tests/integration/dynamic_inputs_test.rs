//! Message-derived inputs for the raw connector handlers (proposal F1).
//!
//! `db_read`, `db_write`, `cache_read` and `cache_write` evaluate the
//! `{"var": ..}` nodes in their resolvable input fields against the message on
//! every call, so a cache key or bind parameter can come from the request
//! rather than only ever being a constant.
//!
//! Every test here drives a real workflow through the HTTP surface, so it
//! covers the engine's compile-then-evaluate path, which a unit test on one
//! field cannot.

use crate::common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

/// A cache key built from the request body: two requests with different ids
/// must land on different entries. Keyed on the literal object
/// `{"var": "data.req.id"}`, both would share one entry and the second read
/// would return the first caller's value.
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
                "/api/v2/data/dyn-cache-ch",
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
            "/api/v2/data/dyn-cache-ch",
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
            "/api/v2/data/dyn-cache-num-ch",
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
            "/api/v2/data/dyn-cache-ttl-ch",
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
            "/api/v2/data/dyn-db-ch",
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
            "/api/v2/data/dyn-db-default-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request");

    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["rows"][0]["bound"], "fallback", "got {body}");
}

/// Constant inputs pass through as written — only `{"var": ..}` nodes are
/// evaluated.
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
            "/api/v2/data/dyn-literal-ch",
            Some(json!({"data": {}})),
        ))
        .await
        .expect("request");

    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert_eq!(body["data"]["cached"]["nested"]["deep"], json!([1, 2, 3]));
}

/// Upload validation accepts a `{"var": ..}` node where the handler
/// resolves one, so a dynamic workflow can actually be deployed.
#[tokio::test]
async fn workflow_validation_accepts_var_nodes_on_resolvable_fields() {
    let app = common::test_app().await;

    let (status, body) = common::deploy::try_upload(
        &app,
        json!({"kind": "workflow", "definition": {
            "name": "dynamic-inputs",
            "tasks": [{
                "id": "t1",
                "name": "read",
                "function": {
                    "name": "cache_read",
                    "input": {"connector": "c", "key": {"var": "data.req.id"}}
                }
            }]
        }}),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "body: {body}");
}

/// The connector name is privileged config, not request data. It stays literal,
/// and validation still rejects a `{"var": ..}` node there.
#[tokio::test]
async fn workflow_validation_rejects_var_nodes_on_literal_fields() {
    let app = common::test_app().await;

    let (status, body) = common::deploy::try_upload(
        &app,
        json!({"kind": "workflow", "definition": {
            "name": "dynamic-connector",
            "tasks": [{
                "id": "t1",
                "name": "read",
                "function": {
                    "name": "cache_read",
                    "input": {"connector": {"var": "data.req.which"}, "key": "k"}
                }
            }]
        }}),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let details = body["error"]["details"]
        .as_array()
        .unwrap_or_else(|| panic!("expected details, got {body}"));
    assert!(
        details.iter().any(
            |d| d["path"] == "definition.tasks[0].function.input.connector"
                && d["code"] == "TYPE_MISMATCH"
        ),
        "got {body}"
    );
}
