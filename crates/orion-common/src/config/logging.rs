use serde::{Deserialize, Serialize};

use crate::errors::OrionError;

/// Valid tracing log levels.
const VALID_LOG_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: String,
    pub format: LogFormat,
    /// Emit one `INFO` access-log line per HTTP request (method, matched
    /// route, status, response length, duration, request id). Written by the
    /// HTTP metrics layer, so it needs `metrics.enabled` as well. On by
    /// default; turn it off where a load balancer or ingress already logs
    /// every request, to save the formatting and the write on the hot path.
    pub access_log: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: LogFormat::Pretty,
            access_log: true,
        }
    }
}

impl LoggingConfig {
    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        if !VALID_LOG_LEVELS.contains(&self.level.to_lowercase().as_str()) {
            return Err(OrionError::Config {
                message: format!(
                    "logging.level '{}' is invalid. Must be one of: {}",
                    self.level,
                    VALID_LOG_LEVELS.join(", ")
                ),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Pretty,
    Json,
}
