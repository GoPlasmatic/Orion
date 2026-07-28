use axum::extract::State;
use axum::{Extension, Json};
use serde_json::{Value, json};

use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::routes::openapi::{BackupFile, BackupListItem, DataEnvelope};
use crate::server::routes::response_helpers::data_response;
use crate::server::state::AppState;

use super::audit_log;

// ============================================================
// Backups (SQLite only)
// ============================================================

/// Filesystem backups are meaningless behind a load balancer: the file lands
/// on whichever node served the request (and cluster storage is
/// Postgres/MySQL anyway, which VACUUM INTO cannot back up). Point operators
/// at the managed database's native mechanisms instead (multi-instance B3).
fn reject_in_cluster_mode(state: &AppState) -> Result<(), OrionError> {
    if state.cluster.enabled {
        return Err(OrionError::BadRequest(
            "Filesystem backups are disabled in cluster mode — the file would land on \
             one arbitrary node. Use your managed database's snapshot/PITR tooling \
             (e.g. pg_dump, RDS/Cloud SQL backups) instead; see the availability docs."
                .to_string(),
        ));
    }
    Ok(())
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/backups",
    tag = "Backups",
    responses(
        (status = 200, description = "Backup created (SQLite only — VACUUM INTO a timestamped file)", body = DataEnvelope<BackupFile>),
        (status = 400, description = "Backup unavailable (non-SQLite backend, or cluster mode — use managed-DB snapshots/PITR)"),
    )
)]
#[tracing::instrument(skip(state, principal))]
pub(crate) async fn create_backup(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
) -> Result<Json<Value>, OrionError> {
    reject_in_cluster_mode(&state)?;
    let backup_dir = state.config.storage.backup_dir.clone();

    // Run blocking filesystem operations off the async runtime
    let dir = backup_dir.clone();
    tokio::task::spawn_blocking(move || std::fs::create_dir_all(&dir))
        .await
        .map_err(|e| OrionError::Internal(format!("spawn_blocking failed: {e}")))?
        .map_err(|e| OrionError::InternalSource {
            context: format!("Failed to create backup directory '{backup_dir}'"),
            source: Box::new(e),
        })?;

    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let filename = format!("orion_backup_{timestamp}.db");
    let backup_path = std::path::Path::new(&backup_dir).join(&filename);
    let backup_path_str = backup_path.to_string_lossy().to_string();

    // VACUUM INTO is SQLite-specific. R26: the pool variant is matched inside
    // `AppStateInner`, not here — a route handler has no business unwrapping a
    // concrete `sqlx` pool.
    let backed_up = state
        .backup_sqlite_into(&backup_path_str)
        .await
        .map_err(|e| OrionError::InternalSource {
            context: "Failed to create database backup".to_string(),
            source: Box::new(e),
        })?;
    if !backed_up {
        return Err(OrionError::BadRequest(
            "Database backup via VACUUM INTO is only supported for SQLite".to_string(),
        ));
    }

    let meta_path = backup_path.clone();
    let metadata = tokio::task::spawn_blocking(move || std::fs::metadata(&meta_path))
        .await
        .map_err(|e| OrionError::Internal(format!("spawn_blocking failed: {e}")))?
        .map_err(|e| OrionError::InternalSource {
            context: "Failed to read backup file metadata".to_string(),
            source: Box::new(e),
        })?;

    // Retention (O6): the backup succeeded, so prune down to the configured
    // count. Backups land on the same disk as the live database, so an
    // unbounded set is a mechanism that can cause the outage it exists to
    // recover from. Prune failures are logged, never surfaced — the backup
    // the caller asked for exists.
    if let Some(retain) = state.config.storage.backup_retention_count {
        let dir = backup_dir.clone();
        match tokio::task::spawn_blocking(move || prune_old_backups(&dir, retain as usize)).await {
            Ok(pruned) if !pruned.is_empty() => {
                tracing::info!(
                    pruned = ?pruned,
                    retained = retain,
                    "Backup retention: pruned old backup files"
                );
            }
            Ok(_) => {}
            // A join error (prune task panicked) is still only a prune
            // failure — the backup the caller asked for exists, so it must
            // not turn into a 500.
            Err(e) => {
                tracing::warn!(error = %e, "Backup retention: prune task failed");
            }
        }
    }

    audit_log(
        &state.audit_log_repo,
        &principal,
        "create",
        "backup",
        &filename,
    );

    Ok(data_response(json!({
        "filename": filename,
        "path": backup_path_str,
        "size_bytes": metadata.len(),
        "created_at": chrono::Utc::now().to_rfc3339(),
    })))
}

/// Delete the oldest `orion_backup_*.db` files in `backup_dir` so that at
/// most `retain` remain, returning the filenames actually removed.
///
/// Filenames embed a sortable UTC timestamp (`orion_backup_%Y%m%d_%H%M%S.db`),
/// so lexicographic order is chronological order — the same invariant
/// `list_backups` sorts by. Only files matching the backup naming pattern are
/// candidates; anything else in the directory is left alone. Called only
/// after a successful backup with `retain >= 1` (validated config), so the
/// file just written is always kept.
fn prune_old_backups(backup_dir: &str, retain: usize) -> Vec<String> {
    let dir = match std::fs::read_dir(backup_dir) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                dir = %backup_dir,
                error = %e,
                "Backup retention: cannot read backup directory; skipping prune"
            );
            return Vec::new();
        }
    };

    let mut names: Vec<String> = dir
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let is_backup = path.extension().is_some_and(|ext| ext == "db")
                && path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("orion_backup_"));
            is_backup.then(|| entry.file_name().to_string_lossy().into_owned())
        })
        .collect();
    if names.len() <= retain {
        return Vec::new();
    }

    names.sort();
    let cutoff = names.len() - retain;
    let mut pruned = Vec::new();
    for name in names.drain(..cutoff) {
        let path = std::path::Path::new(backup_dir).join(&name);
        match std::fs::remove_file(&path) {
            Ok(()) => pruned.push(name),
            Err(e) => {
                tracing::warn!(
                    file = %name,
                    error = %e,
                    "Backup retention: failed to prune old backup file"
                );
            }
        }
    }
    pruned
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/backups",
    tag = "Backups",
    responses(
        (status = 200, description = "List of backup files in the configured backup directory", body = DataEnvelope<Vec<BackupListItem>>),
        (status = 400, description = "Unavailable in cluster mode — backups are node-local files"),
    )
)]
#[tracing::instrument(skip(state))]
pub(crate) async fn list_backups(State(state): State<AppState>) -> Result<Json<Value>, OrionError> {
    reject_in_cluster_mode(&state)?;
    let backup_dir = state.config.storage.backup_dir.clone();

    // Run all blocking filesystem I/O off the async runtime
    let backups = tokio::task::spawn_blocking(move || -> Result<Vec<Value>, OrionError> {
        let dir = match std::fs::read_dir(&backup_dir) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(e) => {
                return Err(OrionError::InternalSource {
                    context: format!("Failed to read backup directory '{backup_dir}'"),
                    source: Box::new(e),
                });
            }
        };

        let mut backups = Vec::new();
        for entry in dir.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "db")
                && path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("orion_backup_"))
                && let Ok(meta) = entry.metadata()
            {
                let modified = meta
                    .modified()
                    .ok()
                    .map(|t| {
                        let dt: chrono::DateTime<chrono::Utc> = t.into();
                        dt.to_rfc3339()
                    })
                    .unwrap_or_default();
                backups.push(json!({
                    "filename": path.file_name().unwrap_or_default().to_string_lossy(),
                    "size_bytes": meta.len(),
                    "modified_at": modified,
                }));
            }
        }

        // Sort by filename (which includes timestamp) descending
        backups.sort_by(|a, b| b["filename"].as_str().cmp(&a["filename"].as_str()));
        Ok(backups)
    })
    .await
    .map_err(|e| OrionError::Internal(format!("spawn_blocking failed: {e}")))??;

    Ok(data_response(backups))
}

#[cfg(test)]
mod tests {
    use super::prune_old_backups;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "orion_prune_test_{label}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&path).expect("test dir");
            Self(path)
        }

        fn touch(&self, name: &str) {
            std::fs::write(self.0.join(name), b"x").expect("test file");
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.0)
                .expect("test dir")
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }

        fn path(&self) -> &str {
            self.0.to_str().expect("utf-8 temp path")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Oldest-first pruning down to the retention count, reported by name.
    #[test]
    fn test_prunes_oldest_backups_beyond_retention() {
        let dir = TempDir::new("oldest");
        dir.touch("orion_backup_20240101_000000.db");
        dir.touch("orion_backup_20240102_000000.db");
        dir.touch("orion_backup_20240103_000000.db");
        dir.touch("orion_backup_20240104_000000.db");

        let mut pruned = prune_old_backups(dir.path(), 2);
        pruned.sort();

        assert_eq!(
            pruned,
            vec![
                "orion_backup_20240101_000000.db",
                "orion_backup_20240102_000000.db"
            ],
            "the two oldest must be pruned and reported"
        );
        assert_eq!(
            dir.names(),
            vec![
                "orion_backup_20240103_000000.db",
                "orion_backup_20240104_000000.db"
            ],
            "the two newest must survive"
        );
    }

    /// Files that are not `orion_backup_*.db` are never candidates — the
    /// live database or an operator's own files must not be collateral.
    #[test]
    fn test_prune_ignores_non_backup_files() {
        let dir = TempDir::new("foreign");
        dir.touch("orion.db");
        dir.touch("notes.txt");
        dir.touch("orion_backup_20240101_000000.db");
        dir.touch("orion_backup_20240102_000000.db");

        let pruned = prune_old_backups(dir.path(), 1);

        assert_eq!(pruned, vec!["orion_backup_20240101_000000.db"]);
        assert_eq!(
            dir.names(),
            vec!["notes.txt", "orion.db", "orion_backup_20240102_000000.db"],
            "unrelated files must be untouched"
        );
    }

    /// At or under the retention count nothing happens — including in an
    /// empty or missing directory.
    #[test]
    fn test_prune_is_a_noop_at_or_under_retention() {
        let dir = TempDir::new("noop");
        dir.touch("orion_backup_20240101_000000.db");

        assert!(prune_old_backups(dir.path(), 1).is_empty());
        assert!(prune_old_backups(dir.path(), 5).is_empty());
        assert_eq!(dir.names(), vec!["orion_backup_20240101_000000.db"]);
        assert!(prune_old_backups("/nonexistent/orion-prune-test", 1).is_empty());
    }
}
