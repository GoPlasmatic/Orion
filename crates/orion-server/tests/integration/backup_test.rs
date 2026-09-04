//! Coverage for the SQLite backup endpoints (`POST`/`GET /api/v1/admin/backups`).
//!
//! There is no restore endpoint to test: restore is the offline
//! stop → replace the database file → start procedure documented in
//! `docs/src/operate/backup-restore.md` (O5/C20). This file was named
//! `backup_restore_test.rs` while covering only create/list/contains-data.

use crate::common;

use crate::common::{body_json, json_request};
use axum::http::StatusCode;
use tower::ServiceExt;

/// Unique temporary directory for both the backup dir and the SQLite DB,
/// removed on drop — so a failing assertion cannot leak the directory (the
/// explicit-cleanup version did exactly that).
struct TestDirs {
    base: String,
    backup_dir: String,
}

fn make_test_dirs(label: &str) -> TestDirs {
    let base = format!(
        "{}/orion_backup_test_{}_{}_{}",
        std::env::temp_dir().display(),
        label,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let backup_dir = format!("{}/backups", base);
    std::fs::create_dir_all(&backup_dir).unwrap();
    TestDirs { base, backup_dir }
}

impl Drop for TestDirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Build a test app that uses a file-based SQLite database.
/// This is required for backup tests because `VACUUM INTO` does not work
/// reliably with sqlx's in-memory SQLite pool (each connection in the pool
/// gets a separate in-memory database).
async fn backup_test_app(
    base_dir: &str,
    backup_dir: &str,
    backup_retention_count: Option<u32>,
) -> axum::Router {
    let db_path = format!("{}/test.db", base_dir);
    let config = orion::config::AppConfig {
        storage: orion::config::StorageConfig {
            url: format!("sqlite:{}", db_path),
            backup_dir: backup_dir.to_string(),
            backup_retention_count,
            max_connections: 5,
            ..Default::default()
        },
        ..Default::default()
    };
    common::test_app_with_config(config).await
}

// ============================================================
// 0. There is no restore endpoint (O5)
// ============================================================

/// The docs used to tell operators to `POST /api/v1/admin/restore` (and
/// `/backup`, singular). Neither route exists — pin that so the claim cannot
/// silently reappear in the docs.
#[tokio::test]
async fn test_restore_endpoint_does_not_exist() {
    let app = common::test_app().await;

    for path in ["/api/v1/admin/restore", "/api/v1/admin/backup"] {
        let resp = app
            .clone()
            .oneshot(json_request("POST", path, None))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "{path} must not be a registered route"
        );
    }
}

// ============================================================
// 1. Create a backup
// ============================================================

#[tokio::test]
async fn test_create_backup() {
    let dirs = make_test_dirs("create");
    let (base_dir, backup_dir) = (dirs.base.clone(), dirs.backup_dir.clone());
    let app = backup_test_app(&base_dir, &backup_dir, None).await;

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
}

// ============================================================
// 2. List backups (two backups, sorted descending)
// ============================================================

#[tokio::test]
async fn test_list_backups() {
    let dirs = make_test_dirs("list");
    let (base_dir, backup_dir) = (dirs.base.clone(), dirs.backup_dir.clone());
    let app = backup_test_app(&base_dir, &backup_dir, None).await;

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
}

// ============================================================
// 3. Retention prunes oldest-first after a successful backup (O6)
// ============================================================

/// With `storage.backup_retention_count = 2`, a successful backup prunes the
/// directory oldest-first down to two files. The stale backups are seeded on
/// disk with dates that sort before anything created now, so the test is
/// deterministic without sleeping through filename-timestamp resolution.
#[tokio::test]
async fn test_backup_retention_prunes_oldest_backups() {
    let dirs = make_test_dirs("retention");
    let (base_dir, backup_dir) = (dirs.base.clone(), dirs.backup_dir.clone());
    let stale = [
        "orion_backup_20200101_000000.db",
        "orion_backup_20200102_000000.db",
        "orion_backup_20200103_000000.db",
    ];
    for name in stale {
        std::fs::write(format!("{backup_dir}/{name}"), b"stale").unwrap();
    }

    let app = backup_test_app(&base_dir, &backup_dir, Some(2)).await;
    let resp = app
        .clone()
        .oneshot(json_request("POST", "/api/v1/admin/backups", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let new_backup = body["data"]["filename"]
        .as_str()
        .expect("filename should be a string")
        .to_string();

    // Four backups existed the moment the new one was written; retention 2
    // keeps the newest two: the fresh backup and the newest stale file.
    let mut on_disk: Vec<String> = std::fs::read_dir(&backup_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    assert_eq!(
        on_disk,
        vec!["orion_backup_20200103_000000.db".to_string(), new_backup],
        "retention must keep exactly the newest 2 backups"
    );

    // The list endpoint agrees with the disk.
    let resp = app
        .clone()
        .oneshot(json_request("GET", "/api/v1/admin/backups", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["data"]
            .as_array()
            .expect("data should be an array")
            .len(),
        2,
        "GET /backups must list only the retained files"
    );
}

// ============================================================
// 4. Backup contains data (non-trivial size after inserting records)
// ============================================================

#[tokio::test]
async fn test_backup_contains_data() {
    let dirs = make_test_dirs("data");
    let (base_dir, backup_dir) = (dirs.base.clone(), dirs.backup_dir.clone());
    let app = backup_test_app(&base_dir, &backup_dir, None).await;

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
}

// ============================================================
// 4. Concurrent backups (B4)
// ============================================================

/// B4: several backups requested in the same second all succeed, with
/// distinct files.
///
/// The filename carried seconds only, so two requests inside one second chose
/// the same path — and `VACUUM INTO` refuses a destination that exists, so one
/// of the two answered 500. Any scheduler firing on a boundary, an operator
/// double-clicking, or a retry hits this.
///
/// Four concurrent requests, one assertion each on the two things that were
/// wrong: every request succeeded, and every backup is its own file (a shared
/// name would also show up as four responses naming three files).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_backups_all_succeed_with_distinct_files() {
    let dirs = make_test_dirs("concurrent");
    let (base_dir, backup_dir) = (dirs.base.clone(), dirs.backup_dir.clone());
    // No retention: pruning would delete the very files this counts.
    let app = backup_test_app(&base_dir, &backup_dir, None).await;

    let mut requests = Vec::new();
    for _ in 0..4 {
        let app = app.clone();
        requests.push(tokio::spawn(async move {
            let resp = app
                .oneshot(json_request("POST", "/api/v1/admin/backups", None))
                .await
                .unwrap();
            let status = resp.status();
            let body = body_json(resp).await;
            (status, body)
        }));
    }

    let mut filenames = std::collections::BTreeSet::new();
    for request in requests {
        let (status, body) = request.await.unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "every concurrent backup must succeed, got {status}: {body}"
        );
        let filename = body["data"]["filename"]
            .as_str()
            .expect("filename")
            .to_string();
        let path = body["data"]["path"].as_str().expect("path");
        assert!(
            std::path::Path::new(path).exists(),
            "reported backup is missing from disk: {path}"
        );
        filenames.insert(filename);
    }

    assert_eq!(
        filenames.len(),
        4,
        "each backup must have its own file, got: {filenames:?}"
    );

    // And the directory agrees — nothing was silently overwritten.
    let on_disk: Vec<String> = std::fs::read_dir(&backup_dir)
        .expect("backup dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("orion_backup_"))
        .collect();
    assert_eq!(on_disk.len(), 4, "files on disk: {on_disk:?}");
}

/// Retention still counts correctly when the backups it prunes were created
/// concurrently — the prune runs under the same lock, so it never races
/// another backup's file into or out of the set it is deleting from.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_backups_respect_the_retention_count() {
    let dirs = make_test_dirs("concurrent_retention");
    let (base_dir, backup_dir) = (dirs.base.clone(), dirs.backup_dir.clone());
    let app = backup_test_app(&base_dir, &backup_dir, Some(2)).await;

    let mut requests = Vec::new();
    for _ in 0..5 {
        let app = app.clone();
        requests.push(tokio::spawn(async move {
            app.oneshot(json_request("POST", "/api/v1/admin/backups", None))
                .await
                .unwrap()
                .status()
        }));
    }
    for request in requests {
        assert_eq!(request.await.unwrap(), StatusCode::OK);
    }

    let remaining: Vec<String> = std::fs::read_dir(&backup_dir)
        .expect("backup dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("orion_backup_"))
        .collect();
    assert_eq!(
        remaining.len(),
        2,
        "retention must land on exactly the configured count: {remaining:?}"
    );
}
