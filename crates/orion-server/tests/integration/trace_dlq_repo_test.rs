use orion::storage::DbPool;
use orion::storage::repositories::trace_dlq::{SqlTraceDlqRepository, TraceDlqRepository};

/// Create a DLQ repository backed by an in-memory SQLite database, plus the
/// pool for direct fixture manipulation.
async fn dlq_repo() -> (SqlTraceDlqRepository, DbPool) {
    let storage_config = orion::config::StorageConfig {
        url: "sqlite::memory:".to_string(),
        max_connections: 1,
        ..Default::default()
    };
    let pool = orion::storage::init_pool(&storage_config).await.unwrap();
    (SqlTraceDlqRepository::new(pool.clone()), pool)
}

/// Entries become due 1s after enqueue and the claim compares against the
/// DB clock (`datetime('now')`) — backdate instead of sleeping past it.
async fn backdate_next_retry(pool: &DbPool, id: &str) {
    pool.execute_query(
        &format!(
            "UPDATE trace_dlq SET next_retry_at = datetime('now', '-2 seconds') \
             WHERE id = '{id}'"
        ),
        sea_query_sqlx::SqlxValues(sea_query::Values(Vec::new())),
    )
    .await
    .expect("backdate next_retry_at");
}

#[tokio::test]
async fn test_enqueue_and_claim_pending() {
    let (repo, pool) = dlq_repo().await;

    let entry = repo
        .enqueue(
            "trace-1",
            "my-channel",
            r#"{"key":"value"}"#,
            r#"{}"#,
            "engine error",
            0,
            5,
        )
        .await
        .unwrap();

    assert_eq!(entry.trace_id, "trace-1");
    assert_eq!(entry.channel, "my-channel");
    assert_eq!(entry.retry_count, 0);
    assert_eq!(entry.max_retries, 5);

    backdate_next_retry(&pool, &entry.id).await;

    let pending = repo.claim_pending("test-node", 10, 60).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, entry.id);
}

#[tokio::test]
async fn test_record_retry_increments_count() {
    let (repo, pool) = dlq_repo().await;

    let entry = repo
        .enqueue("trace-2", "ch", r#"{"a":1}"#, r#"{}"#, "err", 0, 5)
        .await
        .unwrap();

    // Prove the entry is claimable first, so the emptiness below can only
    // come from record_retry's future next_retry_at — not from the initial
    // 1s enqueue window.
    backdate_next_retry(&pool, &entry.id).await;
    let claimed = repo.claim_pending("test-node", 10, 60).await.unwrap();
    assert_eq!(claimed.len(), 1, "entry must be claimable before the retry");

    // Set next_retry_at far in the future (also releases the claim).
    let future_time = chrono::Utc::now().naive_utc() + chrono::Duration::seconds(3600);
    repo.record_retry(&entry.id, future_time).await.unwrap();

    let pending = repo.claim_pending("test-node", 10, 60).await.unwrap();
    assert!(
        pending.is_empty(),
        "entry with future next_retry_at should not be pending"
    );
}

#[tokio::test]
async fn test_mark_exhausted() {
    let (repo, pool) = dlq_repo().await;

    let entry = repo
        .enqueue("trace-3", "ch", r#"{"b":2}"#, r#"{}"#, "err", 0, 3)
        .await
        .unwrap();

    repo.mark_exhausted(&entry.id).await.unwrap();

    // Backdate so the due-time filter passes: the emptiness below can only
    // come from the exhaustion filter (retry_count >= max_retries).
    backdate_next_retry(&pool, &entry.id).await;
    let pending = repo.claim_pending("test-node", 10, 60).await.unwrap();
    assert!(pending.is_empty(), "exhausted entry should not be pending");
}

#[tokio::test]
async fn test_remove() {
    let (repo, pool) = dlq_repo().await;

    let entry = repo
        .enqueue("trace-4", "ch", r#"{"c":3}"#, r#"{}"#, "err", 0, 5)
        .await
        .unwrap();

    // Claimable while present (backdated past the 1s enqueue window)...
    backdate_next_retry(&pool, &entry.id).await;
    let claimed = repo.claim_pending("test-node", 10, 60).await.unwrap();
    assert_eq!(claimed.len(), 1, "entry must be claimable before removal");

    repo.remove(&entry.id).await.unwrap();

    // ...and gone after removal — emptiness cannot be a due-time artifact.
    let pending = repo.claim_pending("test-node", 10, 60).await.unwrap();
    assert!(pending.is_empty(), "removed entry should not be pending");
}

#[tokio::test]
async fn test_claim_pending_respects_next_retry_at() {
    let (repo, _pool) = dlq_repo().await;

    let _entry = repo
        .enqueue("trace-5", "ch", r#"{"d":4}"#, r#"{}"#, "err", 0, 5)
        .await
        .unwrap();

    // Immediately after enqueue, next_retry_at is ~1s in the future
    let pending = repo.claim_pending("test-node", 10, 60).await.unwrap();
    assert!(
        pending.is_empty(),
        "entry should not be pending before next_retry_at"
    );
}
