use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::errors::OrionError;

/// Admin API authentication configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdminAuthConfig {
    /// Enable authentication for admin API endpoints.
    pub enabled: bool,
    /// One or more API keys (multiple values support zero-downtime rotation).
    /// Each entry is either a plaintext key or `sha256:<64-hex>` — the SHA-256
    /// digest of the key — so operators can keep hashes rather than secrets at
    /// rest (S11). Empty strings are ignored.
    pub api_keys: Vec<String>,
    /// Header name to extract the API key from.
    /// When "Authorization" (default), expects `Bearer <token>` format.
    /// For other values (e.g. "X-API-Key"), expects the raw key value.
    pub header: String,
}

/// Minimum length for a *plaintext* `admin_auth.api_keys` entry. 32 characters
/// is the usual floor for a bearer credential — `openssl rand -hex 32` produces
/// 64. Shorter keys are a hard error in production and a warning elsewhere, so
/// local development stays ergonomic. `sha256:` entries are exempt: a digest
/// says nothing about the strength of the key behind it.
const MIN_PLAINTEXT_KEY_LEN: usize = 32;

/// A configured admin API key, normalized to its SHA-256 digest so the
/// middleware always compares fixed-width values (S11).
pub struct AdminKey {
    /// SHA-256 digest of the key, compared against the digest of the
    /// presented token.
    pub digest: [u8; 32],
    /// True when configured in the `sha256:<hex>` hash-at-rest form.
    pub hashed: bool,
}

/// Decode a `sha256:` entry's 64-hex-char payload into a digest.
fn decode_sha256_hex(s: &str) -> Option<[u8; 32]> {
    hex::decode(s).ok()?.try_into().ok()
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

    /// The effective keys as SHA-256 digests: `sha256:` entries decoded,
    /// plaintext entries hashed. Malformed `sha256:` entries are skipped —
    /// `validate()` rejects them at config load.
    pub fn admin_keys(&self) -> Vec<AdminKey> {
        self.effective_keys()
            .into_iter()
            .filter_map(|key| {
                if let Some(hex_digest) = key.strip_prefix("sha256:") {
                    decode_sha256_hex(hex_digest).map(|digest| AdminKey {
                        digest,
                        hashed: true,
                    })
                } else {
                    Some(AdminKey {
                        digest: Sha256::digest(key.as_bytes()).into(),
                        hashed: false,
                    })
                }
            })
            .collect()
    }

    pub(crate) fn validate(&self, is_production: bool) -> Result<(), OrionError> {
        if self.enabled && self.effective_keys().is_empty() {
            return Err(OrionError::Config {
                message:
                    "At least one admin API key must be configured when admin auth is enabled. \
                     Set admin_auth.api_keys"
                        .to_string(),
            });
        }
        for key in self.effective_keys() {
            if let Some(hex_digest) = key.strip_prefix("sha256:") {
                if decode_sha256_hex(hex_digest).is_none() {
                    let shown: String = hex_digest.chars().take(16).collect();
                    return Err(OrionError::Config {
                        message: format!(
                            "admin_auth.api_keys: 'sha256:' entries must be followed by the \
                             64-character hex SHA-256 digest of the key, got 'sha256:{shown}'"
                        ),
                    });
                }
                // A digest carries no information about the strength of the key
                // it was derived from, so the length floor below cannot apply.
                continue;
            }
            // Plaintext keys: enforce a minimum length. `api_keys = ["a"]` was
            // previously a valid production admin credential (proposal S12).
            if key.len() < MIN_PLAINTEXT_KEY_LEN {
                let message = format!(
                    "admin_auth.api_keys: plaintext keys must be at least \
                     {MIN_PLAINTEXT_KEY_LEN} characters (got one of length {}). \
                     Generate one with `openssl rand -hex 32`, or store the digest \
                     as 'sha256:<64-hex>'",
                    key.len()
                );
                if is_production {
                    return Err(OrionError::Config { message });
                }
                tracing::warn!("{message}");
            }
        }
        if !self.enabled {
            if is_production {
                return Err(OrionError::Config {
                    message: "admin_auth must be enabled when environment starts with 'prod'. \
                              Set admin_auth.enabled = true and configure admin_auth.api_keys"
                        .to_string(),
                });
            }
            tracing::warn!(
                "Admin auth is disabled. For production, enable admin_auth with a strong API key"
            );
        }
        Ok(())
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

    fn config_with_keys(keys: &[&str]) -> AdminAuthConfig {
        AdminAuthConfig {
            enabled: true,
            api_keys: keys.iter().map(|k| k.to_string()).collect(),
            header: "Authorization".to_string(),
        }
    }

    #[test]
    fn test_admin_auth_config_default() {
        let config = AdminAuthConfig::default();
        assert!(!config.enabled);
        assert!(config.api_keys.is_empty());
        assert_eq!(config.header, "Authorization");
    }

    #[test]
    fn test_effective_keys_returns_configured_keys() {
        let config = config_with_keys(&["key-a", "key-b"]);
        assert_eq!(config.effective_keys(), vec!["key-a", "key-b"]);
    }

    #[test]
    fn test_effective_keys_filters_empty_strings() {
        let config = config_with_keys(&["", "key-a", ""]);
        assert_eq!(config.effective_keys(), vec!["key-a"]);
    }

    #[test]
    fn test_effective_keys_empty() {
        let config = AdminAuthConfig::default();
        assert!(config.effective_keys().is_empty());
    }

    #[test]
    fn test_admin_keys_plaintext_entry_is_hashed() {
        let config = config_with_keys(&["my-secret"]);
        let keys = config.admin_keys();
        assert_eq!(keys.len(), 1);
        assert!(!keys[0].hashed);
        let expected: [u8; 32] = Sha256::digest(b"my-secret").into();
        assert_eq!(keys[0].digest, expected);
    }

    #[test]
    fn test_admin_keys_sha256_entry_matches_plaintext_digest() {
        let digest_hex = hex::encode(Sha256::digest(b"my-secret"));
        let entry = format!("sha256:{digest_hex}");
        let config = config_with_keys(&[&entry]);
        let keys = config.admin_keys();
        assert_eq!(keys.len(), 1);
        assert!(keys[0].hashed);
        // The stored hash and a freshly hashed plaintext token must agree
        let presented: [u8; 32] = Sha256::digest(b"my-secret").into();
        assert_eq!(keys[0].digest, presented);
    }

    #[test]
    fn test_admin_keys_uppercase_hex_accepted() {
        let digest_hex = hex::encode(Sha256::digest(b"my-secret")).to_uppercase();
        let entry = format!("sha256:{digest_hex}");
        let config = config_with_keys(&[&entry]);
        assert_eq!(config.admin_keys().len(), 1);
        assert!(config.validate(false).is_ok());
    }

    #[test]
    fn test_validate_rejects_malformed_sha256_entries() {
        for bad in [
            "sha256:",
            "sha256:abc",
            "sha256:zz00000000000000000000000000000000000000000000000000000000000000",
        ] {
            let config = config_with_keys(&[bad]);
            let err = config.validate(false).expect_err("should reject");
            assert!(
                err.to_string().contains("sha256"),
                "error for '{bad}' should mention sha256: {err}"
            );
        }
    }

    #[test]
    fn test_validate_accepts_valid_sha256_entry() {
        let entry = format!("sha256:{}", hex::encode(Sha256::digest(b"k")));
        let config = config_with_keys(&[&entry]);
        assert!(config.validate(false).is_ok());
    }

    // -- plaintext key strength (S12) -----------------------------------

    #[test]
    fn short_plaintext_key_is_rejected_in_production() {
        // `api_keys = ["a"]` was a valid production admin credential.
        let err = config_with_keys(&["a"])
            .validate(true)
            .expect_err("a 1-char production admin key must be refused");
        let message = err.to_string();
        assert!(
            message.contains("at least 32 characters"),
            "the error must say what is wrong: {message}"
        );
        assert!(
            message.contains("openssl rand"),
            "the error must say how to fix it: {message}"
        );
    }

    #[test]
    fn short_plaintext_key_is_only_a_warning_outside_production() {
        // Local development stays ergonomic.
        assert!(config_with_keys(&["dev"]).validate(false).is_ok());
    }

    #[test]
    fn long_plaintext_key_is_accepted_in_production() {
        let key = "x".repeat(MIN_PLAINTEXT_KEY_LEN);
        assert!(config_with_keys(&[&key]).validate(true).is_ok());
    }

    #[test]
    fn hashed_key_is_exempt_from_the_length_floor_in_production() {
        // A digest is always 64 hex chars and says nothing about the strength
        // of the key behind it, so the floor cannot meaningfully apply.
        let entry = format!("sha256:{}", hex::encode(Sha256::digest(b"short")));
        assert!(config_with_keys(&[&entry]).validate(true).is_ok());
    }
}
