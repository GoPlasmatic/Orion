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
        return Err(OrionError::validation(
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

    // One backup at a time on this node (B4). Held to the end of the handler,
    // so it also covers the retention prune below — see
    // `AppStateInner::backup_lock` for why concurrent backups are not just a
    // naming problem.
    let _backup_guard = state.backup_lock.lock().await;

    // Directory and destination together, in one trip off the async runtime:
    // choosing a free name is a filesystem question, and asking it anywhere
    // else means asking it before the directory exists.
    let dir = backup_dir.clone();
    let (filename, backup_path) = tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&dir)?;
        Ok::<_, std::io::Error>(unique_backup_path(&dir, chrono::Utc::now()))
    })
    .await
    .map_err(|e| OrionError::internal_from("spawn_blocking failed", e))?
    .map_err(|e| OrionError::Internal {
        context: format!("Failed to create backup directory '{backup_dir}'"),
        source: Some(Box::new(e)),
    })?;
    let backup_path_str = backup_path.to_string_lossy().to_string();

    // VACUUM INTO is SQLite-specific. R26: the pool variant is matched inside
    // `AppStateInner`, not here — a route handler has no business unwrapping a
    // concrete `sqlx` pool.
    let backed_up = state
        .backup_sqlite_into(&backup_path_str)
        .await
        .map_err(|e| OrionError::Internal {
            context: "Failed to create database backup".to_string(),
            source: Some(Box::new(e)),
        })?;
    if !backed_up {
        return Err(OrionError::validation(
            "Database backup via VACUUM INTO is only supported for SQLite".to_string(),
        ));
    }

    let meta_path = backup_path.clone();
    let metadata = tokio::task::spawn_blocking(move || std::fs::metadata(&meta_path))
        .await
        .map_err(|e| OrionError::internal_from("spawn_blocking failed", e))?
        .map_err(|e| OrionError::Internal {
            context: "Failed to read backup file metadata".to_string(),
            source: Some(Box::new(e)),
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
        &state.audit_queue,
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

/// A backup destination in `dir` that does not already exist, and its
/// filename.
///
/// `VACUUM INTO` refuses a destination that exists, so two backups that agree
/// on a name are one backup and one 500. The name used to carry seconds only,
/// which made "the same name" mean "the same second" — reachable by two
/// operators, a script, and any scheduler that fires on a boundary.
///
/// Two defences, because neither is enough alone. Milliseconds narrow the
/// window; the collision suffix closes it, because a clock has no resolution
/// that a caller cannot land twice inside. The suffix runs under
/// `AppStateInner::backup_lock`, so the check and the `VACUUM INTO` that
/// follows cannot interleave with another backup on this node — and nothing
/// else writes this directory (cluster mode refuses backups entirely).
///
/// **Lexicographic order must stay chronological order.** It is the whole
/// basis of retention: [`prune_old_backups`] sorts these names and deletes
/// from the front, and `list_backups` sorts them descending to answer
/// "newest first". Both parts hold. Milliseconds are zero-padded and appended
/// in place, so they order within a second. The suffix separator is `_`
/// (0x5F), which sorts *after* the `.` (0x2E) of an unsuffixed name, so
/// `…_123.db` < `…_123_1.db` < `…_124.db` — later files sort later. A `-`
/// separator would have inverted the first of those.
fn unique_backup_path(
    dir: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> (String, std::path::PathBuf) {
    /// Enough that reaching the end means something other than a name
    /// collision is wrong. Each attempt is one `stat` of a path that almost
    /// never exists.
    const MAX_ATTEMPTS: u32 = 64;

    let stamp = now.format("%Y%m%d_%H%M%S_%3f");
    let mut candidate = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        candidate = match attempt {
            0 => format!("orion_backup_{stamp}.db"),
            n => format!("orion_backup_{stamp}_{n}.db"),
        };
        let path = std::path::Path::new(dir).join(&candidate);
        if !path.exists() {
            return (candidate, path);
        }
    }
    // Every candidate for this millisecond is taken, which the lock makes
    // essentially impossible. Hand back the last one rather than inventing a
    // name that breaks the ordering above: `VACUUM INTO` then fails saying the
    // file exists, which is the truth.
    let path = std::path::Path::new(dir).join(&candidate);
    (candidate, path)
}

/// Whether a directory entry is one of Orion's own backup files.
///
/// Shared by `prune_old_backups` and `list_backups` so the pruner can never
/// disagree with the lister about what a backup is.
fn is_backup_file(path: &std::path::Path) -> bool {
    path.extension().is_some_and(|ext| ext == "db")
        && path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("orion_backup_"))
}

/// Delete the oldest `orion_backup_*.db` files in `backup_dir` so that at
/// most `retain` remain, returning the filenames actually removed.
///
/// Filenames embed a sortable UTC timestamp (see [`unique_backup_path`]), so
/// lexicographic order is chronological order — the same invariant
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
            is_backup_file(&path).then(|| entry.file_name().to_string_lossy().into_owned())
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
                return Err(OrionError::Internal {
                    context: format!("Failed to read backup directory '{backup_dir}'"),
                    source: Some(Box::new(e)),
                });
            }
        };

        let mut backups = Vec::new();
        for entry in dir.flatten() {
            let path = entry.path();
            if is_backup_file(&path)
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
    .map_err(|e| OrionError::internal_from("spawn_blocking failed", e))??;

    Ok(data_response(backups))
}

#[cfg(test)]
mod tests {
    use super::{prune_old_backups, unique_backup_path};

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

    /// B4: two backups in the same millisecond must not choose the same
    /// destination. `VACUUM INTO` refuses an existing file, so a shared name
    /// is one backup and one 500 — and with second precision that was every
    /// pair of requests inside the same second.
    #[test]
    fn two_backups_in_one_instant_get_different_names() {
        let dir = TempDir::new("collide");
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-04T12:00:00.123Z")
            .expect("test timestamp")
            .with_timezone(&chrono::Utc);

        let (first, first_path) = unique_backup_path(dir.path(), now);
        std::fs::write(&first_path, b"x").expect("test file");
        // The same instant, exactly as a second concurrent request sees it.
        let (second, _) = unique_backup_path(dir.path(), now);

        assert_ne!(first, second, "the second backup would overwrite the first");
        assert_eq!(first, "orion_backup_20260904_120000_123.db");
        assert_eq!(second, "orion_backup_20260904_120000_123_1.db");
    }

    /// Retention depends on it: `prune_old_backups` deletes from the front of
    /// a lexicographic sort and `list_backups` reverses one, so a name that
    /// sorts out of order deletes the wrong file.
    #[test]
    fn names_sort_chronologically_including_collisions() {
        let dir = TempDir::new("order");
        let at = |ms: &str| {
            chrono::DateTime::parse_from_rfc3339(&format!("2026-09-04T12:00:00.{ms}Z"))
                .expect("test timestamp")
                .with_timezone(&chrono::Utc)
        };

        // Two in the same millisecond, then one in the next.
        let (a, a_path) = unique_backup_path(dir.path(), at("123"));
        std::fs::write(&a_path, b"x").expect("test file");
        let (b, b_path) = unique_backup_path(dir.path(), at("123"));
        std::fs::write(&b_path, b"x").expect("test file");
        let (c, _) = unique_backup_path(dir.path(), at("124"));

        let mut sorted = vec![c.clone(), b.clone(), a.clone()];
        sorted.sort();
        assert_eq!(
            sorted,
            vec![a, b, c],
            "creation order and lexicographic order must agree"
        );
    }

    /// The suffix keeps climbing rather than settling on the first collision.
    #[test]
    fn a_third_collision_takes_the_next_suffix() {
        let dir = TempDir::new("third");
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-04T12:00:00.500Z")
            .expect("test timestamp")
            .with_timezone(&chrono::Utc);

        for expected in [
            "orion_backup_20260904_120000_500.db",
            "orion_backup_20260904_120000_500_1.db",
            "orion_backup_20260904_120000_500_2.db",
        ] {
            let (name, path) = unique_backup_path(dir.path(), now);
            assert_eq!(name, expected);
            std::fs::write(&path, b"x").expect("test file");
        }
    }

    /// A name produced here must be one the pruner and the lister recognise —
    /// they filter on the prefix and extension, and a format change that broke
    /// that would silently stop retention working.
    #[test]
    fn a_generated_name_is_recognised_as_a_backup_file() {
        let dir = TempDir::new("recognised");
        let (_, path) = unique_backup_path(dir.path(), chrono::Utc::now());
        assert!(super::is_backup_file(&path), "{}", path.display());
    }
}
