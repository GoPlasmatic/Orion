//! Postgres-as-storage integration tests.
//!
//! Boots a Postgres testcontainer and runs Orion's own migrations against it.
//!
//! A separate test binary by choice, not necessity: the backend is carried on
//! `DbPool`, so these could be modules of the integration binary. Merging them
//! is a test-topology change (see `tests/README.md`).
//!
//! Run with: `cargo test --test storage_postgres -- --ignored`

// Tests unwrap and panic on what they did not expect; the workspace's
// lints against both are for production code.
#![expect(clippy::unwrap_used, clippy::panic)]

use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

use orion::config::StorageConfig;
use orion::storage::DbPool;

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

/// D8 keyset pagination, on the backend whose `timestamp` type the cursor
/// actually has to bind against.
///
/// The SQLite tests in `orion-storage/src/repositories/traces.rs` compare a
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

#[path = "support/repository_contract.rs"]
mod contract;

/// Every repository behaviour in the shared contract, on Postgres.
///
/// The contract is written once (`tests/support/repository_contract.rs`) and
/// driven from here, from `storage_mysql.rs`, and from the integration binary
/// for SQLite — so a case added for one backend is asserted on all three
/// rather than becoming a fourth hand-written round-trip.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_postgres -- --ignored"]
async fn postgres_satisfies_the_repository_contract() {
    let (_container, pool) = postgres_pool().await;
    contract::run_all("postgres", &pool).await;
}

/// The cron claim under real concurrency on the backend whose locking actually
/// matters.
///
/// SQLite serialises writers by construction, so a single-winner claim there
/// proves nothing about Postgres, where two transactions can select the same
/// rows unless `FOR UPDATE SKIP LOCKED` says otherwise. This is the test that
/// makes the `skip_locked` argument in `claim_update_query` load-bearing.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_postgres -- --ignored"]
async fn postgres_cron_claim_single_winner() {
    use orion::storage::repositories::cron::{
        ClaimRequest, CronRepository, NewOccurrence, SqlCronRepository, status, trigger,
    };

    let (_container, pool) = postgres_pool().await;
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
#[ignore = "needs Docker; run with: cargo test --test storage_postgres -- --ignored"]
async fn postgres_cron_singleton_admits_one_occurrence() {
    use orion::storage::repositories::cron::{
        AttemptStart, ClaimRequest, CronRepository, NewOccurrence, SingletonRequest,
        SqlCronRepository, status, trigger,
    };

    let (_container, pool) = postgres_pool().await;
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
/// key, and exactly three start — each in its own slot — with no error. A
/// waiting attempt holds no slot while it waits, so there is no lock cycle
/// to break.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_postgres -- --ignored"]
async fn postgres_cron_slots_admit_n_under_concurrency() {
    use orion::storage::repositories::cron::{
        AttemptStart, ClaimRequest, CronRepository, NewOccurrence, SingletonRequest,
        SqlCronRepository, status, trigger,
    };

    let (_container, pool) = postgres_pool().await;
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
    assert_eq!(errors, 0, "no attempt may fail: {results:?}");
    assert_eq!(slots.len(), 3, "exactly three of six start: {results:?}");
}

/// #352 on a real backend: a cancel ends a dead holder's hold on every slot
/// it held, so the next occurrence of the key starts once the grace is over
/// rather than when the lease its node last renewed runs out.
#[tokio::test]
#[ignore = "needs Docker; run with: cargo test --test storage_postgres -- --ignored"]
async fn postgres_cron_cancel_frees_a_dead_holders_slots() {
    use orion::storage::repositories::cron::{
        AttemptStart, ClaimRequest, CronRepository, NewOccurrence, SingletonRequest,
        SqlCronRepository, status, trigger,
    };

    let (_container, pool) = postgres_pool().await;
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
