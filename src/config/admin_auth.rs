use serde::{Deserialize, Serialize};

/// Admin API authentication configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AdminAuthConfig {
    /// Enable authentication for admin API endpoints.
    pub enabled: bool,
    /// One or more API keys (multiple values support zero-downtime rotation).
    /// Empty strings are ignored.
    pub api_keys: Vec<String>,
    /// Header name to extract the API key from.
    /// When "Authorization" (default), expects `Bearer <token>` format.
    /// For other values (e.g. "X-API-Key"), expects the raw key value.
    pub header: String,
}

impl AdminAuthConfig {
    /// Return the effective list of API keys (non-empty `api_keys` entries).
    pub fn effective_keys(&self) -> Vec<&str> {
        self.api_keys
            .iter()
            .filter(|k| !k.is_empty())
            .map(String::as_str)
            .collect()
    }
}

impl Default for AdminAuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_keys: Vec::new(),
            header: "Authorization".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_admin_auth_config_default() {
        let config = AdminAuthConfig::default();
        assert!(!config.enabled);
        assert!(config.api_keys.is_empty());
        assert_eq!(config.header, "Authorization");
    }

    #[test]
    fn test_effective_keys_returns_configured_keys() {
        let config = AdminAuthConfig {
            enabled: true,
            api_keys: vec!["key-a".to_string(), "key-b".to_string()],
            header: "Authorization".to_string(),
        };
        assert_eq!(config.effective_keys(), vec!["key-a", "key-b"]);
    }

    #[test]
    fn test_effective_keys_filters_empty_strings() {
        let config = AdminAuthConfig {
            enabled: true,
            api_keys: vec!["".to_string(), "key-a".to_string(), "".to_string()],
            header: "Authorization".to_string(),
        };
        assert_eq!(config.effective_keys(), vec!["key-a"]);
    }

    #[test]
    fn test_effective_keys_empty() {
        let config = AdminAuthConfig::default();
        assert!(config.effective_keys().is_empty());
    }
}
