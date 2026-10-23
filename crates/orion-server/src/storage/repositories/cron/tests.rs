use super::*;
use chrono::Duration;

async fn test_repo() -> SqlCronRepository {
    SqlCronRepository::new(crate::storage::test_sqlite_pool().await)
}

fn occurrence<'a>(id: &'a str, channel_id: &'a str, at: NaiveDateTime) -> NewOccurrence<'a> {
    NewOccurrence {
        id,
        channel_id,
        channel_name: "nightly",
        channel_version: 1,
        workflow_id: Some("wf"),
        trigger: trigger::CRON,
        scheduled_for: at,
        status: status::PENDING,
        error_message: None,
    }
}

/// Materialise one occurrence due in the past, and hand back its id.
async fn seed_due(repo: &SqlCronRepository, id: &str, channel_id: &str) -> NaiveDateTime {
    let due = repo.db_now().await.expect("db now") - Duration::seconds(30);
    assert!(
        repo.insert_occurrence(occurrence(id, channel_id, due))
            .await
            .expect("insert")
    );
    due
}

/// Expire a lease by writing it into the past.
///
/// A zero-second lease is **not** expired: `sql_now` is `datetime('now')`
/// on SQLite, which has one-second granularity, so a lease written at
/// `now` compares equal to `now` for the rest of that second and
/// `claimed_until < now` is false. Production leases are tens of seconds so
/// this never bites there, but a test that wants "already expired" has to
/// say so explicitly rather than race the clock's resolution.
async fn expire_claim(repo: &SqlCronRepository, id: &str) {
    let past = repo.db_now().await.expect("db now") - Duration::hours(1);
    let (sql, values) = build_sqlx(
        repo.pool.backend(),
        Query::update()
            .table(CronOccurrences::Table)
            .value(CronOccurrences::ClaimedUntil, past)
            .and_where(Expr::col(CronOccurrences::Id).eq(id)),
    );
    repo.pool.execute_query(&sql, values).await.expect("expire");
}

async fn expire_singleton(repo: &SqlCronRepository, key: &str) {
    let past = repo.db_now().await.expect("db now") - Duration::hours(1);
    let (sql, values) = build_sqlx(
        repo.pool.backend(),
        Query::update()
            .table(CronSingletons::Table)
            .value(CronSingletons::LeaseUntil, past)
            .and_where(Expr::col(CronSingletons::SingletonKey).eq(key)),
    );
    repo.pool.execute_query(&sql, values).await.expect("expire");
}

/// Age a row's `created_at`, for the retention tests — same clock-resolution
/// reason as [`expire_claim`].
async fn age_row(repo: &SqlCronRepository, id: &str, by: Duration) {
    let then = repo.db_now().await.expect("db now") - by;
    let (sql, values) = build_sqlx(
        repo.pool.backend(),
        Query::update()
            .table(CronOccurrences::Table)
            .value(CronOccurrences::CreatedAt, then)
            .and_where(Expr::col(CronOccurrences::Id).eq(id)),
    );
    repo.pool.execute_query(&sql, values).await.expect("age");
}

fn claim(claimant: &str, lease_secs: u64) -> ClaimRequest<'_> {
    ClaimRequest {
        claimant,
        limit: 10,
        lease_secs,
    }
}

// ---- materialisation ----

/// The property the whole reconciler rests on: the identity is
/// `(channel_id, scheduled_for)`, so a retried tick or a second reconciler
/// cannot produce a second row — even though the caller mints a fresh id
/// each time.
#[tokio::test]
async fn materialisation_is_idempotent_on_the_scheduled_instant() {
    let repo = test_repo().await;
    let at = repo.db_now().await.expect("db now");

    assert!(
        repo.insert_occurrence(occurrence("a", "ch", at))
            .await
            .expect("first")
    );
    assert!(
        !repo
            .insert_occurrence(occurrence("b", "ch", at))
            .await
            .expect("second"),
        "a second id for the same instant must lose"
    );

    let page = repo
        .list_paginated(&CronOccurrenceFilter::default())
        .await
        .expect("list");
    assert_eq!(page.total, 1);
    assert_eq!(page.data[0].id, "a");

    // A different instant, and a different channel at the same instant, are
    // both distinct occurrences.
    assert!(
        repo.insert_occurrence(occurrence("c", "ch", at + Duration::seconds(1)))
            .await
            .expect("next instant")
    );
    assert!(
        repo.insert_occurrence(occurrence("d", "other", at))
            .await
            .expect("other channel")
    );
}

// ---- the cursor ----

#[tokio::test]
async fn a_cursor_advances_only_from_the_value_it_was_planned_from() {
    let repo = test_repo().await;
    let t0 = repo.db_now().await.expect("db now");
    let t1 = t0 + Duration::hours(1);
    let t2 = t0 + Duration::hours(2);

    repo.upsert_cursor(ScheduleCursor {
        channel_id: "ch",
        channel_version: 1,
        config_hash: "hash",
        next_fire_at: t0,
    })
    .await
    .expect("upsert");

    // The winner moves it…
    assert!(repo.advance_cursor("ch", t0, t1).await.expect("advance"));
    // …and a reconciler that planned from the old value writes nothing,
    // rather than dragging the cursor backwards and replaying a pass.
    assert!(!repo.advance_cursor("ch", t0, t2).await.expect("stale"));

    let states = repo.schedule_states().await.expect("states");
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].next_fire_at, t1);
}

/// A schedule edit resets the cursor and clears a pause; the two are one
/// write so a reactivated channel cannot come back still paused.
#[tokio::test]
async fn upserting_a_cursor_clears_a_pause() {
    let repo = test_repo().await;
    let t0 = repo.db_now().await.expect("db now");
    let cursor = ScheduleCursor {
        channel_id: "ch",
        channel_version: 1,
        config_hash: "hash",
        next_fire_at: t0,
    };
    repo.upsert_cursor(cursor.clone()).await.expect("upsert");
    repo.pause_cursor("ch").await.expect("pause");
    assert!(
        repo.schedule_states().await.expect("states")[0]
            .paused_at
            .is_some()
    );

    repo.upsert_cursor(ScheduleCursor {
        channel_version: 2,
        config_hash: "other",
        next_fire_at: t0 + Duration::hours(3),
        ..cursor
    })
    .await
    .expect("re-upsert");
    let state = repo.schedule_states().await.expect("states").remove(0);
    assert!(
        state.paused_at.is_none(),
        "reactivation must clear the pause"
    );
    assert_eq!(state.channel_version, 2);
    assert_eq!(state.config_hash, "other");
}

// ---- claiming ----

#[tokio::test]
async fn a_claim_leases_the_row_and_blocks_a_second_claimant() {
    let repo = test_repo().await;
    seed_due(&repo, "occ", "ch").await;

    let first = repo.claim_due(claim("node-a", 60)).await.expect("claim");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].status, status::CLAIMED);
    assert_eq!(first[0].claimed_by.as_deref(), Some("node-a"));
    // The attempt counter moves on the claim, so it counts attempts even
    // when a node dies before settling one.
    assert_eq!(first[0].attempt, 1);

    assert!(
        repo.claim_due(claim("node-b", 60))
            .await
            .expect("claim")
            .is_empty(),
        "a live lease must not be claimable"
    );
}

/// Crash recovery: the lease is the only thing standing between a dead
/// node's work and a peer picking it up.
#[tokio::test]
async fn an_expired_claim_is_reclaimable_and_a_live_one_is_not() {
    let repo = test_repo().await;
    seed_due(&repo, "occ", "ch").await;

    repo.claim_due(claim("node-a", 60)).await.expect("claim");
    expire_claim(&repo, "occ").await;
    let reclaimed = repo.claim_due(claim("node-b", 60)).await.expect("reclaim");
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].claimed_by.as_deref(), Some("node-b"));
    assert_eq!(reclaimed[0].attempt, 2, "the takeover is a second attempt");

    // And now that node-b holds a live lease, nobody else may.
    assert!(
        repo.claim_due(claim("node-c", 60))
            .await
            .expect("claim")
            .is_empty()
    );
}

/// An occurrence whose time has not come is not work yet.
#[tokio::test]
async fn an_occurrence_due_in_the_future_is_not_claimed() {
    let repo = test_repo().await;
    let later = repo.db_now().await.expect("db now") + Duration::hours(1);
    repo.insert_occurrence(occurrence("occ", "ch", later))
        .await
        .expect("insert");
    assert!(
        repo.claim_due(claim("node-a", 60))
            .await
            .expect("claim")
            .is_empty()
    );
}

#[tokio::test]
async fn the_claim_batch_is_bounded_and_oldest_first() {
    let repo = test_repo().await;
    let now = repo.db_now().await.expect("db now");
    for i in 0..5 {
        repo.insert_occurrence(occurrence(
            &format!("occ-{i}"),
            "ch",
            now - Duration::minutes(10 - i),
        ))
        .await
        .expect("insert");
    }
    let claimed = repo
        .claim_due(ClaimRequest {
            claimant: "node-a",
            limit: 2,
            lease_secs: 60,
        })
        .await
        .expect("claim");
    assert_eq!(claimed.len(), 2);
    // Oldest first: a backlog drains in the order the work was due.
    assert_eq!(claimed[0].id, "occ-0");
    assert_eq!(claimed[1].id, "occ-1");
}

// ---- the singleton ----

async fn start(
    repo: &SqlCronRepository,
    occurrence: &CronOccurrence,
    claimant: &str,
    key: Option<&str>,
) -> AttemptStart {
    start_slots(repo, occurrence, claimant, key, 1).await
}

async fn start_slots(
    repo: &SqlCronRepository,
    occurrence: &CronOccurrence,
    claimant: &str,
    key: Option<&str>,
    slots: u32,
) -> AttemptStart {
    repo.start_attempt(
        occurrence,
        claimant,
        1,
        key.map(|key| SingletonRequest {
            key,
            holder: claimant,
            lease_secs: 60,
            slots,
        }),
        60,
    )
    .await
    .expect("start")
}

fn held(slot: u32, fencing_token: i64) -> AttemptStart {
    AttemptStart::Started {
        held: Some(HeldSlot {
            slot,
            fencing_token,
        }),
    }
}

/// The non-overlap guarantee, at its narrowest: two occurrences, one key,
/// and only one of them may be running.
#[tokio::test]
async fn one_singleton_key_admits_one_occurrence() {
    let repo = test_repo().await;
    let now = repo.db_now().await.expect("db now");
    for (i, id) in ["first", "second"].iter().enumerate() {
        repo.insert_occurrence(occurrence(id, "ch", now - Duration::minutes(5 - i as i64)))
            .await
            .expect("insert");
    }
    let claimed = repo.claim_due(claim("node-a", 60)).await.expect("claim");
    assert_eq!(claimed.len(), 2);

    assert_eq!(
        start(&repo, &claimed[0], "node-a", Some("key")).await,
        held(0, 1)
    );
    assert_eq!(
        start(&repo, &claimed[1], "node-a", Some("key")).await,
        AttemptStart::SingletonBusy,
        "a live key must refuse the second occurrence"
    );
}

#[tokio::test]
async fn distinct_keys_do_not_contend() {
    let repo = test_repo().await;
    let now = repo.db_now().await.expect("db now");
    for (i, id) in ["first", "second"].iter().enumerate() {
        repo.insert_occurrence(occurrence(id, "ch", now - Duration::minutes(5 - i as i64)))
            .await
            .expect("insert");
    }
    let claimed = repo.claim_due(claim("node-a", 60)).await.expect("claim");
    for (occurrence, key) in claimed.iter().zip(["key-a", "key-b"]) {
        assert!(matches!(
            start(&repo, occurrence, "node-a", Some(key)).await,
            AttemptStart::Started { .. }
        ));
    }
}

/// `allow` takes no row at all, so two occurrences of the same channel run
/// side by side.
#[tokio::test]
async fn without_a_singleton_occurrences_overlap() {
    let repo = test_repo().await;
    let now = repo.db_now().await.expect("db now");
    for (i, id) in ["first", "second"].iter().enumerate() {
        repo.insert_occurrence(occurrence(id, "ch", now - Duration::minutes(5 - i as i64)))
            .await
            .expect("insert");
    }
    let claimed = repo.claim_due(claim("node-a", 60)).await.expect("claim");
    for occurrence in &claimed {
        assert_eq!(
            start(&repo, occurrence, "node-a", None).await,
            AttemptStart::Started { held: None }
        );
    }
}

/// The takeover path, and the reason the token exists: the new holder gets
/// a higher generation, and the old one's conditional writes stop matching.
#[tokio::test]
async fn taking_over_an_expired_key_bumps_the_fencing_token() {
    let repo = test_repo().await;
    let now = repo.db_now().await.expect("db now");
    for (i, id) in ["first", "second"].iter().enumerate() {
        repo.insert_occurrence(occurrence(id, "ch", now - Duration::minutes(5 - i as i64)))
            .await
            .expect("insert");
    }
    let claimed = repo.claim_due(claim("node-a", 60)).await.expect("claim");

    assert_eq!(
        start(&repo, &claimed[0], "node-a", Some("key")).await,
        held(0, 1)
    );

    // node-a dies. Both its occurrence claim and its singleton lease run
    // out, and node-b picks the second occurrence up.
    expire_singleton(&repo, "key").await;
    expire_claim(&repo, &claimed[1].id).await;
    let taken_over = repo
        .claim_due(claim("node-b", 60))
        .await
        .expect("reclaim")
        .remove(0);
    assert_eq!(taken_over.id, claimed[1].id);

    assert_eq!(
        start(&repo, &taken_over, "node-b", Some("key")).await,
        held(0, 2),
        "an expired key is takeable, under a higher generation"
    );

    // The superseded holder cannot renew, and cannot release the row the
    // new holder owns — which is what stops a third occurrence slipping in
    // alongside it.
    assert!(
        !repo
            .renew(&claimed[0].id, "node-a", Some(slot0(1)), 60)
            .await
            .expect("renew"),
        "a superseded holder must not renew"
    );
    assert!(
        !repo
            .release_singleton("key", &claimed[0].id, slot0(1))
            .await
            .expect("release"),
        "a superseded holder must not delete the new holder's row"
    );
    assert!(
        repo.release_singleton("key", &taken_over.id, slot0(2))
            .await
            .expect("release"),
        "the real holder releases its own row"
    );
}

/// An occurrence another node took over is not one this node may start.
#[tokio::test]
async fn starting_an_occurrence_this_node_no_longer_holds_is_lost() {
    let repo = test_repo().await;
    seed_due(&repo, "occ", "ch").await;
    let mine = repo
        .claim_due(claim("node-a", 60))
        .await
        .expect("claim")
        .remove(0);
    // This node stalls long enough to lose its lease, and someone else
    // takes the occurrence over.
    expire_claim(&repo, "occ").await;
    repo.claim_due(claim("node-b", 60)).await.expect("reclaim");

    assert_eq!(
        start(&repo, &mine, "node-a", None).await,
        AttemptStart::Lost,
        "a stale claimant must write nothing"
    );
}

/// The heartbeat's mechanism: a renewal moves both deadlines forward, so a
/// run outlasts the lease it started under.
///
/// Both halves are conditional on ownership, which is the other thing this
/// pins: the renewal names the occurrence, the holder and the token, so a
/// superseded node's beat extends nothing.
#[tokio::test]
async fn a_renewal_extends_both_the_claim_and_the_singleton() {
    let repo = test_repo().await;
    seed_due(&repo, "occ", "ch").await;
    let claimed = repo
        .claim_due(claim("node-a", 60))
        .await
        .expect("claim")
        .remove(0);
    let token = match start(&repo, &claimed, "node-a", Some("key")).await {
        AttemptStart::Started { held } => held.expect("a slot"),
        other => unreachable!("the attempt must start, got {other:?}"),
    };

    // Age both deadlines into the past, as a stalled node would leave them.
    expire_claim(&repo, "occ").await;
    expire_singleton(&repo, "key").await;
    let before = repo.get_by_id("occ").await.expect("get");
    assert!(before.claimed_until.expect("a lease") < repo.db_now().await.expect("now"));

    assert!(
        repo.renew("occ", "node-a", Some(token), 600)
            .await
            .expect("renew"),
        "the holder must be able to extend its own lease"
    );
    let after = repo.get_by_id("occ").await.expect("get");
    assert!(
        after.claimed_until.expect("a lease") > repo.db_now().await.expect("now"),
        "a renewal must move the claim deadline into the future"
    );
    // And the occurrence is no longer reclaimable, which is the point.
    assert!(
        repo.claim_due(claim("node-b", 60))
            .await
            .expect("claim")
            .is_empty(),
        "a renewed occurrence must not be takeable"
    );

    // A wrong token renews nothing, however live the claim is.
    assert!(
        !repo
            .renew(
                "occ",
                "node-a",
                Some(HeldSlot {
                    fencing_token: token.fencing_token + 1,
                    ..token
                }),
                600
            )
            .await
            .expect("renew"),
        "a superseded token must extend nothing"
    );
}

// ---- slots ----

fn slot0(fencing_token: i64) -> HeldSlot {
    HeldSlot {
        slot: 0,
        fencing_token,
    }
}

/// `n` occurrences due and claimed by `node-a`, oldest first.
async fn claimed_n(repo: &SqlCronRepository, n: usize) -> Vec<CronOccurrence> {
    let now = repo.db_now().await.expect("db now");
    for i in 0..n {
        let id = format!("occ-{i}");
        repo.insert_occurrence(occurrence(
            &id,
            "ch",
            now - Duration::minutes(10 - i as i64),
        ))
        .await
        .expect("insert");
    }
    let claimed = repo.claim_due(claim("node-a", 60)).await.expect("claim");
    assert_eq!(claimed.len(), n);
    claimed
}

async fn expire_slot(repo: &SqlCronRepository, key: &str, slot: u32) {
    let past = repo.db_now().await.expect("db now") - Duration::hours(1);
    let (sql, values) = build_sqlx(
        repo.pool.backend(),
        Query::update()
            .table(CronSingletonSlots::Table)
            .value(CronSingletonSlots::LeaseUntil, past)
            .and_where(Expr::col(CronSingletonSlots::SingletonKey).eq(key))
            .and_where(Expr::col(CronSingletonSlots::Slot).eq(i64::from(slot))),
    );
    repo.pool.execute_query(&sql, values).await.expect("expire");
}

async fn count_rows(repo: &SqlCronRepository, table: &str) -> i64 {
    let (sql, values) = build_sqlx(
        repo.pool.backend(),
        Query::select()
            .expr(Expr::cust("COUNT(*)"))
            .from(sea_query::Alias::new(table)),
    );
    let (count,): (i64,) = repo.pool.fetch_one_as(&sql, values).await.expect("count");
    count
}

/// The lease on each singleton row an occurrence holds, soonest first.
async fn slot_leases(repo: &SqlCronRepository, occurrence_id: &str) -> Vec<NaiveDateTime> {
    let mut leases = Vec::new();
    for (table, column) in [
        ("cron_singletons", "occurrence_id"),
        ("cron_singleton_slots", "occurrence_id"),
    ] {
        let (sql, values) = build_sqlx(
            repo.pool.backend(),
            Query::select()
                .column(sea_query::Alias::new("lease_until"))
                .from(sea_query::Alias::new(table))
                .and_where(Expr::col(sea_query::Alias::new(column)).eq(occurrence_id)),
        );
        let rows: Vec<(NaiveDateTime,)> =
            repo.pool.fetch_all_as(&sql, values).await.expect("leases");
        leases.extend(rows.into_iter().map(|(lease,)| lease));
    }
    leases.sort();
    leases
}

/// #352: a node that died mid-run held its slots until the lease it last
/// renewed ran out, and nothing could release them sooner. A cancel ends
/// the hold at the grace, whichever slot it is in.
#[tokio::test]
async fn a_cancel_frees_a_dead_holders_slots_after_the_grace() {
    let repo = test_repo().await;
    let claimed = claimed_n(&repo, 3).await;
    for (slot, occurrence) in claimed[..2].iter().enumerate() {
        assert_eq!(
            start_slots(&repo, occurrence, "node-a", Some("match"), 2).await,
            held(slot as u32, 1)
        );
    }
    assert_eq!(
        start_slots(&repo, &claimed[2], "node-a", Some("match"), 2).await,
        AttemptStart::SingletonBusy
    );

    let now = repo.db_now().await.expect("db now");
    for occurrence in &claimed[..2] {
        let cancelled = repo
            .cancel(&occurrence.id, "cancelled by an operator", 0)
            .await
            .expect("cancel");
        assert_eq!(cancelled.status, status::FAILED);
        assert_eq!(cancelled.claimed_by, None);
        assert_eq!(
            cancelled.error_message.as_deref(),
            Some("cancelled by an operator")
        );
        let leases = slot_leases(&repo, &occurrence.id).await;
        assert_eq!(leases.len(), 1, "one slot per occurrence");
        assert!(leases[0] <= now + Duration::seconds(1), "{leases:?}");
    }

    // SQLite's clock has one-second granularity, and a lease ending at
    // `now` is live for the rest of that second.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    // `SingletonBusy` rolled back without settling, so its claim stands
    // and the attempt can start again.
    let started = start_slots(&repo, &claimed[2], "node-a", Some("match"), 2).await;
    assert!(
        matches!(started, AttemptStart::Started { held: Some(_) }),
        "{started:?}"
    );
}

/// A live holder is fenced rather than raced: its heartbeat and its settle
/// both match nothing, so it stops at its next beat and writes nothing.
#[tokio::test]
async fn a_cancelled_holder_can_neither_renew_nor_settle() {
    let repo = test_repo().await;
    let occurrence = claimed_n(&repo, 1).await.remove(0);
    let held = match start(&repo, &occurrence, "node-a", Some("match")).await {
        AttemptStart::Started { held } => held,
        _ => None,
    }
    .expect("must start holding a slot");

    repo.cancel(&occurrence.id, "cancelled", 30)
        .await
        .expect("cancel");

    assert!(
        !repo
            .renew(&occurrence.id, "node-a", Some(held), 60)
            .await
            .expect("renew")
    );
    assert!(
        !repo
            .settle(Settlement {
                occurrence_id: &occurrence.id,
                claimant: "node-a",
                status: status::COMPLETED,
                error_message: None,
                trace_id: None,
            })
            .await
            .expect("settle")
    );
    assert_eq!(
        repo.get_by_id(&occurrence.id).await.expect("get").status,
        status::FAILED,
        "the cancel stands"
    );
    // Its cleanup still releases the slot it held.
    assert!(
        repo.release_singleton("match", &occurrence.id, held)
            .await
            .expect("release")
    );
}

#[tokio::test]
async fn a_finished_occurrence_cannot_be_cancelled() {
    let repo = test_repo().await;
    let occurrence = claimed_n(&repo, 1).await.remove(0);
    start(&repo, &occurrence, "node-a", None).await;
    repo.settle(Settlement {
        occurrence_id: &occurrence.id,
        claimant: "node-a",
        status: status::COMPLETED,
        error_message: None,
        trace_id: None,
    })
    .await
    .expect("settle");

    assert!(matches!(
        repo.cancel(&occurrence.id, "x", 30).await,
        Err(OrionError::Conflict(_))
    ));
    assert!(matches!(
        repo.cancel("no-such-occurrence", "x", 30).await,
        Err(OrionError::NotFound(_))
    ));
}

/// A pending occurrence holds nothing, so a cancel only settles it, and a
/// cancelled occurrence is retried like any failed one.
#[tokio::test]
async fn a_cancelled_occurrence_can_be_retried() {
    let repo = test_repo().await;
    seed_due(&repo, "occ-p", "ch").await;
    let cancelled = repo
        .cancel("occ-p", "not tonight", 30)
        .await
        .expect("cancel");
    assert_eq!(cancelled.status, status::FAILED);
    assert_eq!(count_rows(&repo, "cron_singletons").await, 0);

    let retried = repo.requeue("occ-p").await.expect("requeue");
    assert_eq!(retried.status, status::PENDING);
    assert_eq!(retried.error_message, None);
}

/// The bound, at its narrowest: `n` slots admit `n` occurrences, each in
/// its own slot, and refuse the next.
#[tokio::test]
async fn n_slots_admit_n_occurrences_and_refuse_the_next() {
    let repo = test_repo().await;
    let claimed = claimed_n(&repo, 4).await;
    for (slot, occurrence) in claimed[..3].iter().enumerate() {
        assert_eq!(
            start_slots(&repo, occurrence, "node-a", Some("worker"), 3).await,
            held(slot as u32, 1)
        );
    }
    assert_eq!(
        start_slots(&repo, &claimed[3], "node-a", Some("worker"), 3).await,
        AttemptStart::SingletonBusy,
        "every slot is live, so the fourth must be refused"
    );
    let row = repo.get_by_id(&claimed[2].id).await.expect("get");
    assert_eq!(row.singleton_slot, Some(2), "the ledger names the slot");
    assert_eq!(row.fencing_token, Some(1));
}

/// Slot 0 is the row a single lock always used, so `slots = 1` — and every
/// binary that predates slots — contends on the same row.
#[tokio::test]
async fn slot_zero_is_the_legacy_row() {
    let repo = test_repo().await;
    let claimed = claimed_n(&repo, 2).await;
    assert_eq!(
        start(&repo, &claimed[0], "node-a", Some("worker")).await,
        held(0, 1)
    );
    assert_eq!(count_rows(&repo, "cron_singletons").await, 1);
    assert_eq!(count_rows(&repo, "cron_singleton_slots").await, 0);
    assert_eq!(
        start_slots(&repo, &claimed[1], "node-a", Some("worker"), 2).await,
        held(1, 1)
    );
    assert_eq!(count_rows(&repo, "cron_singleton_slots").await, 1);
}

#[tokio::test]
async fn a_released_slot_is_reused_lowest_first() {
    let repo = test_repo().await;
    let claimed = claimed_n(&repo, 4).await;
    for occurrence in &claimed[..3] {
        start_slots(&repo, occurrence, "node-a", Some("worker"), 3).await;
    }
    let one = HeldSlot {
        slot: 1,
        fencing_token: 1,
    };
    assert!(
        repo.release_singleton("worker", &claimed[1].id, one)
            .await
            .expect("release")
    );
    assert_eq!(
        start_slots(&repo, &claimed[3], "node-a", Some("worker"), 3).await,
        held(1, 1),
        "the freed slot is the lowest one free"
    );
}

/// Takeover works per slot, under the slot's own token.
#[tokio::test]
async fn an_expired_slot_is_taken_over_and_its_token_bumped() {
    let repo = test_repo().await;
    let claimed = claimed_n(&repo, 3).await;
    for occurrence in &claimed[..2] {
        start_slots(&repo, occurrence, "node-a", Some("worker"), 2).await;
    }
    expire_slot(&repo, "worker", 1).await;
    assert_eq!(
        start_slots(&repo, &claimed[2], "node-a", Some("worker"), 2).await,
        held(1, 2),
        "an expired slot is takeable, under a higher generation"
    );
    let superseded = HeldSlot {
        slot: 1,
        fencing_token: 1,
    };
    assert!(
        !repo
            .renew(&claimed[1].id, "node-a", Some(superseded), 60)
            .await
            .expect("renew"),
        "a superseded holder must not renew"
    );
    assert!(
        !repo
            .release_singleton("worker", &claimed[1].id, superseded)
            .await
            .expect("release"),
        "a superseded holder must not delete the new holder's row"
    );
}

/// A renewal and a release name one slot, and touch no other.
#[tokio::test]
async fn renew_and_release_address_the_held_slot_only() {
    let repo = test_repo().await;
    let claimed = claimed_n(&repo, 2).await;
    for occurrence in &claimed {
        start_slots(&repo, occurrence, "node-a", Some("worker"), 2).await;
    }
    let wrong_slot = HeldSlot {
        slot: 0,
        fencing_token: 1,
    };
    assert!(
        !repo
            .renew(&claimed[1].id, "node-a", Some(wrong_slot), 60)
            .await
            .expect("renew"),
        "slot 0 is the other occurrence's"
    );
    assert!(
        !repo
            .release_singleton("worker", &claimed[1].id, wrong_slot)
            .await
            .expect("release")
    );
    let right = HeldSlot {
        slot: 1,
        fencing_token: 1,
    };
    assert!(
        repo.renew(&claimed[1].id, "node-a", Some(right), 60)
            .await
            .expect("renew")
    );
    assert!(
        repo.release_singleton("worker", &claimed[1].id, right)
            .await
            .expect("release")
    );
    assert_eq!(
        repo.singleton_holds(&["worker"]).await.expect("holds")["worker"],
        1,
        "slot 0 is still held"
    );
}

/// Channels sharing a key may disagree on `slots`: each is admitted only
/// to the slots below its own bound, so a one-slot channel waits for slot
/// 0 however many higher slots are free.
#[tokio::test]
async fn a_channel_with_fewer_slots_only_sees_the_low_slots() {
    let repo = test_repo().await;
    let claimed = claimed_n(&repo, 3).await;
    assert_eq!(
        start_slots(&repo, &claimed[0], "node-a", Some("worker"), 3).await,
        held(0, 1)
    );
    assert_eq!(
        start(&repo, &claimed[1], "node-a", Some("worker")).await,
        AttemptStart::SingletonBusy
    );
    assert_eq!(
        start_slots(&repo, &claimed[2], "node-a", Some("worker"), 3).await,
        held(1, 1)
    );
}

/// A node that dies after starting an attempt leaves its slot leased; the
/// peer that re-claims the occurrence must re-take *that* slot — fencing
/// the dead holder — rather than burn a second one.
#[tokio::test]
async fn restarting_the_same_occurrence_does_not_take_a_second_slot() {
    let repo = test_repo().await;
    let claimed = claimed_n(&repo, 2).await;
    for occurrence in &claimed {
        start_slots(&repo, occurrence, "node-a", Some("worker"), 3).await;
    }
    // Slot 0 frees up, and the holder of slot 1 dies mid-run.
    assert!(
        repo.release_singleton("worker", &claimed[0].id, slot0(1))
            .await
            .expect("release")
    );
    expire_claim(&repo, &claimed[1].id).await;
    let taken_over = repo
        .claim_due(claim("node-b", 60))
        .await
        .expect("reclaim")
        .remove(0);
    assert_eq!(taken_over.id, claimed[1].id);
    assert_eq!(
        start_slots(&repo, &taken_over, "node-b", Some("worker"), 3).await,
        held(1, 2),
        "the occurrence re-takes its own slot under a new token"
    );
    assert_eq!(
        repo.singleton_holds(&["worker"]).await.expect("holds")["worker"],
        1
    );
}

#[tokio::test]
async fn singleton_holds_counts_live_leases_across_both_tables() {
    let repo = test_repo().await;
    let claimed = claimed_n(&repo, 4).await;
    for occurrence in &claimed[..3] {
        start_slots(&repo, occurrence, "node-a", Some("worker"), 3).await;
    }
    start(&repo, &claimed[3], "node-a", Some("other")).await;
    expire_slot(&repo, "worker", 2).await;
    let holds = repo
        .singleton_holds(&["worker", "other", "idle"])
        .await
        .expect("holds");
    assert_eq!(
        holds.get("worker"),
        Some(&2),
        "an expired lease is not held"
    );
    assert_eq!(holds.get("other"), Some(&1));
    assert_eq!(holds.get("idle"), None);
    assert!(repo.singleton_holds(&[]).await.expect("holds").is_empty());
}

/// Fail closed: when the database is gone, claiming must **error** rather
/// than answer "nothing is due".
///
/// The distinction is the whole safety property. An empty answer is
/// indistinguishable from a healthy idle tick, so a worker would keep
/// looping quietly while occurrences piled up — and, worse, a *singleton*
/// read that failed open would let a second node start alongside a live
/// holder. The worker's `Err` arm records `db_unavailable` and claims
/// nothing; this pins that the arm is reachable.
#[tokio::test]
async fn a_dead_database_errors_rather_than_reporting_nothing_due() {
    let repo = test_repo().await;
    seed_due(&repo, "occ", "ch").await;
    // Sanity: there *is* work, so an empty answer below would be a lie
    // rather than the truth.
    assert_eq!(
        repo.claim_due(claim("node-a", 60))
            .await
            .expect("claim")
            .len(),
        1
    );

    // Close the underlying pool: every subsequent statement fails the way
    // an unreachable database does.
    let pool = match &repo.pool {
        crate::storage::DbPool::Sqlite(pool) => pool,
        _ => unreachable!("the in-memory test pool is SQLite"),
    };
    pool.close().await;

    assert!(
        repo.claim_due(claim("node-b", 60)).await.is_err(),
        "a claim against a dead database must fail, not report an empty queue"
    );
    assert!(
        repo.db_now().await.is_err(),
        "the clock read fails closed too, so a pass cannot plan against a guess"
    );
    assert!(
        repo.start_attempt(
            &CronOccurrence {
                id: "occ".into(),
                ..repo_row()
            },
            "node-b",
            1,
            Some(SingletonRequest {
                key: "key",
                holder: "node-b",
                lease_secs: 60,
                slots: 1,
            }),
            60,
        )
        .await
        .is_err(),
        "singleton acquisition must fail closed rather than assume the key is free"
    );
}

/// A row shaped like the ledger's, for the calls that need one without
/// reading it back.
fn repo_row() -> CronOccurrence {
    let now = chrono::Utc::now().naive_utc();
    CronOccurrence {
        id: String::new(),
        channel_id: "ch".into(),
        channel_name: "nightly".into(),
        channel_version: 1,
        executing_version: None,
        workflow_id: Some("wf".into()),
        trigger: trigger::CRON.into(),
        scheduled_for: now,
        status: status::CLAIMED.into(),
        attempt: 1,
        claimed_by: Some("node-b".into()),
        claimed_until: None,
        singleton_key: None,
        fencing_token: None,
        singleton_slot: None,
        trace_id: None,
        error_message: None,
        started_at: None,
        completed_at: None,
        created_at: now,
        updated_at: now,
    }
}

// ---- settling ----

#[tokio::test]
async fn settling_requires_still_holding_the_claim() {
    let repo = test_repo().await;
    seed_due(&repo, "occ", "ch").await;
    let claimed = repo
        .claim_due(claim("node-a", 60))
        .await
        .expect("claim")
        .remove(0);
    start(&repo, &claimed, "node-a", None).await;

    assert!(
        !repo
            .settle(Settlement {
                occurrence_id: "occ",
                claimant: "node-b",
                status: status::COMPLETED,
                error_message: None,
                trace_id: None,
            })
            .await
            .expect("settle"),
        "a node that does not hold the claim writes nothing"
    );

    assert!(
        repo.settle(Settlement {
            occurrence_id: "occ",
            claimant: "node-a",
            status: status::COMPLETED,
            error_message: None,
            trace_id: Some("trace-1"),
        })
        .await
        .expect("settle")
    );
    let row = repo.get_by_id("occ").await.expect("get");
    assert_eq!(row.status, status::COMPLETED);
    assert_eq!(row.trace_id.as_deref(), Some("trace-1"));
    assert!(row.completed_at.is_some());
    assert!(row.claimed_until.is_none(), "a settled row holds no lease");
}

#[tokio::test]
async fn a_deferred_occurrence_returns_to_pending_and_is_claimable_again() {
    let repo = test_repo().await;
    seed_due(&repo, "occ", "ch").await;
    let claimed = repo
        .claim_due(claim("node-a", 60))
        .await
        .expect("claim")
        .remove(0);

    assert!(
        repo.release_claim(&claimed.id, "node-a")
            .await
            .expect("release")
    );
    let again = repo.claim_due(claim("node-b", 60)).await.expect("claim");
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].id, "occ");
}

// ---- retry ----

#[tokio::test]
async fn retry_keeps_the_occurrence_identity() {
    let repo = test_repo().await;
    let due = seed_due(&repo, "occ", "ch").await;
    let claimed = repo
        .claim_due(claim("node-a", 60))
        .await
        .expect("claim")
        .remove(0);
    start(&repo, &claimed, "node-a", None).await;
    repo.settle(Settlement {
        occurrence_id: "occ",
        claimant: "node-a",
        status: status::FAILED,
        error_message: Some("boom"),
        trace_id: None,
    })
    .await
    .expect("settle");

    let requeued = repo.requeue("occ").await.expect("requeue");
    assert_eq!(requeued.id, "occ");
    assert_eq!(requeued.status, status::PENDING);
    assert_eq!(
        requeued.scheduled_for, due,
        "a retry is another attempt at the same scheduled instant, not a new one"
    );
    assert_eq!(requeued.error_message, None);
    // And it is claimable again, as a second attempt.
    let again = repo.claim_due(claim("node-a", 60)).await.expect("claim");
    assert_eq!(again[0].attempt, 2);
}

/// Re-running finished work is not a retry — it is a new run, which is what
/// the manual trigger is for.
#[tokio::test]
async fn a_completed_occurrence_cannot_be_retried() {
    let repo = test_repo().await;
    seed_due(&repo, "occ", "ch").await;
    let claimed = repo
        .claim_due(claim("node-a", 60))
        .await
        .expect("claim")
        .remove(0);
    start(&repo, &claimed, "node-a", None).await;
    repo.settle(Settlement {
        occurrence_id: "occ",
        claimant: "node-a",
        status: status::COMPLETED,
        error_message: None,
        trace_id: None,
    })
    .await
    .expect("settle");

    let err = repo.requeue("occ").await.expect_err("must refuse");
    assert!(matches!(err, OrionError::Conflict(_)), "{err:?}");
}

// ---- retention and reporting ----

/// Retention removes history, never a backlog: a `pending` occurrence older
/// than the window is work that has not happened yet.
#[tokio::test]
async fn retention_deletes_terminal_rows_and_keeps_pending_ones() {
    let repo = test_repo().await;
    let old = repo.db_now().await.expect("db now") - Duration::days(30);

    repo.insert_occurrence(occurrence("pending-old", "ch", old))
        .await
        .expect("insert");
    repo.insert_occurrence(NewOccurrence {
        status: status::COMPLETED,
        ..occurrence("done-old", "ch", old + Duration::seconds(1))
    })
    .await
    .expect("insert");

    // Both rows were created now, so a one-hour window keeps everything.
    assert_eq!(repo.delete_terminal_older_than(1).await.expect("delete"), 0);
    // Age both past the window: the terminal row goes and the pending one
    // stays, because a pending occurrence older than the retention window
    // is a backlog rather than history.
    age_row(&repo, "pending-old", Duration::days(30)).await;
    age_row(&repo, "done-old", Duration::days(30)).await;
    assert_eq!(repo.delete_terminal_older_than(1).await.expect("delete"), 1);
    let page = repo
        .list_paginated(&CronOccurrenceFilter::default())
        .await
        .expect("list");
    assert_eq!(page.total, 1);
    assert_eq!(page.data[0].id, "pending-old");
}

#[tokio::test]
async fn the_listing_filters_by_channel_status_and_window() {
    let repo = test_repo().await;
    let now = repo.db_now().await.expect("db now");
    repo.insert_occurrence(occurrence("a", "ch-1", now - Duration::hours(2)))
        .await
        .expect("insert");
    repo.insert_occurrence(NewOccurrence {
        status: status::COMPLETED,
        ..occurrence("b", "ch-1", now - Duration::hours(1))
    })
    .await
    .expect("insert");
    repo.insert_occurrence(occurrence("c", "ch-2", now))
        .await
        .expect("insert");

    let by_channel = repo
        .list_paginated(&CronOccurrenceFilter {
            channel_id: Some("ch-1".into()),
            ..Default::default()
        })
        .await
        .expect("list");
    assert_eq!(by_channel.total, 2);

    let by_status = repo
        .list_paginated(&CronOccurrenceFilter {
            status: Some(status::COMPLETED.into()),
            ..Default::default()
        })
        .await
        .expect("list");
    assert_eq!(by_status.total, 1);
    assert_eq!(by_status.data[0].id, "b");

    let windowed = repo
        .list_paginated(&CronOccurrenceFilter {
            since: Some(now - Duration::minutes(90)),
            ..Default::default()
        })
        .await
        .expect("list");
    assert_eq!(windowed.total, 2);
}

#[tokio::test]
async fn the_status_view_reads_the_newest_occurrence_and_the_backlog() {
    let repo = test_repo().await;
    let now = repo.db_now().await.expect("db now");
    assert_eq!(repo.pending_count(None).await.expect("count"), 0);
    assert_eq!(repo.oldest_pending_age_secs().await.expect("age"), None);

    repo.insert_occurrence(occurrence("old", "ch", now - Duration::minutes(10)))
        .await
        .expect("insert");
    repo.insert_occurrence(occurrence("new", "ch", now - Duration::minutes(1)))
        .await
        .expect("insert");
    repo.insert_occurrence(occurrence("other", "ch-2", now))
        .await
        .expect("insert");

    assert_eq!(repo.pending_count(None).await.expect("count"), 3);
    assert_eq!(repo.pending_count(Some("ch")).await.expect("count"), 2);

    let latest = repo
        .latest_for_channel("ch")
        .await
        .expect("latest")
        .expect("a row");
    assert_eq!(latest.channel_id, "ch");

    let age = repo
        .oldest_pending_age_secs()
        .await
        .expect("age")
        .expect("an age");
    assert!(
        (595..=605).contains(&age),
        "the oldest pending occurrence is ten minutes overdue, got {age}s"
    );

    assert!(
        repo.latest_for_channel("absent")
            .await
            .expect("latest")
            .is_none()
    );
}

/// Deleting a channel takes its ledger with it; archiving does not, which
/// is why this is a separate call rather than part of the pause.
#[tokio::test]
async fn purging_a_channel_removes_its_cursor_and_its_occurrences() {
    let repo = test_repo().await;
    let now = repo.db_now().await.expect("db now");
    repo.insert_occurrence(occurrence("a", "ch", now))
        .await
        .expect("insert");
    repo.insert_occurrence(occurrence("b", "keep", now))
        .await
        .expect("insert");
    repo.upsert_cursor(ScheduleCursor {
        channel_id: "ch",
        channel_version: 1,
        config_hash: "hash",
        next_fire_at: now,
    })
    .await
    .expect("upsert");

    repo.purge_channel("ch").await.expect("purge");
    assert!(repo.schedule_states().await.expect("states").is_empty());
    let page = repo
        .list_paginated(&CronOccurrenceFilter::default())
        .await
        .expect("list");
    assert_eq!(page.total, 1);
    assert_eq!(page.data[0].channel_id, "keep");
}

// ---- the SQL itself ----

/// The per-backend claim shapes, pinned the way the trace DLQ's are: the
/// two that matter are that Postgres locks with SKIP LOCKED (so two nodes
/// cannot claim one row mid-transaction) and that the limit travels as a
/// bound value rather than inlined.
#[test]
fn per_backend_claim_shapes() {
    use sea_query::{MysqlQueryBuilder, PostgresQueryBuilder, SqliteQueryBuilder};

    let (sql, values) = claim_update_query(
        "node-a",
        25,
        "LOCALTIMESTAMP",
        "LOCALTIMESTAMP + interval '60 seconds'",
        true,
    )
    .build(PostgresQueryBuilder);
    assert!(sql.contains("RETURNING"), "{sql}");
    assert!(sql.contains("FOR UPDATE SKIP LOCKED"), "{sql}");
    assert!(sql.contains("\"scheduled_for\""), "{sql}");
    assert!(sql.contains("\"claimed_until\""), "{sql}");
    assert!(
        sql.contains("LIMIT $"),
        "limit must be a placeholder: {sql}"
    );
    assert!(
        values.iter().any(|v| *v == sea_query::Value::from(25u64)),
        "limit must travel as a bound value: {values:?}"
    );

    let (sql, _) = claim_update_query(
        "node-a",
        25,
        "datetime('now')",
        "datetime('now', '+60 seconds')",
        false,
    )
    .build(SqliteQueryBuilder);
    assert!(sql.contains("RETURNING"), "{sql}");
    assert!(
        !sql.contains("FOR UPDATE"),
        "SQLite has no row locks: {sql}"
    );

    // MySQL has SKIP LOCKED but no UPDATE … RETURNING, so it is two
    // statements inside one transaction.
    let (sql, _) = claim_select_query(25, "UTC_TIMESTAMP()").build(MysqlQueryBuilder);
    assert!(sql.contains("FOR UPDATE SKIP LOCKED"), "{sql}");
    let (sql, _) =
        lease_claimed_query("node-a", "UTC_TIMESTAMP()", ["a", "b"]).build(MysqlQueryBuilder);
    assert!(sql.contains("`claimed_by`"), "{sql}");
    assert!(!sql.contains("RETURNING"), "{sql}");
}

/// `trigger` is a reserved word in MySQL, so the column has to come out
/// quoted or every insert fails on that backend alone.
#[test]
fn the_trigger_column_is_quoted_on_every_backend() {
    use sea_query::{Iden, MysqlQueryBuilder, PostgresQueryBuilder, SqliteQueryBuilder};
    assert_eq!(Iden::to_string(&CronOccurrences::Trigger), "trigger");

    let insert = || {
        Query::insert()
            .into_table(CronOccurrences::Table)
            .columns([CronOccurrences::Id, CronOccurrences::Trigger])
            .values_panic(["a".into(), trigger::CRON.into()])
            .to_owned()
    };
    assert!(insert().build(MysqlQueryBuilder).0.contains("`trigger`"));
    assert!(
        insert()
            .build(PostgresQueryBuilder)
            .0
            .contains("\"trigger\"")
    );
    assert!(insert().build(SqliteQueryBuilder).0.contains("\"trigger\""));
}
