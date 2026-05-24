//! Env-var substitution for config text (TOML or JSON-as-string).
//!
//! Recognized syntax:
//! - `${VAR}` — required; errors if `VAR` is unset.
//! - `${VAR:-default}` — optional; uses `default` (which may be empty)
//!   when `VAR` is unset.
//! - `$$` — escape for a literal `$`.
//!
//! The substitution is intentionally a single pass and does not recurse:
//! the value of `${VAR}` is inserted verbatim and is not itself scanned.
//! That avoids any chance of an env-var injecting another `${...}` lookup.

use crate::errors::OrionError;

/// Substitute `${VAR}` / `${VAR:-default}` in `input` using the process
/// environment.
pub fn substitute(input: &str, source_label: &str) -> Result<String, OrionError> {
    substitute_with(input, source_label, |k| std::env::var(k).ok())
}

/// Substitute `${VAR}` / `${VAR:-default}` in `input` using `lookup` as
/// the variable resolver. Used by tests to avoid touching the real env.
pub fn substitute_with<F>(input: &str, source_label: &str, lookup: F) -> Result<String, OrionError>
where
    F: Fn(&str) -> Option<String>,
{
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'$' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if next == b'$' {
                out.push('$');
                i += 2;
                continue;
            }
            if next == b'{' {
                // Find matching closing '}'. Variable references don't nest,
                // so a simple forward scan is sufficient.
                let start = i + 2;
                let mut end = None;
                let mut j = start;
                while j < bytes.len() {
                    if bytes[j] == b'}' {
                        end = Some(j);
                        break;
                    }
                    j += 1;
                }
                let Some(end) = end else {
                    return Err(OrionError::Config {
                        message: format!(
                            "Unterminated ${{...}} expression in {source_label} starting at byte {i}"
                        ),
                    });
                };
                let expr = &input[start..end];
                let (name, default) = match expr.find(":-") {
                    Some(idx) => (&expr[..idx], Some(&expr[idx + 2..])),
                    None => (expr, None),
                };
                if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    return Err(OrionError::Config {
                        message: format!(
                            "Invalid env var name '{name}' in {source_label} (allowed: [A-Z0-9_])"
                        ),
                    });
                }
                match lookup(name) {
                    Some(v) => out.push_str(&v),
                    None => match default {
                        Some(d) => out.push_str(d),
                        None => {
                            return Err(OrionError::Config {
                                message: format!(
                                    "Required environment variable '{name}' is not set (referenced in {source_label}). \
                                     Set the variable or use '${{{name}:-default}}' to provide a fallback."
                                ),
                            });
                        }
                    },
                }
                i = end + 1;
                continue;
            }
        }
        // SAFETY: i indexes a byte in `input`; we slice up to and including
        // a valid UTF-8 boundary by using char_indices semantics — but to
        // keep the loop byte-oriented we extract the char starting at i.
        let c_end = next_char_boundary(bytes, i);
        out.push_str(&input[i..c_end]);
        i = c_end;
    }
    Ok(out)
}

fn next_char_boundary(bytes: &[u8], i: usize) -> usize {
    // UTF-8 leading-byte classification. Continuation bytes (0x80..0xC0)
    // shouldn't appear as a char start in valid UTF-8 input — treat them
    // as 1-byte advances to keep the loop progressing safely.
    let b = bytes[i];
    let len = if b < 0xC0 {
        1
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    };
    (i + len).min(bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn simple_substitution() {
        let out = substitute_with(
            "url = ${DB_URL}",
            "test",
            env(&[("DB_URL", "postgres://x")]),
        )
        .unwrap();
        assert_eq!(out, "url = postgres://x");
    }

    #[test]
    fn default_used_when_unset() {
        let out = substitute_with("port = ${PORT:-8080}", "test", env(&[])).unwrap();
        assert_eq!(out, "port = 8080");
    }

    #[test]
    fn empty_default_is_allowed() {
        let out = substitute_with("v = '${EMPTY:-}'", "test", env(&[])).unwrap();
        assert_eq!(out, "v = ''");
    }

    #[test]
    fn missing_required_var_errors() {
        let err = substitute_with("v = ${NOPE}", "test", env(&[])).unwrap_err();
        match err {
            OrionError::Config { message } => {
                assert!(message.contains("NOPE"));
                assert!(message.contains("test"));
            }
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn dollar_dollar_escapes_to_single_dollar() {
        let out = substitute_with("price = $$5", "test", env(&[])).unwrap();
        assert_eq!(out, "price = $5");
    }

    #[test]
    fn multiple_substitutions_one_string() {
        let out = substitute_with(
            "${A}/${B}/${C:-fallback}",
            "test",
            env(&[("A", "x"), ("B", "y")]),
        )
        .unwrap();
        assert_eq!(out, "x/y/fallback");
    }

    #[test]
    fn unterminated_brace_errors() {
        let err = substitute_with("v = ${OOPS", "test", env(&[])).unwrap_err();
        assert!(matches!(err, OrionError::Config { .. }));
    }

    #[test]
    fn invalid_var_name_errors() {
        let err =
            substitute_with("v = ${bad-name}", "test", env(&[("bad-name", "x")])).unwrap_err();
        assert!(matches!(err, OrionError::Config { .. }));
    }

    #[test]
    fn no_substitution_when_no_dollar() {
        let out = substitute_with("plain text", "test", env(&[])).unwrap();
        assert_eq!(out, "plain text");
    }

    #[test]
    fn dollar_not_followed_by_brace_is_literal() {
        let out = substitute_with("amount: $5", "test", env(&[])).unwrap();
        assert_eq!(out, "amount: $5");
    }

    #[test]
    fn substitution_is_not_recursive() {
        // The value of A contains ${B} as a literal string — it must NOT be
        // re-evaluated as another env var. Prevents env-var injection.
        let out = substitute_with(
            "v = ${A}",
            "test",
            env(&[("A", "literal-${B}"), ("B", "secret")]),
        )
        .unwrap();
        assert_eq!(out, "v = literal-${B}");
    }

    #[test]
    fn unicode_pass_through() {
        let out = substitute_with("π = ${PI}", "test", env(&[("PI", "3.14")])).unwrap();
        assert_eq!(out, "π = 3.14");
    }
}
