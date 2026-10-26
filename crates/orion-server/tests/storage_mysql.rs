//! MySQL-as-storage integration tests.
//!
//! Boots a MySQL testcontainer and runs Orion's own migrations against it —
//! the first coverage of MySQL as Orion's storage backend (previously it was
//! only tested as an external `db_read`/`db_write` connector).
//!
//! A separate test binary, but no longer a forced one: the backend used to be
//! a process-global `OnceLock` that the integration binary pinned to SQLite,
//! and it is now carried on `DbPool` itself. Merging the per-backend binaries
//! is a test-topology change (see `tests/README.md`), not part of removing the
//! global.
//!
//! Run with: `cargo test --test storage_mysql -- --ignored`

// Tests unwrap and panic on what they did not expect; the workspace's
// lints against both are for production code.
#![allow(clippy::unwrap_used, clippy::panic)]

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

/// DLQ claim semantics on real MySQL: SKIP LOCKED tx claim + lease blocking
/// (multi-instance-ha A3).
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_mysql -- --ignored"]
async fn mysql_dlq_claim_leases_rows() {
    use orion::storage::repositories::trace_dlq::{SqlTraceDlqRepository, TraceDlqRepository};

    let (_container, pool) = mysql_pool().await;
    let repo = SqlTraceDlqRepository::new(pool.clone());

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
    use orion::storage::repositories::traces::{
        SqlTraceRepository, TraceFilter, TraceReader, TraceSink,
    };

    let (_container, pool) = mysql_pool().await;
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

#[path = "support/repository_contract.rs"]
mod contract;

/// Every repository behaviour in the shared contract, on MySQL.
///
/// The backend this matters most for: MySQL has no `RETURNING`, so
/// `archive_latest_active` runs an entirely separate implementation here, and
/// its equivalence to the single-statement form on the other two is something
/// only a shared contract can assert.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_mysql -- --ignored"]
async fn mysql_satisfies_the_repository_contract() {
    let (_container, pool) = mysql_pool().await;
    contract::run_all("mysql", &pool).await;
}

/// The cron claim under real concurrency on the backend whose locking actually
/// matters.
///
/// SQLite serialises writers by construction, so a single-winner claim there
/// proves nothing about Postgres, where two transactions can select the same
/// rows unless `FOR UPDATE SKIP LOCKED` says otherwise. This is the test that
/// makes the `skip_locked` argument in `claim_update_query` load-bearing.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_mysql -- --ignored"]
async fn mysql_cron_claim_single_winner() {
    use orion::storage::repositories::cron::{
        ClaimRequest, CronRepository, NewOccurrence, SqlCronRepository, status, trigger,
    };

    let (_container, pool) = mysql_pool().await;
    let repo = std::sync::Arc::new(SqlCronRepository::new(pool.clone()));

    let due = repo.db_now().await.expect("db now") - chrono::Duration::seconds(30);
    assert!(
        repo.insert_occurrence(NewOccurrence {
            id: "occ-1",
            channel_id: "ch",
            channel_name: "nightly",
            channel_version: 1,
            workflow_id: Some("wf"),
            trigger: trigger::CRON,
            scheduled_for: due,
            status: status::PENDING,
            error_message: None,
        })
        .await
        .expect("insert")
    );

    // Two nodes claim concurrently. Exactly one may win the single row.
    let (a, b) = tokio::join!(
        repo.claim_due(ClaimRequest {
            claimant: "node-a",
            limit: 10,
            lease_secs: 60,
        }),
        repo.claim_due(ClaimRequest {
            claimant: "node-b",
            limit: 10,
            lease_secs: 60,
        }),
    );
    let a = a.expect("claim a");
    let b = b.expect("claim b");
    assert_eq!(
        a.len() + b.len(),
        1,
        "exactly one node may claim an occurrence"
    );

    // And the identity index holds against a concurrent double-materialisation,
    // which is what lets the reconciler run on every node with no lease.
    let scheduled = due + chrono::Duration::seconds(1);
    let make = || NewOccurrence {
        id: "",
        channel_id: "ch",
        channel_name: "nightly",
        channel_version: 1,
        workflow_id: Some("wf"),
        trigger: trigger::CRON,
        scheduled_for: scheduled,
        status: status::PENDING,
        error_message: None,
    };
    let (first, second) = tokio::join!(
        repo.insert_occurrence(NewOccurrence {
            id: "occ-a",
            ..make()
        }),
        repo.insert_occurrence(NewOccurrence {
            id: "occ-b",
            ..make()
        }),
    );
    let winners = [first.expect("insert a"), second.expect("insert b")]
        .into_iter()
        .filter(|created| *created)
        .count();
    assert_eq!(
        winners, 1,
        "two reconcilers must produce exactly one occurrence per instant"
    );
}

/// Singleton acquisition across two concurrent claimants on a real backend:
/// the row is the lock, and only one occurrence may hold it.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_mysql -- --ignored"]
async fn mysql_cron_singleton_admits_one_occurrence() {
    use orion::storage::repositories::cron::{
        AttemptStart, ClaimRequest, CronRepository, NewOccurrence, SingletonRequest,
        SqlCronRepository, status, trigger,
    };

    let (_container, pool) = mysql_pool().await;
    let repo = std::sync::Arc::new(SqlCronRepository::new(pool.clone()));
    let now = repo.db_now().await.expect("db now");

    for (i, id) in ["first", "second"].iter().enumerate() {
        repo.insert_occurrence(NewOccurrence {
            id,
            channel_id: "ch",
            channel_name: "nightly",
            channel_version: 1,
            workflow_id: Some("wf"),
            trigger: trigger::CRON,
            scheduled_for: now - chrono::Duration::seconds(30 - i as i64),
            status: status::PENDING,
            error_message: None,
        })
        .await
        .expect("insert");
    }

    let claimed = repo
        .claim_due(ClaimRequest {
            claimant: "node-a",
            limit: 10,
            lease_secs: 60,
        })
        .await
        .expect("claim");
    assert_eq!(claimed.len(), 2);

    let singleton = |occurrence_id: &str| {
        let _ = occurrence_id;
        Some(SingletonRequest {
            key: "shared-key",
            holder: "node-a",
            lease_secs: 60,
            slots: 1,
        })
    };
    let first = repo
        .start_attempt(&claimed[0], "node-a", 1, singleton(&claimed[0].id), 60)
        .await
        .expect("start first");
    let second = repo
        .start_attempt(&claimed[1], "node-a", 1, singleton(&claimed[1].id), 60)
        .await
        .expect("start second");

    assert!(matches!(first, AttemptStart::Started { .. }));
    assert_eq!(
        second,
        AttemptStart::SingletonBusy,
        "a live singleton key must refuse a second occurrence"
    );
}

/// `slots` under real concurrency: six attempts race for three slots of one
/// key. The bound and slot exclusivity must hold whatever happens. At
/// REPEATABLE READ, InnoDB can abort one of two attempts that probed the same
/// absent slot row (their gap locks conflict); that attempt errors, and the
/// worker retries it when its claim expires — the same fail-safe as a single
/// lock.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_mysql -- --ignored"]
async fn mysql_cron_slots_admit_n_under_concurrency() {
    use orion::storage::repositories::cron::{
        AttemptStart, ClaimRequest, CronRepository, NewOccurrence, SingletonRequest,
        SqlCronRepository, status, trigger,
    };

    let (_container, pool) = mysql_pool().await;
    let repo = std::sync::Arc::new(SqlCronRepository::new(pool.clone()));
    let now = repo.db_now().await.expect("db now");
    for i in 0..6i64 {
        repo.insert_occurrence(NewOccurrence {
            id: &format!("occ-{i}"),
            channel_id: "ch",
            channel_name: "workers",
            channel_version: 1,
            workflow_id: Some("wf"),
            trigger: trigger::CRON,
            scheduled_for: now - chrono::Duration::seconds(60 - i),
            status: status::PENDING,
            error_message: None,
        })
        .await
        .expect("insert");
    }
    let claimed = repo
        .claim_due(ClaimRequest {
            claimant: "node-a",
            limit: 10,
            lease_secs: 60,
        })
        .await
        .expect("claim");
    assert_eq!(claimed.len(), 6);

    let attempts = claimed.into_iter().map(|occurrence| {
        let repo = repo.clone();
        tokio::spawn(async move {
            repo.start_attempt(
                &occurrence,
                "node-a",
                1,
                Some(SingletonRequest {
                    key: "workers",
                    holder: "node-a",
                    lease_secs: 60,
                    slots: 3,
                }),
                60,
            )
            .await
        })
    });
    let results: Vec<_> = futures::future::join_all(attempts)
        .await
        .into_iter()
        .map(|joined| joined.expect("join"))
        .collect();

    let mut slots = Vec::new();
    let mut errors = 0;
    for result in &results {
        match result {
            Ok(AttemptStart::Started { held: Some(held) }) => slots.push(held.slot),
            Ok(AttemptStart::SingletonBusy) => {}
            Ok(other) => panic!("unexpected outcome {other:?}"),
            Err(_) => errors += 1,
        }
    }
    slots.sort_unstable();
    let distinct = {
        let mut d = slots.clone();
        d.dedup();
        d
    };
    assert_eq!(distinct, slots, "no slot is held twice: {results:?}");
    assert!(
        slots.iter().all(|slot| *slot < 3),
        "only slots 0..3: {results:?}"
    );
    assert!(
        slots.len() <= 3,
        "never more than three at once: {results:?}"
    );
    assert!(
        slots.len() + errors <= 6 && !slots.is_empty(),
        "at least one attempt starts: {results:?}"
    );
}

/// #352 on a real backend: a cancel ends a dead holder's hold on every slot
/// it held, so the next occurrence of the key starts once the grace is over
/// rather than when the lease its node last renewed runs out.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_mysql -- --ignored"]
async fn mysql_cron_cancel_frees_a_dead_holders_slots() {
    use orion::storage::repositories::cron::{
        AttemptStart, ClaimRequest, CronRepository, NewOccurrence, SingletonRequest,
        SqlCronRepository, status, trigger,
    };

    let (_container, pool) = mysql_pool().await;
    let repo = SqlCronRepository::new(pool.clone());
    let now = repo.db_now().await.expect("db now");
    for (i, id) in ["held-0", "held-1", "waiting"].iter().enumerate() {
        repo.insert_occurrence(NewOccurrence {
            id,
            channel_id: "ch",
            channel_name: "matches",
            channel_version: 1,
            workflow_id: Some("wf"),
            trigger: trigger::CRON,
            scheduled_for: now - chrono::Duration::seconds(30 - i as i64),
            status: status::PENDING,
            error_message: None,
        })
        .await
        .expect("insert");
    }
    let claimed = repo
        .claim_due(ClaimRequest {
            claimant: "node-a",
            limit: 10,
            lease_secs: 600,
        })
        .await
        .expect("claim");
    assert_eq!(claimed.len(), 3);

    let start = |occurrence| {
        repo.start_attempt(
            occurrence,
            "node-a",
            1,
            Some(SingletonRequest {
                key: "match",
                holder: "node-a",
                lease_secs: 600,
                slots: 2,
            }),
            600,
        )
    };
    for occurrence in &claimed[..2] {
        assert!(matches!(
            start(occurrence).await.expect("start"),
            AttemptStart::Started { held: Some(_) }
        ));
    }
    assert_eq!(
        start(&claimed[2]).await.expect("start"),
        AttemptStart::SingletonBusy
    );

    for occurrence in &claimed[..2] {
        let cancelled = repo
            .cancel(&occurrence.id, "cancelled by an operator", 0)
            .await
            .expect("cancel");
        assert_eq!(cancelled.status, status::FAILED);
    }
    // Leases compare against the database clock at one-second resolution.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert!(matches!(
        start(&claimed[2]).await.expect("start"),
        AttemptStart::Started { held: Some(_) }
    ));
}

/// Run `operation` once through the built-in `org.orion.mysql` provider, as
/// a served run would: inside a run admitted for exactly that operation on
/// the connection.
async fn mysql_provider_call(
    provider: &orion::extensions::MysqlProviderBackend,
    operation: &str,
    input: serde_json::Value,
) -> Result<orion::extensions::ConnectorOutput, orion::extensions::ConnectorError> {
    use orion::extensions::{
        AdmittedRun, AuthorityGrant, BudgetLimits, ConnectionReference, DRIVER_MYSQL,
        EffectiveConnectionIdentity, ProviderBackend, ProviderRef,
        foundation::builtin_provider_digest,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::time::{Duration, Instant};

    let digest = builtin_provider_digest(DRIVER_MYSQL).expect("a built-in driver");
    let connection = ConnectionReference {
        connection_id: "orders-mysql".into(),
        revision: orion::extensions::Revision::FIRST,
        provider: ProviderRef {
            publisher: "test".to_string(),
            provider_id: DRIVER_MYSQL.into(),
            version: "2.0.0".to_string(),
            digest: digest.clone(),
        },
    };
    let identity = EffectiveConnectionIdentity {
        provider_digest: digest,
        connection_id: "orders-mysql".into(),
        revision: orion::extensions::Revision::FIRST,
        secret_hash: "sha256:none".to_string(),
    };
    let run = AdmittedRun::admit(
        "run-mysql-provider",
        1,
        Instant::now() + Duration::from_secs(60),
        BudgetLimits::default(),
        AuthorityGrant {
            allowed_operations: BTreeMap::from([(
                "orders-mysql".to_string(),
                BTreeSet::from([operation.to_string()]),
            )]),
            declared_dependencies: BTreeMap::new(),
            allowed_channels: BTreeSet::new(),
        },
    );
    let invocation = run
        .context()
        .start_invocation(connection, identity, operation, None, 1)
        .expect("invocation starts");
    let attempt = invocation.next_attempt().expect("an attempt");
    provider.invoke(&attempt, operation, input).await
}

/// The built-in MySQL provider against a real server: a write and a read
/// with the values bound to `?`, a result held to the row cap while it
/// streams, a duplicate key reported as the statement's fault (not
/// retryable), and a lock wait timeout reported as the database asking for
/// the retry.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_mysql -- --ignored"]
async fn mysql_foundation_provider_runs_bound_statements() {
    use orion::extensions::{
        DRIVER_MYSQL, ImmutableDeploymentStore, MysqlProviderBackend, RetryAdvice, SqlLimits,
        codes, foundation::builtin_provider_digest,
    };
    use serde_json::json;
    use sqlx::Connection;

    let container = Mysql::default()
        .start()
        .await
        .expect("start mysql container");
    let port = container
        .get_host_port_ipv4(3306)
        .await
        .expect("mysql port");
    let url = format!("mysql://127.0.0.1:{port}/test");

    // Sessions the provider opens wait one second for a row lock, not fifty.
    let mut admin = sqlx::MySqlConnection::connect(&format!("mysql://root@127.0.0.1:{port}/test"))
        .await
        .expect("admin connection");
    sqlx::query("SET GLOBAL innodb_lock_wait_timeout = 1")
        .execute(&mut admin)
        .await
        .expect("set lock wait timeout");

    // The revision names the server with no userinfo (sqlx connects as
    // `root`), so the public-config credential screen admits it.
    // The store's own tables live wherever the node's state database is;
    // the provider reads the revision from the store's catalog.
    let state_db = orion::storage::init_pool(&orion::config::StorageConfig {
        url: "sqlite::memory:".to_string(),
        max_connections: 1,
        ..Default::default()
    })
    .await
    .expect("state database");
    let store = ImmutableDeploymentStore::default();
    store
        .store_connection_revision_in_db(
            &state_db,
            "orders-mysql",
            1,
            &builtin_provider_digest(DRIVER_MYSQL).expect("a built-in driver"),
            json!({"connection_string": url}),
            Default::default(),
        )
        .await
        .expect("connection revision stored");
    let provider = MysqlProviderBackend::with_deployment_store(4, store).with_limits(SqlLimits {
        max_rows: 2,
        max_result_bytes: 0,
    });

    mysql_provider_call(
        &provider,
        "execute",
        json!({"sql": "CREATE TABLE items (id INT PRIMARY KEY, label VARCHAR(64) NOT NULL)"}),
    )
    .await
    .expect("create table");
    for (id, label) in [(1, "one"), (2, "x'; DROP TABLE items; --"), (3, "three")] {
        let out = mysql_provider_call(
            &provider,
            "execute",
            json!({
                "sql": "INSERT INTO items (id, label) VALUES (?, ?)",
                "params": [id, label],
            }),
        )
        .await
        .expect("insert");
        assert_eq!(out.data["rows_affected"], 1);
    }

    let out = mysql_provider_call(
        &provider,
        "query",
        json!({"sql": "SELECT id, label FROM items WHERE id = ?", "params": [2]}),
    )
    .await
    .expect("query");
    assert_eq!(out.data["count"], 1);
    assert_eq!(out.data["rows"][0]["id"], 2);
    assert_eq!(out.data["rows"][0]["label"], "x'; DROP TABLE items; --");
    assert_eq!(out.metadata["driver"], DRIVER_MYSQL);

    // Three rows against a cap of two.
    let err = mysql_provider_call(
        &provider,
        "query",
        json!({"sql": "SELECT id FROM items ORDER BY id"}),
    )
    .await
    .expect_err("the row cap holds");
    assert_eq!(err.code, codes::INVALID_INPUT);
    assert!(err.message.contains("2 rows"), "{}", err.message);

    // A duplicate key is the statement's fault: retrying will not change it.
    let err = mysql_provider_call(
        &provider,
        "execute",
        json!({"sql": "INSERT INTO items (id, label) VALUES (?, ?)", "params": [1, "again"]}),
    )
    .await
    .expect_err("duplicate key");
    assert_eq!(err.code, codes::INVALID_INPUT, "{}", err.message);
    assert_eq!(err.retry_advice, RetryAdvice::Never);
    assert!(err.message.contains("Duplicate"), "{}", err.message);

    // A row held by another transaction times the write out (error 1205):
    // the database asking for the retry.
    let mut tx = admin.begin().await.expect("begin");
    sqlx::query("SELECT id FROM items WHERE id = 1 FOR UPDATE")
        .fetch_all(&mut *tx)
        .await
        .expect("lock row");
    let err = mysql_provider_call(
        &provider,
        "execute",
        json!({"sql": "UPDATE items SET label = ? WHERE id = ?", "params": ["locked", 1]}),
    )
    .await
    .expect_err("lock wait timeout");
    assert_eq!(err.code, codes::UNAVAILABLE, "{}", err.message);
    assert_eq!(err.retry_advice, RetryAdvice::Transient);
    assert!(err.message.contains("Lock wait timeout"), "{}", err.message);
    tx.rollback().await.expect("rollback");
}
