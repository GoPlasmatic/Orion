//! Optional encryption at rest for `connectors.config_json` (H3).
//!
//! The column is `text` on all three backends and connector configs may hold
//! literal credentials; database dumps, replicas and backup files all carry
//! it in clear. With `storage.connector_encryption_key` set, the repository
//! encrypts the whole document on every write and decrypts on every read —
//! transparently, so nothing above the repository layer knows the column is
//! ciphertext.
//!
//! Format: `enc:v1:<base64(nonce || ciphertext)>`, AES-256-GCM, a fresh
//! random 96-bit nonce per write. The prefix is what makes a mixed estate
//! workable: rows written before the key was set stay readable (pass-through)
//! and re-encrypt on their next write, so turning encryption on is a config
//! change plus a rewrite of whatever should stop being plaintext — not a
//! migration. Turning it *off* requires decrypting rows first; a prefixed row
//! with no key configured is a loud error, never silently served as the
//! literal `enc:v1:…` string.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::errors::OrionError;

/// Envelope prefix. Versioned so a future cipher change can coexist with
/// stored `v1` rows instead of invalidating them.
const PREFIX: &str = "enc:v1:";

/// AES-256-GCM nonce width in bytes.
const NONCE_LEN: usize = 12;

/// The connector-config cipher, built once at startup from
/// `storage.connector_encryption_key`.
pub struct ConfigCipher {
    cipher: Aes256Gcm,
}

impl std::fmt::Debug for ConfigCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never derive: a derived Debug would print key schedule material.
        f.write_str("ConfigCipher")
    }
}

impl ConfigCipher {
    /// Build from the 64-hex-char (32-byte) key the config carries.
    pub fn from_hex(hex_key: &str) -> Result<Self, OrionError> {
        let bytes = hex::decode(hex_key).ok().filter(|b| b.len() == 32);
        let Some(bytes) = bytes else {
            return Err(OrionError::Config {
                message: "storage.connector_encryption_key must be the 64-character hex \
                          encoding of a 32-byte key. Generate one with `openssl rand -hex 32`"
                    .to_string(),
            });
        };
        let key = Key::<Aes256Gcm>::from_slice(&bytes);
        Ok(Self {
            cipher: Aes256Gcm::new(key),
        })
    }

    /// Whether a stored value carries the encryption envelope.
    pub fn is_encrypted(stored: &str) -> bool {
        stored.starts_with(PREFIX)
    }

    /// Encrypt a config document for storage.
    pub fn encrypt(&self, plaintext: &str) -> Result<String, OrionError> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|_| OrionError::internal("connector config encryption failed"))?;
        let mut payload = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        payload.extend_from_slice(&nonce);
        payload.extend_from_slice(&ciphertext);
        Ok(format!("{PREFIX}{}", BASE64.encode(payload)))
    }

    /// Decrypt a stored value. Unprefixed values pass through unchanged —
    /// that is what lets pre-encryption rows keep loading.
    pub fn decrypt(&self, stored: &str) -> Result<String, OrionError> {
        let Some(encoded) = stored.strip_prefix(PREFIX) else {
            return Ok(stored.to_string());
        };
        let payload = BASE64
            .decode(encoded)
            .ok()
            .filter(|p| p.len() > NONCE_LEN)
            .ok_or_else(|| {
                OrionError::internal("stored connector config has a malformed encryption envelope")
            })?;
        let (nonce, ciphertext) = payload.split_at(NONCE_LEN);
        let plaintext = self
            .cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| {
                // Wrong key or tampered row — GCM authenticates, so the two
                // are indistinguishable by design. Loud either way.
                OrionError::internal(
                    "stored connector config failed to decrypt: wrong \
                     storage.connector_encryption_key, or the row was modified \
                     outside Orion",
                )
            })?;
        String::from_utf8(plaintext)
            .map_err(|_| OrionError::internal("decrypted connector config is not UTF-8"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher() -> ConfigCipher {
        ConfigCipher::from_hex(&"ab".repeat(32)).expect("valid key")
    }

    #[test]
    fn round_trips_and_fresh_nonce_per_write() {
        let c = cipher();
        let doc = r#"{"type":"http","auth":{"token":"s3cret"}}"#;
        let a = c.encrypt(doc).expect("encrypt");
        let b = c.encrypt(doc).expect("encrypt");
        assert!(ConfigCipher::is_encrypted(&a));
        assert_ne!(a, b, "nonce reuse would break GCM outright");
        assert_eq!(c.decrypt(&a).expect("decrypt"), doc);
        assert_eq!(c.decrypt(&b).expect("decrypt"), doc);
    }

    #[test]
    fn plaintext_rows_pass_through() {
        let c = cipher();
        let doc = r#"{"type":"http"}"#;
        assert_eq!(c.decrypt(doc).expect("pass-through"), doc);
    }

    #[test]
    fn the_wrong_key_fails_loudly() {
        let stored = cipher().encrypt("{}").expect("encrypt");
        let other = ConfigCipher::from_hex(&"cd".repeat(32)).expect("valid key");
        assert!(other.decrypt(&stored).is_err());
    }

    #[test]
    fn a_tampered_row_fails_loudly() {
        let c = cipher();
        let stored = c.encrypt("{}").expect("encrypt");
        // Flip a ciphertext byte inside the base64 payload.
        let mut payload = BASE64
            .decode(stored.strip_prefix(PREFIX).expect("enveloped"))
            .expect("valid base64");
        let last = payload.len() - 1;
        payload[last] ^= 0x01;
        let tampered = format!("{PREFIX}{}", BASE64.encode(payload));
        assert!(c.decrypt(&tampered).is_err(), "GCM must refuse a forgery");
    }

    #[test]
    fn key_validation() {
        assert!(ConfigCipher::from_hex("").is_err());
        assert!(ConfigCipher::from_hex("abcd").is_err());
        assert!(ConfigCipher::from_hex(&"zz".repeat(32)).is_err());
        assert!(ConfigCipher::from_hex(&"ab".repeat(32)).is_ok());
    }
}
