//! The package receipt key rule: what a package name, version or content
//! hash may be spelled as.
//!
//! One rule with several callers — the receipt route refuses a bad key with
//! a `400`, and `compile` / `package export` refuse the same key before an
//! artifact is written, so a version the target would refuse at `apply` is
//! caught where it is chosen.

/// A package name, at most. The MySQL column is sized to it.
pub const MAX_PACKAGE_NAME_LEN: usize = 128;
/// A package version, at most. The MySQL column is sized to it.
pub const MAX_PACKAGE_VERSION_LEN: usize = 64;
/// A content hash, at most.
pub const MAX_PACKAGE_HASH_LEN: usize = 128;

/// A receipt key: non-empty, at most `max_len` characters, and drawn from a
/// charset that stays unambiguous in URLs, shell commands and audit rows —
/// letters, digits, `.`, `_` and `-`.
///
/// # Errors
///
/// The reason, naming `field`.
pub fn package_key(field: &str, value: &str, max_len: usize) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value.len() > max_len {
        return Err(format!(
            "{field} must be at most {max_len} characters, got {}",
            value.len()
        ));
    }
    if let Some(bad) = value
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    {
        return Err(format!(
            "{field} contains unsupported character '{bad}' — use letters, digits, \
             '.', '_' and '-'"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_bounded_non_empty_and_plain() {
        package_key("version", "1.4.0-rc_1", MAX_PACKAGE_VERSION_LEN).expect("plain");
        let err = package_key("version", " ", 64).expect_err("empty");
        assert!(err.contains("must not be empty"), "{err}");
        let err = package_key("version", &"a".repeat(65), 64).expect_err("long");
        assert!(err.contains("at most 64"), "{err}");
        for bad in ["1.0/rc", "1.4.0+build", "a b"] {
            let err = package_key("version", bad, 64).expect_err(bad);
            assert!(err.contains("unsupported character"), "{err}");
        }
    }
}
