//! The deployment store's artifact collector as a node runs it: registered
//! under the task supervisor from `storage.artifact_gc_interval_secs`, and a
//! pass that takes what nothing serves from while the deployment keeps
//! serving.
//!
//! What a pass protects is asserted case by case in the store's unit tests
//! and, on every backend, in the repository contract; this is the node.

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use crate::common::{
    body_json, create_and_activate_channel, json_request, simple_log_workflow,
    test_state_with_config,
};

#[tokio::test]
async fn the_collector_is_a_supervised_job_unless_its_interval_is_zero() {
    let state = test_state_with_config(orion::config::AppConfig::default()).await;
    assert!(
        state
            .tasks
            .is_registered(orion::runtime::deployment_gc::JOB)
    );

    let mut config = orion::config::AppConfig::default();
    config.storage.artifact_gc_interval_secs = 0;
    let state = test_state_with_config(config).await;
    assert!(
        !state
            .tasks
            .is_registered(orion::runtime::deployment_gc::JOB)
    );
}

#[tokio::test]
async fn a_pass_collects_an_abandoned_upload_and_leaves_the_deployment_serving() {
    let state = test_state_with_config(orion::config::AppConfig::default()).await;
    let app = orion::server::build_router(state.clone());
    create_and_activate_channel(&app, "gc-orders", simple_log_workflow("GC WF")).await;

    let orphan = crate::common::deploy::upload(
        &app,
        json!({"kind": "workflow", "definition": simple_log_workflow("Never Deployed")}),
    )
    .await;
    let orion::storage::DbPool::Sqlite(pool) = &state.db_pool else {
        panic!("the harness runs on SQLite");
    };

    // Inside the grace window the upload is waiting for its apply.
    orion::runtime::deployment_gc::collect_once(&state)
        .await
        .expect("pass");
    let get = |digest: String| {
        let app = app.clone();
        async move {
            app.oneshot(json_request(
                "GET",
                &format!("/api/v2/admin/definitions/{digest}"),
                None,
            ))
            .await
            .unwrap()
            .status()
        }
    };
    assert_eq!(get(orphan.clone()).await, StatusCode::OK);

    sqlx::query(
        "UPDATE artifact_objects SET created_at = datetime('now', '-2 days') WHERE digest = ?1",
    )
    .bind(&orphan)
    .execute(pool)
    .await
    .unwrap();
    // One pass marks it, the next deletes its bytes.
    assert_eq!(
        orion::runtime::deployment_gc::collect_once(&state)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        get(orphan.clone()).await,
        StatusCode::OK,
        "draining keeps the bytes"
    );
    assert_eq!(
        orion::runtime::deployment_gc::collect_once(&state)
            .await
            .unwrap(),
        1
    );
    assert_eq!(get(orphan.clone()).await, StatusCode::NOT_FOUND);
    let (payload_null,): (bool,) =
        sqlx::query_as("SELECT payload_bytes IS NULL FROM artifact_objects WHERE digest = ?1")
            .bind(&orphan)
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(payload_null, "the bytes are gone from the database");

    // The deployment never noticed.
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v2/data/gc-orders",
            Some(json!({"data": {"x": 1}})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{}", body_json(resp).await);

    // Uploading it again brings it back.
    let again = crate::common::deploy::upload(
        &app,
        json!({"kind": "workflow", "definition": simple_log_workflow("Never Deployed")}),
    )
    .await;
    assert_eq!(again, orphan);
    assert_eq!(get(orphan).await, StatusCode::OK);
}
