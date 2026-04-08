mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::{body_json, json_request};
use tokio::sync::RwLock;
use tower::ServiceExt;

/// Create a unique temporary directory for both the backup dir and the SQLite DB.
/// Returns (db_dir, backup_dir). The caller is responsible for cleanup.
fn make_test_dirs(label: &str) -> (String, String) {
    let base = format!(
        "{}/orion_backup_test_{}_{}",
        std::env::temp_dir().display(),
        label,
        std::process::id()
    );
    let backup_dir = format!("{}/backups", base);
    std::fs::create_dir_all(&backup_dir).unwrap();
    (base, backup_dir)
}

/// Clean up all test directories.
fn cleanup_dirs(base: &str) {
    let _ = std::fs::remove_dir_all(base);
}

/// Build a test app that uses a file-based SQLite database.
/// This is required for backup tests because `VACUUM INTO` does not work
/// reliably with sqlx's in-memory SQLite pool (each connection in the pool
/// gets a separate in-memory database).
async fn backup_test_app(base_dir: &str, backup_dir: &str) -> axum::Router {
    use orion::channel::ChannelRegistry;
    use orion::connector::ConnectorRegistry;
    use orion::server::rate_limit::RateLimitState;
    use orion::server::state::AppState;
    use orion::storage::repositories::audit_logs::SqlAuditLogRepository;
    use orion::storage::repositories::channels::SqlChannelRepository;
    use orion::storage::repositories::connectors::SqlConnectorRepository;
    use orion::storage::repositories::trace_dlq::SqlTraceDlqRepository;
    use orion::storage::repositories::traces::SqlTraceRepository;
    use orion::storage::repositories::workflows::SqlWorkflowRepository;

    sqlx::any::install_default_drivers();

    let db_path = format!("{}/test.db", base_dir);

    let config = orion::config::AppConfig {
        storage: orion::config::StorageConfig {
            url: format!("sqlite:{}", db_path),
            backup_dir: backup_dir.to_string(),
            max_connections: 5,
            ..Default::default()
        },
        ..Default::default()
    };

    let storage_config = orion::config::StorageConfig {
        url: format!("sqlite:{}", db_path),
        max_connections: 5,
        ..Default::default()
    };
    let pool = orion::storage::init_pool(&storage_config).await.unwrap();

    let channel_repo = Arc::new(SqlChannelRepository::new(pool.clone()));
    let workflow_repo = Arc::new(SqlWorkflowRepository::new(pool.clone()));
    let connector_repo = Arc::new(SqlConnectorRepository::new(pool.clone()));
    let trace_repo = Arc::new(SqlTraceRepository::new(pool.clone()));
    let audit_log_repo = Arc::new(SqlAuditLogRepository::new(pool.clone()));
    let connector_registry = Arc::new(ConnectorRegistry::new(
        config.engine.circuit_breaker.clone(),
    ));
    let channel_registry = Arc::new(ChannelRegistry::new());
    let cache_pool = Arc::new(orion::connector::cache_backend::CachePool::new(
        config.engine.max_pool_cache_entries,
        60,
    ));
    let sql_pool_cache = Arc::new(orion::connector::pool_cache::SqlPoolCache::new(
        config.engine.max_pool_cache_entries,
    ));
    let mongo_pool_cache = Arc::new(orion::connector::mongo_pool::MongoPoolCache::new(
        config.engine.max_pool_cache_entries,
    ));

    let http_client = reqwest::Client::new();
    let engine = Arc::new(RwLock::new(Arc::new(dataflow_rs::Engine::new(
        vec![],
        None,
    ))));
    let custom_functions = orion::engine::build_custom_functions(
        connector_registry.clone(),
        http_client.clone(),
        engine.clone(),
        &config.engine,
        cache_pool.clone(),
        sql_pool_cache.clone(),
        mongo_pool_cache.clone(),
    );
    let built_engine = dataflow_rs::Engine::new(vec![], Some(custom_functions));
    *engine.write().await = Arc::new(built_engine);

    let dlq_repo: Option<Arc<dyn orion::storage::repositories::trace_dlq::TraceDlqRepository>> =
        Some(Arc::new(SqlTraceDlqRepository::new(pool.clone())));
    let test_queue_config = orion::config::QueueConfig {
        workers: 2,
        buffer_size: 100,
        shutdown_timeout_secs: 30,
        processing_timeout_ms: 60_000,
        max_result_size_bytes: 1_048_576,
        max_queue_memory_bytes: 104_857_600,
        ..Default::default()
    };
    let (trace_queue, _worker_handle) = orion::queue::start_workers(
        &test_queue_config,
        engine.clone(),
        trace_repo.clone() as Arc<dyn orion::storage::repositories::traces::TraceRepository>,
        dlq_repo,
    );

    let metrics_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .unwrap_or_else(|_| {
            metrics_exporter_prometheus::PrometheusBuilder::new()
                .build_recorder()
                .handle()
        });

    let rate_limit_state = if config.rate_limit.enabled {
        Some(Arc::new(RateLimitState::from_config(&config.rate_limit)))
    } else {
        None
    };

    let state = AppState {
        engine,
        channel_repo,
        workflow_repo,
        connector_repo,
        trace_repo,
        audit_log_repo,
        connector_registry,
        cache_pool,
        channel_registry,
        trace_queue,
        db_pool: pool,
        config: Arc::new(config),
        start_time: chrono::Utc::now(),
        metrics_handle,
        http_client,
        datalogic: Arc::new(datalogic_rs::DataLogic::new()),
        rate_limit_state,
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        sql_pool_cache,
        mongo_pool_cache,
        kafka_consumer_handle: Arc::new(tokio::sync::Mutex::new(None)),
        kafka_producer: None,
    };

    orion::server::build_router(state)
}

// ============================================================
// 1. Create a backup
// ============================================================

#[tokio::test]
async fn test_create_backup() {
    let (base_dir, backup_dir) = make_test_dirs("create");
    let app = backup_test_app(&base_dir, &backup_dir).await;

    let resp = app
        .clone()
        .oneshot(json_request("POST", "/api/v1/admin/backups", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let data = &body["data"];

    // Filename starts with the expected prefix
    let filename = data["filename"]
        .as_str()
        .expect("filename should be a string");
    assert!(
        filename.starts_with("orion_backup_"),
        "filename should start with 'orion_backup_', got: {}",
        filename
    );
    assert!(
        filename.ends_with(".db"),
        "filename should end with '.db', got: {}",
        filename
    );

    // size_bytes is present and > 0
    let size = data["size_bytes"]
        .as_u64()
        .expect("size_bytes should be a number");
    assert!(size > 0, "backup should have non-zero size");

    // path and created_at are present
    assert!(
        data["path"].as_str().is_some(),
        "response should include 'path'"
    );
    assert!(
        data["created_at"].as_str().is_some(),
        "response should include 'created_at'"
    );

    cleanup_dirs(&base_dir);
}

// ============================================================
// 2. List backups (two backups, sorted descending)
// ============================================================

#[tokio::test]
async fn test_list_backups() {
    let (base_dir, backup_dir) = make_test_dirs("list");
    let app = backup_test_app(&base_dir, &backup_dir).await;

    // Create first backup
    let resp = app
        .clone()
        .oneshot(json_request("POST", "/api/v1/admin/backups", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Small delay so the second backup gets a different timestamp in its filename
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Create second backup
    let resp = app
        .clone()
        .oneshot(json_request("POST", "/api/v1/admin/backups", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // List backups
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/backups", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let backups = body["data"].as_array().expect("data should be an array");

    assert_eq!(backups.len(), 2, "should list exactly 2 backups");

    // Verify sorted by filename descending (newest first)
    let first = backups[0]["filename"].as_str().unwrap();
    let second = backups[1]["filename"].as_str().unwrap();
    assert!(
        first > second,
        "backups should be sorted descending by filename: '{}' should come after '{}'",
        first,
        second
    );

    // Each entry should have required fields
    for backup in backups {
        assert!(backup["filename"].as_str().is_some());
        assert!(backup["size_bytes"].as_u64().is_some());
        assert!(backup["modified_at"].as_str().is_some());
    }

    cleanup_dirs(&base_dir);
}

// ============================================================
// 3. Backup contains data (non-trivial size after inserting records)
// ============================================================

#[tokio::test]
async fn test_backup_contains_data() {
    let (base_dir, backup_dir) = make_test_dirs("data");
    let app = backup_test_app(&base_dir, &backup_dir).await;

    // Insert some workflows
    for i in 0..3 {
        let resp = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/api/v1/admin/workflows",
                Some(common::simple_log_workflow(&format!("Backup WF {}", i))),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    // Insert some connectors
    for i in 0..2 {
        common::create_connector(&app, common::db_connector(&format!("backup-conn-{}", i))).await;
    }

    // Create a backup
    let resp = app
        .clone()
        .oneshot(json_request("POST", "/api/v1/admin/backups", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let data = &body["data"];

    let path = data["path"].as_str().expect("path should be present");
    let size = data["size_bytes"]
        .as_u64()
        .expect("size_bytes should be present");

    // Verify the file actually exists on disk
    assert!(
        std::path::Path::new(path).exists(),
        "backup file should exist at: {}",
        path
    );

    // A SQLite file with schema + data should be larger than a bare header (4096+)
    assert!(
        size > 4096,
        "backup with data should be larger than 4096 bytes, got: {}",
        size
    );

    // Double-check with filesystem metadata
    let fs_meta = std::fs::metadata(path).expect("should be able to stat backup file");
    assert_eq!(
        fs_meta.len(),
        size,
        "reported size should match filesystem metadata"
    );

    cleanup_dirs(&base_dir);
}
