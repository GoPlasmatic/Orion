//! `orion-server preflight` against a real database.
//!
//! The unit tests in `src/preflight.rs` cover the per-entity rules. These cover
//! the part only a database can show: that the scan reaches stored rows written
//! before 1.0, pages through them, and stays quiet on a clean estate.
//!
//! Rows in the pre-1.0 shapes are written by creating a valid row and then
//! rewriting its JSON column directly. That is not a shortcut around the
//! validators — it is the situation being tested. The 1.0 create path refuses
//! these shapes, so the only way one exists is that 0.3.0 wrote it, and the
//! whole point of preflight is to find rows nobody can create any more.

use orion::preflight;
use orion::storage::repositories::channels::{
    ChannelRepository, CreateChannelRequest, SqlChannelRepository,
};
use orion::storage::repositories::workflows::{
    CreateWorkflowRequest, SqlWorkflowRepository, WorkflowRepository,
};
use orion::storage::schema::{Channels, Workflows};
use orion::storage::{DbPool, build_sqlx, models};
use sea_query::{Expr, Query};
use serde_json::json;

async fn pool() -> DbPool {
    let storage = orion::config::StorageConfig {
        url: "sqlite::memory:".to_string(),
        ..Default::default()
    };
    orion::storage::init_pool(&storage).await.expect("pool")
}

/// Write `config_json` straight onto a stored channel, the way 0.3.0 left it.
async fn set_channel_config(pool: &DbPool, channel_id: &str, config_json: &str) {
    let (sql, values) = build_sqlx(
        Query::update()
            .table(Channels::Table)
            .value(Channels::ConfigJson, config_json)
            .and_where(Expr::col(Channels::ChannelId).eq(channel_id)),
    );
    pool.execute_query(&sql, values).await.expect("update");
}

/// Same for a workflow's `tasks_json`.
async fn set_workflow_tasks(pool: &DbPool, workflow_id: &str, tasks_json: &str) {
    let (sql, values) = build_sqlx(
        Query::update()
            .table(Workflows::Table)
            .value(Workflows::TasksJson, tasks_json)
            .and_where(Expr::col(Workflows::WorkflowId).eq(workflow_id)),
    );
    pool.execute_query(&sql, values).await.expect("update");
}

async fn create_channel(repo: &SqlChannelRepository, name: &str) -> String {
    repo.create(&CreateChannelRequest {
        channel_id: None,
        name: name.to_string(),
        description: None,
        channel_type: models::ChannelType::Sync,
        protocol: models::ChannelProtocol::Rest,
        methods: Some(vec!["POST".to_string()]),
        route_pattern: Some(format!("/{name}")),
        topic: None,
        consumer_group: None,
        transport_config: json!({}),
        workflow_id: None,
        config: json!({}),
        priority: 0,
    })
    .await
    .expect("create channel")
    .channel_id
}

async fn create_workflow(repo: &SqlWorkflowRepository, name: &str) -> String {
    repo.create(&CreateWorkflowRequest {
        workflow_id: None,
        name: name.to_string(),
        description: None,
        priority: 0,
        condition: json!(true),
        tasks: json!([{ "id": "t", "name": "T", "function": { "name": "log", "input": {} } }]),
        tags: vec![],
        continue_on_error: false,
    })
    .await
    .expect("create workflow")
    .workflow_id
}

#[tokio::test]
async fn a_clean_estate_reports_nothing() {
    let pool = pool().await;
    let channels = SqlChannelRepository::new(pool.clone());
    let workflows = SqlWorkflowRepository::new(pool.clone());

    create_channel(&channels, "clean-ch").await;
    create_workflow(&workflows, "clean-wf").await;

    let findings = preflight::scan(&channels, &workflows).await.expect("scan");
    assert!(findings.is_empty(), "{findings:?}");
}

#[tokio::test]
async fn an_empty_database_reports_nothing() {
    let pool = pool().await;
    let channels = SqlChannelRepository::new(pool.clone());
    let workflows = SqlWorkflowRepository::new(pool);

    let findings = preflight::scan(&channels, &workflows).await.expect("scan");
    assert!(findings.is_empty(), "{findings:?}");
}

/// The headline case: a channel stored by 0.3.0 with the `cors` spelling. It
/// would be quarantined at load — served by nothing, with no indication at the
/// admin API until someone looks at `/health`. Preflight names it beforehand.
#[tokio::test]
async fn a_stored_pre_1_0_cors_channel_is_found() {
    let pool = pool().await;
    let channels = SqlChannelRepository::new(pool.clone());
    let workflows = SqlWorkflowRepository::new(pool.clone());

    let id = create_channel(&channels, "legacy-cors").await;
    set_channel_config(
        &pool,
        &id,
        r#"{"cors": {"allowed_origins": ["https://app.example"]}}"#,
    )
    .await;

    let findings = preflight::scan(&channels, &workflows).await.expect("scan");
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(findings[0].entity.contains("legacy-cors"), "{findings:?}");
    assert!(
        findings[0].remedy.contains("origin_allow_list"),
        "{findings:?}"
    );
}

#[tokio::test]
async fn a_stored_pre_1_0_backpressure_channel_is_found() {
    let pool = pool().await;
    let channels = SqlChannelRepository::new(pool.clone());
    let workflows = SqlWorkflowRepository::new(pool.clone());

    let id = create_channel(&channels, "legacy-bp").await;
    set_channel_config(&pool, &id, r#"{"backpressure": {"max_concurrent": 25}}"#).await;

    let findings = preflight::scan(&channels, &workflows).await.expect("scan");
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(
        findings[0].remedy.contains("max_concurrent_per_node"),
        "{findings:?}"
    );
}

/// Checklist row 14 — the break that would otherwise surface on live traffic,
/// because the workflow keeps loading and activating either way.
#[tokio::test]
async fn a_stored_schema_less_dialect_task_is_found() {
    let pool = pool().await;
    let channels = SqlChannelRepository::new(pool.clone());
    let workflows = SqlWorkflowRepository::new(pool.clone());

    let id = create_workflow(&workflows, "dialect-wf").await;
    set_workflow_tasks(
        &pool,
        &id,
        &json!([{
            "id": "read", "name": "Read",
            "function": { "name": "data_query", "input": {
                "connector": "orders-db",
                "query": { "source": "orders" },
                "output": "data.orders"
            }}
        }])
        .to_string(),
    )
    .await;

    let findings = preflight::scan(&channels, &workflows).await.expect("scan");
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].check, "14");
    assert!(findings[0].entity.contains("dialect-wf"), "{findings:?}");
    assert!(findings[0].entity.contains("read"), "{findings:?}");
}

/// A pre-1.0 `data_write` still carrying the flat envelope.
#[tokio::test]
async fn a_stored_flat_data_write_envelope_is_found() {
    let pool = pool().await;
    let channels = SqlChannelRepository::new(pool.clone());
    let workflows = SqlWorkflowRepository::new(pool.clone());

    let id = create_workflow(&workflows, "flat-write-wf").await;
    set_workflow_tasks(
        &pool,
        &id,
        &json!([{
            "id": "w", "name": "W",
            "function": { "name": "data_write", "input": {
                "connector": "orders-db",
                "schema": { "unmapped": "identity" },
                "op": "insert", "target": "orders", "values": { "id": 1 },
                "output": "data.w"
            }}
        }])
        .to_string(),
    )
    .await;

    let findings = preflight::scan(&channels, &workflows).await.expect("scan");
    assert!(
        findings.iter().any(|f| f.problem.contains("write")),
        "{findings:?}"
    );
}

/// Every problem in the estate is reported in one run — an operator should not
/// have to fix one, re-run, and discover the next.
#[tokio::test]
async fn every_finding_is_reported_in_one_pass() {
    let pool = pool().await;
    let channels = SqlChannelRepository::new(pool.clone());
    let workflows = SqlWorkflowRepository::new(pool.clone());

    let cors_id = create_channel(&channels, "multi-cors").await;
    set_channel_config(&pool, &cors_id, r#"{"cors": {"allowed_origins": []}}"#).await;
    let bp_id = create_channel(&channels, "multi-bp").await;
    set_channel_config(&pool, &bp_id, r#"{"backpressure": {"max_concurrent": 5}}"#).await;
    create_channel(&channels, "multi-ok").await;

    let wf_id = create_workflow(&workflows, "multi-wf").await;
    set_workflow_tasks(
        &pool,
        &wf_id,
        &json!([{
            "id": "read", "name": "Read",
            "function": { "name": "data_query", "input": {
                "connector": "db", "query": { "source": "t" }, "output": "data.t"
            }}
        }])
        .to_string(),
    )
    .await;
    create_workflow(&workflows, "multi-ok-wf").await;

    let findings = preflight::scan(&channels, &workflows).await.expect("scan");
    assert_eq!(findings.len(), 3, "{findings:?}");

    // Channels first, then workflows — a stable order, so the report is
    // diffable between runs.
    assert!(findings[0].entity.starts_with("channel "), "{findings:?}");
    assert!(findings[1].entity.starts_with("channel "), "{findings:?}");
    assert!(findings[2].entity.starts_with("workflow "), "{findings:?}");
}
