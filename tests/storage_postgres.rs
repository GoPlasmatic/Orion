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
        .enqueue("trace-1", "orders", "{}", "{}", "boom", 0, 5)
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

/// D1: `record_retry` and `mark_exhausted` clear `claimed_until`, a timestamp
/// column. Binding a NULL *string* there is a 42804 type mismatch on postgres,
/// and every call site discarded the error — so on postgres both writes failed
/// silently and entries were re-claimed forever. SQLite's dynamic typing hides
/// this entirely, which is why the coverage has to live here.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_postgres -- --ignored"]
async fn postgres_dlq_record_retry_and_mark_exhausted_persist() {
    use orion::storage::repositories::trace_dlq::{SqlTraceDlqRepository, TraceDlqRepository};

    let (_container, pool) = postgres_pool().await;
    let repo = SqlTraceDlqRepository::new(pool.clone());
    let DbPool::Postgres(pg) = &pool else {
        panic!("postgres expected");
    };

    // -- record_retry --
    let entry = repo
        .enqueue("trace-retry", "orders", "{}", "{}", "boom", 0, 5)
        .await
        .expect("enqueue");
    sqlx::query("UPDATE trace_dlq SET next_retry_at = LOCALTIMESTAMP - interval '2 seconds'")
        .execute(pg)
        .await
        .expect("backdate");
    let claimed = repo.claim_pending("node-a", 10, 60).await.expect("claim");
    assert_eq!(claimed.len(), 1, "entry must be claimable before the retry");

    let next_retry = chrono::Utc::now().naive_utc() - chrono::Duration::seconds(2);
    repo.record_retry(&entry.id, next_retry)
        .await
        .expect("record_retry must succeed on postgres");

    let (retry_count, claimed_by, claimed_until): (
        i64,
        Option<String>,
        Option<chrono::NaiveDateTime>,
    ) = sqlx::query_as(
        "SELECT retry_count, claimed_by, claimed_until FROM trace_dlq WHERE id = $1",
    )
    .bind(&entry.id)
    .fetch_one(pg)
    .await
    .expect("read back");
    assert_eq!(retry_count, 1, "record_retry must increment retry_count");
    assert!(claimed_by.is_none(), "record_retry must release the claim");
    assert!(claimed_until.is_none(), "record_retry must clear the lease");

    // Lease released and due again -> another node can claim it.
    let reclaimed = repo.claim_pending("node-b", 10, 60).await.expect("reclaim");
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].retry_count, 1);

    // -- mark_exhausted --
    repo.mark_exhausted(&entry.id)
        .await
        .expect("mark_exhausted must succeed on postgres");

    let (retry_count, max_retries, claimed_until): (i64, i64, Option<chrono::NaiveDateTime>) =
        sqlx::query_as(
            "SELECT retry_count, max_retries, claimed_until FROM trace_dlq WHERE id = $1",
        )
        .bind(&entry.id)
        .fetch_one(pg)
        .await
        .expect("read back");
    assert_eq!(
        retry_count, max_retries,
        "mark_exhausted must retire the entry"
    );
    assert!(
        claimed_until.is_none(),
        "mark_exhausted must clear the lease"
    );

    sqlx::query("UPDATE trace_dlq SET next_retry_at = LOCALTIMESTAMP - interval '2 seconds'")
        .execute(pg)
        .await
        .expect("backdate");
    assert!(
        repo.claim_pending("node-c", 10, 60)
            .await
            .expect("claim")
            .is_empty(),
        "an exhausted entry must never be claimed again"
    );
}
