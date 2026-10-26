//! [`Digest`]: the one spelling of a content digest, `sha256:<64 lowercase hex>`.
//!
//! Every artifact Orion stores by content — a provider package, a definition,
//! a plugin component, a connection's configuration — is named by the SHA-256
//! of its bytes in this form. A `Digest` is parsed once, where the text comes
//! in (a request, a stored row, a file on disk), and trusted after: holding
//! one means the shape was checked, so nothing downstream re-checks it.
//!
//! Like the [`crate::enums`] types, it is **strict on purpose**: it is for
//! the request side and for server-internal use. The response DTOs keep their
//! digest fields as `String`, so their tolerant deserialization is unchanged.
//! Serialized, a `Digest` is the plain string, so a field changing from
//! `String` to `Digest` does not change the wire.

use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A `sha256:<64 lowercase hex>` content digest, checked when it was made.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Digest(String);

/// Why a string is not a [`Digest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestError {
    got: String,
}

impl fmt::Display for DigestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "'{}' is not a digest: expected sha256:<64 lowercase hex>",
            self.got
        )
    }
}

impl std::error::Error for DigestError {}

const HEX_LEN: usize = 64;

fn is_lower_hex(b: u8) -> bool {
    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
}

impl Digest {
    /// The algorithm prefix every digest carries.
    pub const PREFIX: &'static str = "sha256:";

    /// `s` as a digest, refused unless it is `sha256:` followed by exactly 64
    /// lowercase hex characters. Uppercase hex is refused rather than folded:
    /// two spellings of one digest would be two keys.
    pub fn parse(s: &str) -> Result<Self, DigestError> {
        let well_formed = s
            .strip_prefix(Self::PREFIX)
            .is_some_and(|hex| hex.len() == HEX_LEN && hex.bytes().all(is_lower_hex));
        if well_formed {
            Ok(Self(s.to_string()))
        } else {
            Err(DigestError { got: s.to_string() })
        }
    }

    /// The digest of a SHA-256 hash value (the 32 bytes a hasher finishes
    /// with).
    pub fn from_sha256(hash: [u8; 32]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(Self::PREFIX.len() + HEX_LEN);
        out.push_str(Self::PREFIX);
        for b in hash {
            out.push(HEX[usize::from(b >> 4)] as char);
            out.push(HEX[usize::from(b & 0x0f)] as char);
        }
        Self(out)
    }

    /// The whole digest, prefix included.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The 64 hex characters after the prefix: safe as a file name, since
    /// they are exactly `[0-9a-f]{64}`.
    pub fn hex(&self) -> &str {
        &self.0[Self::PREFIX.len()..]
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl FromStr for Digest {
    type Err = DigestError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<String> for Digest {
    type Error = DigestError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        // Reuse the allocation when the text is already well formed.
        match Self::parse(&s) {
            Ok(_) => Ok(Self(s)),
            Err(e) => Err(e),
        }
    }
}

impl TryFrom<&str> for Digest {
    type Error = DigestError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::parse(s)
    }
}

impl From<Digest> for String {
    fn from(d: Digest) -> Self {
        d.0
    }
}

impl Deref for Digest {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Digest {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for Digest {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl PartialEq<str> for Digest {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for Digest {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for Digest {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}

impl PartialEq<Digest> for str {
    fn eq(&self, other: &Digest) -> bool {
        self == other.0
    }
}

impl PartialEq<Digest> for &str {
    fn eq(&self, other: &Digest) -> bool {
        *self == other.0
    }
}

impl PartialEq<Digest> for String {
    fn eq(&self, other: &Digest) -> bool {
        *self == other.0
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const ZEROS: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn only_the_canonical_spelling_parses() {
        assert!(Digest::parse(ZEROS).is_ok());
        assert!(Digest::parse(&ZEROS.to_uppercase()).is_err());
        assert!(Digest::parse(&ZEROS.replace("sha256:", "SHA256:")).is_err());
        assert!(Digest::parse("sha256:abc").is_err());
        assert!(Digest::parse(&ZEROS[Digest::PREFIX.len()..]).is_err());
        assert!(Digest::parse(&format!("{ZEROS}0")).is_err());
        assert!(Digest::parse(&ZEROS.replace("00", "0g")).is_err());
        assert!(Digest::parse(&ZEROS.replacen('0', "/", 1)).is_err());
    }

    #[test]
    fn a_hash_value_spells_as_lowercase_hex() {
        let mut hash = [0u8; 32];
        hash[0] = 0xab;
        hash[31] = 0x0f;
        let d = Digest::from_sha256(hash);
        assert_eq!(Digest::parse(d.as_str()), Ok(d.clone()));
        assert!(d.hex().starts_with("ab00"));
        assert!(d.hex().ends_with("000f"));
    }

    #[test]
    fn the_wire_form_is_the_string() {
        let d = Digest::parse(ZEROS).unwrap();
        assert_eq!(serde_json::to_value(&d).unwrap(), serde_json::json!(ZEROS));
        let back: Digest = serde_json::from_value(serde_json::json!(ZEROS)).unwrap();
        assert_eq!(back, d);
        assert!(serde_json::from_value::<Digest>(serde_json::json!("sha256:abc")).is_err());
    }
}
