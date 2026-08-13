use serde::{Deserialize, Serialize};

use crate::config::validation::{require_nonempty, require_nonzero};
use crate::errors::OrionError;

/// Multi-instance (HA) coordination settings.
///
/// With `enabled = false` (the default) Orion behaves exactly as a single
/// node: no epoch watcher, no shared backends, no job leases. When enabled,
/// N replicas sharing one Postgres/MySQL and one Redis behave as a single
/// logical system: config changes propagate via a DB epoch, dedup/response
/// caches default to the shared Redis, and background jobs single-flight.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClusterConfig {
    /// Enable multi-replica coordination.
    pub enabled: bool,
    /// Redis URL for shared dedup / response cache / rate limiting.
    /// Required when `enabled = true`.
    pub redis_url: String,
    /// How often each node polls the config epoch for changes made through
    /// other nodes, in milliseconds.
    pub epoch_poll_interval_ms: u64,
    /// Stable identity for this instance. Auto-generated UUID when empty.
    /// Also used as the Kafka `group.instance.id` (static membership), which
    /// caps its length at 64 characters.
    pub instance_id: String,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            redis_url: String::new(),
            epoch_poll_interval_ms: 2000,
            instance_id: String::new(),
        }
    }
}

impl ClusterConfig {
    /// The node identity to use: the configured value, or a fresh boot-time
    /// UUID when empty (D4: instances are cattle). The single policy point —
    /// callers that need a stable value must store the result once.
    pub fn effective_instance_id(&self) -> String {
        if self.instance_id.is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            self.instance_id.clone()
        }
    }

    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        require_nonzero(
            self.epoch_poll_interval_ms,
            "cluster.epoch_poll_interval_ms",
        )?;
        if self.enabled {
            require_nonempty(&self.redis_url, "cluster.redis_url")?;
        }
        if self.instance_id.len() > 64 {
            return Err(OrionError::Config {
                message: "cluster.instance_id must be at most 64 characters \
                          (it doubles as the Kafka group.instance.id)"
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
    fn test_default_is_disabled() {
        let config = ClusterConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.epoch_poll_interval_ms, 2000);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_enabled_requires_redis_url() {
        let config = ClusterConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(config.validate().is_err());

        let config = ClusterConfig {
            enabled: true,
            redis_url: "redis://localhost:6379".to_string(),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_rejects_oversized_instance_id() {
        let config = ClusterConfig {
            instance_id: "x".repeat(65),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }
}
