//! Cryptographic primitives shared across the tree: the binary-encoding table
//! and the MAC helpers.
//!
//! These lived in `engine::operators`, which is where the JSONLogic
//! `base64_encode` / `hex_decode` family is registered. That made the operator
//! module the accidental home of shared crypto, and four modules that have
//! nothing to do with JSONLogic reached *upward* into the engine to borrow it:
//! `channel::auth` for HMAC webhook verification, `connector::sigv4` for AWS
//! request signing, `jwt` for decoding key material, and the `crypto` task
//! function. A primitive every layer needs belongs below all of them.
//!
//! Nothing here is new — it is the same code, one level down — and that is
//! deliberate: these are the spellings the security-sensitive paths already
//! agreed on, and the value of having one of each is exactly that it is one.

use base64::Engine as _;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use hmac::Mac;
use hmac::digest::KeyInit;

/// Standard-alphabet decoder that accepts padded and unpadded input.
/// Encoding always uses the canonical [`base64::engine::general_purpose::STANDARD`]
/// (padded); this leniency is decode-only.
const B64_STD_LENIENT: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// URL-safe-alphabet decoder that accepts padded and unpadded input.
/// Encoding always uses [`base64::engine::general_purpose::URL_SAFE_NO_PAD`] —
/// the unpadded RFC 4648 §5 form JWS uses, per the #259 encoding table.
const B64_URL_LENIENT: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Which alphabet an encode/decode call speaks. One vocabulary for the
/// JSONLogic operators, the `crypto` task function, channel HMAC auth and JWT
/// key material, so the #259 encoding table is implemented exactly once.
#[derive(Debug, Clone, Copy)]
pub enum Codec {
    Base64,
    Base64Url,
    Hex,
}

impl Codec {
    /// The canonical name → codec table (`hex`, `base64`, `base64url`).
    /// Callers own their defaults and error wording.
    pub fn parse(name: &str) -> Option<Codec> {
        match name {
            "hex" => Some(Codec::Hex),
            "base64" => Some(Codec::Base64),
            "base64url" => Some(Codec::Base64Url),
            _ => None,
        }
    }
}

/// Canonical encoding of `bytes` per the #259 table: hex lowercase, base64
/// standard padded, base64url unpadded (the JWS form).
pub fn encode_bytes(codec: Codec, bytes: &[u8]) -> String {
    match codec {
        Codec::Base64 => base64::engine::general_purpose::STANDARD.encode(bytes),
        Codec::Base64Url => base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
        Codec::Hex => hex::encode(bytes),
    }
}

/// Strict decode per the same table; the base64 forms tolerate padded and
/// unpadded input.
pub fn decode_bytes(codec: Codec, s: &str) -> Result<Vec<u8>, String> {
    match codec {
        Codec::Base64 => B64_STD_LENIENT.decode(s).map_err(|e| e.to_string()),
        Codec::Base64Url => B64_URL_LENIENT.decode(s).map_err(|e| e.to_string()),
        Codec::Hex => hex::decode(s).map_err(|e| e.to_string()),
    }
}

/// Compute an HMAC over `data` — the one spelling of the MAC primitive that
/// the `crypto` function and SigV4 signing share.
pub fn mac_compute<M: Mac + KeyInit>(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = M::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Verify an HMAC — constant-time and length-checked (`verify_slice`), which
/// is the reason the verify surfaces exist at all: without this helper the
/// obvious spelling is `==` on the computed MAC. Shared by the `crypto`
/// function's `hmac_verify` and channel HMAC auth.
pub fn mac_verify<M: Mac + KeyInit>(key: &[u8], data: &[u8], signature: &[u8]) -> bool {
    let Ok(mut mac) = M::new_from_slice(key) else {
        return false;
    };
    mac.update(data);
    mac.verify_slice(signature).is_ok()
}

/// `n` bytes from the operating-system CSPRNG.
///
/// Lives here rather than at each call site because the alternatives are all
/// wrong in the same quiet way: `rand::random::<u64>()` gives 8 bytes when the
/// caller asked for 32, and a `Uuid` gives 122 bits of entropy inside a
/// structure whose version and variant nibbles are fixed. Both look like a
/// nonce and neither is one at the width a CSRF `state` or a PKCE verifier
/// needs (RFC 7636 §4.1 asks for 32 octets).
///
/// `rand::rng()` is the thread-local generator seeded from the OS and
/// periodically reseeded — the same source `engine::operators`'s `random`
/// operator draws from, so a nonce minted here and one minted in JSONLogic
/// have the same provenance.
pub fn random_bytes(n: usize) -> Vec<u8> {
    use rand::Rng as _;
    let mut buf = vec![0u8; n];
    rand::rng().fill_bytes(&mut buf);
    buf
}

/// Install the process-wide rustls crypto provider if nothing has yet.
///
/// rustls refuses to build a config until one is installed, and the choice is
/// process-global, so it has to be idempotent and reachable from anywhere that
/// opens a TLS connection. That is not only the HTTPS listener: the SMTP
/// connector pool builds its own client config, and reaching up into
/// `server::tls` from `connector::smtp_pool` to install a crypto provider was
/// the layering saying so.
pub fn ensure_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The #259 table, pinned: hex lowercase, base64 padded, base64url
    /// unpadded. Every surface that names an encoding resolves through here,
    /// so a change to any of these three is a change to the wire format of
    /// webhook signatures, JWS segments and `crypto` task output at once.
    #[test]
    fn the_encoding_table_is_one_table() {
        let bytes = b"\xde\xad\xbe\xef\xff";
        assert_eq!(encode_bytes(Codec::Hex, bytes), "deadbeefff");
        assert_eq!(encode_bytes(Codec::Base64, bytes), "3q2+7/8=");
        assert_eq!(encode_bytes(Codec::Base64Url, bytes), "3q2-7_8");

        for codec in [Codec::Hex, Codec::Base64, Codec::Base64Url] {
            let encoded = encode_bytes(codec, bytes);
            assert_eq!(decode_bytes(codec, &encoded).expect("round trip"), bytes);
        }
    }

    /// Decoding tolerates padding in both directions; encoding never does.
    #[test]
    fn base64_decoding_is_indifferent_to_padding() {
        assert_eq!(
            decode_bytes(Codec::Base64, "3q2+7/8").expect("unpadded standard"),
            b"\xde\xad\xbe\xef\xff"
        );
        assert_eq!(
            decode_bytes(Codec::Base64Url, "3q2-7_8=").expect("padded url-safe"),
            b"\xde\xad\xbe\xef\xff"
        );
    }

    #[test]
    fn an_unknown_codec_name_is_not_guessed_at() {
        assert!(Codec::parse("base32").is_none());
        assert!(Codec::parse("BASE64").is_none());
    }

    /// `mac_verify` must reject a signature of the wrong length rather than
    /// panicking or truncating — the case a hand-written `==` gets wrong.
    #[test]
    fn mac_verify_rejects_a_wrong_length_signature() {
        type H = hmac::Hmac<sha2::Sha256>;
        let key = b"a-webhook-secret";
        let data = b"payload";
        let good = mac_compute::<H>(key, data);

        assert!(mac_verify::<H>(key, data, &good));
        assert!(!mac_verify::<H>(key, data, &good[..16]));
        assert!(!mac_verify::<H>(key, data, &[]));
        assert!(!mac_verify::<H>(b"wrong-secret", data, &good));
    }

    /// Width and freshness, the two properties a nonce is used for. A
    /// generator that returned a constant would satisfy the length assertion
    /// alone, which is exactly the failure mode #307 hit with a `jwt_sign`
    /// state whose claims were identical for two sign-ins in one second.
    #[test]
    fn random_bytes_are_the_requested_width_and_do_not_repeat() {
        assert_eq!(random_bytes(32).len(), 32);
        assert_eq!(random_bytes(0).len(), 0);
        assert_ne!(random_bytes(32), random_bytes(32));
    }
}
