//! Issue #254 regression: activate on file-backed WAL SQLite vs. concurrent
//! commits.
//!
//! `activate` is a read-then-write transaction. Started with a plain deferred
//! BEGIN, its first SELECT pins a WAL read snapshot; if any other connection
//! commits before its first UPDATE — in production, the async audit-log
//! writer draining rows queued by the previous admin request — SQLite fails
//! the read→write upgrade with SQLITE_BUSY_SNAPSHOT (extended code 517),
//! which `busy_timeout` never retries. D30 starts these transactions with
//! BEGIN IMMEDIATE instead (`DbPool::begin_write_tx`).
//!
//! This lives outside `common::test_app()` on purpose: `sqlite::memory:`
//! becomes a shared-cache in-memory database where WAL is a no-op, so the
//! race is unreproducible there. Only a file-backed database — the shape
//! `package_cli_e2e_test` runs and CI caught the 517 on — can exercise it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use orion::storage::repositories::audit_logs::{AuditLogRepository, SqlAuditLogRepository};
use orion::storage::repositories::workflows::{
    CreateWorkflowRequest, SqlWorkflowRepository, WorkflowRepository,
};
use serde_json::json;

use crate::common::ScratchDir;

/// Enough iterations that the pre-D30 tree fails near-certainly (each one is
/// an independent chance for a hammer commit to land inside the activate
/// transaction's read window), while keeping the test a few seconds long.
const ACTIVATE_ITERATIONS: usize = 150;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn activate_survives_concurrent_commits_on_file_backed_sqlite() {
    let dir = ScratchDir::new("busy_snapshot");
    let storage = orion::config::StorageConfig {
        url: dir.url(),
        max_connections: 10,
        ..Default::default()
    };
    let pool = orion::storage::init_pool(&storage).await.expect("pool");

    let workflows = SqlWorkflowRepository::new(pool.clone());
    let workflow_id = workflows
        .create(&CreateWorkflowRequest {
            workflow_id: None,
            name: "busy-snapshot-probe".to_string(),
            description: None,
            priority: 0,
            condition: json!(true),
            tasks: json!([{ "id": "t", "name": "T", "function": { "name": "log", "input": { "message": "t" } } }]),
            tags: vec![],
            loop_config: None,
            continue_on_error: false,
        })
        .await
        .expect("create workflow")
        .workflow_id;

    let stop = Arc::new(AtomicBool::new(false));
    let inserted = Arc::new(AtomicU64::new(0));
    let hammers: Vec<_> = (0..2)
        .map(|h| {
            let audit = SqlAuditLogRepository::new(pool.clone());
            let stop = Arc::clone(&stop);
            let inserted = Arc::clone(&inserted);
            tokio::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    audit
                        .insert("test", "hammer", "workflow", &format!("h{h}"), None)
                        .await
                        .expect("audit insert");
                    inserted.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();

    for i in 0..ACTIVATE_ITERATIONS {
        workflows
            .activate(&workflow_id, 100)
            .await
            .unwrap_or_else(|e| {
                panic!("activate #{i} failed (SQLITE_BUSY_SNAPSHOT regression?): {e}")
            });
        workflows
            .create_new_version(&workflow_id)
            .await
            .expect("new draft");
    }

    stop.store(true, Ordering::Relaxed);
    for hammer in hammers {
        hammer.await.expect("hammer task panicked");
    }
    // A hammer that died or never ran would make the loop above vacuous.
    assert!(
        inserted.load(Ordering::Relaxed) > 0,
        "audit hammer wrote no rows — the race was not exercised"
    );
}
