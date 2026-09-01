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
    jar: impl IntoIterator<Item = &'a str>,
    allowlist: &[String],
) -> serde_json::Map<String, Value> {
    // One pass over the jar rather than one per allowlisted name: a browser
    // jar is routinely several KB, and this runs on every request to a channel
    // that opts in. `entry`-style "first wins" keeps the documented
    // first-duplicate-wins rule without re-scanning.
    let mut out = serde_json::Map::new();
    for (name, value) in jar.into_iter().flat_map(pairs) {
        if out.contains_key(name) {
            continue;
        }
        if allowlist.iter().any(|a| a == name) {
            out.insert(name.to_string(), Value::String(value.to_string()));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The write side (#298)
// ---------------------------------------------------------------------------

/// Build one `Set-Cookie` header value from a workflow's declaration.
///
/// The counterpart to the parser above, and it exists for the same reason:
/// a workflow used to hand-assemble the attribute string with `cat`, which is
/// where a missing `Secure`, a `SameSite` that should have been `Lax`, or an
/// unescaped value quietly comes from. Declaring the parts lets this module
/// own the spelling.
///
/// `Err` carries a reason for the caller to log — every failure on the shaped
/// response path is soft, so a malformed cookie is dropped with a warning
/// rather than failing the request. The refusals are not stylistic: a value
/// carrying `;`, CR or LF would let a workflow that interpolates user input
/// into a cookie inject further attributes, or split the header entirely.
///
/// Attribute order follows RFC 6265 §4.1.1: the pair first, then attributes.
pub(crate) fn format_set_cookie(spec: &Value) -> Result<String, String> {
    let obj = spec.as_object().ok_or("cookie must be an object")?;

    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .ok_or("cookie needs a string 'name'")?;
    if name.is_empty() || !name.bytes().all(is_token_byte) {
        return Err(format!("cookie name {name:?} is not a valid token"));
    }

    // An empty value is the *point* when clearing a cookie (`Max-Age=0`), so
    // it is allowed here even though the parser reads one back as absent.
    let value = obj
        .get("value")
        .and_then(Value::as_str)
        .ok_or("cookie needs a string 'value'")?;
    if !value.bytes().all(is_value_byte) {
        return Err(format!("cookie {name:?} has an unencodable value"));
    }

    let mut out = format!("{name}={value}");

    for (key, attr) in [("path", "Path"), ("domain", "Domain")] {
        if let Some(v) = obj.get(key) {
            let v = v
                .as_str()
                .ok_or_else(|| format!("cookie {name:?}: '{key}' must be a string"))?;
            if !is_attribute_safe(v) {
                return Err(format!("cookie {name:?}: '{key}' has an unsafe value"));
            }
            out.push_str(&format!("; {attr}={v}"));
        }
    }

    if let Some(v) = obj.get("max_age") {
        let secs = v
            .as_i64()
            .ok_or_else(|| format!("cookie {name:?}: 'max_age' must be an integer"))?;
        out.push_str(&format!("; Max-Age={secs}"));
    }

    if let Some(v) = obj.get("expires") {
        let v = v
            .as_str()
            .ok_or_else(|| format!("cookie {name:?}: 'expires' must be a string"))?;
        if !is_attribute_safe(v) {
            return Err(format!("cookie {name:?}: 'expires' has an unsafe value"));
        }
        out.push_str(&format!("; Expires={v}"));
    }

    if let Some(v) = obj.get("same_site") {
        let v = v
            .as_str()
            .ok_or_else(|| format!("cookie {name:?}: 'same_site' must be a string"))?;
        // Spelled back canonically rather than echoed: a browser ignores an
        // unrecognised `SameSite`, so accepting "lax" and emitting it verbatim
        // would silently give the cookie the browser's default instead.
        let canonical = match v.to_ascii_lowercase().as_str() {
            "strict" => "Strict",
            "lax" => "Lax",
            "none" => "None",
            other => {
                return Err(format!(
                    "cookie {name:?}: 'same_site' must be Strict, Lax or None, got {other:?}"
                ));
            }
        };
        out.push_str(&format!("; SameSite={canonical}"));
    }

    for (key, attr) in [("http_only", "HttpOnly"), ("secure", "Secure")] {
        match obj.get(key) {
            None | Some(Value::Bool(false)) => {}
            Some(Value::Bool(true)) => out.push_str(&format!("; {attr}")),
            Some(_) => return Err(format!("cookie {name:?}: '{key}' must be a boolean")),
        }
    }

    Ok(out)
}

/// RFC 6265 §4.1.1 `cookie-name` is an RFC 2616 token: no CTLs, no space, and
/// none of the separator characters.
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_graphic() && !br#"()<>@,;:\"/[]?={}"#.contains(&b)
}

/// RFC 6265 §4.1.1 `cookie-value`, minus the optional surrounding quotes:
/// printable ASCII excluding whitespace, comma, semicolon and backslash. CR
/// and LF are excluded by `is_ascii_graphic`, which is the half that matters —
/// a value carrying either could split the response.
fn is_value_byte(b: u8) -> bool {
    b.is_ascii_graphic() && !matches!(b, b',' | b';' | b'\\' | b'"')
}

/// An attribute value may hold characters a cookie value may not (`Expires`
/// carries commas and spaces), so this refuses only what would end the
/// attribute or the header.
fn is_attribute_safe(v: &str) -> bool {
    !v.bytes().any(|b| matches!(b, b';' | b'\r' | b'\n' | 0))
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

    // -----------------------------------------------------------------
    // The write side (#298)
    // -----------------------------------------------------------------

    fn fmt(spec: serde_json::Value) -> Result<String, String> {
        format_set_cookie(&spec)
    }

    #[test]
    fn a_full_cookie_renders_its_attributes_in_rfc_order() {
        let out = fmt(serde_json::json!({
            "name": "session", "value": "abc.def",
            "path": "/", "domain": "example.com",
            "max_age": 2592000, "same_site": "Lax",
            "http_only": true, "secure": true
        }))
        .expect("valid cookie");
        assert_eq!(
            out,
            "session=abc.def; Path=/; Domain=example.com; Max-Age=2592000; \
SameSite=Lax; HttpOnly; Secure"
        );
    }

    /// Clearing a cookie is the case the parser deliberately reads back as
    /// *absent*, so the writer has to allow what the reader refuses.
    #[test]
    fn an_empty_value_is_allowed_because_that_is_how_a_cookie_is_cleared() {
        assert_eq!(
            fmt(serde_json::json!({"name": "oauth_state", "value": "", "path": "/", "max_age": 0}))
                .expect("valid cookie"),
            "oauth_state=; Path=/; Max-Age=0"
        );
    }

    /// A false flag is absent, not `HttpOnly=false` — the attribute has no
    /// negative form, and emitting one would set it.
    #[test]
    fn a_false_flag_emits_nothing() {
        assert_eq!(
            fmt(
                serde_json::json!({"name": "a", "value": "1", "http_only": false, "secure": false})
            )
            .expect("valid cookie"),
            "a=1"
        );
    }

    /// The refusals that matter: anything that could end the pair, add an
    /// attribute the author did not write, or split the response.
    #[test]
    fn injection_shaped_values_are_refused() {
        for bad in [
            serde_json::json!({"name": "a", "value": "x; Path=/; HttpOnly"}),
            serde_json::json!({"name": "a", "value": "x\r\nSet-Cookie: b=2"}),
            serde_json::json!({"name": "a", "value": "x,y"}),
            serde_json::json!({"name": "a b", "value": "x"}),
            serde_json::json!({"name": "a=b", "value": "x"}),
            serde_json::json!({"name": "", "value": "x"}),
            serde_json::json!({"name": "a", "value": "x", "path": "/; Domain=evil.test"}),
        ] {
            assert!(
                fmt(bad.clone()).is_err(),
                "must be refused, rendered instead: {bad}"
            );
        }
    }

    /// A browser ignores an unrecognised `SameSite` and falls back to its own
    /// default, so echoing the author's casing would quietly change the
    /// cookie's meaning rather than failing.
    #[test]
    fn same_site_is_canonicalised_and_otherwise_refused() {
        assert!(
            fmt(serde_json::json!({"name": "a", "value": "1", "same_site": "lax"}))
                .expect("valid")
                .ends_with("SameSite=Lax")
        );
        assert!(
            fmt(serde_json::json!({"name": "a", "value": "1", "same_site": "sometimes"})).is_err()
        );
    }

    /// A JWT is the value this exists to carry: base64url segments and dots,
    /// none of which may be refused.
    #[test]
    fn a_jwt_value_survives_unchanged() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abc-_123";
        let out = fmt(serde_json::json!({"name": "session", "value": jwt})).expect("valid");
        assert_eq!(out, format!("session={jwt}"));
    }

    /// The two halves agree: what the writer emits, the parser reads back.
    #[test]
    fn what_the_writer_emits_the_parser_reads_back() {
        let out = fmt(serde_json::json!({
            "name": "session", "value": "abc.def", "path": "/", "http_only": true
        }))
        .expect("valid");
        let pair = out.split(';').next().expect("the name=value pair");
        assert_eq!(one(pair, "session").as_deref(), Some("abc.def"));
    }
}
