use serde::{Deserialize, Serialize};

use crate::errors::OrionError;

/// Valid tracing log levels.
const VALID_LOG_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: String,
    pub format: LogFormat,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: LogFormat::Pretty,
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
