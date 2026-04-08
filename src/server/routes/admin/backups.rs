use axum::extract::State;
use axum::{Extension, Json};
use serde_json::{Value, json};

use crate::errors::OrionError;
use crate::server::admin_auth::AdminPrincipal;
use crate::server::routes::response_helpers::data_response;
use crate::server::state::AppState;

use super::audit_log;

// ============================================================
// Backups (SQLite only)
// ============================================================

pub(crate) async fn create_backup(
    State(state): State<AppState>,
    principal: Option<Extension<AdminPrincipal>>,
) -> Result<Json<Value>, OrionError> {
    let backup_dir = &state.config.storage.backup_dir;

    std::fs::create_dir_all(backup_dir).map_err(|e| OrionError::InternalSource {
        context: format!("Failed to create backup directory '{backup_dir}'"),
        source: Box::new(e),
    })?;

    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let filename = format!("orion_backup_{timestamp}.db");
    let backup_path = std::path::Path::new(backup_dir).join(&filename);
    let backup_path_str = backup_path.to_string_lossy().to_string();

    // VACUUM INTO is SQLite-specific
    match &state.db_pool {
        crate::storage::DbPool::Sqlite(p) => {
            sqlx::query(&format!(
                "VACUUM INTO '{}'",
                backup_path_str.replace('\'', "''")
            ))
            .execute(p)
            .await
            .map_err(|e| OrionError::InternalSource {
                context: "Failed to create database backup".to_string(),
                source: Box::new(e),
            })?;
        }
        _ => {
            return Err(OrionError::BadRequest(
                "Database backup via VACUUM INTO is only supported for SQLite".to_string(),
            ));
        }
    }

    let metadata = std::fs::metadata(&backup_path).map_err(|e| OrionError::InternalSource {
        context: "Failed to read backup file metadata".to_string(),
        source: Box::new(e),
    })?;

    audit_log(
        &state.audit_log_repo,
        &principal,
        "create",
        "backup",
        &filename,
    );

    Ok(Json(json!({
        "data": {
            "filename": filename,
            "path": backup_path_str,
            "size_bytes": metadata.len(),
            "created_at": chrono::Utc::now().to_rfc3339(),
        }
    })))
}

pub(crate) async fn list_backups(State(state): State<AppState>) -> Result<Json<Value>, OrionError> {
    let backup_dir = &state.config.storage.backup_dir;

    let dir = match std::fs::read_dir(backup_dir) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(data_response(Vec::<String>::new()));
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

    Ok(data_response(backups))
}
