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
use orion::storage::models::EntityStatus;
use orion::storage::repositories::Repositories;
use orion::storage::repositories::channels::CreateChannelRequest;

/// Run every contract case against `pool`. `backend` names the backend in
/// failure messages, so a red CI log says which one diverged without needing
/// the test name decoded.
pub async fn run_all(backend: &str, pool: &DbPool) {
    let repos = Repositories::new(pool, &orion::config::StorageConfig::default())
        .expect("repositories must build on every backend");

    archive_takes_every_active_version(backend, &repos).await;
    delete_removes_every_version(backend, &repos).await;
    an_audited_write_commits_both_rows(backend, &repos).await;
    a_dropped_audited_write_rolls_back(backend, &repos).await;
    the_current_version_read_returns_the_newest(backend, &repos).await;
    a_bump_stamps_the_scope_with_its_own_epoch(backend, pool).await;
    a_live_write_commits_its_publication(backend, pool, &repos).await;
    a_read_snapshot_is_repeatable(backend, pool).await;
    the_shared_store_claims_once_and_counts_every_increment(backend, pool).await;
    a_shared_rate_window_holds_and_spills_at_most_2x(backend, pool).await;
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

async fn a_live_write_commits_its_publication(backend: &str, pool: &DbPool, repos: &Repositories) {
    use orion::cluster::EpochScope;
    use orion::storage::repositories::cluster::{ClusterRepository, SqlClusterRepository};
    let epochs = SqlClusterRepository::new(pool.clone());
    repos
        .channels
        .create(&channel_request("contract-publication"))
        .await
        .expect("draft");
    repos
        .channels
        .activate("contract-publication")
        .await
        .expect("activate");
    let before = epochs.get_epoch().await.expect("epoch").epoch;
    let mut write = repos
        .audited(audit_event("archive", "contract-publication"))
        .await
        .expect("write");
    repos
        .channels
        .archive_tx(write.tx(), "contract-publication")
        .await
        .expect("archive");
    let committed = write
        .commit_published(Some(EpochScope::Definitions))
        .await
        .expect("commit");
    let row = epochs.get_epoch().await.expect("epoch");
    assert_eq!(
        committed,
        Some(before + 1),
        "{backend}: transaction returns its epoch"
    );
    assert_eq!(
        row.epoch,
        before + 1,
        "{backend}: no post-commit sender is required"
    );
    assert_eq!(
        row.epoch_scope_at, row.epoch,
        "{backend}: scope and epoch agree"
    );
    assert_eq!(row.epoch_scope, "definitions", "{backend}: scope survives");
    assert_eq!(
        repos
            .channels
            .get_by_id("contract-publication")
            .await
            .expect("row")
            .status,
        "archived"
    );
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
        .fetch_all_as("SELECT epoch FROM config_epoch WHERE id = 1", empty())
        .await
        .expect("first read");
    let new_epoch = epochs
        .bump_epoch("definitions")
        .await
        .expect("concurrent commit");
    let pinned: Vec<(i64,)> = snapshot
        .fetch_all_as("SELECT epoch FROM config_epoch WHERE id = 1", empty())
        .await
        .expect("second read");
    assert_eq!(
        first, pinned,
        "{backend}: definition reads share one snapshot"
    );
    snapshot.commit().await.expect("release snapshot");
    assert_eq!(
        new_epoch,
        first[0].0 + 1,
        "{backend}: the writer actually committed"
    );
}

/// A config-epoch bump stamps `epoch_scope_at` with the epoch it produced.
///
/// This is here rather than in a SQLite unit test because the two assignments
/// share one `UPDATE` and the backends disagree about what a right-hand side
/// means there: MySQL evaluates a multi-column `SET` left to right and lets a
/// later assignment read a column the same statement has already updated,
/// while SQLite and PostgreSQL evaluate every right-hand side against the old
/// row. `bump_epoch` writes `epoch_scope_at` before `epoch` so that `epoch + 1`
/// is the same new value on all three — and only a cross-backend case can say
/// so, because on SQLite alone the two orders are indistinguishable.
///
/// The stamp is what lets a reader tell this change's scope from one left
/// behind by an earlier bump: `epoch_scope` is sticky, so a node running a
/// release that predates it advances the counter and leaves a *recognised*
/// scope standing over an epoch it says nothing about.
async fn a_bump_stamps_the_scope_with_its_own_epoch(backend: &str, pool: &DbPool) {
    use orion::cluster::EpochScope;
    use orion::storage::repositories::cluster::{ClusterRepository, SqlClusterRepository};

    let repo = SqlClusterRepository::new(pool.clone());
    let before = repo.get_epoch().await.expect("get_epoch").epoch;
    let bumped = repo.bump_epoch("connectors").await.expect("bump_epoch");
    let row = repo.get_epoch().await.expect("get_epoch");

    assert_eq!(
        row.epoch,
        before + 1,
        "{backend}: the bump must advance once"
    );
    assert_eq!(
        row.epoch, bumped,
        "{backend}: the read-back must match what the bump returned"
    );
    assert_eq!(row.epoch_scope, "connectors", "{backend}: scope round-trip");
    assert_eq!(
        row.epoch_scope_at, row.epoch,
        "{backend}: the scope must be stamped with the epoch it was written \
         for — a value one either side of it means the SET evaluated a \
         right-hand side against the wrong row"
    );
    assert_eq!(
        EpochScope::for_advance(before, row.epoch, row.epoch_scope_at, &row.epoch_scope),
        EpochScope::Connectors,
        "{backend}: a freshly stamped scope must be trusted by a peer one \
         epoch behind, or every bump costs the wide resync the scope exists \
         to avoid"
    );
}

fn channel_request(id: &str) -> CreateChannelRequest {
    serde_json::from_value(serde_json::json!({
        "channel_id": id,
        "name": id,
        "channel_type": "sync",
        "protocol": "rest",
        "route_pattern": format!("/{id}"),
        "methods": ["POST"],
    }))
    .expect("channel request")
}

/// A channel at `version` versions, the newest active, the rest archived.
async fn seed_versions(repos: &Repositories, id: &str, versions: usize) {
    repos
        .channels
        .create(&channel_request(id))
        .await
        .expect("create");
    repos.channels.activate(id).await.expect("activate");
    for _ in 1..versions {
        repos
            .channels
            .create_new_version(id)
            .await
            .expect("new version");
        repos.channels.activate(id).await.expect("activate");
    }
}

/// MySQL runs a different implementation of this than the other two (no
/// `RETURNING`), so it is the case most worth running everywhere.
async fn archive_takes_every_active_version(backend: &str, repos: &Repositories) {
    seed_versions(repos, "contract-archive", 2).await;

    let archived = repos
        .channels
        .archive("contract-archive")
        .await
        .unwrap_or_else(|e| panic!("{backend}: archive failed: {e}"));
    assert_eq!(
        archived.status,
        EntityStatus::Archived.as_str(),
        "{backend}: archive must return the archived row"
    );

    let versions = repos
        .channels
        .list_versions(
            "contract-archive",
            &orion::storage::repositories::helpers::VersionFilter::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("{backend}: list_versions failed: {e}"));
    assert!(
        versions
            .data
            .iter()
            .all(|c| c.status != EntityStatus::Active.as_str()),
        "{backend}: no version may still be active after an archive: {:?}",
        versions
            .data
            .iter()
            .map(|c| (c.version, c.status.clone()))
            .collect::<Vec<_>>()
    );

    assert!(
        repos.channels.archive("contract-archive").await.is_err(),
        "{backend}: archiving with nothing active must be an error"
    );
}

async fn delete_removes_every_version(backend: &str, repos: &Repositories) {
    seed_versions(repos, "contract-delete", 3).await;

    repos
        .channels
        .delete("contract-delete")
        .await
        .unwrap_or_else(|e| panic!("{backend}: delete failed: {e}"));
    assert!(
        repos.channels.get_by_id("contract-delete").await.is_err(),
        "{backend}: every version must be gone after a delete"
    );
    assert!(
        repos.channels.delete("contract-delete").await.is_err(),
        "{backend}: deleting an absent id must be NotFound, not a silent success"
    );
}

fn audit_event(action: &str, resource_id: &str) -> orion::queue::audit_queue::AuditEvent {
    orion::queue::audit_queue::AuditEvent {
        principal: "contract".to_string(),
        action: action.to_string(),
        resource_type: "channel".to_string(),
        resource_id: resource_id.to_string(),
        details: None,
    }
}

async fn audit_total(repos: &Repositories) -> i64 {
    repos
        .audit_logs
        .list_paginated(&orion::storage::repositories::audit_logs::AuditLogFilter::default())
        .await
        .expect("list audit logs")
        .total
}

/// §2.6 across backends. The guarantee rests on transaction semantics, which
/// is precisely the thing that differs between SQLite, Postgres and MySQL —
/// MySQL in particular has non-transactional DDL and its own locking rules.
async fn an_audited_write_commits_both_rows(backend: &str, repos: &Repositories) {
    seed_versions(repos, "contract-audited", 1).await;
    let before = audit_total(repos).await;

    let mut write = repos
        .audited(audit_event("status_archived", "contract-audited"))
        .await
        .unwrap_or_else(|e| panic!("{backend}: begin audited write failed: {e}"));
    repos
        .channels
        .archive_tx(write.tx(), "contract-audited")
        .await
        .unwrap_or_else(|e| panic!("{backend}: archive_tx failed: {e}"));
    write
        .commit()
        .await
        .unwrap_or_else(|e| panic!("{backend}: commit failed: {e}"));

    assert_eq!(
        audit_total(repos).await,
        before + 1,
        "{backend}: the audit row must commit with the change"
    );
    assert_eq!(
        repos
            .channels
            .get_by_id("contract-audited")
            .await
            .expect("read back")
            .status,
        EntityStatus::Archived.as_str(),
        "{backend}: the change must have committed too"
    );
}

async fn a_dropped_audited_write_rolls_back(backend: &str, repos: &Repositories) {
    seed_versions(repos, "contract-rollback", 1).await;
    let before = audit_total(repos).await;

    {
        let mut write = repos
            .audited(audit_event("delete", "contract-rollback"))
            .await
            .unwrap_or_else(|e| panic!("{backend}: begin audited write failed: {e}"));
        repos
            .channels
            .delete_tx(write.tx(), "contract-rollback")
            .await
            .unwrap_or_else(|e| panic!("{backend}: delete_tx failed: {e}"));
    }

    assert!(
        repos.channels.get_by_id("contract-rollback").await.is_ok(),
        "{backend}: an uncommitted audited write must roll the entity write back"
    );
    assert_eq!(
        audit_total(repos).await,
        before,
        "{backend}: and must leave no audit row"
    );
}

/// §5: the `current_*` reads are a correlated `MAX(version)` sub-select now
/// rather than a view. Correlated subqueries are the shape most likely to
/// differ between planners, so the "latest version per id" answer is asserted
/// on each backend rather than inferred from SQLite.
async fn the_current_version_read_returns_the_newest(backend: &str, repos: &Repositories) {
    seed_versions(repos, "contract-current", 3).await;
    // A second entity, so a predicate that lost its correlation (and matched
    // the table-wide maximum) would return the wrong row rather than no rows.
    seed_versions(repos, "contract-current-other", 1).await;

    let listed = repos
        .channels
        .list_paginated(&orion::storage::repositories::channels::ChannelFilter {
            limit: Some(1000),
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("{backend}: list_paginated failed: {e}"));

    let current = listed
        .data
        .iter()
        .find(|c| c.channel_id == "contract-current")
        .unwrap_or_else(|| panic!("{backend}: the current listing must include the channel"));
    assert_eq!(
        current.version, 3,
        "{backend}: the current listing must serve the newest version per id"
    );
    assert_eq!(
        listed
            .data
            .iter()
            .filter(|c| c.channel_id == "contract-current")
            .count(),
        1,
        "{backend}: exactly one row per id"
    );
    assert!(
        listed
            .data
            .iter()
            .any(|c| c.channel_id == "contract-current-other" && c.version == 1),
        "{backend}: an entity at version 1 must survive alongside one at version 3"
    );

    // The workflow side reads through the same predicate.
    assert!(
        repos
            .workflows
            .list_paginated(&Default::default())
            .await
            .is_ok(),
        "{backend}: the workflow current listing must run"
    );
}
