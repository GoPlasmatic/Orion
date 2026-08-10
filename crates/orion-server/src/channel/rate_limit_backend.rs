//! Per-channel rate-limit backends (multi-instance-ha A4).
//!
//! Single node: governor's in-process GCRA (`local`) — precise and free.
//! Cluster: a shared-Redis fixed window, so a configured limit holds across
//! all replicas combined instead of multiplying by the node count. The
//! window is derived from **Redis's own clock** (`TIME`, inside the Lua
//! script that also increments — N8), so every replica ages into the same
//! bucket no matter how its local clock drifts. Fixed windows trade up to 2x
//! burst at a window boundary for simplicity — the configured burst
//! allowance absorbs it in practice.

use async_trait::async_trait;

use super::{KeyedLimiter, build_keyed_limiter};
use crate::errors::OrionError;

#[async_trait]
pub trait RateLimitBackend: Send + Sync {
    /// `Ok(true)` = allow, `Ok(false)` = reject with 429. `Err` = the backing
    /// store could not answer; the **caller** resolves it through the
    /// channel's `rate_limit.on_backend_error` policy (N7) — `allow` fails
    /// open, `deny` refuses with 503. Fail-open is no longer a trait
    /// contract, it is the per-channel default. Takes the key by value:
    /// callers already own one, and governor's keyed store wants `&String`.
    async fn check(&self, key: String) -> Result<bool, OrionError>;
}

/// In-process governor limiter (today's behaviour; N replicas = N× the limit).
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
    async fn check(&self, key: String) -> Result<bool, OrionError> {
        Ok(self.limiter.check_key(&key).is_ok())
    }
}

/// Shared fixed-window limiter on the cluster Redis.
///
/// Key: `orion:rl:{channel}:{key}:{redis_second}`, TTL 2 s (each window is a
/// fresh key, so the every-call EXPIRE is harmless). Window limit = rps +
/// burst, mapping the per-channel `requests_per_second`/`burst` config onto
/// a one-second window.
///
/// N8: the window second comes from Redis `TIME` *inside the script*, not
/// from `SystemTime::now()`. The old form put each node's local clock in the
/// key, so skewed nodes landed in different buckets and the shared limit
/// silently multiplied by the number of skew groups — and `SystemTime` is
/// not even monotonic. One clock (the shared Redis's) now names every
/// window for the whole fleet. The script computes the key from the base it
/// is given, which is fine on the single shared Redis cluster mode requires
/// and would need hash-tagging before ever pointing at Redis Cluster.
pub struct RedisRateLimitBackend {
    conn: redis::aio::ConnectionManager,
    scope: String,
    limit_per_window: u32,
    script: redis::Script,
}

/// `INCR` + `EXPIRE` on the window key named by Redis's own clock, returning
/// the post-increment count. `TIME` is non-deterministic, which is exactly
/// why it must run *here*: script-effect replication (the default since
/// Redis 5) replicates the resulting writes, not the script.
const FIXED_WINDOW_SCRIPT: &str = r#"
local t = redis.call('TIME')
local key = KEYS[1] .. ':' .. t[1]
local count = redis.call('INCR', key)
redis.call('EXPIRE', key, 2)
return count
"#;

impl RedisRateLimitBackend {
    pub fn new(conn: redis::aio::ConnectionManager, scope: String, rps: u32, burst: u32) -> Self {
        Self {
            conn,
            scope,
            limit_per_window: rps.saturating_add(burst).max(1),
            script: redis::Script::new(FIXED_WINDOW_SCRIPT),
        }
    }
}

#[async_trait]
impl RateLimitBackend for RedisRateLimitBackend {
    async fn check(&self, key: String) -> Result<bool, OrionError> {
        let base_key = format!("orion:rl:{}:{}", self.scope, key);
        let mut conn = self.conn.clone();
        let result: Result<i64, redis::RedisError> =
            self.script.key(&base_key).invoke_async(&mut conn).await;
        match result {
            Ok(count) => Ok(count <= i64::from(self.limit_per_window)),
            // N7: not resolved here — the per-channel `on_backend_error`
            // policy lives with the caller, which also logs and records the
            // error metric with the channel in scope.
            Err(e) => Err(OrionError::internal(format!(
                "rate-limit backend '{}': {e}",
                self.scope
            ))),
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
        assert!(backend.check("ip-1".to_string()).await.expect("test"));
        assert!(backend.check("ip-1".to_string()).await.expect("test"));
        assert!(!backend.check("ip-1".to_string()).await.expect("test"));
        // Independent key unaffected.
        assert!(backend.check("ip-2".to_string()).await.expect("test"));
    }
}
