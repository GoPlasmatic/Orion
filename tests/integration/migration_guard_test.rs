//! `storage.auto_migrate = false` startup guard (T5).
//!
//! The "pending migration ⇒ refuse to boot" check is the entire safety
//! mechanism behind the Helm pre-upgrade Job and the compose `migrate`
//! service, and nothing referenced `auto_migrate` from `tests/`. The check
//! used to be inline in `main()`; it now lives in
//! `orion::storage::init_pool_for_startup` so it is reachable without running
//! the binary. `main()` calls exactly that function.

use std::path::Path;

use orion::config::StorageConfig;
use sea_query::Values;
use sea_query_binder::SqlxValues;

use crate::common::ScratchDir;

fn no_values() -> SqlxValues {
    SqlxValues(Values(Vec::new()))
}

fn storage_config(dir: &Path, auto_migrate: bool) -> StorageConfig {
    StorageConfig {
        url: format!("sqlite:{}/orion.db", dir.display()),
        max_connections: 1,
        auto_migrate,
        ..Default::default()
    }
}

/// Bring the database to the N-1 state: fully migrated, then the newest
/// migration retracted from `_sqlx_migrations` so the guard sees it pending.
async fn database_one_migration_behind(dir: &Path) {
    let pool = orion::storage::init_pool(&storage_config(dir, true))
        .await
        .expect("apply all migrations");
    assert!(
        orion::storage::pending_migrations(&pool)
            .await
            .expect("pending")
            .is_empty(),
        "fixture should start fully migrated"
    );

    pool.execute_query(
        "DELETE FROM _sqlx_migrations \
         WHERE version = (SELECT MAX(version) FROM _sqlx_migrations)",
        no_values(),
    )
    .await
    .expect("retract newest migration");

    assert_eq!(
        orion::storage::pending_migrations(&pool)
            .await
            .expect("pending")
            .len(),
        1,
        "fixture should now be exactly one migration behind"
    );
    drop(pool);
}

#[tokio::test]
async fn test_auto_migrate_false_refuses_to_boot_one_migration_behind() {
    let dir = ScratchDir::new("behind");
    database_one_migration_behind(dir.path()).await;

    let err = orion::storage::init_pool_for_startup(&storage_config(dir.path(), false))
        .await
        .err()
        .expect("startup must fail against a stale schema");

    let msg = err.to_string();
    assert!(
        msg.contains("1 pending migration(s)"),
        "error should name the pending count, got: {msg}"
    );
    assert!(
        msg.contains("storage.auto_migrate = false"),
        "error should name the setting responsible, got: {msg}"
    );
    assert!(
        msg.contains("orion-server migrate"),
        "error should point at the deploy step, got: {msg}"
    );
}

#[tokio::test]
async fn test_auto_migrate_false_refuses_to_boot_on_an_empty_database() {
    let dir = ScratchDir::new("empty");

    let err = orion::storage::init_pool_for_startup(&storage_config(dir.path(), false))
        .await
        .err()
        .expect("startup must fail against an unmigrated database");
    assert!(
        err.to_string().contains("pending migration(s)"),
        "got: {err}"
    );
}

#[tokio::test]
async fn test_auto_migrate_false_boots_when_the_schema_is_current() {
    let dir = ScratchDir::new("current");
    let pool = orion::storage::init_pool(&storage_config(dir.path(), true))
        .await
        .expect("apply all migrations");
    drop(pool);

    let pool = orion::storage::init_pool_for_startup(&storage_config(dir.path(), false))
        .await
        .expect("a current schema must boot with auto_migrate = false");
    assert!(
        orion::storage::pending_migrations(&pool)
            .await
            .expect("pending")
            .is_empty()
    );
    drop(pool);
}

#[tokio::test]
async fn test_auto_migrate_true_applies_migrations_instead_of_failing() {
    let dir = ScratchDir::new("auto");

    let pool = orion::storage::init_pool_for_startup(&storage_config(dir.path(), true))
        .await
        .expect("auto_migrate = true must migrate an empty database");
    assert!(
        orion::storage::pending_migrations(&pool)
            .await
            .expect("pending")
            .is_empty(),
        "auto_migrate = true must leave nothing pending"
    );
    drop(pool);
}
