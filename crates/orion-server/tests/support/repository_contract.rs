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
use orion::storage::repositories::packages::PutPackageReceiptRequest;

/// Run every contract case against `pool`. `backend` names the backend in
/// failure messages, so a red CI log says which one diverged without needing
/// the test name decoded.
pub async fn run_all(backend: &str, pool: &DbPool) {
    let repos = Repositories::new(pool, &orion::config::StorageConfig::default())
        .expect("repositories must build on every backend");

    archive_takes_every_active_version(backend, &repos).await;
    delete_removes_every_version(backend, &repos).await;
    applied_receipts_are_immutable(backend, &repos).await;
    an_audited_write_commits_both_rows(backend, &repos).await;
    a_dropped_audited_write_rolls_back(backend, &repos).await;
    the_current_version_read_returns_the_newest(backend, &repos).await;
    a_bump_stamps_the_scope_with_its_own_epoch(backend, pool).await;
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
        EpochScope::for_epoch(row.epoch, row.epoch_scope_at, &row.epoch_scope),
        EpochScope::Connectors,
        "{backend}: a freshly stamped scope must be trusted, or every bump \
         costs the wide resync the scope exists to avoid"
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

/// The receipt immutability rule is enforced by a predicate carried on the
/// write, so it is a concurrency contract as much as a logical one — worth
/// asserting against each backend's own comparison semantics.
async fn applied_receipts_are_immutable(backend: &str, repos: &Repositories) {
    let put = |version: &str, hash: &str, state| PutPackageReceiptRequest {
        version: version.to_string(),
        content_hash: hash.to_string(),
        state,
    };
    use orion::storage::models::PackageState;

    repos
        .packages
        .put(
            "contract-pkg",
            &put("1.0.0", "sha256:aaa", PackageState::Applied),
            "contract",
        )
        .await
        .unwrap_or_else(|e| panic!("{backend}: apply failed: {e}"));

    repos
        .packages
        .put(
            "contract-pkg",
            &put("1.0.0", "sha256:aaa", PackageState::Applied),
            "contract",
        )
        .await
        .unwrap_or_else(|e| panic!("{backend}: re-applying identical content must succeed: {e}"));

    assert!(
        repos
            .packages
            .put(
                "contract-pkg",
                &put("1.0.0", "sha256:zzz", PackageState::Applied),
                "contract",
            )
            .await
            .is_err(),
        "{backend}: an applied version's content must be immutable"
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
