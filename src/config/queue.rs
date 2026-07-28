use serde::{Deserialize, Serialize};

use crate::config::validation::require_nonzero;
use crate::errors::OrionError;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueueConfig {
    /// Maximum number of concurrent async trace workers.
    pub workers: usize,
    /// Channel buffer size for pending traces.
    pub buffer_size: usize,
    /// Timeout in seconds to wait for in-flight traces during shutdown.
    pub shutdown_timeout_secs: u64,
    /// How long to retain completed/failed traces in hours (0 = forever).
    pub trace_retention_hours: u64,
    /// How long to retain admin audit-log entries in days (0 = forever).
    /// Audit rows are written by every admin mutation and are never otherwise
    /// removed, so leaving this at 0 makes `audit_logs` grow without bound.
    pub audit_retention_days: u64,
    /// How often to run the trace and audit-log cleanup tasks in seconds.
    pub trace_cleanup_interval_secs: u64,
    /// Maximum time in milliseconds for processing a single async trace.
    pub processing_timeout_ms: u64,
    /// Maximum size in bytes for serialized trace results. Results exceeding
    /// this limit are rejected (sync) or marked as failed (async). Default 1 MB.
    pub max_result_size_bytes: usize,
    /// Maximum total memory in bytes for queued trace payloads. New submissions
    /// are rejected with 503 when this limit is exceeded. Default 100 MB.
    pub max_queue_memory_bytes: usize,
    /// Enable DLQ retry processing for failed async traces.
    pub dlq_retry_enabled: bool,
    /// Maximum number of retries for DLQ entries before giving up.
    pub dlq_max_retries: i64,
    /// How often to poll the DLQ for pending retries, in seconds.
    pub dlq_poll_interval_secs: u64,
    /// Maximum DLQ entries claimed per retry tick.
    pub dlq_batch_size: i64,
    /// How long a claimed DLQ entry stays leased to one node, in seconds.
    /// Expired leases are re-claimable (crash recovery in cluster mode).
    pub dlq_lease_secs: u64,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            workers: 4,
            buffer_size: 1000,
            shutdown_timeout_secs: 30,
            trace_retention_hours: 72,
            audit_retention_days: 90,
            trace_cleanup_interval_secs: 3600,
            processing_timeout_ms: 60_000,
            max_result_size_bytes: 1_048_576,    // 1 MB
            max_queue_memory_bytes: 104_857_600, // 100 MB
            dlq_retry_enabled: true,
            dlq_max_retries: 5,
            dlq_poll_interval_secs: 30,
            dlq_batch_size: 20,
            dlq_lease_secs: 60,
        }
    }
}

impl QueueConfig {
    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        require_nonzero(self.workers as u64, "queue.workers")?;
        require_nonzero(self.buffer_size as u64, "queue.buffer_size")?;
        require_nonzero(self.processing_timeout_ms, "queue.processing_timeout_ms")?;
        require_nonzero(self.shutdown_timeout_secs, "queue.shutdown_timeout_secs")?;
        // 0 or negative would make every DLQ row invisible to the retry
        // worker (queries filter `retry_count < max_retries`) — a silently
        // dead DLQ. Disable retries via `dlq_retry_enabled` instead.
        if self.dlq_max_retries < 1 {
            return Err(OrionError::Config {
                message: "queue.dlq_max_retries must be >= 1 \
                          (set queue.dlq_retry_enabled = false to disable retries)"
                    .to_string(),
            });
        }
        // Backoff is 2^retry_count seconds; 2^16 (~18h) is already beyond any
        // useful retry cadence, and unbounded values overflow the shift (Q4).
        if self.dlq_max_retries > 16 {
            return Err(OrionError::Config {
                message: "queue.dlq_max_retries must be <= 16 (backoff is \
                          2^retries seconds — 2^16 is ~18 hours between attempts)"
                    .to_string(),
            });
        }
        if self.dlq_batch_size < 1 {
            return Err(OrionError::Config {
                message: "queue.dlq_batch_size must be >= 1".to_string(),
            });
        }
        require_nonzero(self.dlq_lease_secs, "queue.dlq_lease_secs")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_default_passes() {
        assert!(QueueConfig::default().validate().is_ok());
    }

    #[test]
    fn test_validate_rejects_zero_dlq_max_retries() {
        let config = QueueConfig {
            dlq_max_retries: 0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_negative_dlq_max_retries() {
        let config = QueueConfig {
            dlq_max_retries: -1,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_bounds_dlq_max_retries_against_shift_overflow() {
        // Q4: the backoff is `1i64 << retry_count` — 63+ overflows. The
        // validator stops at 16 (~18h between attempts), the shift itself
        // is clamped as defence in depth.
        let config = QueueConfig {
            dlq_max_retries: 17,
            ..Default::default()
        };
        assert!(config.validate().is_err());
        let config = QueueConfig {
            dlq_max_retries: 16,
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }
}
