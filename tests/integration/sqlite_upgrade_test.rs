//! The 0.3.0 → 1.0.0 upgrade with data in place, on SQLite (T27).
//!
//! Postgres and MySQL have data-bearing upgrade tests (`storage_postgres`,
//! `storage_mysql`) — but SQLite is the default backend, the one every
//! 0.1.0–0.3.0 install actually used, and it had none. Migrations 001–003
//! are exactly the schema 0.3.0 shipped (checksum-frozen), so applying only
//! those and seeding rows reproduces a real 0.3.0 database file; 1.0 startup
//! must then apply 004–009 over the existing rows.
//!
//! The one SQLite-specific hazard is `009_json_column_suffixes` (D26): its
//! `ALTER TABLE ... RENAME COLUMN` relies on SQLite ≥ 3.25 rewriting the
//! `current_*` view bodies and the `trg_*_active_immutable` trigger bodies in
//! place. The migration's own comment asserts that behaviour; before this
//! test, nothing in code did — and a trigger left naming a dead column fails
//! at first UPDATE, not at migration time. Unlike its siblings this runs in
//! the default suite: SQLite needs no container.

use orion::config::StorageConfig;
use orion::storage::DbPool;
use orion::storage::repositories::channels::{ChannelRepository, SqlChannelRepository};
use orion::storage::repositories::trace_dlq::{SqlTraceDlqRepository, TraceDlqRepository};
use orion::storage::repositories::workflows::{SqlWorkflowRepository, WorkflowRepository};
use std::path::PathBuf;

/// Self-cleaning scratch directory; `tempfile` is not in the dependency tree.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "orion_sqlite_upgrade_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Self(path)
    }

    fn url(&self) -> String {
        format!("sqlite:{}/orion.db", self.0.display())
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn upgrade_from_0_3_0_sqlite_file_with_data_preserves_rows() {
    let dir = ScratchDir::new();

    // Apply only 001–003: the 0.3.0 schema, with the real checksums in the
    // `_sqlx_migrations` ledger so the later full run continues cleanly.
    let raw = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&format!("{}?mode=rwc", dir.url()))
        .await
        .expect("create the database file");
    let mut pre_1_0 = sqlx::migrate!("./migrations/sqlite");
    let first_three: Vec<_> = pre_1_0.iter().filter(|m| m.version <= 3).cloned().collect();
    assert_eq!(
        first_three.len(),
        3,
        "the 0.3.0 schema is migrations 001–003"
    );
    pre_1_0.migrations = first_three.into();
    pre_1_0.run(&raw).await.expect("apply the 0.3.0 schema");

    // Seed representative rows with plain SQL. `tags` and `methods` are
    // spelled the *old* way on purpose: this is the 0.3.0 schema, before 009
    // renamed them. Distinctive values so the D26 assertions can tell "the
    // column moved" from "the column moved and took the data with it".
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

    // 1.0 startup over the data-bearing 0.3.0 file: applies 004–009.
    let pool = orion::storage::init_pool(&StorageConfig {
        url: dir.url(),
        max_connections: 2,
        ..Default::default()
    })
    .await
    .expect("1.0 startup must migrate a data-bearing 0.3.0 SQLite file");
    assert!(
        orion::storage::pending_migrations(&pool)
            .await
            .expect("pending")
            .is_empty(),
        "everything after 003 must have been applied"
    );
    let DbPool::Sqlite(sq) = &pool else {
        panic!("sqlite expected");
    };

    // -- D26: the rename, over the rows that were already there --

    // 1. The columns moved and kept their values.
    let (tags,): (String,) = sqlx::query_as(
        "SELECT tags_json FROM workflows WHERE workflow_id = 'wf-legacy' AND version = 2",
    )
    .fetch_one(sq)
    .await
    .expect("tags_json must exist after 009 and hold the seeded value");
    assert_eq!(tags, r#"["legacy","kept"]"#);
    let (methods,): (Option<String>,) =
        sqlx::query_as("SELECT methods_json FROM channels WHERE channel_id = 'ch-legacy'")
            .fetch_one(sq)
            .await
            .expect("methods_json must exist after 009 and hold the seeded value");
    assert_eq!(methods.as_deref(), Some(r#"["POST"]"#));

    // 2. The views were rewritten in place: `current_workflows` was created
    //    naming `tags`, and must now serve the renamed column for the seeded
    //    latest-version row.
    let (view_version, view_tags): (i64, String) = sqlx::query_as(
        "SELECT version, tags_json FROM current_workflows WHERE workflow_id = 'wf-legacy'",
    )
    .fetch_one(sq)
    .await
    .expect("current_workflows must exist and serve the renamed column");
    assert_eq!(view_version, 2, "view must resolve the latest version");
    assert_eq!(view_tags, r#"["legacy","kept"]"#);

    // 3. The trigger bodies were rewritten in place — the ≥ 3.25 behaviour
    //    009's comment relies on. `trg_workflows_active_immutable` compared
    //    `OLD.tags`; if the rewrite did not happen, this UPDATE errors with
    //    "no such column" instead of the trigger's own refusal — and if the
    //    trigger were silently dropped, the UPDATE would *succeed*. Both
    //    wrong outcomes are distinguishable from the assertion below.
    let refusal = sqlx::query(
        "UPDATE workflows SET tags_json = '[\"mutated\"]' \
         WHERE workflow_id = 'wf-legacy' AND version = 2",
    )
    .execute(sq)
    .await
    .expect_err("an active workflow's definition must still be immutable after the rename");
    let message = refusal.to_string();
    assert!(
        message.contains("immutable") || message.contains("active"),
        "the refusal must come from the rewritten trigger, not a dangling column: {message}"
    );

    // The repository reads over the migrated rows.
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
    sqlx::query("UPDATE trace_dlq SET next_retry_at = datetime('now', '-2 seconds')")
        .execute(sq)
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
}
