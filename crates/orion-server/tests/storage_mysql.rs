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

/// The D26 rename applied to a database that already has rows in it.
///
/// There is no released 0.x MySQL schema to upgrade from — MySQL storage is
/// new in 1.0.0 — so the "before" state here is migrations 001–010, the set
/// immediately preceding `011_json_column_suffixes`. That is the honest
/// equivalent: it is the schema the rename actually has to transform, and
/// seeding it first is the only way to run 011 over data rather than over
/// empty tables.
///
/// MySQL is the backend where a bare `RENAME COLUMN` does the most damage, and
/// does it *after* the migration reports success:
///
/// * `current_workflows` / `current_channels` stop resolving entirely
///   (ER_VIEW_INVALID 1356) — MySQL expanded `SELECT w.*` into a stored column
///   list at CREATE VIEW time, and every latest-version read goes through
///   those views.
/// * The `trg_*_active_immutable` bodies from 004 keep the old text, so the
///   next UPDATE of *any* workflow or channel row fails with
///   `Unknown column 'tags' in 'OLD'` (1054) — including the archive that the
///   repository layer performs on a perfectly ordinary lifecycle transition.
///
/// So this asserts data, views and triggers separately: on this backend all
/// three can break independently.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_mysql -- --ignored"]
async fn upgrade_across_the_json_column_rename_preserves_rows_views_and_triggers() {
    use orion::storage::repositories::workflows::{SqlWorkflowRepository, WorkflowRepository};

    let container = Mysql::default()
        .start()
        .await
        .expect("start mysql container");
    let port = container
        .get_host_port_ipv4(3306)
        .await
        .expect("mysql port");
    let url = format!("mysql://root@127.0.0.1:{port}/test");

    // Apply everything up to and including 010 — the schema as it stood the
    // instant before the rename — with the real checksums in the ledger, so
    // the full run below continues from 011 rather than re-applying.
    let raw = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect");
    let mut pre_rename = sqlx::migrate!("./migrations/mysql");
    let before: Vec<_> = pre_rename
        .iter()
        .filter(|m| m.version <= 10)
        .cloned()
        .collect();
    assert_eq!(
        before.len(),
        10,
        "the pre-rename schema is migrations 001–010"
    );
    pre_rename.migrations = before.into();
    pre_rename
        .run(&raw)
        .await
        .expect("apply the pre-rename schema");

    // `tags` and `methods` are the *old* names here on purpose — that is what
    // the schema is called at this point. Distinctive values so the assertions
    // can tell "the column moved" from "the column moved with its data".
    sqlx::query(
        "INSERT INTO workflows \
           (workflow_id, version, name, priority, status, rollout_percentage, \
            condition_json, tasks_json, tags, continue_on_error) \
         VALUES \
           ('wf-pre', 1, 'Pre-rename', 5, 'archived', 100, 'true', '[]', '[\"v1\"]', 0), \
           ('wf-pre', 2, 'Pre-rename', 5, 'active', 100, 'true', '[{\"id\":\"t1\"}]', \
            '[\"legacy\",\"kept\"]', 0)",
    )
    .execute(&raw)
    .await
    .expect("seed workflows");
    sqlx::query(
        "INSERT INTO channels \
           (channel_id, version, name, channel_type, protocol, methods, route_pattern, \
            transport_config_json, workflow_id, config_json, status, priority) \
         VALUES \
           ('ch-pre', 1, 'pre-ch', 'sync', 'http', '[\"POST\"]', '/pre', '{}', \
            'wf-pre', '{}', 'active', 0)",
    )
    .execute(&raw)
    .await
    .expect("seed channels");
    raw.close().await;

    // Startup applies 011 (rename + view/trigger rebuild) and 012 (char(64)).
    let pool = orion::storage::init_pool(&StorageConfig {
        url,
        max_connections: 5,
        ..Default::default()
    })
    .await
    .expect("startup must migrate a data-bearing pre-rename database");
    assert!(
        orion::storage::pending_migrations(&pool)
            .await
            .expect("pending")
            .is_empty(),
        "everything after 010 must have been applied"
    );
    let orion::storage::DbPool::Mysql(my) = &pool else {
        panic!("expected mysql pool");
    };

    // 1. The columns moved and kept their values.
    let (tags,): (String,) = sqlx::query_as(
        "SELECT tags_json FROM workflows WHERE workflow_id = 'wf-pre' AND version = 2",
    )
    .fetch_one(my)
    .await
    .expect("tags_json must exist after 011 and hold the seeded value");
    assert_eq!(tags, r#"["legacy","kept"]"#);
    let (methods,): (Option<String>,) =
        sqlx::query_as("SELECT methods_json FROM channels WHERE channel_id = 'ch-pre'")
            .fetch_one(my)
            .await
            .expect("methods_json must exist after 011 and hold the seeded value");
    assert_eq!(methods.as_deref(), Some(r#"["POST"]"#));

    // 2. The views were rebuilt — a bare rename leaves them invalid, and an
    //    invalid view does not even appear in information_schema.COLUMNS, so
    //    the read below is the assertion that matters most.
    let (view_version, view_tags): (i64, String) = sqlx::query_as(
        "SELECT version, tags_json FROM current_workflows WHERE workflow_id = 'wf-pre'",
    )
    .fetch_one(my)
    .await
    .expect("current_workflows must still resolve after the rename (ER_VIEW_INVALID)");
    assert_eq!(view_version, 2, "the view must resolve the latest version");
    assert_eq!(view_tags, r#"["legacy","kept"]"#);
    let (view_methods,): (Option<String>,) =
        sqlx::query_as("SELECT methods_json FROM current_channels WHERE channel_id = 'ch-pre'")
            .fetch_one(my)
            .await
            .expect("current_channels must still resolve after the rename");
    assert_eq!(view_methods.as_deref(), Some(r#"["POST"]"#));

    // 3. The active-immutability triggers were recreated against the new
    //    names, and still reject content changes rather than failing on a
    //    column that no longer exists.
    let err = sqlx::query(
        "UPDATE workflows SET tasks_json = '[{\"tampered\":true}]' \
         WHERE workflow_id = 'wf-pre' AND status = 'active'",
    )
    .execute(my)
    .await
    .expect_err("active content update must be blocked after the rename");
    assert!(
        err.to_string()
            .contains("Cannot modify content of active workflows"),
        "the trigger must reject by name, not fail on `Unknown column 'tags' in 'OLD'`: {err}"
    );
    let err = sqlx::query(
        "UPDATE channels SET config_json = '{\"tampered\":true}' \
         WHERE channel_id = 'ch-pre' AND status = 'active'",
    )
    .execute(my)
    .await
    .expect_err("active channel content update must be blocked after the rename");
    assert!(
        err.to_string()
            .contains("Cannot modify content of active channels"),
        "unexpected error: {err}"
    );
    // The renamed column is itself still in the immutable content set.
    let err = sqlx::query(
        "UPDATE workflows SET tags_json = '[\"tampered\"]' \
         WHERE workflow_id = 'wf-pre' AND status = 'active'",
    )
    .execute(my)
    .await
    .expect_err("tags_json must still be guarded");
    assert!(
        err.to_string()
            .contains("Cannot modify content of active workflows"),
        "unexpected error: {err}"
    );

    // 4. A legitimate lifecycle transition still works — this is the write
    //    that a broken trigger body turns into a hard failure.
    let repo = SqlWorkflowRepository::new(pool.clone());
    repo.archive("wf-pre")
        .await
        .expect("archiving an active workflow must still work after the rename");

    // 5. 012: the token hash is a fixed-width digest column now.
    let (data_type, column_type): (String, String) = sqlx::query_as(
        "SELECT CAST(DATA_TYPE AS CHAR), CAST(COLUMN_TYPE AS CHAR) \
         FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'traces' \
           AND COLUMN_NAME = 'access_token_hash'",
    )
    .fetch_one(my)
    .await
    .expect("introspect traces.access_token_hash");
    assert_eq!(data_type, "char");
    assert_eq!(column_type, "char(64)");
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
