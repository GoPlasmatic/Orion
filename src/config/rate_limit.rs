use serde::{Deserialize, Serialize};

use crate::config::validation::require_nonzero;
use crate::errors::OrionError;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitConfig {
    pub enabled: bool,
    #[serde(default = "default_rps")]
    pub default_rps: u32,
    #[serde(default = "default_burst")]
    pub default_burst: u32,
    #[serde(default)]
    pub endpoints: EndpointRateLimits,
}

fn default_rps() -> u32 {
    100
}

fn default_burst() -> u32 {
    50
}

impl RateLimitConfig {
    pub(crate) fn validate(&self) -> Result<(), OrionError> {
        if self.enabled {
            require_nonzero(
                u64::from(self.default_rps),
                "rate_limit.default_rps (required when rate limiting is enabled)",
            )?;
            require_nonzero(
                u64::from(self.default_burst),
                "rate_limit.default_burst (required when rate limiting is enabled)",
            )?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EndpointRateLimits {
    pub admin_rps: Option<u32>,
    pub data_rps: Option<u32>,
}
