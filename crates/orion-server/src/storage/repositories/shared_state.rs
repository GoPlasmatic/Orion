//! State the nodes of a cluster share through the database
//! (`cluster_shared_state`): a key/value store with expiry, for the default
//! dedup store, the default response cache and namespace counters, and
//! fleet-wide fixed rate-limit windows.
//!
//! Every expiry and every window is judged on the database's clock
//! (`datetime('now')` / `LOCALTIMESTAMP` / `UTC_TIMESTAMP()`), the one clock
//! the whole fleet shares, so a node with a skewed clock cannot land in a
//! window or outlive a claim of its own. Those clock expressions are the only
//! raw SQL here (`Expr::cust`).
//!
//! Keys are stored as SHA-256 hex of the caller's key, so a cache key of any
//! length fits the fixed-width primary key.
//!
//! What has to be atomic across nodes is: a claim is won by exactly one
//! claimant, and an increment is never lost. A claim is one conditional
//! statement at a time (insert if absent, else take over if expired), so two
//! nodes can never both see success. An increment opens a write transaction
//! with an upsert that touches the row, which takes its exclusive lock in the
//! first statement (and creates it when absent), then reads and rewrites it
//! under that lock. Taking the lock any later deadlocks on MySQL: an
//! `INSERT IGNORE` that finds the row holds a shared lock, and two
//! transactions upgrading theirs wait on each other. A rate window is one
//! upsert that adds to the count.

use sea_query::{Expr, ExprTrait, LockType, OnConflict, Query, SimpleExpr};
use sha2::{Digest, Sha256};

use crate::errors::OrionError;
use crate::storage::schema::{ClusterKv, ClusterRateWindows};
use crate::storage::{DbBackend, DbPool, DbTransaction, build_sqlx};

use super::helpers::{insert_if_absent, sql_now, sql_now_plus_secs};

/// How long a finished rate-limit window is kept before the sweep removes it.
/// Windows are one second wide; a few spare seconds keep a slow request's
/// window from being swept while it is still being counted.
const WINDOW_RETENTION_SECS: i64 = 10;

/// Attempts a claim makes when the key it lost to vanished before it could
/// read the holder (removed, or swept, between the two statements).
const CLAIM_ATTEMPTS: usize = 3;

/// The shared store over the node's state database.
#[derive(Clone)]
pub struct SqlSharedState {
    pool: DbPool,
}

impl SqlSharedState {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    fn backend(&self) -> DbBackend {
        self.pool.backend()
    }

    /// The value at `key`, unless absent or expired.
    pub async fn get(&self, key: &str) -> Result<Option<String>, OrionError> {
        let backend = self.backend();
        let (sql, values) = build_sqlx(
            backend,
            Query::select()
                .column(ClusterKv::Value)
                .from(ClusterKv::Table)
                .and_where(Expr::col(ClusterKv::KeyHash).eq(key_hash(key)))
                .and_where(live(backend)),
        );
        let row: Option<(String,)> = self.pool.fetch_optional_as(&sql, values).await?;
        Ok(row.map(|(value,)| value))
    }

    /// The values at `keys`, in the order asked, `None` for a miss.
    pub async fn get_many(&self, keys: &[String]) -> Result<Vec<Option<String>>, OrionError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let backend = self.backend();
        let hashes: Vec<String> = keys.iter().map(|k| key_hash(k)).collect();
        let (sql, values) = build_sqlx(
            backend,
            Query::select()
                .columns([ClusterKv::KeyHash, ClusterKv::Value])
                .from(ClusterKv::Table)
                .and_where(Expr::col(ClusterKv::KeyHash).is_in(hashes.clone()))
                .and_where(live(backend)),
        );
        let rows: Vec<(String, String)> = self.pool.fetch_all_as(&sql, values).await?;
        let found: std::collections::HashMap<String, String> = rows.into_iter().collect();
        Ok(hashes.iter().map(|h| found.get(h).cloned()).collect())
    }

    /// Store `value` at `key`, replacing what is there, expiring after
    /// `ttl_secs` (never, with `None`).
    pub async fn set(
        &self,
        key: &str,
        value: &str,
        ttl_secs: Option<u64>,
    ) -> Result<(), OrionError> {
        let backend = self.backend();
        let mut insert = Query::insert()
            .into_table(ClusterKv::Table)
            .columns([ClusterKv::KeyHash, ClusterKv::Value, ClusterKv::ExpiresAt])
            .values_panic([
                key_hash(key).into(),
                value.into(),
                expiry(backend, ttl_secs),
            ])
            .to_owned();
        insert.on_conflict(
            OnConflict::column(ClusterKv::KeyHash)
                .update_columns([ClusterKv::Value, ClusterKv::ExpiresAt])
                .to_owned(),
        );
        let (sql, values) = build_sqlx(backend, &mut insert);
        self.pool.execute_query(&sql, values).await?;
        Ok(())
    }

    /// Delete `keys`, answering how many were present and live.
    pub async fn remove_many(&self, keys: &[String]) -> Result<u64, OrionError> {
        if keys.is_empty() {
            return Ok(0);
        }
        let backend = self.backend();
        let hashes: Vec<String> = keys.iter().map(|k| key_hash(k)).collect();
        // Expired rows go too, but only live ones count: an expired key is
        // already absent to every reader.
        let (sql, values) = build_sqlx(
            backend,
            Query::delete()
                .from_table(ClusterKv::Table)
                .and_where(Expr::col(ClusterKv::KeyHash).is_in(hashes.clone()))
                .and_where(live(backend)),
        );
        let deleted = self.pool.execute_query(&sql, values).await?;
        let (sql, values) = build_sqlx(
            backend,
            Query::delete()
                .from_table(ClusterKv::Table)
                .and_where(Expr::col(ClusterKv::KeyHash).is_in(hashes)),
        );
        self.pool.execute_query(&sql, values).await?;
        Ok(deleted)
    }

    /// Claim `key` for `owner` for `window_secs`: `None` when the claim is
    /// won (the key was free or expired), `Some(holder)` when someone holds it.
    pub async fn claim(
        &self,
        key: &str,
        owner: &str,
        window_secs: u64,
    ) -> Result<Option<String>, OrionError> {
        let backend = self.backend();
        let hash = key_hash(key);
        let until = Expr::cust(sql_now_plus_secs(backend, window_secs));
        for _ in 0..CLAIM_ATTEMPTS {
            let insert = Query::insert()
                .into_table(ClusterKv::Table)
                .columns([ClusterKv::KeyHash, ClusterKv::Value, ClusterKv::ExpiresAt])
                .values_panic([hash.clone().into(), owner.into(), until.clone()])
                .to_owned();
            if insert_if_absent(&self.pool, insert, ClusterKv::KeyHash).await? > 0 {
                return Ok(None);
            }
            // Present: take it over only if its window has passed.
            let (sql, values) = build_sqlx(
                backend,
                Query::update()
                    .table(ClusterKv::Table)
                    .value(ClusterKv::Value, owner)
                    .value(ClusterKv::ExpiresAt, until.clone())
                    .and_where(Expr::col(ClusterKv::KeyHash).eq(hash.clone()))
                    .and_where(Expr::col(ClusterKv::ExpiresAt).lte(now(backend))),
            );
            if self.pool.execute_query(&sql, values).await? > 0 {
                return Ok(None);
            }
            if let Some(holder) = self.get(key).await? {
                return Ok(Some(holder));
            }
            // Removed between the statements above: try again.
        }
        Err(OrionError::internal(format!(
            "shared store: claim on a key that kept vanishing ({CLAIM_ATTEMPTS} attempts)"
        )))
    }

    /// Add `by` to the integer at `key` and answer the new value. A missing or
    /// expired key counts as `0`, and `ttl_secs` applies only when it is
    /// created, as `INCRBY` + `EXPIRE NX` behave. A value that is not an
    /// integer is an error.
    pub async fn incr_by(
        &self,
        key: &str,
        by: i64,
        ttl_secs: Option<u64>,
    ) -> Result<i64, OrionError> {
        let backend = self.backend();
        let hash = key_hash(key);
        let mut tx = self.pool.begin_write_tx().await?;
        // Created as `0` with the creation ttl, or touched (a no-op update of
        // its own key) so this transaction holds its lock from here on.
        let mut upsert = Query::insert()
            .into_table(ClusterKv::Table)
            .columns([ClusterKv::KeyHash, ClusterKv::Value, ClusterKv::ExpiresAt])
            .values_panic([hash.clone().into(), "0".into(), expiry(backend, ttl_secs)])
            .to_owned();
        upsert.on_conflict(
            OnConflict::column(ClusterKv::KeyHash)
                .update_column(ClusterKv::KeyHash)
                .to_owned(),
        );
        let (sql, values) = build_sqlx(backend, &mut upsert);
        tx.execute_query(&sql, values).await?;

        let (value, is_live) = locked_value(&mut tx, &hash).await?;
        let (next, reset) = if !is_live {
            (by, true)
        } else {
            let current: i64 = value.trim().parse().map_err(|_| {
                OrionError::validation(format!(
                    "value at cache key '{key}' is not an integer, so it cannot be incremented"
                ))
            })?;
            let next = current.checked_add(by).ok_or_else(|| {
                OrionError::validation(format!("incrementing cache key '{key}' overflows"))
            })?;
            (next, false)
        };
        let mut update = Query::update()
            .table(ClusterKv::Table)
            .value(ClusterKv::Value, next.to_string())
            .and_where(Expr::col(ClusterKv::KeyHash).eq(hash))
            .to_owned();
        if reset {
            // An expired counter starts over, with a fresh expiry.
            update.value(ClusterKv::ExpiresAt, expiry(backend, ttl_secs));
        }
        let (sql, values) = build_sqlx(backend, &mut update);
        tx.execute_query(&sql, values).await?;
        tx.commit().await?;
        Ok(next)
    }

    /// Count one hit against `limiter_key` in the current one-second window
    /// on the database's clock, answering the window's count after it.
    pub async fn hit_window(&self, limiter_key: &str) -> Result<i64, OrionError> {
        let backend = self.backend();
        let hash = key_hash(limiter_key);
        let mut tx = self.pool.begin_write_tx().await?;
        let (sql, values) = build_sqlx(backend, Query::select().expr(epoch_secs(backend)));
        let (window,): (i64,) = tx.fetch_optional_as(&sql, values).await?.ok_or_else(|| {
            OrionError::internal("shared store: the clock query returned nothing")
        })?;

        // One statement: the window's first hit creates it at 1, every later
        // one adds to it under the row lock the upsert takes.
        let mut upsert = Query::insert()
            .into_table(ClusterRateWindows::Table)
            .columns([
                ClusterRateWindows::LimiterKey,
                ClusterRateWindows::WindowStart,
                ClusterRateWindows::Hits,
            ])
            .values_panic([hash.clone().into(), window.into(), 1i64.into()])
            .to_owned();
        upsert.on_conflict(
            OnConflict::columns([
                ClusterRateWindows::LimiterKey,
                ClusterRateWindows::WindowStart,
            ])
            .value(
                ClusterRateWindows::Hits,
                Expr::col((ClusterRateWindows::Table, ClusterRateWindows::Hits)).add(1),
            )
            .to_owned(),
        );
        let (sql, values) = build_sqlx(backend, &mut upsert);
        tx.execute_query(&sql, values).await?;
        let in_window = || {
            Expr::col(ClusterRateWindows::LimiterKey)
                .eq(hash.clone())
                .and(Expr::col(ClusterRateWindows::WindowStart).eq(window))
        };
        let (sql, values) = build_sqlx(
            backend,
            Query::select()
                .column(ClusterRateWindows::Hits)
                .from(ClusterRateWindows::Table)
                .and_where(in_window()),
        );
        let (hits,): (i64,) = tx.fetch_optional_as(&sql, values).await?.ok_or_else(|| {
            OrionError::internal("shared store: a rate window vanished mid-count")
        })?;
        tx.commit().await?;
        Ok(hits)
    }

    /// Delete expired keys and finished rate windows, answering how many rows
    /// went. Safe on every node at once: each statement only removes what is
    /// already dead to every reader.
    pub async fn sweep(&self) -> Result<u64, OrionError> {
        let backend = self.backend();
        let (sql, values) = build_sqlx(
            backend,
            Query::delete()
                .from_table(ClusterKv::Table)
                .and_where(Expr::col(ClusterKv::ExpiresAt).lte(now(backend))),
        );
        let keys = self.pool.execute_query(&sql, values).await?;
        let (sql, values) = build_sqlx(
            backend,
            Query::delete()
                .from_table(ClusterRateWindows::Table)
                .and_where(
                    Expr::col(ClusterRateWindows::WindowStart)
                        .lt(epoch_secs(backend).sub(WINDOW_RETENTION_SECS)),
                ),
        );
        let windows = self.pool.execute_query(&sql, values).await?;
        Ok(keys + windows)
    }
}

/// The value at `hash` and whether it is live, read under a row lock.
async fn locked_value(tx: &mut DbTransaction, hash: &str) -> Result<(String, bool), OrionError> {
    let backend = tx.backend();
    let mut select = Query::select()
        .column(ClusterKv::Value)
        .expr(Expr::case(live(backend), 1i64).finally(0i64))
        .from(ClusterKv::Table)
        .and_where(Expr::col(ClusterKv::KeyHash).eq(hash))
        .to_owned();
    // SQLite has no row locks; its write transaction already holds the
    // database's.
    if backend != DbBackend::Sqlite {
        select.lock(LockType::Update);
    }
    let (sql, values) = build_sqlx(backend, &mut select);
    let (value, is_live): (String, i64) = tx
        .fetch_optional_as(&sql, values)
        .await?
        .ok_or_else(|| OrionError::internal("shared store: a counter vanished mid-increment"))?;
    Ok((value, is_live == 1))
}

fn key_hash(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

fn now(backend: DbBackend) -> SimpleExpr {
    Expr::cust(sql_now(backend))
}

/// `now + ttl_secs`, or no expiry.
fn expiry(backend: DbBackend, ttl_secs: Option<u64>) -> SimpleExpr {
    match ttl_secs {
        Some(secs) => Expr::cust(sql_now_plus_secs(backend, secs)),
        None => Expr::cust("NULL"),
    }
}

/// A row that has not expired.
fn live(backend: DbBackend) -> SimpleExpr {
    Expr::col(ClusterKv::ExpiresAt)
        .is_null()
        .or(Expr::col(ClusterKv::ExpiresAt).gt(now(backend)))
}

/// The database's current Unix second.
fn epoch_secs(backend: DbBackend) -> SimpleExpr {
    Expr::cust(match backend {
        DbBackend::Sqlite => "CAST(strftime('%s', 'now') AS INTEGER)",
        DbBackend::Postgres => "CAST(EXTRACT(EPOCH FROM CURRENT_TIMESTAMP) AS BIGINT)",
        DbBackend::Mysql => "UNIX_TIMESTAMP()",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> SqlSharedState {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("the shared store answers");
        crate::storage::migrator_for(DbBackend::Sqlite)
            .run(&pool)
            .await
            .expect("the shared store answers");
        SqlSharedState::new(DbPool::Sqlite(pool))
    }

    #[tokio::test]
    async fn values_round_trip_and_expire() {
        let s = store().await;
        assert_eq!(s.get("a").await.expect("the shared store answers"), None);
        s.set("a", "1", None)
            .await
            .expect("the shared store answers");
        s.set("a", "2", None)
            .await
            .expect("the shared store answers");
        assert_eq!(
            s.get("a")
                .await
                .expect("the shared store answers")
                .as_deref(),
            Some("2")
        );
        s.set("b", "x", Some(0))
            .await
            .expect("the shared store answers");
        assert_eq!(
            s.get("b").await.expect("the shared store answers"),
            None,
            "a zero ttl is already expired"
        );
        assert_eq!(
            s.get_many(&["b".into(), "a".into(), "c".into()])
                .await
                .expect("the shared store answers"),
            vec![None, Some("2".to_string()), None]
        );
        assert_eq!(
            s.remove_many(&["a".into(), "b".into()])
                .await
                .expect("the shared store answers"),
            1
        );
        assert_eq!(s.get("a").await.expect("the shared store answers"), None);
    }

    #[tokio::test]
    async fn a_claim_is_won_once_until_it_expires() {
        let s = store().await;
        assert_eq!(
            s.claim("k", "first", 60)
                .await
                .expect("the shared store answers"),
            None
        );
        assert_eq!(
            s.claim("k", "second", 60)
                .await
                .expect("the shared store answers")
                .as_deref(),
            Some("first")
        );
        // An expired claim is taken over.
        assert_eq!(
            s.claim("short", "a", 0)
                .await
                .expect("the shared store answers"),
            None
        );
        assert_eq!(
            s.claim("short", "b", 60)
                .await
                .expect("the shared store answers"),
            None
        );
        assert_eq!(
            s.get("short")
                .await
                .expect("the shared store answers")
                .as_deref(),
            Some("b")
        );
    }

    #[tokio::test]
    async fn an_increment_counts_from_zero_and_refuses_text() {
        let s = store().await;
        assert_eq!(
            s.incr_by("n", 2, Some(60))
                .await
                .expect("the shared store answers"),
            2
        );
        assert_eq!(
            s.incr_by("n", 3, Some(60))
                .await
                .expect("the shared store answers"),
            5
        );
        assert_eq!(
            s.get("n")
                .await
                .expect("the shared store answers")
                .as_deref(),
            Some("5")
        );
        s.set("t", "text", None)
            .await
            .expect("the shared store answers");
        assert!(s.incr_by("t", 1, None).await.is_err());
        // An expired counter starts over.
        assert_eq!(
            s.incr_by("e", 7, Some(0))
                .await
                .expect("the shared store answers"),
            7
        );
        assert_eq!(
            s.incr_by("e", 1, Some(60))
                .await
                .expect("the shared store answers"),
            1
        );
    }

    #[tokio::test]
    async fn hits_count_within_a_window_and_the_sweep_clears_the_dead() {
        let s = store().await;
        let first = s.hit_window("rl").await.expect("the shared store answers");
        let second = s.hit_window("rl").await.expect("the shared store answers");
        // The two may straddle a second boundary; within one they count up.
        assert!(second == first + 1 || second == 1, "{first} then {second}");
        s.set("gone", "x", Some(0))
            .await
            .expect("the shared store answers");
        assert!(s.sweep().await.expect("the shared store answers") >= 1);
    }
}
