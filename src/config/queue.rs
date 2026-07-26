use serde::{Deserialize, Serialize};

use crate::config::validation::require_nonzero;
use crate::errors::OrionError;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct QueueConfig {
    /// Maximum number of concurrent async trace workers.
    pub workers: usize,
    /// Channel buffer size for pending traces.
    pub buffer_size: usize,
    /// Timeout in seconds to wait for in-flight traces during shutdown.
    pub shutdown_timeout_secs: u64,
    /// How long to retain completed/failed traces in hours (0 = forever).
    pub trace_retention_hours: u64,
    /// How often to run the trace cleanup task in seconds.
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
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            workers: 4,
            buffer_size: 1000,
            shutdown_timeout_secs: 30,
            trace_retention_hours: 72,
            trace_cleanup_interval_secs: 3600,
            processing_timeout_ms: 60_000,
            max_result_size_bytes: 1_048_576,    // 1 MB
            max_queue_memory_bytes: 104_857_600, // 100 MB
            dlq_retry_enabled: true,
            dlq_max_retries: 5,
            dlq_poll_interval_secs: 30,
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
}
