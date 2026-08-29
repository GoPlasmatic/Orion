//! Postgres-as-storage integration tests.
//!
//! Boots a Postgres testcontainer and runs Orion's own migrations against it.
//!
//! A separate test binary, but no longer a forced one: the backend used to be
//! a process-global `OnceLock` that the integration binary pinned to SQLite,
//! and it is now carried on `DbPool` itself, so these could be modules of that
//! binary. Merging them is a test-topology change (see `tests/README.md`), not
//! part of removing the global.
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
        loop_config: None,
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
        .enqueue(orion::storage::repositories::trace_dlq::DlqEnqueue {
            trace_id: "trace-1",
            channel: "orders",
            payload_json: "{}",
            metadata_json: "{}",
            error_message: "boom",
            retry_count: 0,
            max_retries: 5,
        })
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
        .enqueue(orion::storage::repositories::trace_dlq::DlqEnqueue {
            trace_id: "trace-retry",
            channel: "orders",
            payload_json: "{}",
            metadata_json: "{}",
            error_message: "boom",
            retry_count: 0,
            max_retries: 5,
        })
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

/// The 0.3.0 → 1.0.0 upgrade with data in place.
///
/// Migrations 001–003 are exactly the schema 0.3.0 shipped (they are
/// checksum-frozen), so applying only those and seeding rows reproduces a
/// real 0.3.0 Postgres database. 1.0 startup must then apply the remaining
/// migrations over the existing rows, and the repositories must decode them
/// afterwards. Two of those migrations had only ever run against empty
/// tables before this test:
///
/// * `004_bigint_columns` — the INT4→BIGINT widening with its non-idempotent
///   view drop/recreate that the upgrade guide singles out. That read is
///   precisely what 0.3.0 could not do (sqlx-postgres refuses INT4 → i64).
/// * `013_json_column_suffixes` — `tags` → `tags_json` and `methods` →
///   `methods_json` (D26). A rename is the one migration shape where "the
///   rows survived" and "everything that reads them survived" are different
///   questions: Postgres leaves a dependent view publishing the *old* column
///   name and leaves a plpgsql trigger body naming a field that no longer
///   exists, and neither failure appears at migration time. Both are asserted
///   below, over seeded rows.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_postgres -- --ignored"]
async fn upgrade_from_0_3_0_schema_with_data_preserves_rows() {
    use orion::storage::repositories::channels::{ChannelRepository, SqlChannelRepository};
    use orion::storage::repositories::trace_dlq::{SqlTraceDlqRepository, TraceDlqRepository};

    let container = Postgres::default().start().await.expect("start postgres");
    let port = container.get_host_port_ipv4(5432).await.expect("pg port");
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    // Apply only 001–003: the 0.3.0 schema, with the real checksums in the
    // `_sqlx_migrations` ledger so the later full run continues cleanly.
    let raw = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect");
    let mut pre_1_0 = sqlx::migrate!("./migrations/postgres");
    let first_three: Vec<_> = pre_1_0.iter().filter(|m| m.version <= 3).cloned().collect();
    assert_eq!(
        first_three.len(),
        3,
        "the 0.3.0 schema is migrations 001–003"
    );
    pre_1_0.migrations = first_three.into();
    pre_1_0.run(&raw).await.expect("apply the 0.3.0 schema");

    // Seed representative rows with plain SQL — a versioned workflow pair,
    // an active channel referencing it, and a partially retried DLQ entry.
    // (INT4 columns cannot hold anything a 0.3.0 deployment couldn't; the
    // 0.3.0 failure was in decoding, which the repository reads below hit.)
    //
    // `tags` and `methods` are spelled the *old* way here on purpose: this is
    // the 0.3.0 schema, before `013_json_column_suffixes` renamed them. The
    // values are distinctive so the D26 assertions can tell "the column moved"
    // from "the column moved and took the data with it".
    sqlx::query(
        "INSERT INTO workflows \
           (workflow_id, version, name, priority, status, rollout_percentage, condition_json, tasks_json, tags) \
         VALUES \
           ('wf-legacy', 1, 'Legacy workflow', 5, 'archived', 100, 'true', '[]', '[\"v1\"]'), \
           ('wf-legacy', 2, 'Legacy workflow', 5, 'active', 100, 'true', '[{\"id\":\"t1\"}]', '[\"legacy\",\"kept\"]')",
    )
    .execute(&raw)
    .await
    .expect("seed workflows");
    sqlx::query(
        "INSERT INTO channels \
           (channel_id, version, name, channel_type, protocol, methods, route_pattern, workflow_id, status) \
         VALUES \
           ('ch-legacy', 1, 'legacy-ch', 'sync', 'http', '[\"POST\"]', '/legacy', 'wf-legacy', 'active')",
    )
    .execute(&raw)
    .await
    .expect("seed channels");
    sqlx::query(
        "INSERT INTO trace_dlq \
           (id, trace_id, channel, payload_json, error_message, retry_count, max_retries) \
         VALUES ('dlq-legacy', 'trace-legacy', 'legacy-ch', '{}', 'boom', 3, 5)",
    )
    .execute(&raw)
    .await
    .expect("seed trace_dlq");
    raw.close().await;

    // 1.0 startup over the data-bearing 0.3.0 database: applies 004+.
    let pool = orion::storage::init_pool(&StorageConfig {
        url,
        max_connections: 5,
        ..Default::default()
    })
    .await
    .expect("1.0 startup must migrate a data-bearing 0.3.0 database");
    assert!(
        orion::storage::pending_migrations(&pool)
            .await
            .expect("pending")
            .is_empty(),
        "everything after 003 must have been applied"
    );

    // The widening actually happened (004 rewrites these under an
    // ACCESS EXCLUSIVE lock — with rows present, not a no-op).
    let DbPool::Postgres(pg) = &pool else {
        panic!("postgres expected");
    };
    let column_types: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name::text, data_type::text FROM information_schema.columns \
         WHERE table_name = 'workflows' \
           AND column_name IN ('version', 'priority', 'rollout_percentage')",
    )
    .fetch_all(pg)
    .await
    .expect("introspect workflows");
    assert_eq!(column_types.len(), 3);
    for (column, data_type) in &column_types {
        assert_eq!(
            data_type, "bigint",
            "workflows.{column} must be widened to bigint, got {data_type}"
        );
    }

    // 004 drops and recreates the current_* views around the ALTERs; over
    // seeded data the recreated view must still resolve latest-version rows.
    let (view_version,): (i64,) =
        sqlx::query_as("SELECT version FROM current_workflows WHERE workflow_id = 'wf-legacy'")
            .fetch_one(pg)
            .await
            .expect("current_workflows must exist and serve the seeded rows");
    assert_eq!(view_version, 2, "view must resolve the latest version");

    // The repository reads 0.3.0 could not perform: every one of these
    // decodes the formerly-INT4 columns as i64.
    let wf_repo = SqlWorkflowRepository::new(pool.clone());
    let wf = wf_repo.get_by_id("wf-legacy").await.expect("read workflow");
    assert_eq!(wf.version, 2);
    assert_eq!(wf.priority, 5);
    assert_eq!(wf.status, "active");
    assert_eq!(
        wf_repo
            .list_versions(
                "wf-legacy",
                &orion::storage::repositories::helpers::VersionFilter {
                    limit: Some(10),
                    offset: Some(0),
                },
            )
            .await
            .expect("versions")
            .total,
        2,
        "both seeded versions must survive"
    );

    let ch_repo = SqlChannelRepository::new(pool.clone());
    let ch = ch_repo.get_by_id("ch-legacy").await.expect("read channel");
    assert_eq!(ch.name, "legacy-ch");
    assert_eq!(ch.workflow_id.as_deref(), Some("wf-legacy"));
    assert_eq!(ch.status, "active");

    // DLQ counters survive with their values, and the row is still live
    // (3 of 5 retries used, so the retry machinery may claim it again).
    let dlq_repo = SqlTraceDlqRepository::new(pool.clone());
    sqlx::query("UPDATE trace_dlq SET next_retry_at = LOCALTIMESTAMP - interval '2 seconds'")
        .execute(pg)
        .await
        .expect("backdate");
    let claimed = dlq_repo
        .claim_pending("upgrade-node", 10, 60)
        .await
        .expect("claim");
    assert_eq!(claimed.len(), 1, "seeded DLQ entry must still be claimable");
    assert_eq!(claimed[0].id, "dlq-legacy");
    assert_eq!(claimed[0].retry_count, 3);
    assert_eq!(claimed[0].max_retries, 5);

    // -- D26: the rename, over the rows that were already there --

    // 1. The columns moved and kept their values.
    let (tags,): (String,) = sqlx::query_as(
        "SELECT tags_json FROM workflows WHERE workflow_id = 'wf-legacy' AND version = 2",
    )
    .fetch_one(pg)
    .await
    .expect("tags_json must exist after 013 and hold the seeded value");
    assert_eq!(tags, r#"["legacy","kept"]"#);
    let (methods,): (Option<String>,) =
        sqlx::query_as("SELECT methods_json FROM channels WHERE channel_id = 'ch-legacy'")
            .fetch_one(pg)
            .await
            .expect("methods_json must exist after 013 and hold the seeded value");
    assert_eq!(methods.as_deref(), Some(r#"["POST"]"#));

    // 2. The views were rebuilt. `RENAME COLUMN` alone leaves a Postgres view
    //    publishing the name it was created with, so `current_workflows` would
    //    still say `tags` while the table says `tags_json` — silently, until a
    //    `SELECT *` decodes into a row struct that no longer has that field.
    for (view, gone, present) in [
        ("current_workflows", "tags", "tags_json"),
        ("current_channels", "methods", "methods_json"),
    ] {
        let columns: Vec<(String,)> = sqlx::query_as(
            "SELECT column_name::text FROM information_schema.columns WHERE table_name = $1",
        )
        .bind(view)
        .fetch_all(pg)
        .await
        .unwrap_or_else(|e| panic!("introspect {view}: {e}"));
        let columns: Vec<String> = columns.into_iter().map(|(c,)| c).collect();
        assert!(
            columns.iter().any(|c| c == present),
            "{view} must publish `{present}` after 013, has {columns:?}"
        );
        assert!(
            !columns.iter().any(|c| c == gone),
            "{view} still publishes the pre-rename `{gone}` — 013 dropped and \
             recreated it precisely because CREATE OR REPLACE VIEW cannot change a \
             view column's name: {columns:?}"
        );
    }
    // And it still resolves the latest version over the seeded pair.
    let (view_version, view_tags): (i64, String) = sqlx::query_as(
        "SELECT version, tags_json FROM current_workflows WHERE workflow_id = 'wf-legacy'",
    )
    .fetch_one(pg)
    .await
    .expect("the rebuilt view must still serve the seeded rows");
    assert_eq!(view_version, 2);
    assert_eq!(view_tags, r#"["legacy","kept"]"#);

    // 3. The active-immutability triggers still fire. Their plpgsql bodies are
    //    stored as text, so before 013 replaced them the next UPDATE of an
    //    active row raised `record "old" has no field "tags"` — the guard
    //    turning into an unconditional error rather than a guard.
    let err = sqlx::query(
        "UPDATE workflows SET tasks_json = '[{\"tampered\":true}]' \
         WHERE workflow_id = 'wf-legacy' AND status = 'active'",
    )
    .execute(pg)
    .await
    .expect_err("active content update must be blocked after the rename");
    assert!(
        err.to_string()
            .contains("Cannot modify content of active workflows"),
        "the workflows trigger must still reject content changes by name, not fail \
         on a missing field: {err}"
    );
    let err = sqlx::query(
        "UPDATE channels SET config_json = '{\"tampered\":true}' \
         WHERE channel_id = 'ch-legacy' AND status = 'active'",
    )
    .execute(pg)
    .await
    .expect_err("active channel content update must be blocked after the rename");
    assert!(
        err.to_string()
            .contains("Cannot modify content of active channels"),
        "the channels trigger must still reject content changes: {err}"
    );
    // The renamed column is itself still guarded — the predicate reads it.
    let err = sqlx::query(
        "UPDATE workflows SET tags_json = '[\"tampered\"]' \
         WHERE workflow_id = 'wf-legacy' AND status = 'active'",
    )
    .execute(pg)
    .await
    .expect_err("tags_json must still be part of the immutable content set");
    assert!(
        err.to_string()
            .contains("Cannot modify content of active workflows"),
        "unexpected error: {err}"
    );

    // 4. A legitimate transition is still allowed — the trigger did not become
    //    a blanket refusal.
    wf_repo
        .archive("wf-legacy")
        .await
        .expect("archiving an active workflow must still work");
}

/// D8 keyset pagination, on the backend whose `timestamp` type the cursor
/// actually has to bind against.
///
/// The SQLite tests in `src/storage/repositories/traces.rs` compare a
/// `NaiveDateTime` against a `timestamp_text` *string* column; here it is a
/// real `timestamp`, so this is the first proof that the cursor's
/// `(created_at, id)` comparison round-trips through the wire format at all.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_postgres -- --ignored"]
async fn postgres_keyset_pagination_walks_every_trace_once() {
    use orion::storage::repositories::traces::{
        SqlTraceRepository, TraceFilter, TraceReader, TraceSink,
    };

    let (_container, pool) = postgres_pool().await;
    let repo = SqlTraceRepository::new(pool.clone());

    let mut seeded = std::collections::BTreeSet::new();
    for _ in 0..7 {
        seeded.insert(
            repo.store_completed(orion::storage::repositories::traces::TraceCompletedRef {
                channel: "orders",
                channel_id: Some("ch-orders"),
                mode: "sync",
                input_json: None,
                result_json: "{}",
                duration_ms: 1.0,
                task_trace_json: None,
            })
            .await
            .expect("seed trace"),
        );
    }

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
        "the walk must visit every trace once: {seen:?}"
    );
    assert_eq!(
        seen.iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        seeded,
        "and exactly the seeded ones"
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

/// The point of the migration, on the backend it was written for: the planner
/// can satisfy the keyset ordering from `idx_traces_created_at_id` alone.
///
/// `enable_seqscan = off` is what makes this meaningful on a table with seven
/// rows — without it Postgres correctly reads the whole thing. The assertion
/// is that *an index path exists for this ordering*, which is exactly what
/// migration 011 added.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_postgres -- --ignored"]
async fn postgres_keyset_ordering_is_served_by_the_new_index() {
    let (_container, pool) = postgres_pool().await;
    let DbPool::Postgres(pg) = &pool else {
        panic!("expected postgres pool");
    };

    sqlx::query("SET enable_seqscan = off")
        .execute(pg)
        .await
        .expect("disable seqscan");
    let plan: Vec<(String,)> =
        sqlx::query_as("EXPLAIN SELECT * FROM traces ORDER BY created_at DESC, id DESC LIMIT 50")
            .fetch_all(pg)
            .await
            .expect("explain");
    let plan = plan
        .into_iter()
        .map(|(line,)| line)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        plan.contains("idx_traces_created_at_id"),
        "the keyset ordering must be servable from the (created_at, id) index \
         added by migration 011, else every page sorts the table:\n{plan}"
    );
}
