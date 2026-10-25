//! The repository behaviours that must hold on **every** backend, written once.
//!
//! Before this, each backend binary spelled out its own round-trip: SQLite in
//! the unit tests under `src/storage/repositories/`, Postgres in
//! `storage_postgres.rs`, MySQL in `storage_mysql.rs`. Three copies of the
//! same intent, which is why the MySQL-only branches ended up covered by "one
//! round-trip each" — nobody was going to write the fourth assertion three
//! more times.
//!
//! So the assertions live here and take a [`DbPool`]. Each backend binary
//! supplies a pool and runs the whole suite. Adding a case adds it everywhere;
//! a backend that diverges fails naming itself.
//!
//! This matters most for the paths that genuinely fork on backend. MySQL has
//! no `RETURNING`, so `archive_latest_active` runs a completely different
//! implementation there — read the newest active row, archive, read back,
//! inside one transaction — and the equivalence of that path to the
//! single-statement one is exactly the kind of thing a shared contract can
//! assert and three hand-written round-trips cannot.
//!
//! Included with `#[path]` rather than made a crate module: it is test-only
//! support shared between test binaries, and the alternative is public API on
//! the library that exists solely for tests.

#![allow(dead_code)]

use orion::storage::DbPool;
use orion::storage::repositories::Repositories;

/// Run every contract case against `pool`. `backend` names the backend in
/// failure messages, so a red CI log says which one diverged without needing
/// the test name decoded.
pub async fn run_all(backend: &str, pool: &DbPool) {
    Repositories::new(pool, &orion::config::StorageConfig::default())
        .expect("repositories must build on every backend");

    a_read_snapshot_is_repeatable(backend, pool).await;
    the_shared_store_claims_once_and_counts_every_increment(backend, pool).await;
    a_shared_rate_window_holds_and_spills_at_most_2x(backend, pool).await;
    a_deployment_audit_row_commits_with_the_head(backend, pool).await;
    the_artifact_collector_judges_its_grace_on_the_database_clock(backend, pool).await;
    source_leases_are_won_once_and_judged_on_the_database_clock(backend, pool).await;
}

/// A deployment store manifest of one workflow member over `digest`.
fn one_workflow_manifest(id: &str, digest: &str) -> orion::extensions::CanonicalManifest {
    use orion::extensions::{CanonicalManifest, EntityKind, EntityRef, ManifestMember};
    CanonicalManifest {
        schema_version: 2,
        providers: Vec::new(),
        provider_lock_digest: None,
        members: vec![ManifestMember {
            entity: EntityRef {
                kind: EntityKind::Workflow,
                id: id.to_string(),
                revision: 1,
            },
            definition_digest: digest.to_string(),
            owning_package: None,
            requires: Vec::new(),
            rollouts: Vec::new(),
        }],
    }
}

/// The audit row of a deployment apply or rollback is written in the
/// transaction that moves the head: present the moment the commit returns,
/// absent when nothing was committed (an idempotent replay, a lost race).
async fn a_deployment_audit_row_commits_with_the_head(backend: &str, pool: &DbPool) {
    use orion::extensions::deployment_store::Rollback;
    use orion::extensions::{
        CommunityAdmissionPolicy, DeploymentReceipt, ImmutableDeploymentStore,
    };
    use orion::storage::repositories::audit_logs::{
        AuditEvent, AuditLogFilter, AuditLogRepository, SqlAuditLogRepository,
    };

    let audit = SqlAuditLogRepository::new(pool.clone());
    let rows = |action: &'static str| {
        let audit = &audit;
        async move {
            audit
                .list_paginated(&AuditLogFilter {
                    action: Some(action.to_string()),
                    resource_type: Some("deployment".to_string()),
                    ..Default::default()
                })
                .await
                .expect("list audit rows")
                .total
        }
    };
    let event = |action: &'static str| {
        move |r: &DeploymentReceipt| AuditEvent {
            principal: "contract".to_string(),
            action: action.to_string(),
            resource_type: "deployment".to_string(),
            resource_id: r.committed_revision.to_string(),
            details: None,
        }
    };
    let policy = CommunityAdmissionPolicy::new();
    let node_a = ImmutableDeploymentStore::new("contract-a");
    let node_b = ImmutableDeploymentStore::new("contract-b");
    let base = node_a.current_head().0;

    let def = node_a
        .store_definition_artifact_in_db(pool, b"contract-audit-wf-1".to_vec())
        .await
        .expect("store definition");
    let manifest = one_workflow_manifest("contract-audit", &def.digest);
    let plan_a = node_a
        .plan_candidate_in_db(pool, base, manifest.clone())
        .await
        .expect("plan a");
    // A second node planned on the same base before the first committed.
    let plan_b = node_b
        .plan_candidate_in_db(pool, base, manifest)
        .await
        .expect("plan b");

    let (receipt, recorded) = node_a
        .apply_candidate_audited_in_db(
            pool,
            "contract",
            "audit-1",
            &plan_a,
            "r",
            &policy,
            event("v2_deployment_apply"),
        )
        .await
        .expect("apply");
    assert!(recorded.is_some(), "{backend}: the apply moved the head");
    assert_eq!(
        rows("v2_deployment_apply").await,
        1,
        "{backend}: the row is there when the commit returns, not when a queue drains"
    );

    let (replayed, recorded) = node_a
        .apply_candidate_audited_in_db(
            pool,
            "contract",
            "audit-1",
            &plan_a,
            "r",
            &policy,
            event("v2_deployment_apply"),
        )
        .await
        .expect("replay");
    assert_eq!(replayed.committed_revision, receipt.committed_revision);
    assert!(recorded.is_none(), "{backend}: a replay commits nothing");

    let lost = node_b
        .apply_candidate_audited_in_db(
            pool,
            "contract",
            "audit-2",
            &plan_b,
            "r",
            &policy,
            event("v2_deployment_apply"),
        )
        .await
        .expect_err("a stale base loses");
    assert_eq!(lost.code, "DEPLOYMENT_HEAD_CONFLICT", "{backend}");
    assert_eq!(
        rows("v2_deployment_apply").await,
        1,
        "{backend}: neither the replay nor the lost race wrote a row"
    );

    // A rollback's row commits with its head move too.
    let def2 = node_a
        .store_definition_artifact_in_db(pool, b"contract-audit-wf-2".to_vec())
        .await
        .expect("store definition");
    let plan2 = node_a
        .plan_candidate_in_db(
            pool,
            receipt.committed_revision,
            one_workflow_manifest("contract-audit", &def2.digest),
        )
        .await
        .expect("plan 2");
    let (second, _) = node_a
        .apply_candidate_audited_in_db(
            pool,
            "contract",
            "audit-3",
            &plan2,
            "r",
            &policy,
            event("v2_deployment_apply"),
        )
        .await
        .expect("apply 2");
    let ((rolled, _), recorded) = node_a
        .rollback_to_revision_audited_in_db(
            pool,
            &Rollback {
                principal: "contract",
                idempotency_key: "audit-4",
                expected_head_revision: second.committed_revision,
                target_revision: receipt.committed_revision,
                reason: "r",
            },
            &policy,
            event("v2_deployment_rollback"),
        )
        .await
        .expect("rollback");
    assert_eq!(rolled.committed_revision, second.committed_revision + 1);
    assert!(recorded.is_some(), "{backend}");
    assert_eq!(rows("v2_deployment_rollback").await, 1, "{backend}");
}

/// The cluster's shared store on this backend: values expire on the
/// database's clock, a claim is won by exactly one of many concurrent
/// claimants, and concurrent increments are never lost.
async fn the_shared_store_claims_once_and_counts_every_increment(backend: &str, pool: &DbPool) {
    use orion::storage::repositories::shared_state::SqlSharedState;
    let store = SqlSharedState::new(pool.clone());

    store.set("contract:v", "1", None).await.expect("set");
    store
        .set("contract:v", "2", Some(60))
        .await
        .expect("overwrite");
    assert_eq!(
        store.get("contract:v").await.expect("get").as_deref(),
        Some("2"),
        "{backend}"
    );
    store.set("contract:gone", "x", Some(0)).await.expect("set");
    assert_eq!(
        store.get("contract:gone").await.expect("get"),
        None,
        "{backend}: expired"
    );

    let claims = futures::future::join_all((0..8).map(|i| {
        let store = store.clone();
        async move {
            store
                .claim("contract:claim", &format!("owner-{i}"), 60)
                .await
        }
    }))
    .await;
    let won = claims.iter().filter(|c| matches!(c, Ok(None))).count();
    assert_eq!(
        won, 1,
        "{backend}: exactly one claimant wins, got {claims:?}"
    );

    let increments = futures::future::join_all((0..10).map(|_| {
        let store = store.clone();
        async move { store.incr_by("contract:n", 1, Some(60)).await }
    }))
    .await;
    let mut seen: Vec<i64> = increments.into_iter().map(|r| r.expect("incr")).collect();
    seen.sort_unstable();
    assert_eq!(
        seen,
        (1..=10).collect::<Vec<_>>(),
        "{backend}: no increment lost"
    );

    assert_eq!(
        store
            .remove_many(&["contract:v".into(), "contract:n".into()])
            .await
            .expect("remove"),
        2,
        "{backend}"
    );
    store.sweep().await.expect("sweep");
}

/// A fixed window on the database's clock: the limit holds within a second,
/// and crossing into the next lets at most one more window's worth through.
async fn a_shared_rate_window_holds_and_spills_at_most_2x(backend: &str, pool: &DbPool) {
    use orion::channel::{RateLimitBackend, SharedRateLimitBackend};
    use orion::storage::repositories::shared_state::SqlSharedState;
    // limit_per_window = rps + burst = 3.
    let limiter = SharedRateLimitBackend::new(
        SqlSharedState::new(pool.clone()),
        format!("contract-{backend}"),
        3,
        0,
    );
    align_to_fresh_window().await;
    for i in 0..3 {
        assert!(
            limiter.check("caller".into()).await.expect("check"),
            "{backend}: request {i}"
        );
    }
    assert!(
        !limiter.check("caller".into()).await.expect("check"),
        "{backend}: the 4th request in one window is refused"
    );
    align_to_fresh_window().await;
    for i in 0..3 {
        assert!(
            limiter.check("caller".into()).await.expect("check"),
            "{backend}: request {i} in the next window passes"
        );
    }
    assert!(
        !limiter.check("caller".into()).await.expect("check"),
        "{backend}: the spill is bounded to one window"
    );
}

/// Sleep until shortly after the next second boundary, leaving ~800ms for
/// the requests that follow. The node's clock stands in for the database's,
/// which on a test machine is the same clock.
async fn align_to_fresh_window() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock");
    let into_window = u64::from(now.subsec_millis());
    tokio::time::sleep(std::time::Duration::from_millis(1000 - into_window + 60)).await;
}

async fn a_read_snapshot_is_repeatable(backend: &str, pool: &DbPool) {
    use orion::storage::repositories::cluster::{ClusterRepository, SqlClusterRepository};
    // SQLite's concurrent-writer case needs WAL on disk, and is exercised
    // in configuration_recovery_test rather than this in-memory harness.
    if matches!(pool, DbPool::Sqlite(_)) {
        return;
    }
    if let DbPool::Mysql(mysql) = pool {
        // Prove the snapshot helper does not depend on InnoDB's usual
        // default: make every pooled session READ COMMITTED first.
        let mut connections = Vec::new();
        for _ in 0..mysql.options().get_max_connections() {
            let mut conn = mysql.acquire().await.expect("session");
            sqlx::query("SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED")
                .execute(&mut *conn)
                .await
                .expect("change session default");
            connections.push(conn);
        }
        drop(connections);
    }
    let epochs = SqlClusterRepository::new(pool.clone());
    let mut snapshot = pool.begin_snapshot_tx().await.expect("read snapshot");
    let empty = || sea_query_sqlx::SqlxValues(sea_query::Values(Vec::new()));
    let first: Vec<(i64,)> = snapshot
        .fetch_all_as(
            "SELECT breaker_epoch FROM config_epoch WHERE id = 1",
            empty(),
        )
        .await
        .expect("first read");
    let new_epoch = epochs
        .request_breaker_reset("contract:snapshot")
        .await
        .expect("concurrent commit");
    let pinned: Vec<(i64,)> = snapshot
        .fetch_all_as(
            "SELECT breaker_epoch FROM config_epoch WHERE id = 1",
            empty(),
        )
        .await
        .expect("second read");
    assert_eq!(first, pinned, "{backend}: reads share one snapshot");
    snapshot.commit().await.expect("release snapshot");
    assert_eq!(
        new_epoch,
        first[0].0 + 1,
        "{backend}: the writer actually committed"
    );
}

/// Move an artifact's `created_at` two hours into the database's past.
async fn backdate_artifact(pool: &DbPool, digest: &str) {
    let done = match pool {
        DbPool::Sqlite(p) => sqlx::query(
            "UPDATE artifact_objects SET created_at = datetime('now', '-2 hours') WHERE digest = ?1",
        )
        .bind(digest)
        .execute(p)
        .await
        .map(|r| r.rows_affected()),
        DbPool::Postgres(p) => sqlx::query(
            "UPDATE artifact_objects SET created_at = LOCALTIMESTAMP - interval '2 hours' WHERE digest = $1",
        )
        .bind(digest)
        .execute(p)
        .await
        .map(|r| r.rows_affected()),
        DbPool::Mysql(p) => sqlx::query(
            "UPDATE artifact_objects SET created_at = CURRENT_TIMESTAMP - INTERVAL 2 HOUR WHERE digest = ?",
        )
        .bind(digest)
        .execute(p)
        .await
        .map(|r| r.rows_affected()),
    };
    assert_eq!(done.expect("backdate"), 1);
}

/// The deployment store's collector on this backend: an unreferenced
/// artifact inside the grace window — measured on the database clock — is
/// left alone; outside it, it drains on one pass and its bytes are deleted on
/// the next; what the head names is never touched; and a node whose own copy
/// still says active revives a deleted artifact when it uploads it again,
/// because inside the transaction the database row wins.
async fn the_artifact_collector_judges_its_grace_on_the_database_clock(
    backend: &str,
    pool: &DbPool,
) {
    use orion::extensions::{ArtifactState, CommunityAdmissionPolicy, ImmutableDeploymentStore};

    let policy = CommunityAdmissionPolicy::new();
    let node_a = ImmutableDeploymentStore::new("contract-gc-a");
    let node_b = ImmutableDeploymentStore::new("contract-gc-b");
    let deployed = node_a
        .store_definition_artifact_in_db(pool, b"contract-gc-wf".to_vec())
        .await
        .expect("store");
    let base = node_a.current_head().0;
    let plan = node_a
        .plan_candidate_in_db(
            pool,
            base,
            one_workflow_manifest("contract-gc", &deployed.digest),
        )
        .await
        .expect("plan");
    node_a
        .apply_candidate_in_db(pool, "contract", "gc-1", &plan, "r", &policy)
        .await
        .expect("apply");
    let orphan = node_a
        .store_definition_artifact_in_db(pool, b"contract-gc-orphan".to_vec())
        .await
        .expect("store")
        .digest;
    // Node B reads the store (and so holds the orphan as active) before
    // node A collects it.
    node_b
        .store_definition_artifact_in_db(pool, b"contract-gc-b".to_vec())
        .await
        .expect("store");
    assert_eq!(
        node_b.get_artifact(&orphan).map(|a| a.state),
        Some(ArtifactState::Active),
        "{backend}"
    );

    let gc = |node: &ImmutableDeploymentStore| {
        let node = node.clone();
        async move {
            node.mark_and_collect_garbage_in_db(pool, 20, 3600)
                .await
                .expect("collect")
        }
    };
    let pass = gc(&node_a).await;
    assert!(
        !pass.draining.contains(&orphan),
        "{backend}: written seconds ago on the database clock, it is inside the grace window"
    );
    backdate_artifact(pool, &orphan).await;
    let pass = gc(&node_a).await;
    assert!(pass.draining.contains(&orphan), "{backend}: {pass:?}");
    assert!(!pass.draining.contains(&deployed.digest), "{backend}");
    let pass = gc(&node_a).await;
    assert!(pass.deleted.contains(&orphan), "{backend}: {pass:?}");
    assert!(!pass.deleted.contains(&deployed.digest), "{backend}");

    let fresh = ImmutableDeploymentStore::new("contract-gc-reader");
    fresh.load_from_db_pool(pool).await.expect("load");
    let row = fresh.get_artifact(&orphan).expect("tombstone kept");
    assert_eq!(row.state, ArtifactState::Deleted, "{backend}");
    assert!(row.bytes.is_none(), "{backend}: the bytes are gone");
    let head_def = fresh.get_artifact(&deployed.digest).expect("deployed");
    assert_eq!(head_def.state, ArtifactState::Active, "{backend}");
    assert!(head_def.bytes.is_some(), "{backend}");

    // Node B's copy still says active; uploading the bytes again must revive
    // the database row, not answer from the stale copy.
    node_b
        .store_definition_artifact_in_db(pool, b"contract-gc-orphan".to_vec())
        .await
        .expect("re-upload");
    let fresh = ImmutableDeploymentStore::new("contract-gc-reader-2");
    fresh.load_from_db_pool(pool).await.expect("load");
    let row = fresh.get_artifact(&orphan).expect("revived");
    assert_eq!(row.state, ArtifactState::Active, "{backend}");
    assert!(row.bytes.is_some(), "{backend}");
    // ... with a fresh grace window.
    let pass = gc(&node_a).await;
    assert!(!pass.draining.contains(&orphan), "{backend}: {pass:?}");
}

/// Move a source partition's lease into the database's past.
async fn expire_source_lease(pool: &DbPool, channel: &str, partition: &str) {
    let done = match pool {
        DbPool::Sqlite(p) => sqlx::query(
            "UPDATE source_claims SET lease_until = datetime('now', '-1 seconds') \
             WHERE channel_id = ?1 AND partition_id = ?2",
        )
        .bind(channel)
        .bind(partition)
        .execute(p)
        .await
        .map(|r| r.rows_affected()),
        DbPool::Postgres(p) => sqlx::query(
            "UPDATE source_claims SET lease_until = LOCALTIMESTAMP - interval '1 second' \
             WHERE channel_id = $1 AND partition_id = $2",
        )
        .bind(channel)
        .bind(partition)
        .execute(p)
        .await
        .map(|r| r.rows_affected()),
        DbPool::Mysql(p) => sqlx::query(
            "UPDATE source_claims SET lease_until = UTC_TIMESTAMP() - INTERVAL 1 SECOND \
             WHERE channel_id = ? AND partition_id = ?",
        )
        .bind(channel)
        .bind(partition)
        .execute(p)
        .await
        .map(|r| r.rows_affected()),
    };
    assert_eq!(done.expect("expire"), 1);
}

/// Source partition leases on this backend: of many concurrent first
/// claimants exactly one wins fence 1; a live lease is not taken; an expired
/// one — expired on the database's clock — is, with a greater fence; and a
/// checkpoint commits only under the current fence.
async fn source_leases_are_won_once_and_judged_on_the_database_clock(backend: &str, pool: &DbPool) {
    use orion::extensions::{SourceLeaseCoordinator, SourcePartitionKey};
    let key = SourcePartitionKey::new("contract-source", "p0");

    let claims = futures::future::join_all((0..6).map(|i| {
        let coord = SourceLeaseCoordinator::new(pool.clone());
        let key = key.clone();
        async move {
            coord
                .acquire_or_takeover(key, format!("node-{i}"), 1, "ns", 60)
                .await
        }
    }))
    .await;
    let won: Vec<_> = claims.iter().filter_map(|c| c.as_ref().ok()).collect();
    assert_eq!(
        won.len(),
        1,
        "{backend}: exactly one claimant wins: {claims:?}"
    );
    assert_eq!(won[0].fence_token, 1, "{backend}");
    for lost in claims.iter().filter_map(|c| c.as_ref().err()) {
        assert_eq!(
            lost.code, "SOURCE_PARTITION_LEASE_HELD",
            "{backend}: {lost}"
        );
    }
    let holder = won[0].owner_id.clone();

    let coord = SourceLeaseCoordinator::new(pool.clone());
    coord
        .renew(&key, &holder, 1, 60)
        .await
        .unwrap_or_else(|e| panic!("{backend}: renew a live lease: {e}"));
    coord
        .commit_checkpoint(&key, 1, 5, &serde_json::json!({"lsn": 5}))
        .await
        .unwrap_or_else(|e| panic!("{backend}: commit under the live fence: {e}"));

    expire_source_lease(pool, "contract-source", "p0").await;
    assert_eq!(
        coord.renew(&key, &holder, 1, 60).await.unwrap_err().code,
        "SOURCE_LEASE_EXPIRED",
        "{backend}"
    );
    let takeover = coord
        .acquire_or_takeover(key.clone(), "node-late", 1, "ns", 60)
        .await
        .unwrap_or_else(|e| panic!("{backend}: take over an expired lease: {e}"));
    assert_eq!(takeover.fence_token, 2, "{backend}");
    assert_eq!(
        coord
            .commit_checkpoint(&key, 1, 9, &serde_json::json!({"lsn": 9}))
            .await
            .unwrap_err()
            .code,
        "SOURCE_FENCE_STALE",
        "{backend}: the old owner's checkpoint is refused"
    );
    coord
        .commit_checkpoint(&key, 2, 7, &serde_json::json!({"lsn": 7}))
        .await
        .unwrap_or_else(|e| panic!("{backend}: {e}"));
    assert_eq!(
        coord.load_checkpoint(&key, "ns").await.expect("load"),
        Some((7, serde_json::json!({"lsn": 7}), 2)),
        "{backend}"
    );
}
