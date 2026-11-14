//! The conformance kit for a cluster [`SharedStateStore`].

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::task::JoinSet;

use crate::extension_api::{SharedStateStore, StoreErrorKind};

/// Check `store` against the contract [`SharedStateStore`] documents, failing
/// (by panicking, as a test assertion does) at the first case it breaks,
/// with the store's [`name`](SharedStateStore::name) in the message.
///
/// Run it from an async test against a live store: the state database's
/// store runs it on SQLite, PostgreSQL and MySQL, and an edition runs it
/// against its own (a Redis container, say). Every key it writes is under a
/// fresh per-run prefix, so runs against one store do not interfere and
/// nothing needs clearing first. It sleeps for about four seconds in all:
/// expiry and the one-second rate windows are judged on the store's clock,
/// which this kit can only wait on.
///
/// The cases:
///
/// - a value is set, overwritten and read back;
/// - a TTL expires it (a two-second TTL, read after about two), and a zero
///   TTL is already expired;
/// - `get_many` answers in the order asked, `None` for each miss;
/// - `remove_many` counts only live keys and accepts absent ones;
/// - of 8 concurrent claims exactly one wins, and every loser is told the
///   winner's owner;
/// - an expired claim can be taken over;
/// - 10 concurrent increments of one key answer 1 to 10: none is lost;
/// - incrementing text is [`StoreErrorKind::Invalid`];
/// - an increment's TTL is set only when it creates the key, and an
///   expired counter restarts;
/// - `hit_window` counts 1 to N within one aligned second and restarts at 1
///   in the next;
/// - `ping` succeeds, and so does `sweep` when the store `needs_sweep()`.
pub async fn shared_store_contract(store: Arc<dyn SharedStateStore>) {
    let name = store.name().to_string();
    let prefix = format!("contract:{}:", uuid::Uuid::new_v4().simple());
    let key = |k: &str| format!("{prefix}{k}");

    // Set, overwrite, get.
    store.set(&key("v"), "1", None).await.expect("set answers");
    store
        .set(&key("v"), "2", Some(60))
        .await
        .expect("an overwrite answers");
    assert_eq!(
        store.get(&key("v")).await.expect("get answers").as_deref(),
        Some("2"),
        "{name}: an overwrite replaces the value"
    );
    assert_eq!(
        store.get(&key("never-set")).await.expect("get answers"),
        None,
        "{name}: an absent key reads as None"
    );

    // A zero TTL is already expired.
    store
        .set(&key("zero"), "x", Some(0))
        .await
        .expect("set answers");
    assert_eq!(
        store.get(&key("zero")).await.expect("get answers"),
        None,
        "{name}: a zero TTL is already expired"
    );

    // get_many keeps the order asked, with misses.
    store.set(&key("a"), "A", None).await.expect("set answers");
    store.set(&key("b"), "B", None).await.expect("set answers");
    assert_eq!(
        store
            .get_many(&[key("b"), key("missing"), key("a")])
            .await
            .expect("get_many answers"),
        vec![Some("B".to_string()), None, Some("A".to_string())],
        "{name}: get_many answers in the order asked"
    );
    assert_eq!(
        store.get_many(&[]).await.expect("get_many answers"),
        Vec::<Option<String>>::new(),
        "{name}: get_many of nothing is nothing"
    );

    // remove_many counts only live keys and accepts absent ones.
    store
        .set(&key("dead"), "x", Some(0))
        .await
        .expect("set answers");
    assert_eq!(
        store
            .remove_many(&[key("a"), key("dead"), key("absent")])
            .await
            .expect("remove_many answers"),
        1,
        "{name}: remove_many counts only the live key"
    );
    assert_eq!(
        store.get(&key("a")).await.expect("get answers"),
        None,
        "{name}: a removed key is gone"
    );

    // Eight concurrent claims: one wins, every loser names the winner.
    let mut claims = JoinSet::new();
    for i in 0..8 {
        let store = store.clone();
        let claim_key = key("claim");
        claims.spawn(async move {
            let owner = format!("owner-{i}");
            let outcome = store.claim(&claim_key, &owner, 60).await;
            (owner, outcome)
        });
    }
    let mut winners = Vec::new();
    let mut holders = Vec::new();
    while let Some(joined) = claims.join_next().await {
        let (owner, outcome) = joined.expect("a claim task completes");
        match outcome.expect("claim answers") {
            None => winners.push(owner),
            Some(holder) => holders.push(holder),
        }
    }
    assert_eq!(
        winners.len(),
        1,
        "{name}: exactly one of 8 concurrent claims wins, got {winners:?}"
    );
    assert!(
        holders.iter().all(|h| *h == winners[0]),
        "{name}: every loser is told the winner {:?}, got {holders:?}",
        winners[0]
    );

    // An expired claim is taken over.
    assert_eq!(
        store
            .claim(&key("short"), "first", 0)
            .await
            .expect("claim answers"),
        None,
        "{name}: a free key is claimed"
    );
    assert_eq!(
        store
            .claim(&key("short"), "second", 60)
            .await
            .expect("claim answers"),
        None,
        "{name}: an expired claim is taken over"
    );
    assert_eq!(
        store
            .claim(&key("short"), "third", 60)
            .await
            .expect("claim answers")
            .as_deref(),
        Some("second"),
        "{name}: the claim that took over holds"
    );

    // Ten concurrent increments: none lost.
    let mut increments = JoinSet::new();
    for _ in 0..10 {
        let store = store.clone();
        let counter = key("n");
        increments.spawn(async move { store.incr_by(&counter, 1, Some(60)).await });
    }
    let mut seen = Vec::new();
    while let Some(joined) = increments.join_next().await {
        seen.push(
            joined
                .expect("an increment task completes")
                .expect("incr_by answers"),
        );
    }
    seen.sort_unstable();
    assert_eq!(
        seen,
        (1..=10).collect::<Vec<i64>>(),
        "{name}: no concurrent increment is lost"
    );

    // Text is not a counter.
    store
        .set(&key("text"), "words", None)
        .await
        .expect("set answers");
    let refused = store.incr_by(&key("text"), 1, None).await;
    assert!(
        matches!(&refused, Err(e) if e.kind() == StoreErrorKind::Invalid),
        "{name}: incrementing text is Invalid, got {refused:?}"
    );

    // An expired counter restarts.
    assert_eq!(
        store
            .incr_by(&key("restart"), 7, Some(0))
            .await
            .expect("incr_by answers"),
        7,
        "{name}: a missing key counts from 0"
    );
    assert_eq!(
        store
            .incr_by(&key("restart"), 1, Some(60))
            .await
            .expect("incr_by answers"),
        1,
        "{name}: an expired counter restarts"
    );

    // Expiry over time, one wait for both: a value's TTL, and a counter's
    // TTL that a later increment must not extend.
    store
        .set(&key("ttl"), "x", Some(2))
        .await
        .expect("set answers");
    assert_eq!(
        store
            .incr_by(&key("ttl-n"), 1, Some(2))
            .await
            .expect("incr_by answers"),
        1,
        "{name}: a new counter"
    );
    assert_eq!(
        store
            .incr_by(&key("ttl-n"), 1, Some(3600))
            .await
            .expect("incr_by answers"),
        2,
        "{name}: a live counter counts on"
    );
    assert_eq!(
        store
            .get(&key("ttl"))
            .await
            .expect("get answers")
            .as_deref(),
        Some("x"),
        "{name}: a value is live before its TTL"
    );
    // Two seconds, not one: a second-grained store clock (SQLite, MySQL)
    // ends a TTL of N seconds anywhere in (N - 1, N], so a one-second TTL
    // could already be over at the reads above. Either way it is over
    // after a little more than N.
    tokio::time::sleep(Duration::from_millis(2_200)).await;
    assert_eq!(
        store.get(&key("ttl")).await.expect("get answers"),
        None,
        "{name}: a value expires after its TTL"
    );
    assert_eq!(
        store
            .incr_by(&key("ttl-n"), 1, None)
            .await
            .expect("incr_by answers"),
        1,
        "{name}: a counter's TTL is set only on create, so it expired and restarted"
    );

    // Rate windows: 1..N inside one second, 1 again in the next.
    align_to_fresh_second().await;
    for expected in 1..=3 {
        assert_eq!(
            store
                .hit_window(&key("window"))
                .await
                .expect("hit_window answers"),
            expected,
            "{name}: hit {expected} in one window"
        );
    }
    align_to_fresh_second().await;
    assert_eq!(
        store
            .hit_window(&key("window"))
            .await
            .expect("hit_window answers"),
        1,
        "{name}: the next second is a new window"
    );

    store.ping().await.expect("ping answers");
    if store.needs_sweep() {
        store.sweep().await.expect("sweep answers");
    }
}

/// Sleep until shortly after the next second boundary, leaving most of a
/// second for the hits that follow. This process's clock stands in for the
/// store's; the margin absorbs a small skew between them.
async fn align_to_fresh_second() {
    let into = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_millis();
    tokio::time::sleep(Duration::from_millis(u64::from(1_000 - into) + 100)).await;
}
