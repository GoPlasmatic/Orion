//! RFC 6265 cookie-jar parsing — one implementation, shared by
//! `JwtSource::Cookie` and the `request.cookies_to_metadata` allowlist (#270).
//!
//! Before this module the JWT source carried a four-line inline parser with
//! three defects, each fixed here: a quoted value (`name="abc"`, legal per
//! §4.1.1) came back with its quotes attached; `name = value` with spaces
//! around the `=` did not match at all, and trailing whitespace stayed in the
//! value.
//!
//! RFC 6265 leaves several behaviours to the user agent, so this module picks
//! and pins them:
//!
//! - **First duplicate name wins** — matches what the inline parser did.
//! - **Name matching is byte-exact and case-sensitive** (§5.4).
//! - **`=` inside a value is preserved** — [`str::split_once`], never
//!   `split`, or base64 padding truncates the value.
//! - **An empty value yields nothing**, rather than an empty string, so a
//!   cleared cookie reads as absent.

use serde_json::Value;

/// Per-value byte cap. The jar is caller-controlled and allowlisted values
/// land in a persisted trace, so this exists for the same reason
/// `auth.max_token_bytes` does.
const MAX_COOKIE_VALUE_BYTES: usize = 4096;

/// The `name=value` pairs in one `Cookie` header value, in order.
fn pairs(jar: &str) -> impl Iterator<Item = (&str, &str)> {
    jar.split(';').filter_map(|pair| {
        // `split_once`, never `split`: a base64 value carries `=` padding.
        let (name, value) = pair.split_once('=')?;
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        let value = unquote(value.trim());
        if value.is_empty() || value.len() > MAX_COOKIE_VALUE_BYTES {
            return None;
        }
        Some((name, value))
    })
}

/// Strip one layer of double quotes, which RFC 6265 §4.1.1 permits around a
/// value. Only when both are present — a lone quote is part of the value.
fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
}

/// Look one cookie up across every `Cookie` header value supplied.
///
/// Pass more than one value where they are available: HTTP/2 clients may
/// legitimately split a jar across several `cookie` headers (RFC 9113 §8.2.3).
pub(crate) fn lookup<'a>(jar: impl IntoIterator<Item = &'a str>, name: &str) -> Option<String> {
    jar.into_iter()
        .flat_map(pairs)
        .find(|(n, _)| *n == name)
        .map(|(_, v)| v.to_string())
}

/// Collect the allowlisted cookies into a metadata object.
///
/// A listed-but-absent cookie is simply not present — never `null`, never an
/// error — matching how the `claims_to_metadata` filter behaves.
pub(crate) fn collect<'a>(
    jar: impl IntoIterator<Item = &'a str> + Clone,
    allowlist: &[String],
) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    for name in allowlist {
        if let Some(value) = lookup(jar.clone(), name.as_str()) {
            out.insert(name.clone(), Value::String(value));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(jar: &str, name: &str) -> Option<String> {
        lookup([jar], name)
    }

    #[test]
    fn a_plain_jar_parses() {
        let jar = "a=1; b=2; browser_uuid=abc-123";
        assert_eq!(one(jar, "a").as_deref(), Some("1"));
        assert_eq!(one(jar, "browser_uuid").as_deref(), Some("abc-123"));
        assert_eq!(one(jar, "missing"), None);
    }

    /// The three defects the old inline parser had.
    #[test]
    fn quoted_values_and_loose_whitespace_are_handled() {
        assert_eq!(one(r#"sid="abc""#, "sid").as_deref(), Some("abc"));
        assert_eq!(one("sid = abc ", "sid").as_deref(), Some("abc"));
        assert_eq!(one("  sid=abc  ;  b=2", "sid").as_deref(), Some("abc"));
        // A lone quote is data, not a delimiter.
        assert_eq!(one(r#"sid="abc"#, "sid").as_deref(), Some(r#""abc"#));
    }

    /// Split across several headers, as an HTTP/2 client may send it.
    #[test]
    fn a_jar_split_across_headers_is_searched_whole() {
        assert_eq!(lookup(["a=1", "b=2"], "b").as_deref(), Some("2"));
    }

    #[test]
    fn base64_padding_survives() {
        assert_eq!(one("t=YWJjZA==", "t").as_deref(), Some("YWJjZA=="));
    }

    #[test]
    fn name_matching_is_case_sensitive() {
        assert_eq!(one("SID=abc", "sid"), None);
        assert_eq!(one("SID=abc", "SID").as_deref(), Some("abc"));
    }

    #[test]
    fn the_first_duplicate_wins() {
        assert_eq!(one("a=first; a=second", "a").as_deref(), Some("first"));
    }

    #[test]
    fn empty_and_oversized_values_are_absent() {
        assert_eq!(one("a=; b=2", "a"), None);
        assert_eq!(one("a=\"\"; b=2", "a"), None);
        let huge = format!("a={}", "x".repeat(MAX_COOKIE_VALUE_BYTES + 1));
        assert_eq!(one(&huge, "a"), None);
    }

    #[test]
    fn a_malformed_pair_is_skipped_not_fatal() {
        // No `=` at all, and an empty name.
        assert_eq!(one("novalue; =orphan; a=1", "a").as_deref(), Some("1"));
    }

    #[test]
    fn collect_takes_only_the_allowlist() {
        let jar = "browser_uuid=abc; session=secret; other=x";
        let out = collect([jar], &["browser_uuid".to_string(), "absent".to_string()]);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out["browser_uuid"], "abc");
        assert!(
            !out.contains_key("session"),
            "an unlisted cookie must never be copied"
        );
    }
}
