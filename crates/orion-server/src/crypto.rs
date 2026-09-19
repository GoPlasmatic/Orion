//! Cryptographic primitives shared across the tree: the binary-encoding table,
//! the MAC helpers, the artifact digest and the Ed25519 trust check.
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
//! The same argument brought [`sha256_digest`] and [`ed25519`] down from the
//! plugin sandbox when models arrived: a plugin component and a model
//! artifact are identified by the same `sha256:<hex>` string and signed the
//! same way, and neither `plugin` nor `model` may name the other.

use base64::Engine as _;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use hmac::Mac;
use hmac::digest::KeyInit;
use sha2::{Digest as _, Sha256};

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

/// The identity of a stored artifact: `sha256:<64 lowercase hex>` of its
/// bytes.
///
/// One spelling for every artifact Orion stores by content — a plugin
/// component, a model file — so a digest a generation, a trace, a package
/// and a release pipeline name is the same string wherever it appears, and a
/// signature over it ([`ed25519::verify`]) is over the same message.
pub fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

/// Whether `s` has the shape [`sha256_digest`] produces: the `sha256:`
/// prefix and exactly 64 lowercase hex characters.
pub fn is_sha256_digest(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// Detached Ed25519 signatures over an artifact digest.
///
/// Optional hardening on top of admin auth. The trust root for installing an
/// artifact is the admin credential — the one that already reads and writes
/// connector secrets — so a signature adds no new principal; what it adds is
/// a check that survives the upload. The signed message is the digest string
/// exactly as [`sha256_digest`] renders it, so a release pipeline signs the
/// identity a generation, a trace and a package already name, and never
/// needs the bytes in memory to do it. Keys and signatures travel as standard
/// base64.
///
/// Two surfaces consume this with the same policy shape: `[plugins.trust]`
/// (verified at upload and again by every node that loads the version) and
/// `[models.trust]` (verified by the node that admits the artifact). A node
/// with no keys configured checks nothing and stores what it was sent.
pub mod ed25519 {
    use aws_lc_rs::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
    use base64::Engine as _;

    /// An Ed25519 public key, raw.
    pub const KEY_LEN: usize = 32;
    /// An Ed25519 signature.
    pub const SIGNATURE_LEN: usize = 64;

    fn b64() -> base64::engine::GeneralPurpose {
        base64::engine::general_purpose::STANDARD
    }

    /// One configured key, decoded. Refused at config validation rather than
    /// at the first upload, so a typo in a `public_keys` list cannot silently
    /// make every signature fail to verify.
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

    /// Whether `signature` is a valid Ed25519 signature over `digest` by one
    /// of `public_keys`. With no keys configured there is nothing to check
    /// and any signature — or none — passes.
    ///
    /// # Errors
    ///
    /// The reason, in a sentence an author can act on: no signature where one
    /// is required, a signature that is not base64 or not 64 bytes, or one
    /// that no configured key accepts. The caller names the setting the keys
    /// came from; this layer does not know it.
    pub fn verify(
        public_keys: &[String],
        digest: &str,
        signature: Option<&str>,
    ) -> Result<(), String> {
        if public_keys.is_empty() {
            return Ok(());
        }
        let Some(signature) = signature.map(str::trim).filter(|s| !s.is_empty()) else {
            return Err(format!(
                "this node requires a signature over the digest ({} trust key(s) configured) \
                 and none was given",
                public_keys.len()
            ));
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

    /// A `.sig` file's text as the one-line base64 an upload carries.
    ///
    /// Every ASCII whitespace character is removed first, so a trailing
    /// newline, CRLF line ends and a `base64` that wrapped the 88 characters
    /// at 76 all read as the signature they spell. The result must decode to
    /// exactly [`SIGNATURE_LEN`] bytes, and is re-encoded canonically.
    ///
    /// # Errors
    ///
    /// Empty, not base64, or not 64 bytes — each said in a sentence.
    pub fn normalize_signature(text: &str) -> Result<String, String> {
        let compact: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        if compact.is_empty() {
            return Err(format!(
                "empty — expected the base64 of a {SIGNATURE_LEN}-byte Ed25519 signature"
            ));
        }
        let bytes = b64()
            .decode(&compact)
            .map_err(|e| format!("not base64: {e}"))?;
        if bytes.len() != SIGNATURE_LEN {
            return Err(format!(
                "decodes to {} bytes, an Ed25519 signature is {SIGNATURE_LEN}",
                bytes.len()
            ));
        }
        Ok(b64().encode(bytes))
    }

    /// A signing key, for tests and for the `orion-server plugin|model
    /// keygen|sign` verbs that produce the signature an upload carries. The
    /// server never holds one: it verifies, it does not sign.
    pub struct SigningKey(Ed25519KeyPair);

    impl SigningKey {
        /// A fresh key pair.
        pub fn generate() -> Self {
            Self(Ed25519KeyPair::generate().expect("Ed25519 key generation cannot fail"))
        }

        /// A key from an unencrypted PKCS#8 private key in PEM — what
        /// [`Self::to_pkcs8_pem`] and `openssl genpkey -algorithm ed25519`
        /// both write. PKCS#8 v1 and v2 are both accepted.
        ///
        /// # Errors
        ///
        /// No `PRIVATE KEY` block (an encrypted or OpenSSH-format key names
        /// the conversion), or a key that is not Ed25519.
        pub fn from_pkcs8_pem(pem: &str) -> Result<Self, String> {
            use rustls::pki_types::PrivatePkcs8KeyDer;
            use rustls::pki_types::pem::PemObject as _;

            let der = PrivatePkcs8KeyDer::from_pem_slice(pem.as_bytes()).map_err(|_| {
                "not a PEM \"PRIVATE KEY\" block — an encrypted or OpenSSH-format key is not \
                 supported; convert it with `openssl pkey -in <key> -out signer.pem`"
                    .to_string()
            })?;
            Ed25519KeyPair::from_pkcs8(der.secret_pkcs8_der())
                .map(Self)
                .map_err(|e| format!("not an Ed25519 key ({e})"))
        }

        /// The key as an unencrypted PKCS#8 v1 private key in PEM, base64 at
        /// 64 columns — the form `openssl genpkey -algorithm ed25519` writes,
        /// so either tool reads the other's key.
        pub fn to_pkcs8_pem(&self) -> String {
            let der = self
                .0
                .to_pkcs8v1()
                .expect("an Ed25519 key pair always serialises to PKCS#8 v1");
            let body = b64().encode(der.as_ref());
            let mut pem = String::from("-----BEGIN PRIVATE KEY-----\n");
            let mut rest = body.as_str();
            while !rest.is_empty() {
                let (line, tail) = rest.split_at(rest.len().min(64));
                pem.push_str(line);
                pem.push('\n');
                rest = tail;
            }
            pem.push_str("-----END PRIVATE KEY-----\n");
            pem
        }

        /// The public half, base64 — what goes in a `trust.public_keys` list.
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

        const DIGEST: &str =
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        #[test]
        fn a_signature_by_a_configured_key_verifies_and_nothing_else_does() {
            let key = SigningKey::generate();
            let other = SigningKey::generate();
            let keys = vec![other.public_key_base64(), key.public_key_base64()];
            let sig = key.sign(DIGEST);

            verify(&keys, DIGEST, Some(&sig)).expect("signed by the second configured key");
            let err =
                verify(&keys, &DIGEST.replace('0', "1"), Some(&sig)).expect_err("other digest");
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

        /// A throw-away key written by `openssl genpkey -algorithm ed25519`,
        /// its public half as `openssl pkey -pubout | tail -c 32 | base64`
        /// prints it, and `openssl pkeyutl -sign -rawin` over [`DIGEST`],
        /// base64. Ed25519 is deterministic (RFC 8032), so the golden value
        /// pins interop with the documented OpenSSL pipeline with no OpenSSL
        /// at test time.
        const OPENSSL_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
             MC4CAQAwBQYDK2VwBCIEIIpW4b4xhXfqCE3oQChl9i9WwKgVBGt3tl8ug/2dD1R8\n\
             -----END PRIVATE KEY-----\n";
        const OPENSSL_PUBLIC: &str = "BSJl36HbLWCKvvaOZP/hycrqS0xySEa2UUQAjj2Xpws=";
        const OPENSSL_SIGNATURE: &str = "uDv0vIveDKYsMvPL9aSQKt3epQHbc5sG6iDAlQcSFGyzvkQhgcP88/uRELpGAKPnUg7rz8FPTQfkeTmBJaqPDQ==";

        #[test]
        fn an_openssl_key_loads_and_signs_as_openssl_does() {
            let key = SigningKey::from_pkcs8_pem(OPENSSL_PEM).expect("openssl's PKCS#8 v1");
            assert_eq!(key.public_key_base64(), OPENSSL_PUBLIC);
            assert_eq!(key.sign(DIGEST), OPENSSL_SIGNATURE);
            verify(
                &[OPENSSL_PUBLIC.to_string()],
                DIGEST,
                Some(OPENSSL_SIGNATURE),
            )
            .expect("the golden signature verifies");
            // And what this side writes is what openssl wrote.
            assert_eq!(key.to_pkcs8_pem(), OPENSSL_PEM);
        }

        #[test]
        fn a_generated_key_round_trips_through_pem() {
            let key = SigningKey::generate();
            let pem = key.to_pkcs8_pem();
            assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----\n"), "{pem}");
            let back = SigningKey::from_pkcs8_pem(&pem).expect("round trip");
            assert_eq!(back.public_key_base64(), key.public_key_base64());
            assert_eq!(back.sign(DIGEST), key.sign(DIGEST));
        }

        #[test]
        fn a_non_ed25519_or_encrypted_pem_is_refused_with_a_reason() {
            // `openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256`.
            let p256 = "-----BEGIN PRIVATE KEY-----\n\
                MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgU+6kHM8j3fHwZ3uE\n\
                4hJHGT5FFyJFtX3eItG0SYKhxlyhRANCAAQ8wczARjrBBPQMdpPzqA1q4hE4eT61\n\
                3NB5BrMK3NRcF12Nct5f5uuDK/XIVkCtJMxQCaSmhUc1mnZaGYQQR1+A\n\
                -----END PRIVATE KEY-----\n";
            let err = SigningKey::from_pkcs8_pem(p256).err().expect("not Ed25519");
            assert!(err.contains("not an Ed25519 key"), "{err}");
            let encrypted = "-----BEGIN ENCRYPTED PRIVATE KEY-----\nMAA=\n\
                             -----END ENCRYPTED PRIVATE KEY-----\n";
            let err = SigningKey::from_pkcs8_pem(encrypted)
                .err()
                .expect("encrypted");
            assert!(err.contains("encrypted or OpenSSH"), "{err}");
            let err = SigningKey::from_pkcs8_pem("hello").err().expect("no PEM");
            assert!(err.contains("openssl pkey"), "{err}");
        }

        #[test]
        fn normalize_signature_tolerates_whitespace_and_nothing_else() {
            assert_eq!(
                normalize_signature(&format!("{OPENSSL_SIGNATURE}\n")).expect("newline"),
                OPENSSL_SIGNATURE
            );
            let (a, b) = OPENSSL_SIGNATURE.split_at(76);
            assert_eq!(
                normalize_signature(&format!("{a}\r\n{b}\r\n")).expect("wrapped"),
                OPENSSL_SIGNATURE
            );
            let err = normalize_signature("not base64!").expect_err("garbage");
            assert!(err.contains("not base64"), "{err}");
            let err = normalize_signature(&b64().encode([7u8; 63])).expect_err("63 bytes");
            assert!(err.contains("decodes to 63 bytes"), "{err}");
            let err = normalize_signature(" \n").expect_err("empty");
            assert!(err.contains("empty"), "{err}");
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

    /// The digest spelling every artifact shares: prefix, lowercase hex, 71
    /// characters. A plugin component and a model file hash identically.
    #[test]
    fn the_artifact_digest_is_prefixed_lowercase_hex() {
        let digest = sha256_digest(b"hello world");
        assert_eq!(
            digest,
            "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        assert!(is_sha256_digest(&digest));
        assert!(!is_sha256_digest(&digest.to_uppercase()));
        assert!(!is_sha256_digest("sha256:abc"));
        assert!(!is_sha256_digest(&digest["sha256:".len()..]));
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
