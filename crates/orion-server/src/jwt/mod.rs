//! The shared JWT core of #267 — one verification pipeline and one signer
//! behind three surfaces: the `jwt` channel auth mode, `jwt_verify`, and
//! `jwt_sign`.
//!
//! Design stance (RFC 8725 throughout): the caller's **algorithm allowlist is
//! mandatory** and checked before anything else — which alone makes
//! `alg: none` and downgrade attacks unrepresentable; nothing from the token
//! header is trusted beyond `kid` routing; key/algorithm family mismatches
//! are compile-time (config) errors; HS secrets shorter than the hash length
//! are refused (RFC 7518 §3.2). Verification errors carry a typed
//! [`RejectReason`] — surfaced in metrics and traces, never on the wire
//! except for expiry (the one failure a well-behaved client acts on).

pub mod jwks;

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Validation};
use serde_json::Value;

/// The clock-skew policy both verify surfaces share: this default, capped at
/// this maximum (the channel mode refuses a larger configured value, the
/// `jwt_verify` task clamps).
pub const DEFAULT_LEEWAY_SECS: u64 = 30;
pub const MAX_LEEWAY_SECS: u64 = 300;
/// Default cap on the size of a presented token.
pub const DEFAULT_MAX_TOKEN_BYTES: usize = 8_192;

/// Refuse a non-HTTPS JWKS URL — keys fetched over plaintext are keys an
/// on-path attacker chose. One rule for every surface that configures one;
/// callers prefix the field name.
pub fn validate_jwks_url(url: &str) -> Result<(), String> {
    if url.starts_with("https://") {
        Ok(())
    } else {
        Err(format!(
            "must be HTTPS — keys fetched over plaintext are keys an on-path \
             attacker chose (got '{url}')"
        ))
    }
}

/// Why a token was refused. `as_str` feeds metrics/trace labels; the wire
/// response stays uniform (see `RejectReason::wire_description`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    Missing,
    Oversized,
    Malformed,
    AlgRejected,
    UnknownKid,
    BadSignature,
    Expired,
    NotYetValid,
    IssuerMismatch,
    AudienceMismatch,
    /// A required spec claim (`exp`) is absent.
    MissingClaim,
    /// JWKS could not be fetched and nothing usable was cached.
    KeysUnavailable,
}

impl RejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Oversized => "oversized",
            Self::Malformed => "malformed",
            Self::AlgRejected => "alg_rejected",
            Self::UnknownKid => "unknown_kid",
            Self::BadSignature => "bad_signature",
            Self::Expired => "expired",
            Self::NotYetValid => "not_yet_valid",
            Self::IssuerMismatch => "issuer_mismatch",
            Self::AudienceMismatch => "audience_mismatch",
            Self::MissingClaim => "missing_claim",
            Self::KeysUnavailable => "keys_unavailable",
        }
    }

    /// The one reason named on the wire: expiry, which a client answers with
    /// a refresh. Every other cause is uniform — distinguishing them tells an
    /// attacker which half they got right, while an expired token's holder
    /// already knows its `exp`.
    pub fn wire_description(self) -> Option<&'static str> {
        matches!(self, Self::Expired).then_some("token expired")
    }
}

/// One accepted verification key, compiled: the parsed key material plus the
/// routing facts (`kid`, algorithm).
pub struct StaticKey {
    pub kid: Option<String>,
    pub algorithm: Algorithm,
    pub key: DecodingKey,
}

/// A compiled verifier — everything shared by the channel mode and
/// `jwt_verify`.
pub struct Verifier {
    pub static_keys: Vec<StaticKey>,
    pub jwks_url: Option<String>,
    pub algorithms: Vec<Algorithm>,
    pub issuer: Vec<String>,
    pub audience: Vec<String>,
    pub leeway_secs: u64,
    pub require_exp: bool,
    pub max_token_bytes: usize,
    /// One `jsonwebtoken::Validation` per allowed algorithm, built on first
    /// use — `set_issuer`/`set_audience` allocate `HashSet<String>`s, and the
    /// channel mode's Verifier is long-lived, so rebuilding them per request
    /// would be per-request garbage. Constructors pass `OnceLock::new()`.
    pub validations: std::sync::OnceLock<Vec<(Algorithm, Validation)>>,
}

impl Verifier {
    /// Verify one compact JWS and return its claims. The fail-fast order is
    /// the module contract: size → header parse → allowlist → key routing →
    /// signature → temporal/identity claims.
    pub async fn verify(&self, token: &str) -> Result<Value, RejectReason> {
        if token.is_empty() {
            return Err(RejectReason::Missing);
        }
        if token.len() > self.max_token_bytes {
            return Err(RejectReason::Oversized);
        }
        let header = jsonwebtoken::decode_header(token).map_err(|_| RejectReason::Malformed)?;
        // The allowlist first: nothing else about the token is even looked at
        // for an algorithm the channel did not opt into.
        if !self.algorithms.contains(&header.alg) {
            return Err(RejectReason::AlgRejected);
        }

        let validation = self.validation(header.alg);
        let kid = header.kid.as_deref();

        // Static keys: exact-kid matches first, then kid-less keys of the
        // right algorithm (a config without kids means "try them").
        let mut saw_candidate = false;
        let mut last: Option<RejectReason> = None;
        for exact_kid in [true, false] {
            for candidate in self.static_keys.iter().filter(|k| {
                k.algorithm == header.alg
                    && if exact_kid {
                        kid.is_some() && k.kid.as_deref() == kid
                    } else {
                        k.kid.is_none()
                    }
            }) {
                saw_candidate = true;
                match try_key(token, &candidate.key, validation) {
                    Ok(claims) => return Ok(claims),
                    // A wrong signature may just be the wrong key — keep
                    // trying. Any *claim* failure means the signature held,
                    // so it is the real answer.
                    Err(RejectReason::BadSignature) => last = Some(RejectReason::BadSignature),
                    Err(other) => return Err(other),
                }
            }
        }

        // JWKS: routed by kid; a miss forces one rate-limited refetch.
        if let Some(url) = &self.jwks_url {
            let keys = jwks::decoding_keys(url, kid, header.alg).await?;
            for key in &keys {
                saw_candidate = true;
                match try_key(token, key, validation) {
                    Ok(claims) => return Ok(claims),
                    Err(RejectReason::BadSignature) => last = Some(RejectReason::BadSignature),
                    Err(other) => return Err(other),
                }
            }
        }

        Err(if saw_candidate {
            last.unwrap_or(RejectReason::BadSignature)
        } else {
            RejectReason::UnknownKid
        })
    }

    /// The cached `Validation` for one allowed algorithm. `alg` must have
    /// passed the allowlist check, which is also what bounds the cache.
    fn validation(&self, alg: Algorithm) -> &Validation {
        let validations = self.validations.get_or_init(|| {
            self.algorithms
                .iter()
                .map(|&a| (a, self.build_validation(a)))
                .collect()
        });
        validations
            .iter()
            .find_map(|(a, v)| (*a == alg).then_some(v))
            .expect("algorithm allowlist membership checked before key routing")
    }

    fn build_validation(&self, alg: Algorithm) -> Validation {
        let mut v = Validation::new(alg);
        v.leeway = self.leeway_secs;
        v.validate_exp = true;
        v.validate_nbf = true;
        if self.require_exp {
            v.set_required_spec_claims(&["exp"]);
        } else {
            v.required_spec_claims.clear();
        }
        if self.issuer.is_empty() {
            v.iss = None;
        } else {
            v.set_issuer(&self.issuer);
        }
        if self.audience.is_empty() {
            v.validate_aud = false;
        } else {
            v.set_audience(&self.audience);
        }
        v
    }
}

fn try_key(token: &str, key: &DecodingKey, validation: &Validation) -> Result<Value, RejectReason> {
    use jsonwebtoken::errors::ErrorKind;
    match jsonwebtoken::decode::<Value>(token, key, validation) {
        Ok(data) => Ok(data.claims),
        Err(e) => Err(match e.kind() {
            ErrorKind::ExpiredSignature => RejectReason::Expired,
            ErrorKind::ImmatureSignature => RejectReason::NotYetValid,
            ErrorKind::InvalidIssuer => RejectReason::IssuerMismatch,
            ErrorKind::InvalidAudience => RejectReason::AudienceMismatch,
            ErrorKind::MissingRequiredClaim(_) => RejectReason::MissingClaim,
            ErrorKind::InvalidSignature => RejectReason::BadSignature,
            // Anything structural (base64, JSON, key shape) — the token, not
            // the config, is the malformed party at this point.
            _ => RejectReason::BadSignature,
        }),
    }
}

/// The algorithm-string table — one place, both directions, so config errors
/// can list exactly what exists. `ES512` is deliberately absent: the
/// underlying library does not implement P-521, and a value that fails at
/// runtime is worse than absence.
pub const ALGORITHM_NAMES: &[&str] = &[
    "HS256", "HS384", "HS512", "RS256", "RS384", "RS512", "PS256", "PS384", "PS512", "ES256",
    "ES384", "EdDSA",
];

pub fn parse_algorithm(name: &str) -> Result<Algorithm, String> {
    Ok(match name {
        "HS256" => Algorithm::HS256,
        "HS384" => Algorithm::HS384,
        "HS512" => Algorithm::HS512,
        "RS256" => Algorithm::RS256,
        "RS384" => Algorithm::RS384,
        "RS512" => Algorithm::RS512,
        "PS256" => Algorithm::PS256,
        "PS384" => Algorithm::PS384,
        "PS512" => Algorithm::PS512,
        "ES256" => Algorithm::ES256,
        "ES384" => Algorithm::ES384,
        "EdDSA" => Algorithm::EdDSA,
        other => {
            return Err(format!(
                "algorithm '{other}' is not supported — one of {}",
                ALGORITHM_NAMES.join(", ")
            ));
        }
    })
}

/// How a textual HS secret becomes bytes — the #259 `key_encoding` precedent
/// (`utf8` default, or `base64`/`hex` for binary secrets, Supabase-class).
pub fn secret_bytes(secret: &str, key_encoding: Option<&str>) -> Result<Vec<u8>, String> {
    use crate::engine::operators::{Codec, decode_bytes};
    match key_encoding {
        None | Some("utf8") => Ok(secret.as_bytes().to_vec()),
        Some("base64") => decode_bytes(Codec::Base64, secret)
            .map_err(|e| format!("key does not decode as base64: {e}")),
        Some("hex") => {
            decode_bytes(Codec::Hex, secret).map_err(|e| format!("key does not decode as hex: {e}"))
        }
        Some(other) => Err(format!(
            "key_encoding '{other}' is not supported — utf8 (default), base64, hex"
        )),
    }
}

/// The minimum HS secret length per RFC 7518 §3.2: at least the hash length.
fn hs_min_len(alg: Algorithm) -> usize {
    match alg {
        Algorithm::HS256 => 32,
        Algorithm::HS384 => 48,
        _ => 64,
    }
}

/// Build a verification key from resolved material: an HS secret (with its
/// encoding) or a PEM public key for the asymmetric families. The family
/// check is structural — `from_rsa_pem` on an EC key fails — and the HS
/// length floor is enforced here so it holds on every surface.
pub fn decoding_key(
    algorithm: Algorithm,
    material: &str,
    key_encoding: Option<&str>,
) -> Result<DecodingKey, String> {
    use jsonwebtoken::AlgorithmFamily;
    match algorithm.family() {
        AlgorithmFamily::Hmac => {
            let bytes = secret_bytes(material, key_encoding)?;
            if bytes.len() < hs_min_len(algorithm) {
                return Err(format!(
                    "HS secret is {} bytes; RFC 7518 requires at least the hash length \
                     ({} bytes) — a shorter secret weakens the MAC",
                    bytes.len(),
                    hs_min_len(algorithm)
                ));
            }
            Ok(DecodingKey::from_secret(&bytes))
        }
        AlgorithmFamily::Rsa => DecodingKey::from_rsa_pem(material.as_bytes())
            .map_err(|e| format!("not a usable RSA public key PEM: {e}")),
        AlgorithmFamily::Ec => DecodingKey::from_ec_pem(material.as_bytes())
            .map_err(|e| format!("not a usable EC public key PEM: {e}")),
        AlgorithmFamily::Ed => DecodingKey::from_ed_pem(material.as_bytes())
            .map_err(|e| format!("not a usable Ed25519 public key PEM: {e}")),
    }
}

/// Build a signing key from resolved material — `decoding_key`'s inverse,
/// taking the *private* half for the asymmetric families.
pub fn encoding_key(
    algorithm: Algorithm,
    material: &str,
    key_encoding: Option<&str>,
) -> Result<EncodingKey, String> {
    use jsonwebtoken::AlgorithmFamily;
    match algorithm.family() {
        AlgorithmFamily::Hmac => {
            let bytes = secret_bytes(material, key_encoding)?;
            if bytes.len() < hs_min_len(algorithm) {
                return Err(format!(
                    "HS secret is {} bytes; RFC 7518 requires at least the hash length \
                     ({} bytes)",
                    bytes.len(),
                    hs_min_len(algorithm)
                ));
            }
            Ok(EncodingKey::from_secret(&bytes))
        }
        AlgorithmFamily::Rsa => EncodingKey::from_rsa_pem(material.as_bytes())
            .map_err(|e| format!("not a usable RSA private key PEM: {e}")),
        AlgorithmFamily::Ec => EncodingKey::from_ec_pem(material.as_bytes())
            .map_err(|e| format!("not a usable EC private key PEM: {e}")),
        AlgorithmFamily::Ed => EncodingKey::from_ed_pem(material.as_bytes())
            .map_err(|e| format!("not a usable Ed25519 private key PEM: {e}")),
    }
}

/// Sign `claims` — already a complete claims object — into a compact JWS.
pub fn sign(
    algorithm: Algorithm,
    key: &EncodingKey,
    kid: Option<String>,
    claims: &Value,
) -> Result<String, String> {
    let mut header = jsonwebtoken::Header::new(algorithm);
    header.kid = kid;
    jsonwebtoken::encode(&header, claims, key).map_err(|e| format!("signing failed: {e}"))
}

/// Throwaway keypairs for the test suites — they secure nothing and do not
/// outlive the test binary that generates them.
///
/// Generated in-process rather than committed as `testkeys/*.pem`, which is
/// what they were until the repo-wide `*.pem` ignore rule (there to keep TLS
/// material out of a public repo) silently kept them out of git as well. They
/// existed on the author's disk, so every local build passed; CI checked out
/// a tree without them and could not compile any test target. Generating
/// removes both halves of that trap: nothing to forget to commit, and no
/// private key in the repository to have to reason about.
///
/// One generation per key type per test binary, on first use — `LazyLock`
/// rather than a `fn`, because RSA-2048 keygen is the one expensive step here
/// and the JWT suite reaches for the same key from several tests.
#[cfg(test)]
pub mod testkeys {
    use std::sync::LazyLock;

    /// A PKCS#8 private key and its SPKI public key, both PEM — precisely the
    /// two shapes `EncodingKey::from_*_pem` and `DecodingKey::from_*_pem`
    /// accept, so these drop straight into a channel `key` field.
    pub struct Keypair {
        pub private: String,
        pub public: String,
    }

    fn generate(alg: &'static rcgen::SignatureAlgorithm) -> Keypair {
        let key = rcgen::KeyPair::generate_for(alg).expect("test keypair generation");
        Keypair {
            private: key.serialize_pem(),
            public: key.public_key_pem(),
        }
    }

    /// RSA-2048 — serves both the RS (PKCS#1 v1.5) and PS (PSS) families.
    pub static RSA: LazyLock<Keypair> = LazyLock::new(|| generate(&rcgen::PKCS_RSA_SHA256));
    /// NIST P-256, for ES256.
    pub static EC: LazyLock<Keypair> = LazyLock::new(|| generate(&rcgen::PKCS_ECDSA_P256_SHA256));
    /// Ed25519, for EdDSA.
    pub static ED: LazyLock<Keypair> = LazyLock::new(|| generate(&rcgen::PKCS_ED25519));
}
