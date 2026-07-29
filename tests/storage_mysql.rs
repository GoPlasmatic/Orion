//! MySQL-as-storage integration tests.
//!
//! Boots a MySQL testcontainer and runs Orion's own migrations against it —
//! the first coverage of MySQL as Orion's storage backend (previously it was
//! only tested as an external `db_read`/`db_write` connector).
//!
//! This is a separate test binary (not part of `tests/integration/`) because
//! the storage backend is pinned per process via a global `OnceLock`
//! (`DB_BACKEND`): the integration binary pins SQLite, so MySQL-backed tests
//! must run in their own process. Same reason there is one binary per backend.
//!
//! Run with: `cargo test --test storage_mysql -- --ignored`

use testcontainers::runners::AsyncRunner;
use testcontainers_modules::mysql::Mysql;

use orion::config::StorageConfig;

async fn mysql_pool() -> (
    testcontainers::ContainerAsync<Mysql>,
    orion::storage::DbPool,
) {
    let container = Mysql::default()
        .start()
        .await
        .expect("start mysql container");
    let port = container
        .get_host_port_ipv4(3306)
        .await
        .expect("mysql port");
    let pool = orion::storage::init_pool(&StorageConfig {
        url: format!("mysql://root@127.0.0.1:{port}/test"),
        max_connections: 5,
        ..Default::default()
    })
    .await
    .expect("init_pool + migrations must succeed on MySQL");
    (container, pool)
}

/// Migrations apply cleanly from an empty database (regression test for the
/// `DELIMITER` directives that used to make 001 unexecutable through sqlx),
/// and the repository layer round-trips through MySQL-rendered SQL.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_mysql -- --ignored"]
async fn mysql_migrations_apply_and_repo_roundtrip() {
    let (_container, pool) = mysql_pool().await;

    // No pending migrations after init_pool.
    let pending = orion::storage::pending_migrations(&pool)
        .await
        .expect("list pending");
    assert!(
        pending.is_empty(),
        "expected no pending migrations, got: {pending:?}"
    );

    // Repo roundtrip: create a draft workflow, read it back, activate it.
    use orion::storage::repositories::workflows::{
        CreateWorkflowRequest, SqlWorkflowRepository, WorkflowRepository,
    };
    let repo = SqlWorkflowRepository::new(pool.clone());
    let req = CreateWorkflowRequest {
        workflow_id: Some("wf-mysql".to_string()),
        name: "MySQL storage test".to_string(),
        description: None,
        priority: 0,
        condition: serde_json::Value::Bool(true),
        tasks: serde_json::json!([]),
        tags: vec![],
        continue_on_error: false,
    };
    let wf = repo.create(&req).await.expect("create draft");
    assert_eq!(wf.status, "draft");

    // Single-draft trigger: a second draft for the same workflow_id must fail.
    let dup = repo.create(&req).await;
    assert!(dup.is_err(), "second draft must be rejected by the trigger");

    let activated = repo.activate("wf-mysql", 100).await.expect("activate");
    assert_eq!(activated.status, "active");

    // Active-immutability trigger (004): raw content UPDATE on an active row
    // must be rejected even though it bypasses the repository layer.
    let orion::storage::DbPool::Mysql(mysql) = &pool else {
        panic!("expected mysql pool");
    };
    let raw_update = sqlx::query(
        "UPDATE workflows SET tasks_json = '[{\"tampered\":true}]' \
         WHERE workflow_id = 'wf-mysql' AND status = 'active'",
    )
    .execute(mysql)
    .await;
    let err = raw_update.expect_err("active content update must be blocked");
    assert!(
        err.to_string()
            .contains("Cannot modify content of active workflows"),
        "unexpected error: {err}"
    );

    // Legitimate lifecycle transition still works.
    repo.archive("wf-mysql")
        .await
        .expect("archive active workflow");
}

/// DLQ claim semantics on real MySQL: SKIP LOCKED tx claim + lease blocking
/// (multi-instance-ha A3).
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_mysql -- --ignored"]
async fn mysql_dlq_claim_leases_rows() {
    use orion::storage::repositories::trace_dlq::{SqlTraceDlqRepository, TraceDlqRepository};

    let (_container, pool) = mysql_pool().await;
    let repo = SqlTraceDlqRepository::new(pool.clone());

    let entry = repo
        .enqueue("trace-1", "orders", "{}", "{}", "boom", 0, 5)
        .await
        .expect("enqueue");
    let orion::storage::DbPool::Mysql(mysql) = &pool else {
        panic!("mysql expected");
    };
    sqlx::query(
        "UPDATE trace_dlq SET next_retry_at = DATE_SUB(UTC_TIMESTAMP(), INTERVAL 2 SECOND)",
    )
    .execute(mysql)
    .await
    .expect("backdate");

    let claimed = repo.claim_pending("node-a", 10, 60).await.expect("claim");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, entry.id);

    // Leased — nothing left for a second claimant.
    let claimed = repo.claim_pending("node-b", 10, 60).await.expect("claim");
    assert!(claimed.is_empty());

    // Expired lease is re-claimable.
    sqlx::query(
        "UPDATE trace_dlq SET claimed_until = DATE_SUB(UTC_TIMESTAMP(), INTERVAL 1 SECOND)",
    )
    .execute(mysql)
    .await
    .expect("expire");
    assert_eq!(
        repo.claim_pending("node-c", 10, 60)
            .await
            .expect("claim")
            .len(),
        1
    );
}

/// D8 keyset pagination on MySQL — the backend where the tie-break is not
/// optional.
///
/// `traces.created_at` is a `datetime` with no fractional seconds, so a burst
/// of traces genuinely shares one `created_at` value. A cursor over
/// `created_at` alone would either loop forever or skip the whole group; only
/// the `(created_at, id)` comparison makes progress. That is not reproducible
/// on SQLite or Postgres, whose timestamps are sub-second.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_mysql -- --ignored"]
async fn mysql_keyset_pagination_walks_every_trace_once() {
    use orion::storage::repositories::traces::{SqlTraceRepository, TraceFilter, TraceRepository};

    let (_container, pool) = mysql_pool().await;
    let repo = SqlTraceRepository::new(pool.clone());

    let mut seeded = std::collections::BTreeSet::new();
    for _ in 0..7 {
        seeded.insert(
            repo.store_completed("orders", Some("ch-orders"), "sync", None, "{}", 1.0, None)
                .await
                .expect("seed trace"),
        );
    }

    // Pin every row to one timestamp so the walk can only advance on `id`.
    let orion::storage::DbPool::Mysql(mysql) = &pool else {
        panic!("expected mysql pool");
    };
    sqlx::query("UPDATE traces SET created_at = '2026-01-01 00:00:00'")
        .execute(mysql)
        .await
        .expect("flatten created_at");

    let mut seen = Vec::new();
    let mut cursor = None;
    for _ in 0..10 {
        let page = repo
            .list_paginated(&TraceFilter {
                limit: Some(3),
                cursor: cursor.clone(),
                ..Default::default()
            })
            .await
            .expect("keyset page");
        seen.extend(page.data.iter().map(|t| t.id.clone()));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(
        seen.len(),
        7,
        "seven traces sharing a `created_at` must still be walked exactly \
         once each — the cursor's id tie-break is what makes that possible: \
         {seen:?}"
    );
    assert_eq!(
        seen.iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        seeded
    );

    // `total` is opt-in (D8) and the count itself still has to be right.
    assert_eq!(
        repo.list_paginated(&TraceFilter::default())
            .await
            .expect("default page")
            .total,
        None
    );
    assert_eq!(
        repo.list_paginated(&TraceFilter {
            include_total: Some(true),
            ..Default::default()
        })
        .await
        .expect("counted page")
        .total,
        Some(7)
    );
}
