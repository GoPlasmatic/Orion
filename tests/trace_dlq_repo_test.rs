use orion::storage::repositories::trace_dlq::{SqlTraceDlqRepository, TraceDlqRepository};

/// Create a DLQ repository backed by an in-memory SQLite database.
async fn dlq_repo() -> SqlTraceDlqRepository {
    let storage_config = orion::config::StorageConfig {
        url: "sqlite::memory:".to_string(),
        max_connections: 1,
        ..Default::default()
    };
    let pool = orion::storage::init_pool(&storage_config).await.unwrap();
    SqlTraceDlqRepository::new(pool)
}

#[tokio::test]
async fn test_enqueue_and_list_pending() {
    let repo = dlq_repo().await;

    let entry = repo
        .enqueue(
            "trace-1",
            "my-channel",
            r#"{"key":"value"}"#,
            r#"{}"#,
            "engine error",
            5,
        )
        .await
        .unwrap();

    assert_eq!(entry.trace_id, "trace-1");
    assert_eq!(entry.channel, "my-channel");
    assert_eq!(entry.retry_count, 0);
    assert_eq!(entry.max_retries, 5);

    // Wait a moment so next_retry_at (1s from creation) is in the past
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    let pending = repo.list_pending(10).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, entry.id);
}

#[tokio::test]
async fn test_record_retry_increments_count() {
    let repo = dlq_repo().await;

    let entry = repo
        .enqueue("trace-2", "ch", r#"{"a":1}"#, r#"{}"#, "err", 5)
        .await
        .unwrap();

    // Set next_retry_at far in the future
    let future_time =
        (chrono::Utc::now().naive_utc() + chrono::Duration::seconds(3600)).to_string();
    repo.record_retry(&entry.id, &future_time).await.unwrap();

    // Entry should not appear in pending (next_retry_at is in the future)
    // Wait briefly for the initial 1s retry window
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let pending = repo.list_pending(10).await.unwrap();
    assert!(
        pending.is_empty(),
        "entry with future next_retry_at should not be pending"
    );
}

#[tokio::test]
async fn test_mark_exhausted() {
    let repo = dlq_repo().await;

    let entry = repo
        .enqueue("trace-3", "ch", r#"{"b":2}"#, r#"{}"#, "err", 3)
        .await
        .unwrap();

    repo.mark_exhausted(&entry.id).await.unwrap();

    // Exhausted entries (retry_count >= max_retries) should not appear in pending
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let pending = repo.list_pending(10).await.unwrap();
    assert!(pending.is_empty(), "exhausted entry should not be pending");
}

#[tokio::test]
async fn test_remove() {
    let repo = dlq_repo().await;

    let entry = repo
        .enqueue("trace-4", "ch", r#"{"c":3}"#, r#"{}"#, "err", 5)
        .await
        .unwrap();

    repo.remove(&entry.id).await.unwrap();

    // Removed entry should not appear in pending
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let pending = repo.list_pending(10).await.unwrap();
    assert!(pending.is_empty(), "removed entry should not be pending");
}

#[tokio::test]
async fn test_list_pending_respects_next_retry_at() {
    let repo = dlq_repo().await;

    let _entry = repo
        .enqueue("trace-5", "ch", r#"{"d":4}"#, r#"{}"#, "err", 5)
        .await
        .unwrap();

    // Immediately after enqueue, next_retry_at is ~1s in the future
    let pending = repo.list_pending(10).await.unwrap();
    assert!(
        pending.is_empty(),
        "entry should not be pending before next_retry_at"
    );
}
