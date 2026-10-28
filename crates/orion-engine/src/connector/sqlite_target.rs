//! One filesystem boundary for SQLite providers and database connections.

use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

#[cfg(test)]
use sqlx::ConnectOptions;
use sqlx::sqlite::SqliteConnectOptions;

#[derive(Debug, thiserror::Error)]
pub enum SqliteTargetError {
    #[error("Invalid SQLite connection string: {0}")]
    Url(#[from] sqlx::Error),
    #[error("{0}")]
    Boundary(&'static str),
    #[error("Cannot resolve SQLite filesystem boundary: {0}")]
    Filesystem(#[from] std::io::Error),
}

const RESERVED_NAMES: &[&str] = &["orion.db", "orion.sqlite", "orion-state.db"];

/// Parse a URL once, then replace its filename with the confined absolute path.
/// Passing these options directly to SQLx avoids decoding a filename twice.
pub fn confined_options(root: &Path, url: &str) -> Result<SqliteConnectOptions, SqliteTargetError> {
    let url = url.trim();
    if !url.starts_with("sqlite:") {
        return Err(SqliteTargetError::Boundary(
            "SQLite connections require a sqlite: connection string",
        ));
    }
    let options = SqliteConnectOptions::from_str(url)?;
    if is_memory_url(url) {
        return Ok(options);
    }
    let candidate = options.get_filename();
    if candidate.as_os_str().is_empty() {
        return Err(SqliteTargetError::Boundary(
            "SQLite file path must not be empty",
        ));
    }
    if candidate
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(SqliteTargetError::Boundary(
            "SQLite connector path must not contain '..' traversal segments",
        ));
    }
    let root = resolve_existing_prefix(&std::path::absolute(root)?)?;
    let target = resolve_existing_prefix(&root.join(candidate))?;
    if !target.starts_with(&root) || target == root {
        return Err(SqliteTargetError::Boundary(
            "SQLite connector path must resolve beneath engine.sqlite_connector_root",
        ));
    }
    for path in [candidate, target.as_path()] {
        if path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|name| {
                RESERVED_NAMES
                    .iter()
                    .any(|reserved| reserved.eq_ignore_ascii_case(name))
            })
        {
            return Err(SqliteTargetError::Boundary(
                "SQLite filename is reserved for Orion's configuration database",
            ));
        }
    }
    Ok(options.filename(target))
}

/// Match SQLx's explicit memory forms after its URL parser has validated them.
/// Avoid SQLx's URL formatter here: named memory filenames can contain a colon.
pub fn is_memory_url(url: &str) -> bool {
    let url = url
        .trim()
        .trim_start_matches("sqlite://")
        .trim_start_matches("sqlite:");
    let (filename, query) = url.split_once('?').unwrap_or((url, ""));
    filename == ":memory:"
        || url::form_urlencoded::parse(query.as_bytes())
            .any(|(key, value)| key == "mode" && value == "memory")
}

/// Resolve symlinks in existing ancestors without requiring the database or
/// its directory to exist. A dangling symlink is an error, not a missing leaf.
fn resolve_existing_prefix(path: &Path) -> std::io::Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => path.canonicalize(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or(e)?;
            let name = path.file_name().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid SQLite path")
            })?;
            Ok(resolve_existing_prefix(parent)?.join(name))
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
#[expect(clippy::panic)]
mod tests {
    use super::*;

    struct SqliteSecret(String);

    #[async_trait::async_trait]
    impl crate::connector::secrets::SecretResolver for SqliteSecret {
        fn scheme(&self) -> &'static str {
            "fixture"
        }
        async fn resolve(&self, _: &str) -> Result<String, crate::errors::OrionError> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn database_pools_confine_resolved_secrets_and_preserve_url_options() {
        use crate::connector::{
            ConnectorConfig,
            pool_cache::{SqlPool, SqlPoolCache},
        };
        let dir = tempfile::tempdir().expect("SQLite fixture is valid");
        let cache = SqlPoolCache::with_sqlite_root(4, dir.path());
        let config = |url: &str| {
            serde_json::from_value(serde_json::json!({
                "connection_string": url, "max_connections": 1,
            }))
            .expect("SQLite fixture is valid")
        };
        let SqlPool::Sqlite(pool) = cache
            .get_pool("orders", &config("sqlite:orders%252F.db?mode=rwc"))
            .await
            .expect("SQLite fixture is valid")
        else {
            panic!("sqlite")
        };
        sqlx::query("CREATE TABLE orders (id INTEGER)")
            .execute(&pool)
            .await
            .expect("SQLite fixture is valid");
        assert!(
            dir.path().join("orders%2F.db").exists(),
            "filename must be decoded exactly once"
        );
        pool.close().await;
        let SqlPool::Sqlite(read_only) = cache
            .get_pool("read-only", &config("sqlite:orders%252F.db?mode=ro"))
            .await
            .expect("SQLite fixture is valid")
        else {
            panic!("sqlite")
        };
        assert!(
            sqlx::query("INSERT INTO orders VALUES (1)")
                .execute(&read_only)
                .await
                .is_err()
        );
        read_only.close().await;
        let outside = dir
            .path()
            .parent()
            .expect("SQLite fixture is valid")
            .join("outside.db");
        let resolvers: Vec<Box<dyn crate::connector::secrets::SecretResolver>> = vec![Box::new(
            SqliteSecret(format!("sqlite:{}?mode=rwc", outside.display())),
        )];
        let resolved = crate::connector::resolve_connector_config(
            "secret-db",
            "secret-db",
            "db",
            r#"{"connection_string":"fixture://target"}"#,
            None,
            &resolvers,
        )
        .await
        .expect("SQLite fixture is valid");
        let ConnectorConfig::Db(db) = resolved else {
            panic!("db")
        };
        assert!(cache.get_pool("secret-db", &db).await.is_err());
        assert!(!outside.exists());
        assert!(
            cache
                .get_pool("reserved", &config("sqlite:orion.db?mode=rwc"))
                .await
                .is_err()
        );
        assert!(!dir.path().join("orion.db").exists());
    }

    #[test]
    fn parsed_paths_obey_the_boundary_and_keep_options() {
        let dir = tempfile::tempdir().expect("SQLite fixture is valid");
        let root = dir
            .path()
            .canonicalize()
            .expect("SQLite fixture is valid")
            .join("databases");
        let options = confined_options(&root, "sqlite:tenant/orders.db?mode=rwc&cache=shared")
            .expect("SQLite fixture is valid");
        assert_eq!(options.get_filename(), root.join("tenant/orders.db"));
        let normalized = options.to_url_lossy();
        assert!(
            normalized
                .query_pairs()
                .any(|(k, v)| k == "mode" && v == "rwc")
        );
        assert!(
            normalized
                .query_pairs()
                .any(|(k, v)| k == "cache" && v == "shared")
        );
        let absolute = format!("sqlite:{}?mode=ro", root.join("orders.db").display());
        assert!(
            confined_options(&root, &absolute)
                .expect("SQLite fixture is valid")
                .to_url_lossy()
                .query_pairs()
                .any(|(k, v)| k == "mode" && v == "ro")
        );
        for url in [
            "sqlite:../outside.db",
            "sqlite:%2e%2e/outside.db",
            "sqlite:/etc/outside.db",
            "sqlite:%2fetc/outside.db",
            "sqlite:orion.db",
            "sqlite:orion%2edb",
            "sqlite:",
        ] {
            assert!(confined_options(&root, url).is_err(), "{url}");
        }
        for url in [
            "sqlite::memory:",
            "sqlite://:memory:",
            "sqlite:private?mode=memory&cache=private",
        ] {
            assert!(
                {
                    confined_options(&root, url).expect("SQLite fixture is valid");
                    is_memory_url(url)
                },
                "{url}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_escape_or_hide_reserved_files() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().expect("SQLite fixture is valid");
        let root = dir.path().join("root");
        std::fs::create_dir(&root).expect("SQLite fixture is valid");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).expect("SQLite fixture is valid");
        symlink(&outside, root.join("escape")).expect("SQLite fixture is valid");
        assert!(confined_options(&root, "sqlite:escape/new.db?mode=rwc").is_err());
        symlink(outside.join("missing.db"), root.join("dangling.db"))
            .expect("SQLite fixture is valid");
        assert!(confined_options(&root, "sqlite:dangling.db?mode=rwc").is_err());
        std::fs::write(root.join("orion.db"), b"").expect("SQLite fixture is valid");
        symlink(root.join("orion.db"), root.join("alias.db")).expect("SQLite fixture is valid");
        assert!(confined_options(&root, "sqlite:alias.db").is_err());
        symlink(&root, dir.path().join("root-alias")).expect("SQLite fixture is valid");
        assert!(confined_options(&dir.path().join("root-alias"), "sqlite:orders.db").is_ok());
    }
}
