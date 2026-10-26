//! The repository behaviours that must hold on **every** backend, written once.
//!
//! Before this, each backend binary spelled out its own round-trip: SQLite in
//! the unit tests under `orion-storage/src/repositories/`, Postgres in
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
    concurrent_applies_on_one_base_commit_exactly_once(backend, pool).await;
    a_package_admission_connection_and_revocation_read_back(backend, pool).await;
    a_collection_racing_an_apply_never_takes_what_it_deploys(backend, pool).await;
    the_artifact_collector_judges_its_grace_on_the_database_clock(backend, pool).await;
    source_leases_are_won_once_and_judged_on_the_database_clock(backend, pool).await;
    every_update_moves_updated_at(backend, pool).await;
}

/// A deployment store manifest of one workflow member over `digest`.
fn one_workflow_manifest(id: &str, digest: &str) -> orion::test_support::CanonicalManifest {
    use orion::test_support::{CanonicalManifest, EntityKind, EntityRef, ManifestMember};
    CanonicalManifest {
        schema_version: 2,
        providers: Vec::new(),
        provider_lock_digest: None,
        members: vec![ManifestMember {
            entity: EntityRef {
                kind: EntityKind::Workflow,
                id: id.to_string(),
                revision: orion_extension_api::Revision::FIRST,
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
    use orion::storage::repositories::audit_logs::{
        AuditEvent, AuditLogFilter, AuditLogRepository, SqlAuditLogRepository,
    };
    use orion::test_support::Rollback;
    use orion::test_support::{Admission, DeploymentReceipt, ImmutableDeploymentStore};

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
    let policy = Admission::community();
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
    assert_eq!(lost.code(), "DEPLOYMENT_HEAD_CONFLICT", "{backend}");
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

/// Applies on one base from several nodes at once: exactly one commits and
/// every other loses the head CAS with `DEPLOYMENT_HEAD_CONFLICT` — never a
/// storage error, never a second revision on the same base. Then the
/// winner's request replayed from several nodes at once answers its receipt
/// every time and commits nothing more.
async fn concurrent_applies_on_one_base_commit_exactly_once(backend: &str, pool: &DbPool) {
    use orion::test_support::{Admission, ImmutableDeploymentStore};

    const NODES: usize = 6;
    let policy = Admission::community();
    let seed = ImmutableDeploymentStore::new("contract-cc-seed");
    seed.load_from_db_pool(pool).await.expect("load");
    let base = seed.current_head().0;

    let mut nodes = Vec::new();
    for i in 0..NODES {
        let node = ImmutableDeploymentStore::new(format!("contract-cc-{i}"));
        let def = node
            .store_definition_artifact_in_db(pool, format!("contract-cc-wf-{i}").into_bytes())
            .await
            .expect("store definition");
        let plan = node
            .plan_candidate_in_db(
                pool,
                base,
                one_workflow_manifest("contract-cc", &def.digest),
            )
            .await
            .expect("plan");
        nodes.push((node, plan));
    }
    let results = futures::future::join_all(nodes.iter().enumerate().map(|(i, (node, plan))| {
        let key = format!("cc-{i}");
        let policy = &policy;
        async move {
            node.apply_candidate_in_db(pool, "contract", &key, plan, "r", policy)
                .await
        }
    }))
    .await;
    let winners: Vec<usize> = results
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.is_ok().then_some(i))
        .collect();
    assert_eq!(winners.len(), 1, "{backend}: {results:?}");
    for r in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(r.code(), "DEPLOYMENT_HEAD_CONFLICT", "{backend}: {r:?}");
    }
    let winner = winners[0];
    let committed = results[winner].clone().expect("the winner's receipt");
    assert_eq!(committed.committed_revision, base + 1, "{backend}");
    assert!(committed.created_at.is_some(), "{backend}");
    let persisted = ImmutableDeploymentStore::persisted_head(pool)
        .await
        .expect("head")
        .expect("a head");
    assert_eq!(persisted.0, base + 1, "{backend}: one revision on the base");

    // The winner's request, replayed from fresh nodes at once.
    let (_, plan) = &nodes[winner];
    let key = format!("cc-{winner}");
    let replays = futures::future::join_all((0..NODES).map(|i| {
        let node = ImmutableDeploymentStore::new(format!("contract-cc-replay-{i}"));
        let (key, policy) = (&key, &policy);
        async move {
            node.apply_candidate_in_db(pool, "contract", key, plan, "r", policy)
                .await
        }
    }))
    .await;
    for replay in replays {
        let replay = replay.unwrap_or_else(|e| panic!("{backend}: a replay failed: {e}"));
        assert_eq!(
            replay.committed_revision, committed.committed_revision,
            "{backend}"
        );
        assert_eq!(replay.operation_id, committed.operation_id, "{backend}");
        assert_eq!(replay.created_at, committed.created_at, "{backend}");
    }
    assert_eq!(
        ImmutableDeploymentStore::persisted_head(pool)
            .await
            .expect("head"),
        Some(persisted),
        "{backend}: a replay commits nothing"
    );
}

/// The fixture package's archive entries (`tests/fixtures/v2/package-identity`).
fn fixture_package() -> Vec<orion::test_support::ArchiveEntryInput> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/v2/package-identity/bundle.json");
    let bundle: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir).expect("fixture")).expect("json");
    let pkg = &bundle["providers"][1];
    let mut entries: Vec<_> = pkg["payloads"]
        .as_object()
        .expect("payloads")
        .iter()
        .map(|(path, text)| {
            let bytes = text.as_str().expect("text").as_bytes().to_vec();
            orion::test_support::ArchiveEntryInput {
                path: path.clone(),
                compressed_size: bytes.len(),
                bytes,
                is_symlink: false,
            }
        })
        .collect();
    let inventory = orion::test_support::canonical_bytes(&pkg["inventory"]).expect("inventory");
    entries.push(orion::test_support::ArchiveEntryInput {
        path: "inventory.json".to_string(),
        compressed_size: inventory.len(),
        bytes: inventory,
        is_symlink: false,
    });
    entries
}

/// Every statement a package's life writes, on this backend: the upload
/// (idempotent, and refused for a different digest under the same
/// identity), the admission, a connection revision with secret locators and
/// bindings, an apply naming both, and the revocation — each read back by a
/// fresh node as it was written.
async fn a_package_admission_connection_and_revocation_read_back(backend: &str, pool: &DbPool) {
    use orion::test_support::{
        Admission, CanonicalManifest, EntityKind, EntityRef, ImmutableDeploymentStore,
        ManifestMember,
    };
    use orion_extension_api::ProviderRef;

    let policy = Admission::community();
    let node = ImmutableDeploymentStore::new("contract-pkg");
    node.load_from_db_pool(pool).await.expect("load");
    let pkg = node
        .ingest_verified_archive_in_db(pool, &fixture_package())
        .await
        .expect("ingest");
    let again = node
        .ingest_verified_archive_in_db(pool, &fixture_package())
        .await
        .expect("an identical upload is idempotent");
    assert_eq!(again, pkg, "{backend}");
    node.admit_package_in_db(pool, &pkg.package_digest, &policy, None)
        .await
        .expect("admit");
    let conn = node
        .store_connection_revision_with_bindings_in_db(
            pool,
            "contract-pkg-conn",
            1,
            &pkg.package_digest,
            serde_json::json!({"url": "https://api.example.com", "retries": 2, "tls": true}),
            [("auth.token".to_string(), "env://CONTRACT_TOKEN".to_string())].into(),
            [("crm".to_string(), "contract-crm".to_string())].into(),
        )
        .await
        .expect("connection");
    let def = node
        .store_definition_artifact_in_db(pool, b"contract-pkg-wf".to_vec())
        .await
        .expect("definition");
    let member = |kind, id: &str, digest: &str| ManifestMember {
        entity: EntityRef {
            kind,
            id: id.to_string(),
            revision: orion_extension_api::Revision::FIRST,
        },
        definition_digest: digest.to_string(),
        owning_package: None,
        requires: Vec::new(),
        rollouts: Vec::new(),
    };
    let manifest = CanonicalManifest {
        schema_version: 2,
        providers: vec![ProviderRef {
            publisher: pkg.publisher.clone(),
            provider_id: pkg.provider_id.clone(),
            version: pkg.version.clone(),
            digest: pkg.package_digest.clone(),
        }],
        provider_lock_digest: None,
        members: vec![
            member(EntityKind::Workflow, "contract-pkg-wf", &def.digest),
            member(
                EntityKind::Connection,
                "contract-pkg-conn",
                &conn.configuration_digest,
            ),
        ],
    };
    let plan = node
        .plan_candidate_in_db(pool, node.current_head().0, manifest)
        .await
        .expect("plan");
    let receipt = node
        .apply_candidate_in_db(pool, "contract", "pkg-1", &plan, "r", &policy)
        .await
        .expect("apply");

    let fresh = ImmutableDeploymentStore::new("contract-pkg-reader");
    fresh.load_from_db_pool(pool).await.expect("load");
    let stored = fresh.get_package(&pkg.package_digest).expect("package");
    assert_eq!(*stored, pkg, "{backend}: the package reads back whole");
    assert!(
        matches!(fresh.package_schemas(&pkg.package_digest), Some(Ok(_))),
        "{backend}"
    );
    assert_eq!(
        *fresh
            .get_connection("contract-pkg-conn", orion_extension_api::Revision::FIRST)
            .expect("connection"),
        conn,
        "{backend}: the connection reads back whole"
    );
    let admission = fresh.get_admission(&pkg.package_digest).expect("admission");
    assert!(!admission.revoked, "{backend}");
    let revision = fresh
        .get_revision(receipt.committed_revision)
        .expect("revision");
    assert!(
        revision.created_at.is_some(),
        "{backend}: the commit time reads back"
    );

    let before = fresh.current_head();
    node.revoke_package_in_db(pool, &pkg.package_digest)
        .await
        .expect("revoke");
    fresh.load_from_db_pool(pool).await.expect("load");
    assert!(
        fresh
            .get_admission(&pkg.package_digest)
            .expect("admission")
            .revoked,
        "{backend}"
    );
    assert_eq!(
        fresh.current_head(),
        (before.0, before.1 + 1),
        "{backend}: the epoch moved"
    );
    let err = node
        .revoke_package_in_db(
            pool,
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        )
        .await
        .expect_err("no admission");
    assert_eq!(err.code(), "ADMISSION_NOT_FOUND", "{backend}");
}

/// A collection pass and an apply racing over an unreferenced definition
/// outside its grace window: they serialise on the head, so either the apply
/// commits first and the pass leaves the definition its head now names, or
/// the pass drains it first and the apply is refused. Never a committed head
/// naming a draining definition.
async fn a_collection_racing_an_apply_never_takes_what_it_deploys(backend: &str, pool: &DbPool) {
    use orion::test_support::{Admission, ArtifactState, ImmutableDeploymentStore};

    let policy = Admission::community();
    for round in 0..4 {
        let deployer = ImmutableDeploymentStore::new(format!("contract-race-a-{round}"));
        let collector = ImmutableDeploymentStore::new(format!("contract-race-b-{round}"));
        deployer.load_from_db_pool(pool).await.expect("load");
        let def = deployer
            .store_definition_artifact_in_db(pool, format!("contract-race-{round}").into_bytes())
            .await
            .expect("store");
        backdate_artifact(pool, &def.digest).await;
        let plan = deployer
            .plan_candidate_in_db(
                pool,
                deployer.current_head().0,
                one_workflow_manifest("contract-race", &def.digest),
            )
            .await
            .expect("plan");
        let key = format!("race-{round}");
        let (applied, collected) = futures::future::join(
            deployer.apply_candidate_in_db(pool, "contract", &key, &plan, "r", &policy),
            collector.mark_and_collect_garbage_in_db(pool, 1, 3600),
        )
        .await;
        let collected = collected.expect("collect");
        let state = deployer
            .fetch_artifact(pool, &def.digest)
            .await
            .expect("read")
            .expect("stored")
            .state;
        match applied {
            Ok(_) => {
                assert!(
                    !collected.draining.contains(&def.digest),
                    "{backend}: {collected:?}"
                );
                assert_eq!(state, ArtifactState::Active, "{backend}");
            }
            Err(e) => {
                assert_eq!(e.code(), "ARTIFACT_DRAINING_OR_DELETED", "{backend}: {e:?}");
                assert!(collected.draining.contains(&def.digest), "{backend}");
                assert_eq!(state, ArtifactState::Draining, "{backend}");
            }
        }
    }
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
/// the next; what the head names is never touched; and any node uploading
/// the bytes again revives the deleted row, because the upload is judged
/// against the database row, not against anything a node holds.
async fn the_artifact_collector_judges_its_grace_on_the_database_clock(
    backend: &str,
    pool: &DbPool,
) {
    use orion::test_support::{Admission, ArtifactState, ImmutableDeploymentStore};

    let policy = Admission::community();
    let node_a = ImmutableDeploymentStore::new("contract-gc-a");
    let node_b = ImmutableDeploymentStore::new("contract-gc-b");
    node_a.load_from_db_pool(pool).await.expect("load");
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
    let reader = &node_b;
    let state = |digest: String| async move {
        reader
            .fetch_artifact(pool, &digest)
            .await
            .expect("read artifact")
            .map(|a| (a.state, a.bytes.is_some()))
    };
    assert_eq!(
        state(orphan.clone()).await,
        Some((ArtifactState::Active, true)),
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

    assert_eq!(
        state(orphan.clone()).await,
        Some((ArtifactState::Deleted, false)),
        "{backend}: the tombstone is kept and the bytes are gone"
    );
    assert_eq!(
        state(deployed.digest.clone()).await,
        Some((ArtifactState::Active, true)),
        "{backend}"
    );

    // Another node uploading the bytes again revives the database row.
    node_b
        .store_definition_artifact_in_db(pool, b"contract-gc-orphan".to_vec())
        .await
        .expect("re-upload");
    assert_eq!(
        state(orphan.clone()).await,
        Some((ArtifactState::Active, true)),
        "{backend}: revived"
    );
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
    use orion::test_support::{SourceLeaseCoordinator, SourcePartitionKey};
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

/// Long before any write this test makes.
const BACKDATED: &str = "2000-01-01 00:00:00";

/// Set `updated_at` on the row of `table` whose `key_col` is `key` back to
/// [`BACKDATED`], through a statement that sets nothing else.
async fn backdate(pool: &DbPool, table: &str, key_col: &str, key: &str) {
    use sea_query::{Alias, Expr, ExprTrait, Query};
    let (sql, values) = orion::storage::build_sqlx(
        pool.backend(),
        Query::update()
            .table(Alias::new(table))
            .value(
                Alias::new("updated_at"),
                Expr::cust(format!("'{BACKDATED}'")),
            )
            .and_where(Expr::col(Alias::new(key_col)).eq(key)),
    );
    pool.execute_query(&sql, values)
        .await
        .unwrap_or_else(|e| panic!("backdate {table}: {e}"));
}

async fn updated_at(pool: &DbPool, table: &str, key_col: &str, key: &str) -> chrono::NaiveDateTime {
    use sea_query::{Alias, Expr, ExprTrait, Query};
    let (sql, values) = orion::storage::build_sqlx(
        pool.backend(),
        Query::select()
            .column(Alias::new("updated_at"))
            .from(Alias::new(table))
            .and_where(Expr::col(Alias::new(key_col)).eq(key)),
    );
    let (at,): (chrono::NaiveDateTime,) = pool
        .fetch_one_as(&sql, values)
        .await
        .unwrap_or_else(|e| panic!("read {table}.updated_at: {e}"));
    at
}

/// Assert `write` moved the row's `updated_at` off [`BACKDATED`]: the
/// statement sets it, since no trigger does any more
/// (`003_drop_updated_at_triggers`).
async fn assert_moves<F: std::future::Future<Output = ()>>(
    backend: &str,
    pool: &DbPool,
    (table, key_col, key): (&str, &str, &str),
    what: &str,
    write: F,
) {
    backdate(pool, table, key_col, key).await;
    let before = updated_at(pool, table, key_col, key).await;
    write.await;
    let after = updated_at(pool, table, key_col, key).await;
    assert!(
        after > before,
        "{backend}: {what} must move {table}.updated_at (was {before}, now {after})"
    );
}

/// Every UPDATE on the tables that carry an `updated_at` without a trigger
/// behind it — traces, trace_dlq and the four cron tables — sets the column
/// itself.
async fn every_update_moves_updated_at(backend: &str, pool: &DbPool) {
    use orion::storage::repositories::cron::{
        AttemptStart, ClaimRequest, CronRepository, NewOccurrence, ScheduleCursor, Settlement,
        SingletonRequest, SqlCronRepository, status, trigger,
    };
    use orion::storage::repositories::trace_dlq::{
        DlqEnqueue, SqlTraceDlqRepository, TraceDlqRepository,
    };
    use orion::storage::repositories::traces::{SqlTraceRepository, TraceCompletion, TraceSink};

    // traces: the status write and the one-statement completion.
    let traces = SqlTraceRepository::new(pool.clone());
    let trace = traces
        .create_pending("ua-ch", None, "async", Some("{}"), None)
        .await
        .expect("create trace");
    let row = ("traces", "id", trace.id.as_str());
    assert_moves(backend, pool, row, "update_status", async {
        traces
            .update_status(&trace.id, "running", None)
            .await
            .expect("update_status");
    })
    .await;
    assert_moves(backend, pool, row, "complete", async {
        traces
            .complete(
                &trace.id,
                TraceCompletion {
                    result_json: r#"{"ok":true}"#,
                    duration_ms: 1.0,
                    task_trace_json: None,
                    status: "completed",
                    error_message: None,
                },
            )
            .await
            .expect("complete");
    })
    .await;
    let done = orion::storage::repositories::traces::TraceReader::get_by_id(&traces, &trace.id)
        .await
        .expect("read trace");
    assert_eq!(done.status, "completed", "{backend}");
    assert_eq!(
        done.result_json.as_deref(),
        Some(r#"{"ok":true}"#),
        "{backend}"
    );
    assert!(
        done.completed_at.is_some(),
        "{backend}: complete sets completed_at"
    );

    // trace_dlq: every UPDATE the retry loop and the admin API make.
    let dlq = SqlTraceDlqRepository::new(pool.clone());
    let entry = dlq
        .enqueue(DlqEnqueue {
            trace_id: "ua-trace",
            channel: "ua-ch",
            payload_json: "{}",
            metadata_json: "{}",
            error_message: "boom",
            retry_count: 0,
            max_retries: 5,
        })
        .await
        .expect("enqueue");
    let row = ("trace_dlq", "id", entry.id.as_str());
    assert_moves(backend, pool, row, "requeue", async {
        let returned = dlq.requeue(&entry.id).await.expect("requeue");
        assert!(
            returned.updated_at.and_utc().timestamp() > 946_684_800,
            "{backend}: the row requeue returns carries the new updated_at, not the old one"
        );
    })
    .await;
    // Due an hour ago, so the claim below finds it whatever the clocks say.
    assert_moves(backend, pool, row, "record_retry", async {
        dlq.record_retry(
            &entry.id,
            chrono::Utc::now().naive_utc() - chrono::Duration::hours(1),
        )
        .await
        .expect("record_retry");
    })
    .await;
    assert_moves(backend, pool, row, "claim_pending", async {
        let claimed = dlq.claim_pending("ua-node", 100, 60).await.expect("claim");
        assert!(
            claimed.iter().any(|e| e.id == entry.id),
            "{backend}: the entry is due"
        );
    })
    .await;
    assert_moves(backend, pool, row, "mark_exhausted", async {
        dlq.mark_exhausted(&entry.id).await.expect("mark_exhausted");
    })
    .await;

    // cron_schedule_state: the cursor's three UPDATEs.
    let cron = SqlCronRepository::new(pool.clone());
    let now = cron.db_now().await.expect("db now");
    let cursor = |next_fire_at| ScheduleCursor {
        channel_id: "ua-cron",
        channel_version: 1,
        config_hash: "h",
        next_fire_at,
    };
    cron.upsert_cursor(cursor(now))
        .await
        .expect("insert cursor");
    let row = ("cron_schedule_state", "channel_id", "ua-cron");
    let later = now + chrono::Duration::minutes(5);
    assert_moves(backend, pool, row, "upsert_cursor (update)", async {
        cron.upsert_cursor(cursor(later))
            .await
            .expect("update cursor");
    })
    .await;
    assert_moves(backend, pool, row, "advance_cursor", async {
        let advanced = cron
            .advance_cursor("ua-cron", later, later + chrono::Duration::minutes(5))
            .await
            .expect("advance");
        assert!(advanced, "{backend}: the cursor holds the planned value");
    })
    .await;
    assert_moves(backend, pool, row, "pause_cursor", async {
        cron.pause_cursor("ua-cron").await.expect("pause");
    })
    .await;

    // cron_occurrences, cron_singletons and cron_singleton_slots: a claim,
    // two attempts holding slots 0 and 1 of one key, their renewals, and a
    // settle.
    for (i, id) in ["ua-occ-0", "ua-occ-1"].iter().enumerate() {
        cron.insert_occurrence(NewOccurrence {
            id,
            channel_id: "ua-cron",
            channel_name: "ua",
            channel_version: 1,
            workflow_id: Some("wf"),
            trigger: trigger::CRON,
            scheduled_for: now - chrono::Duration::seconds(60 - i as i64),
            status: status::PENDING,
            error_message: None,
        })
        .await
        .expect("insert occurrence");
    }
    let mut claimed = Vec::new();
    assert_moves(
        backend,
        pool,
        ("cron_occurrences", "id", "ua-occ-0"),
        "claim_due",
        async {
            claimed = cron
                .claim_due(ClaimRequest {
                    claimant: "ua-node",
                    limit: 100,
                    lease_secs: 60,
                })
                .await
                .expect("claim");
        },
    )
    .await;
    let mut held = Vec::new();
    for id in ["ua-occ-0", "ua-occ-1"] {
        let occurrence = claimed
            .iter()
            .find(|o| o.id == id)
            .unwrap_or_else(|| panic!("{backend}: {id} was claimed"));
        let mut started = None;
        assert_moves(
            backend,
            pool,
            ("cron_occurrences", "id", id),
            "start_attempt",
            async {
                started = Some(
                    cron.start_attempt(
                        occurrence,
                        "ua-node",
                        1,
                        Some(SingletonRequest {
                            key: "ua-key",
                            holder: "ua-node",
                            lease_secs: 60,
                            slots: 2,
                        }),
                        60,
                    )
                    .await
                    .expect("start"),
                );
            },
        )
        .await;
        match started {
            Some(AttemptStart::Started { held: Some(slot) }) => held.push((id, slot)),
            other => panic!("{backend}: {id} should start in a slot, got {other:?}"),
        }
    }
    for (id, slot) in &held {
        // Slot 0 is the `cron_singletons` row; the key's only other slot is
        // its one `cron_singleton_slots` row.
        let table = if slot.slot == 0 {
            "cron_singletons"
        } else {
            "cron_singleton_slots"
        };
        let (key_col, key) = ("singleton_key", "ua-key");
        backdate(pool, "cron_occurrences", "id", id).await;
        assert_moves(backend, pool, (table, key_col, key), "renew", async {
            assert!(
                cron.renew(id, "ua-node", Some(*slot), 60)
                    .await
                    .expect("renew"),
                "{backend}: {id} still owns its attempt"
            );
        })
        .await;
        assert!(
            updated_at(pool, "cron_occurrences", "id", id)
                .await
                .and_utc()
                .timestamp()
                > 946_684_800,
            "{backend}: renew must move cron_occurrences.updated_at too"
        );
    }
    let slots: Vec<u32> = held.iter().map(|(_, s)| s.slot).collect();
    assert!(
        slots.contains(&0) && slots.contains(&1),
        "{backend}: both singleton tables were exercised: {slots:?}"
    );
    assert_moves(
        backend,
        pool,
        ("cron_occurrences", "id", "ua-occ-0"),
        "settle",
        async {
            assert!(
                cron.settle(Settlement {
                    occurrence_id: "ua-occ-0",
                    claimant: "ua-node",
                    status: status::COMPLETED,
                    error_message: None,
                    trace_id: None,
                })
                .await
                .expect("settle"),
                "{backend}: the claimant settles its own occurrence"
            );
        },
    )
    .await;
}
