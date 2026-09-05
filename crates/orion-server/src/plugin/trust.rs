//! `[plugins.trust]`: detached Ed25519 signatures over a component digest.
//!
//! Optional hardening on top of admin auth. The trust root for installing a
//! plugin is the admin credential — the one that already reads and writes
//! connector secrets — so a signature adds no new principal; what it adds is
//! a check that survives the upload. The signed message is the digest string
//! exactly as the server computes it (`sha256:<64 hex>`), so a release
//! pipeline signs the identity a generation, a trace and a package already
//! name, and never needs the bytes in memory to do it. Keys and signatures
//! travel as standard base64.
//!
//! Verified twice: when an upload arrives (`services::plugins::prepare`),
//! and again by every node that loads the version (`loader::load_one`), so a
//! row that reached the database by any other path — an import on a node
//! with no keys, a peer's activation — is checked by the node that runs it.
//! A node with no keys configured checks nothing and stores what it was sent.

use aws_lc_rs::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use base64::Engine as _;

/// An Ed25519 public key, raw.
pub const KEY_LEN: usize = 32;
/// An Ed25519 signature.
pub const SIGNATURE_LEN: usize = 64;

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// One configured key, decoded. Refused at config validation rather than at
/// the first upload, so a typo in `public_keys` cannot silently make every
/// signature fail to verify.
pub fn parse_public_key(encoded: &str) -> Result<Vec<u8>, String> {
    let bytes = b64()
        .decode(encoded.trim())
        .map_err(|e| format!("not base64: {e}"))?;
    if bytes.len() != KEY_LEN {
        return Err(format!(
            "an Ed25519 public key is {KEY_LEN} bytes, this one decodes to {}",
            bytes.len()
        ));
    }
    Ok(bytes)
}

/// Whether `signature` is a valid Ed25519 signature over `digest` by one of
/// `public_keys`. With no keys configured there is nothing to check and any
/// signature — or none — passes.
///
/// # Errors
///
/// The reason, in a sentence an author can act on: no signature where one is
/// required, a signature that is not base64 or not 64 bytes, or one that no
/// configured key accepts.
pub fn verify(public_keys: &[String], digest: &str, signature: Option<&str>) -> Result<(), String> {
    if public_keys.is_empty() {
        return Ok(());
    }
    let Some(signature) = signature.map(str::trim).filter(|s| !s.is_empty()) else {
        return Err(
            "this node requires a signature over the component digest (plugins.trust.public_keys \
             is set) and none was given"
                .to_string(),
        );
    };
    let sig = b64()
        .decode(signature)
        .map_err(|e| format!("signature is not base64: {e}"))?;
    if sig.len() != SIGNATURE_LEN {
        return Err(format!(
            "an Ed25519 signature is {SIGNATURE_LEN} bytes, this one decodes to {}",
            sig.len()
        ));
    }
    for key in public_keys {
        let key = parse_public_key(key)?;
        if UnparsedPublicKey::new(&ED25519, key)
            .verify(digest.as_bytes(), &sig)
            .is_ok()
        {
            return Ok(());
        }
    }
    Err(format!(
        "the signature does not verify over {digest} with any of the {} configured key(s)",
        public_keys.len()
    ))
}

/// A signing key, for tests and tooling that produce the signature an upload
/// carries. The server never holds one: it verifies, it does not sign.
pub struct SigningKey(Ed25519KeyPair);

impl SigningKey {
    /// A fresh key pair.
    pub fn generate() -> Self {
        Self(Ed25519KeyPair::generate().expect("Ed25519 key generation cannot fail"))
    }

    /// The public half, base64 — what goes in `plugins.trust.public_keys`.
    pub fn public_key_base64(&self) -> String {
        b64().encode(self.0.public_key().as_ref())
    }

    /// The signature over `digest`, base64 — what an upload carries.
    pub fn sign(&self, digest: &str) -> String {
        b64().encode(self.0.sign(digest.as_bytes()).as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn a_signature_by_a_configured_key_verifies_and_nothing_else_does() {
        let key = SigningKey::generate();
        let other = SigningKey::generate();
        let keys = vec![other.public_key_base64(), key.public_key_base64()];
        let sig = key.sign(DIGEST);

        verify(&keys, DIGEST, Some(&sig)).expect("signed by the second configured key");
        let err = verify(&keys, &DIGEST.replace('0', "1"), Some(&sig)).expect_err("other digest");
        assert!(err.contains("does not verify"), "{err}");
        let err = verify(&[other.public_key_base64()], DIGEST, Some(&sig))
            .expect_err("a key that did not sign");
        assert!(err.contains("does not verify"), "{err}");
        let err = verify(&keys, DIGEST, None).expect_err("no signature");
        assert!(err.contains("none was given"), "{err}");
        let err = verify(&keys, DIGEST, Some("not base64!")).expect_err("garbage");
        assert!(err.contains("not base64"), "{err}");
        let err = verify(&keys, DIGEST, Some(&b64().encode([0u8; 10]))).expect_err("short");
        assert!(err.contains("64 bytes"), "{err}");
    }

    #[test]
    fn no_configured_key_means_nothing_is_checked() {
        verify(&[], DIGEST, None).expect("no keys, no check");
        verify(&[], DIGEST, Some("anything")).expect("no keys, no check");
    }

    #[test]
    fn a_public_key_must_decode_to_thirty_two_bytes() {
        assert!(parse_public_key("nope").is_err());
        let err = parse_public_key(&b64().encode([1u8; 31])).expect_err("31 bytes");
        assert!(err.contains("32 bytes"), "{err}");
        assert_eq!(
            parse_public_key(&SigningKey::generate().public_key_base64())
                .expect("valid")
                .len(),
            KEY_LEN
        );
    }
}
