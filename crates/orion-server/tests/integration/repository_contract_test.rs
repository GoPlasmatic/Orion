//! The SQLite driver for the shared repository contract.
//!
//! The contract itself is `tests/support/repository_contract.rs`; the Postgres
//! and MySQL drivers are in `storage_postgres.rs` and `storage_mysql.rs` and
//! need Docker. This one runs on every `cargo test`, so a contract case that
//! is simply wrong is caught before anyone starts a container.

#[path = "../support/repository_contract.rs"]
mod contract;

#[tokio::test]
async fn sqlite_satisfies_the_repository_contract() {
    let pool = orion::storage::init_pool(&orion::config::StorageConfig {
        url: "sqlite::memory:".to_string(),
        max_connections: 1,
        ..Default::default()
    })
    .await
    .expect("sqlite pool");
    contract::run_all("sqlite", &pool).await;
}
