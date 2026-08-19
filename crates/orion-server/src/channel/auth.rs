//! Per-channel authentication for the HTTP data plane.
//!
//! `admin_auth` guards `/api/v1/admin` and nothing else, so before this module
//! every data channel was open to anyone who could reach the port. The two
//! controls the documentation pointed at are not substitutes:
//! `origin_allow_list` reads a client-supplied header, and a `validation_logic`
//! header comparison puts the credential in the stored config in plain text and
//! compares it with an early exit.
//!
//! Configuration is [`ChannelAuthConfig`]; this module turns it into a
//! [`CompiledAuth`] once at channel load, so the request path never parses a
//! config, resolves an `env://` reference, or hashes a stored key. Enforcement
//! happens in `guards::apply_guards`, through the transport matrix, so every
//! HTTP ingress inherits it from one place.
//!
//! Two modes ship here. `api_key` covers the common "our other service calls
//! this" case; `hmac` covers inbound webhooks, whose signature is computed over
//! the **raw body** and so must be checked before the JSON is parsed.

// KeyInit is what carries `new_from_slice` from hmac 0.13 on: the constructor
// moved off the concrete Hmac type onto the crypto-common trait, so it has to
// be in scope to be called.
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

use crate::channel::config::{AuthMode, ChannelAuthConfig};
use crate::channel::guards::HeaderLookup;
use crate::config::constant_time_eq;
use crate::errors::OrionError;

type HmacSha256 = Hmac<Sha256>;

/// A channel's authentication policy, resolved and pre-hashed at load time.
#[derive(Debug, Clone)]
pub enum CompiledAuth {
    ApiKey {
        header: String,
        /// Prefix stripped from the header value before comparison, e.g.
        /// `"Bearer "`.
        scheme: Option<String>,
        /// SHA-256 of each accepted key. Digests rather than keys so the
        /// comparison is over a fixed width and reveals neither the length nor
        /// the content of the expected value through timing — the same policy
        /// `admin_auth` applies (S11).
        digests: Vec<[u8; 32]>,
    },
    Hmac {
        header: String,
        secret: Vec<u8>,
        signature_prefix: Option<String>,
    },
}

/// The refusal every failure path returns.
///
/// One message for every cause — wrong key, absent header, malformed
/// signature — because distinguishing them tells an unauthenticated caller
/// which half of the credential they got right.
fn refused() -> OrionError {
    OrionError::Unauthorized("Channel authentication failed".into())
}

impl CompiledAuth {
    /// Resolve secrets and pre-hash keys. `Err` is a human-readable reason, and
    /// the caller turns it into a channel load issue: a channel whose auth
    /// cannot be built is quarantined rather than served without it (F35).
    pub async fn compile(cfg: &ChannelAuthConfig) -> Result<Self, String> {
        match cfg.mode {
            AuthMode::ApiKey => {
                let keys = cfg
                    .keys
                    .as_ref()
                    .filter(|k| !k.is_empty())
                    .ok_or("auth.mode = \"api_key\" requires a non-empty auth.keys")?;

                let header = cfg
                    .header
                    .clone()
                    .unwrap_or_else(|| "Authorization".to_string());
                // `Authorization: Bearer <key>` is the conventional spelling and
                // the one an unset `scheme` should produce; a custom header like
                // `X-API-Key` carries the bare key.
                let scheme = match cfg.scheme {
                    Some(ref s) => Some(s.clone()),
                    None if header.eq_ignore_ascii_case("authorization") => {
                        Some("Bearer ".to_string())
                    }
                    None => None,
                };

                let mut digests = Vec::with_capacity(keys.len());
                for key in keys {
                    let resolved = resolve_secret(key, "auth.keys").await?;
                    if resolved.is_empty() {
                        return Err("auth.keys contains an empty key".to_string());
                    }
                    digests.push(Sha256::digest(resolved.as_bytes()).into());
                }

                Ok(Self::ApiKey {
                    header,
                    scheme,
                    digests,
                })
            }
            AuthMode::Hmac => {
                let secret = cfg
                    .secret
                    .as_ref()
                    .ok_or("auth.mode = \"hmac\" requires auth.secret")?;
                let secret = resolve_secret(secret, "auth.secret").await?;
                if secret.is_empty() {
                    return Err("auth.secret resolved to an empty value".to_string());
                }
                Ok(Self::Hmac {
                    header: cfg
                        .header
                        .clone()
                        .unwrap_or_else(|| "X-Signature".to_string()),
                    secret: secret.into_bytes(),
                    signature_prefix: cfg.signature_prefix.clone(),
                })
            }
        }
    }

    /// Authenticate one request.
    ///
    /// `raw_body` is the bytes exactly as received. HMAC is defined over them,
    /// so it must be checked before any parse: re-serializing parsed JSON
    /// reorders keys and normalises whitespace, and the signature would never
    /// match again.
    pub fn authenticate(
        &self,
        header: HeaderLookup<'_>,
        raw_body: Option<&[u8]>,
    ) -> Result<(), OrionError> {
        match self {
            Self::ApiKey {
                header: name,
                scheme,
                digests,
            } => {
                let presented = header(name).ok_or_else(refused)?;
                let presented = match scheme {
                    Some(prefix) => presented
                        .strip_prefix(prefix.as_str())
                        .ok_or_else(refused)?
                        .to_string(),
                    None => presented,
                };
                let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
                if digests.iter().any(|d| constant_time_eq(&digest, d)) {
                    Ok(())
                } else {
                    Err(refused())
                }
            }
            Self::Hmac {
                header: name,
                secret,
                signature_prefix,
            } => {
                let presented = header(name).ok_or_else(refused)?;
                let presented = match signature_prefix {
                    Some(prefix) => presented
                        .strip_prefix(prefix.as_str())
                        .ok_or_else(refused)?,
                    None => presented.as_str(),
                };
                let signature = decode_signature(presented).ok_or_else(refused)?;

                // An absent body is an empty one: a signed GET webhook signs
                // zero bytes, and treating that as "cannot verify" would refuse
                // a legitimately signed request.
                let body = raw_body.unwrap_or(&[]);
                let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| refused())?;
                mac.update(body);
                // `verify_slice` is constant-time and length-checked.
                mac.verify_slice(&signature).map_err(|_| refused())
            }
        }
    }
}

/// Decode a signature presented as hex (Stripe, GitHub) or base64 (Shopify).
///
/// Hex is tried first because it is unambiguous at 64 characters; a base64
/// SHA-256 signature is 44 characters and never valid hex, so the two cannot be
/// confused.
fn decode_signature(presented: &str) -> Option<Vec<u8>> {
    if let Ok(bytes) = hex::decode(presented) {
        return Some(bytes);
    }
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(presented)
        .ok()
}

/// Resolve an `env://VAR` reference, or pass a literal through.
///
/// The same resolver connector secrets use, so an operator has one mechanism to
/// learn and production credentials never have to sit in a stored channel
/// config.
async fn resolve_secret(value: &str, field: &str) -> Result<String, String> {
    crate::connector::secrets::resolve_secret_string(value, field).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::config::ChannelAuthConfig;

    fn api_key_config(keys: &[&str]) -> ChannelAuthConfig {
        ChannelAuthConfig {
            mode: AuthMode::ApiKey,
            keys: Some(keys.iter().map(|k| k.to_string()).collect()),
            header: None,
            scheme: None,
            secret: None,
            signature_prefix: None,
        }
    }

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.to_string())
        }
    }

    #[tokio::test]
    async fn api_key_defaults_to_bearer_on_authorization() {
        let auth = CompiledAuth::compile(&api_key_config(&["s3cret"]))
            .await
            .expect("compiles");
        let headers = [("Authorization", "Bearer s3cret")];
        assert!(auth.authenticate(&lookup(&headers), None).is_ok());
    }

    /// The bare key without the scheme prefix is not accepted — otherwise the
    /// prefix would be decorative.
    #[tokio::test]
    async fn api_key_requires_the_scheme_prefix() {
        let auth = CompiledAuth::compile(&api_key_config(&["s3cret"]))
            .await
            .expect("compiles");
        let headers = [("Authorization", "s3cret")];
        assert!(auth.authenticate(&lookup(&headers), None).is_err());
    }

    /// A custom header carries the bare key, with no prefix to strip.
    #[tokio::test]
    async fn a_custom_header_takes_a_bare_key() {
        let mut cfg = api_key_config(&["s3cret"]);
        cfg.header = Some("X-API-Key".to_string());
        let auth = CompiledAuth::compile(&cfg).await.expect("compiles");
        let headers = [("X-API-Key", "s3cret")];
        assert!(auth.authenticate(&lookup(&headers), None).is_ok());
    }

    #[tokio::test]
    async fn a_wrong_or_missing_key_is_refused() {
        let auth = CompiledAuth::compile(&api_key_config(&["s3cret"]))
            .await
            .expect("compiles");
        assert!(
            auth.authenticate(&lookup(&[("Authorization", "Bearer nope")]), None)
                .is_err()
        );
        assert!(auth.authenticate(&lookup(&[]), None).is_err());
    }

    /// Several keys are accepted at once, which is what makes rotation
    /// possible without a window of refusals.
    #[tokio::test]
    async fn any_configured_key_is_accepted() {
        let auth = CompiledAuth::compile(&api_key_config(&["old", "new"]))
            .await
            .expect("compiles");
        for key in ["old", "new"] {
            let headers = [("Authorization", format!("Bearer {key}"))];
            let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
            assert!(auth.authenticate(&lookup(&pairs), None).is_ok(), "{key}");
        }
    }

    #[tokio::test]
    async fn api_key_mode_requires_keys() {
        let mut cfg = api_key_config(&[]);
        cfg.keys = None;
        assert!(CompiledAuth::compile(&cfg).await.is_err());
        let empty = api_key_config(&[]);
        assert!(CompiledAuth::compile(&empty).await.is_err());
    }

    fn hmac_config(secret: &str, prefix: Option<&str>) -> ChannelAuthConfig {
        ChannelAuthConfig {
            mode: AuthMode::Hmac,
            keys: None,
            header: Some("X-Signature".to_string()),
            scheme: None,
            secret: Some(secret.to_string()),
            signature_prefix: prefix.map(str::to_string),
        }
    }

    fn sign_hex(secret: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac key");
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    #[tokio::test]
    async fn hmac_accepts_a_correct_hex_signature() {
        let auth = CompiledAuth::compile(&hmac_config("whsec", None))
            .await
            .expect("compiles");
        let body = br#"{"id":"evt_1","amount":2000}"#;
        let headers = [("X-Signature", sign_hex("whsec", body))];
        let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert!(auth.authenticate(&lookup(&pairs), Some(body)).is_ok());
    }

    /// The GitHub spelling: `sha256=<hex>`.
    #[tokio::test]
    async fn hmac_strips_a_configured_signature_prefix() {
        let auth = CompiledAuth::compile(&hmac_config("whsec", Some("sha256=")))
            .await
            .expect("compiles");
        let body = br#"{"action":"opened"}"#;
        let headers = [("X-Signature", format!("sha256={}", sign_hex("whsec", body)))];
        let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert!(auth.authenticate(&lookup(&pairs), Some(body)).is_ok());
    }

    /// The Shopify spelling: base64.
    #[tokio::test]
    async fn hmac_accepts_a_base64_signature() {
        let auth = CompiledAuth::compile(&hmac_config("whsec", None))
            .await
            .expect("compiles");
        let body = br#"{"order":1}"#;
        let mut mac = HmacSha256::new_from_slice(b"whsec").expect("hmac key");
        mac.update(body);
        use base64::Engine;
        let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        let headers = [("X-Signature", sig)];
        let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert!(auth.authenticate(&lookup(&pairs), Some(body)).is_ok());
    }

    /// The property the whole mode exists for: one byte changed in the body
    /// invalidates the signature.
    #[tokio::test]
    async fn hmac_refuses_a_tampered_body() {
        let auth = CompiledAuth::compile(&hmac_config("whsec", None))
            .await
            .expect("compiles");
        let signed = br#"{"amount":2000}"#;
        let tampered = br#"{"amount":9999}"#;
        let headers = [("X-Signature", sign_hex("whsec", signed))];
        let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert!(auth.authenticate(&lookup(&pairs), Some(tampered)).is_err());
    }

    #[tokio::test]
    async fn hmac_refuses_a_signature_from_the_wrong_secret() {
        let auth = CompiledAuth::compile(&hmac_config("whsec", None))
            .await
            .expect("compiles");
        let body = br#"{"a":1}"#;
        let headers = [("X-Signature", sign_hex("attacker", body))];
        let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert!(auth.authenticate(&lookup(&pairs), Some(body)).is_err());
    }

    #[tokio::test]
    async fn hmac_refuses_a_malformed_or_absent_signature() {
        let auth = CompiledAuth::compile(&hmac_config("whsec", None))
            .await
            .expect("compiles");
        let body = br#"{"a":1}"#;
        assert!(
            auth.authenticate(&lookup(&[("X-Signature", "not-a-signature!")]), Some(body))
                .is_err()
        );
        assert!(auth.authenticate(&lookup(&[]), Some(body)).is_err());
    }

    #[tokio::test]
    async fn hmac_mode_requires_a_secret() {
        let mut cfg = hmac_config("x", None);
        cfg.secret = None;
        assert!(CompiledAuth::compile(&cfg).await.is_err());
    }

    /// Every refusal reads the same, so a caller cannot learn which half of the
    /// credential was right by comparing messages.
    #[tokio::test]
    async fn every_refusal_carries_the_same_message() {
        let auth = CompiledAuth::compile(&api_key_config(&["s3cret"]))
            .await
            .expect("compiles");
        let missing = auth
            .authenticate(&lookup(&[]), None)
            .expect_err("no header is a refusal");
        let wrong = auth
            .authenticate(&lookup(&[("Authorization", "Bearer nope")]), None)
            .expect_err("a wrong key is a refusal");
        assert_eq!(missing.to_string(), wrong.to_string());
    }
}
