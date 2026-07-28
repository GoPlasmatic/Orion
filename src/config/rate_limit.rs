use serde::{Deserialize, Serialize};

use crate::config::validation::require_nonzero;
use crate::errors::OrionError;

/// `Default` is implemented by hand rather than derived so that it agrees with
/// the `#[serde(default = "…")]` attributes below. A derived `Default` would
/// give `0`/`0` while a config file declaring `[rate_limit]` and omitting the
/// keys gives `100`/`50` — so `ORION_RATE_LIMIT__ENABLED=true` with no config
/// file (the pure-env shape the Helm chart and Docker image encourage) failed
/// startup validation with "must be > 0" (F36).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitConfig {
    pub enabled: bool,
    #[serde(default = "default_rps")]
    pub default_rps: u32,
    #[serde(default = "default_burst")]
    pub default_burst: u32,
    /// CIDR blocks (or bare IPs) of reverse proxies whose `X-Forwarded-For` /
    /// `X-Real-IP` headers are trusted for client identification. When the
    /// direct peer is not in this list — including the default empty list —
    /// forwarded headers are ignored and the peer IP is the client identity,
    /// so untrusted clients cannot mint a fresh rate-limit bucket per request
    /// by spoofing the header (S8).
    pub trusted_proxies: Vec<String>,
    #[serde(default)]
    pub endpoints: EndpointRateLimits,
}

fn default_rps() -> u32 {
    100
}

fn default_burst() -> u32 {
    50
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_rps: default_rps(),
            default_burst: default_burst(),
            trusted_proxies: Vec::new(),
            endpoints: EndpointRateLimits::default(),
        }
    }
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
        for entry in &self.trusted_proxies {
            parse_proxy_entry(entry).map_err(|reason| OrionError::Config {
                message: format!("rate_limit.trusted_proxies: invalid entry '{entry}': {reason}"),
            })?;
        }
        Ok(())
    }

    /// The `trusted_proxies` entries parsed into networks. Invalid entries
    /// are skipped — `validate()` rejects them at config load, so this only
    /// drops entries when validation was bypassed (e.g. hand-built configs
    /// in tests).
    pub fn parsed_trusted_proxies(&self) -> Vec<ipnet::IpNet> {
        self.trusted_proxies
            .iter()
            .filter_map(|entry| parse_proxy_entry(entry).ok())
            .collect()
    }
}

/// Parse one trusted-proxy entry: a CIDR block (`10.0.0.0/8`) or a bare IP
/// (`10.0.0.1`, treated as /32 or /128).
fn parse_proxy_entry(entry: &str) -> Result<ipnet::IpNet, &'static str> {
    let entry = entry.trim();
    if let Ok(net) = entry.parse::<ipnet::IpNet>() {
        return Ok(net);
    }
    entry
        .parse::<std::net::IpAddr>()
        .map(ipnet::IpNet::from)
        .map_err(|_| "expected an IP address or CIDR block (e.g. \"10.0.0.0/8\")")
}

/// Default requests-per-second for the admin plane when `rate_limit.enabled`.
///
/// Previously `None`, which meant admin traffic fell back to `default_rps`
/// (100) — the same budget as the anonymous data plane, on the surface that
/// holds every mutating operation and the credentials to reach them. Admin use
/// is interactive and low-volume; 20/s is generous for a human or a deploy
/// pipeline and far below what online credential guessing needs (S12).
fn default_admin_rps() -> Option<u32> {
    Some(20)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EndpointRateLimits {
    /// Per-client limit for admin routes. `None` means "no separate limit" —
    /// fall back to `default_rps`.
    #[serde(default = "default_admin_rps")]
    pub admin_rps: Option<u32>,
    /// Per-client limit for the data plane. `None` falls back to `default_rps`.
    pub data_rps: Option<u32>,
}

impl Default for EndpointRateLimits {
    fn default() -> Self {
        Self {
            admin_rps: default_admin_rps(),
            data_rps: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_proxies(entries: &[&str]) -> RateLimitConfig {
        RateLimitConfig {
            trusted_proxies: entries.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn test_validate_accepts_cidr_and_bare_ip() {
        let config = config_with_proxies(&["10.0.0.0/8", "192.168.1.1", "fd00::/8", "::1"]);
        assert!(config.validate().is_ok());
        assert_eq!(config.parsed_trusted_proxies().len(), 4);
    }

    #[test]
    fn test_validate_rejects_malformed_entry() {
        for bad in ["not-an-ip", "10.0.0.0/33", "10.0.0/8", ""] {
            let config = config_with_proxies(&[bad]);
            let err = config.validate().expect_err("should reject");
            assert!(
                err.to_string().contains("trusted_proxies"),
                "error for '{bad}' should mention trusted_proxies: {err}"
            );
        }
    }

    #[test]
    fn test_bare_ip_parses_as_host_network() {
        let config = config_with_proxies(&["192.168.1.1"]);
        let nets = config.parsed_trusted_proxies();
        assert_eq!(nets.len(), 1);
        assert_eq!(nets[0].prefix_len(), 32);
    }

    /// F36: enabling rate limiting with no config file at all — the pure-env
    /// deployment shape — used to fail validation, because the derived
    /// `Default` gave `0`/`0` while the serde defaults gave `100`/`50`.
    #[test]
    fn test_enabled_on_derived_default_passes_validation() {
        let config = RateLimitConfig {
            enabled: true,
            ..Default::default()
        };
        assert_eq!(config.default_rps, 100);
        assert_eq!(config.default_burst, 50);
        config
            .validate()
            .expect("enabling rate limiting without a config file must validate");
    }

    /// The derived `Default` and the value a config file that declares the
    /// section but sets nothing produces must be the same config.
    #[test]
    fn test_derived_default_matches_empty_section() {
        let from_file: RateLimitConfig =
            toml::from_str("").expect("an empty rate_limit section must deserialize");
        let derived = RateLimitConfig::default();
        assert_eq!(from_file.default_rps, derived.default_rps);
        assert_eq!(from_file.default_burst, derived.default_burst);
        assert_eq!(from_file.enabled, derived.enabled);
    }

    #[test]
    fn test_default_has_no_trusted_proxies() {
        let config = RateLimitConfig::default();
        assert!(config.trusted_proxies.is_empty());
        assert!(config.parsed_trusted_proxies().is_empty());
        assert!(config.validate().is_ok());
    }
}
