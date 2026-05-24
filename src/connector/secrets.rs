//! Secret-reference resolvers for connector configs.
//!
//! Each string field in a connector's `config_json` may be a `scheme://value`
//! reference instead of a literal value. A registered `SecretResolver` for
//! that scheme replaces the string with the resolved secret before the
//! connector config is deserialized into its typed form.
//!
//! v0.2 ships a single resolver: `env://VAR_NAME` reads from the process
//! environment. The trait leaves room for `vault://`, `aws-sm://`, etc.
//! later without changing call sites.
//!
//! ## Relationship to A5
//!
//! [`crate::config::env_substitute`] resolves `${VAR}` placeholders in
//! the raw config TOML / JSON text — purely textual, runs before any
//! parsing. B5's `env://` operates on parsed string values, so it can
//! resolve secrets inside structured fields without leaking template
//! syntax into JSON validation.

use serde_json::Value;

use crate::errors::OrionError;

/// Resolves a `scheme://reference` string to its underlying secret value.
pub trait SecretResolver: Send + Sync {
    /// The URI scheme this resolver handles, without the `://` (e.g. `"env"`).
    fn scheme(&self) -> &'static str;

    /// Resolve the part of the reference after `scheme://`. Returns the
    /// secret value or an error describing why resolution failed.
    fn resolve(&self, reference: &str) -> Result<String, OrionError>;
}

/// Reads secrets from the process environment.
///
/// `env://DB_PASSWORD` → `std::env::var("DB_PASSWORD")`.
pub struct EnvSecretResolver;

impl SecretResolver for EnvSecretResolver {
    fn scheme(&self) -> &'static str {
        "env"
    }
    fn resolve(&self, reference: &str) -> Result<String, OrionError> {
        if reference.is_empty()
            || !reference
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(OrionError::Config {
                message: format!(
                    "Invalid env-var name '{reference}' in env:// reference (allowed: [A-Z0-9_])"
                ),
            });
        }
        std::env::var(reference).map_err(|_| OrionError::Config {
            message: format!(
                "env-var '{reference}' is not set (referenced via env:// in a connector config)"
            ),
        })
    }
}

/// Returns the default v0.2 resolver registry: env:// only. Add new
/// resolvers here when supporting additional schemes (e.g. vault://).
pub fn default_resolvers() -> Vec<Box<dyn SecretResolver>> {
    vec![Box::new(EnvSecretResolver)]
}

/// Walk `value` recursively, replacing each `scheme://reference` string
/// with the value from the matching resolver. Strings without a
/// recognized scheme pass through unchanged. Other JSON types (numbers,
/// bools, null) are never modified.
pub fn resolve_in_place(
    value: &mut Value,
    resolvers: &[Box<dyn SecretResolver>],
    source_label: &str,
) -> Result<(), OrionError> {
    match value {
        Value::String(s) => {
            if let Some((scheme, reference)) = parse_reference(s)
                && let Some(resolver) = resolvers.iter().find(|r| r.scheme() == scheme)
            {
                let resolved = resolver.resolve(reference).map_err(|e| match e {
                    OrionError::Config { message } => OrionError::Config {
                        message: format!("{source_label}: {message}"),
                    },
                    other => other,
                })?;
                *s = resolved;
            }
        }
        Value::Object(map) => {
            for v in map.values_mut() {
                resolve_in_place(v, resolvers, source_label)?;
            }
        }
        Value::Array(arr) => {
            for v in arr {
                resolve_in_place(v, resolvers, source_label)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Extract `(scheme, reference)` from a string of the form `scheme://reference`.
/// The scheme must be lowercase alphanumeric (`+` allowed for future
/// composite schemes like `aws-sm`). Returns `None` for anything that
/// doesn't look like a secret reference so plain URLs and connection
/// strings (e.g. `https://...`, `postgres://...`) are left alone.
///
/// **Recognized prefixes:** only schemes that exactly match a registered
/// resolver are resolved. `https://` is not in the registry and so flows
/// through untouched.
fn parse_reference(s: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = s.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '+')
    {
        return None;
    }
    Some((scheme, rest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Test-only resolver that returns canned values from a map. Lets us
    /// exercise `resolve_in_place` without touching the real environment.
    struct StubResolver {
        scheme: &'static str,
        values: std::collections::HashMap<&'static str, &'static str>,
    }
    impl SecretResolver for StubResolver {
        fn scheme(&self) -> &'static str {
            self.scheme
        }
        fn resolve(&self, reference: &str) -> Result<String, OrionError> {
            self.values
                .get(reference)
                .map(|v| (*v).to_string())
                .ok_or_else(|| OrionError::Config {
                    message: format!("stub: '{reference}' not registered"),
                })
        }
    }

    fn stub(values: &[(&'static str, &'static str)]) -> Vec<Box<dyn SecretResolver>> {
        vec![Box::new(StubResolver {
            scheme: "env",
            values: values.iter().copied().collect(),
        })]
    }

    #[test]
    fn parse_reference_recognizes_scheme() {
        assert_eq!(parse_reference("env://FOO"), Some(("env", "FOO")));
        assert_eq!(
            parse_reference("https://example.com"),
            Some(("https", "example.com"))
        );
    }

    #[test]
    fn parse_reference_rejects_uppercase_scheme() {
        // Schemes are lowercase; "ENV://..." stays as a literal so it's
        // not silently resolved despite the typo.
        assert_eq!(parse_reference("ENV://FOO"), None);
    }

    #[test]
    fn parse_reference_returns_none_for_plain_string() {
        assert_eq!(parse_reference("plain text"), None);
        assert_eq!(parse_reference(""), None);
    }

    #[test]
    fn resolve_in_place_replaces_string() {
        let mut v = json!({ "token": "env://API_TOKEN" });
        resolve_in_place(&mut v, &stub(&[("API_TOKEN", "s3cret")]), "test").expect("test");
        assert_eq!(v["token"], "s3cret");
    }

    #[test]
    fn resolve_in_place_leaves_unknown_schemes_alone() {
        // https:// has no resolver — must pass through unchanged.
        let mut v = json!({ "url": "https://example.com/api" });
        resolve_in_place(&mut v, &stub(&[]), "test").expect("test");
        assert_eq!(v["url"], "https://example.com/api");
    }

    #[test]
    fn resolve_in_place_recurses_into_objects() {
        let mut v = json!({
            "auth": { "type": "bearer", "token": "env://TOK" },
            "max_retries": 3
        });
        resolve_in_place(&mut v, &stub(&[("TOK", "abc")]), "test").expect("test");
        assert_eq!(v["auth"]["token"], "abc");
        assert_eq!(v["max_retries"], 3);
    }

    #[test]
    fn resolve_in_place_recurses_into_arrays() {
        let mut v = json!({ "brokers": ["env://B1", "literal:9092"] });
        resolve_in_place(&mut v, &stub(&[("B1", "broker.local:9092")]), "test").expect("test");
        assert_eq!(v["brokers"][0], "broker.local:9092");
        assert_eq!(v["brokers"][1], "literal:9092");
    }

    #[test]
    fn missing_env_var_errors_with_source_label() {
        let mut v = json!({ "token": "env://NOPE" });
        let err = resolve_in_place(&mut v, &stub(&[]), "connector 'foo'").expect_err("test");
        let OrionError::Config { message } = err else {
            unreachable!("expected Config error");
        };
        assert!(message.contains("NOPE"));
        assert!(message.contains("connector 'foo'"));
    }

    #[test]
    fn env_resolver_rejects_invalid_var_name() {
        let r = EnvSecretResolver;
        assert!(r.resolve("").is_err());
        assert!(r.resolve("has-hyphen").is_err());
        assert!(r.resolve("with space").is_err());
    }
}
