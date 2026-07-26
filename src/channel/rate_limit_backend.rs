//! Per-channel rate-limit backends (multi-instance-ha A4).
//!
//! Single node: governor's in-process GCRA (`local`) — precise and free.
//! Cluster: a shared-Redis fixed window, so a configured limit holds across
//! all replicas combined instead of multiplying by the node count. Fixed
//! window (`INCR` + `EXPIRE` on a per-second key, one atomic pipeline, no
//! Lua) trades up to 2x burst at a window boundary for simplicity — the
//! configured burst allowance absorbs it in practice.

use async_trait::async_trait;

use super::{KeyedLimiter, build_keyed_limiter};

#[async_trait]
pub trait RateLimitBackend: Send + Sync {
    /// `true` = allow, `false` = reject with 429. Implementations fail open
    /// (allow + error metric) when their backing store is unreachable —
    /// consistent with the dedup fail-open policy. Takes the key by value:
    /// callers already own one, and governor's keyed store wants `&String`.
    async fn check(&self, key: String) -> bool;
}

/// In-process governor limiter (today's behavior; N replicas = N× the limit).
pub struct LocalRateLimitBackend {
    limiter: KeyedLimiter,
}

impl LocalRateLimitBackend {
    pub fn new(rps: u32, burst: u32) -> Self {
        Self {
            limiter: build_keyed_limiter(rps, burst),
        }
    }
}

#[async_trait]
impl RateLimitBackend for LocalRateLimitBackend {
    async fn check(&self, key: String) -> bool {
        self.limiter.check_key(&key).is_ok()
    }
}

/// Shared fixed-window limiter on the cluster Redis.
///
/// Key: `orion:rl:{channel}:{key}:{unix_second}`, TTL 2 s (each window is a
/// fresh key, so the every-call EXPIRE is harmless). Window limit = rps +
/// burst, mapping the per-channel `requests_per_second`/`burst` config onto
/// a one-second window.
pub struct RedisRateLimitBackend {
    conn: redis::aio::MultiplexedConnection,
    scope: String,
    limit_per_window: u32,
}

impl RedisRateLimitBackend {
    pub fn new(
        conn: redis::aio::MultiplexedConnection,
        scope: String,
        rps: u32,
        burst: u32,
    ) -> Self {
        Self {
            conn,
            scope,
            limit_per_window: rps.saturating_add(burst).max(1),
        }
    }
}

#[async_trait]
impl RateLimitBackend for RedisRateLimitBackend {
    async fn check(&self, key: String) -> bool {
        let window = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let redis_key = format!("orion:rl:{}:{}:{}", self.scope, key, window);
        let mut conn = self.conn.clone();
        let result: Result<(i64, i64), redis::RedisError> = redis::pipe()
            .atomic()
            .incr(&redis_key, 1u32)
            .expire(&redis_key, 2)
            .query_async(&mut conn)
            .await;
        match result {
            Ok((count, _)) => count <= i64::from(self.limit_per_window),
            Err(e) => {
                crate::metrics::record_error("rate_limit_backend");
                tracing::warn!(
                    scope = %self.scope,
                    error = %e,
                    "Rate-limit Redis error; failing open (request allowed)"
                );
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_local_backend_enforces_burst() {
        let backend = LocalRateLimitBackend::new(1, 2);
        // Burst capacity of 2 → first two pass, third rejected.
        assert!(backend.check("ip-1".to_string()).await);
        assert!(backend.check("ip-1".to_string()).await);
        assert!(!backend.check("ip-1".to_string()).await);
        // Independent key unaffected.
        assert!(backend.check("ip-2".to_string()).await);
    }
}
