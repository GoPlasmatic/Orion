//! Postgres-as-storage integration tests.
//!
//! Boots a Postgres testcontainer and runs Orion's own migrations against it.
//! Separate test binary because the storage backend is pinned per process via
//! the global `DB_BACKEND` OnceLock (the integration binary pins SQLite).
//!
//! Run with: `cargo test --test storage_postgres -- --ignored`

use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

use orion::config::StorageConfig;
use orion::storage::DbPool;
use orion::storage::repositories::workflows::{
    CreateWorkflowRequest, SqlWorkflowRepository, WorkflowRepository,
};

async fn postgres_pool() -> (testcontainers::ContainerAsync<Postgres>, DbPool) {
    let container = Postgres::default().start().await.expect("start postgres");
    let port = container.get_host_port_ipv4(5432).await.expect("pg port");
    let pool = orion::storage::init_pool(&StorageConfig {
        url: format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres"),
        max_connections: 5,
        ..Default::default()
    })
    .await
    .expect("init_pool + migrations must succeed on Postgres");
    (container, pool)
}

fn workflow_request(id: &str) -> CreateWorkflowRequest {
    CreateWorkflowRequest {
        workflow_id: Some(id.to_string()),
        name: "PG storage test".to_string(),
        description: None,
        priority: 0,
        condition: serde_json::Value::Bool(true),
        tasks: serde_json::json!([]),
        tags: vec![],
        continue_on_error: false,
    }
}

#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_postgres -- --ignored"]
async fn postgres_migrations_apply_and_triggers_enforce() {
    let (_container, pool) = postgres_pool().await;

    let pending = orion::storage::pending_migrations(&pool)
        .await
        .expect("list pending");
    assert!(
        pending.is_empty(),
        "expected no pending migrations, got: {pending:?}"
    );

    // Repo roundtrip: draft → activate.
    let repo = SqlWorkflowRepository::new(pool.clone());
    let wf = repo
        .create(&workflow_request("wf-pg"))
        .await
        .expect("create");
    assert_eq!(wf.status, "draft");

    // Single-draft enforcement (partial unique index on postgres).
    let dup = repo.create(&workflow_request("wf-pg")).await;
    assert!(dup.is_err(), "second draft must be rejected");

    let activated = repo.activate("wf-pg", 100).await.expect("activate");
    assert_eq!(activated.status, "active");

    // Active-immutability trigger (004): raw content UPDATE on an active row
    // must be rejected even though it bypasses the repository layer.
    let DbPool::Postgres(pg) = &pool else {
        panic!("expected postgres pool");
    };
    let raw_update = sqlx::query(
        "UPDATE workflows SET tasks_json = '[{\"tampered\":true}]' \
         WHERE workflow_id = 'wf-pg' AND status = 'active'",
    )
    .execute(pg)
    .await;
    let err = raw_update.expect_err("active content update must be blocked");
    assert!(
        err.to_string()
            .contains("Cannot modify content of active workflows"),
        "unexpected error: {err}"
    );

    // Legitimate lifecycle transition still works.
    repo.archive("wf-pg")
        .await
        .expect("archive active workflow");
}

/// DLQ claim semantics on real Postgres: SKIP LOCKED claim, lease blocking,
/// and single-winner under concurrency (multi-instance-ha A3).
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_postgres -- --ignored"]
async fn postgres_dlq_claim_single_winner() {
    use orion::storage::repositories::trace_dlq::{SqlTraceDlqRepository, TraceDlqRepository};

    let (_container, pool) = postgres_pool().await;
    let repo = std::sync::Arc::new(SqlTraceDlqRepository::new(pool.clone()));

    let entry = repo
        .enqueue("trace-1", "orders", "{}", "{}", "boom", 5)
        .await
        .expect("enqueue");
    let DbPool::Postgres(pg) = &pool else {
        panic!("postgres expected");
    };
    sqlx::query("UPDATE trace_dlq SET next_retry_at = LOCALTIMESTAMP - interval '2 seconds'")
        .execute(pg)
        .await
        .expect("backdate");

    // Two nodes claim concurrently — exactly one wins the single row.
    let (a, b) = tokio::join!(
        repo.claim_pending("node-a", 10, 60),
        repo.claim_pending("node-b", 10, 60),
    );
    let a = a.expect("claim a");
    let b = b.expect("claim b");
    assert_eq!(a.len() + b.len(), 1, "exactly one node must claim the row");

    // Expired lease is re-claimable.
    sqlx::query("UPDATE trace_dlq SET claimed_until = LOCALTIMESTAMP - interval '1 seconds'")
        .execute(pg)
        .await
        .expect("expire");
    let reclaimed = repo.claim_pending("node-c", 10, 60).await.expect("reclaim");
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].id, entry.id);
}
