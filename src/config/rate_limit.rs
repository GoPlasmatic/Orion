use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EndpointRateLimits {
    pub admin_rps: Option<u32>,
    pub data_rps: Option<u32>,
}
