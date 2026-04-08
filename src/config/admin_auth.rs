use serde::{Deserialize, Serialize};

/// Admin API authentication configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AdminAuthConfig {
    /// Enable authentication for admin API endpoints.
    pub enabled: bool,
    /// Single API key (backward-compatible shorthand).
    pub api_key: String,
    /// Multiple API keys for zero-downtime rotation.
    /// Both `api_key` and `api_keys` are merged — duplicates are ignored.
    pub api_keys: Vec<String>,
    /// Header name to extract the API key from.
    /// When "Authorization" (default), expects `Bearer <token>` format.
    /// For other values (e.g. "X-API-Key"), expects the raw key value.
    pub header: String,
}

impl AdminAuthConfig {
    /// Return the effective list of API keys (merges single key + key list).
    pub fn effective_keys(&self) -> Vec<&str> {
        let mut keys: Vec<&str> = self
            .api_keys
            .iter()
            .filter(|k| !k.is_empty())
            .map(|k| k.as_str())
            .collect();
        if !self.api_key.is_empty() && !keys.contains(&self.api_key.as_str()) {
            keys.push(&self.api_key);
        }
        keys
    }
}

impl Default for AdminAuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: String::new(),
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
        assert!(config.api_key.is_empty());
        assert_eq!(config.header, "Authorization");
    }

    #[test]
    fn test_effective_keys_single_key_only() {
        let config = AdminAuthConfig {
            enabled: true,
            api_key: "key-a".to_string(),
            api_keys: vec![],
            header: "Authorization".to_string(),
        };
        assert_eq!(config.effective_keys(), vec!["key-a"]);
    }

    #[test]
    fn test_effective_keys_multiple_keys_only() {
        let config = AdminAuthConfig {
            enabled: true,
            api_key: String::new(),
            api_keys: vec!["key-b".to_string(), "key-c".to_string()],
            header: "Authorization".to_string(),
        };
        assert_eq!(config.effective_keys(), vec!["key-b", "key-c"]);
    }

    #[test]
    fn test_effective_keys_merged_no_duplicates() {
        let config = AdminAuthConfig {
            enabled: true,
            api_key: "key-a".to_string(),
            api_keys: vec!["key-a".to_string(), "key-b".to_string()],
            header: "Authorization".to_string(),
        };
        // api_key "key-a" is already in api_keys, so no duplicate
        assert_eq!(config.effective_keys(), vec!["key-a", "key-b"]);
    }

    #[test]
    fn test_effective_keys_merged_with_unique() {
        let config = AdminAuthConfig {
            enabled: true,
            api_key: "key-c".to_string(),
            api_keys: vec!["key-a".to_string(), "key-b".to_string()],
            header: "Authorization".to_string(),
        };
        assert_eq!(config.effective_keys(), vec!["key-a", "key-b", "key-c"]);
    }

    #[test]
    fn test_effective_keys_empty() {
        let config = AdminAuthConfig::default();
        assert!(config.effective_keys().is_empty());
    }

    #[test]
    fn test_effective_keys_filters_empty_strings() {
        let config = AdminAuthConfig {
            enabled: true,
            api_key: String::new(),
            api_keys: vec!["".to_string(), "key-a".to_string(), "".to_string()],
            header: "Authorization".to_string(),
        };
        assert_eq!(config.effective_keys(), vec!["key-a"]);
    }
}
