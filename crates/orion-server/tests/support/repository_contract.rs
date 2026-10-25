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
