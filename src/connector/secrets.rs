//! Secret-reference resolvers for connector configs.
//!
//! Each string field in a connector's `config_json` may be a `scheme://value`
//! reference instead of a literal value. A registered `SecretResolver` for
//! that scheme replaces the string with the resolved secret before the
//! connector config is deserialized into its typed form.
//!
//! v1.0 ships a single working resolver: `env://VAR_NAME` reads from the
//! process environment. The schemes reserved for later backends
//! ([`RESERVED_SCHEMES`]) are registered too, but resolve to a hard error —
//! a reference that cannot be resolved must never reach the remote system as
//! its own literal text.
//!
//! ## Relationship to A5
//!
//! `config::env_substitute` resolves `${VAR}` placeholders in
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
///
/// The variable can be named anything, with one caveat: connectors live in
/// the database, so the C4d unknown-variable guard cannot know which names
/// they reference and refuses any `ORION_*` name that is not a setting. A
/// secret that has to sit in the `ORION_` namespace therefore needs the
/// reserved [`crate::config::RESERVED_ENV_PREFIX`] — `env://ORION_SECRET_…`.
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

/// Secret-backend schemes Orion recognises but does not implement yet.
///
/// They are registered so that a reference using one fails loudly. Without
/// this, `vault://secret/db#password` has no matching resolver, passes through
/// [`resolve_in_place`] untouched, and is handed to the database *as the
/// password* — the connector then fails authentication with nothing pointing at
/// the unresolved secret. Implementing one of these means replacing its entry
/// with a real resolver.
pub const RESERVED_SCHEMES: &[&str] = &["vault", "aws-sm", "gcp-sm", "azure-kv"];

/// Rejects a reserved scheme with an explanatory error. See
/// [`RESERVED_SCHEMES`].
pub struct ReservedSchemeResolver {
    scheme: &'static str,
}

impl SecretResolver for ReservedSchemeResolver {
    fn scheme(&self) -> &'static str {
        self.scheme
    }
    fn resolve(&self, _reference: &str) -> Result<String, OrionError> {
        Err(OrionError::Config {
            message: format!(
                "secret scheme '{}://' is reserved but not supported in this build; \
                 supply the value via env:// or a literal instead",
                self.scheme
            ),
        })
    }
}

/// Whether `s` is a reference to a secret *this build knows how to resolve*.
///
/// Used by the masking policy to let a reference survive where a value would
/// not: `env://STRIPE_KEY` names a variable, it is not a credential, and
/// masking it breaks `GET /export` → `POST /import` for every connector
/// authored the recommended way.
///
/// The scheme check is the whole point and must stay strict. `parse_reference`
/// alone recognises *any* `scheme://rest`, which includes
/// `postgres://user:password@host/db` — treating that as a reference would
/// exempt real credentials from masking. Only `env://` and the reserved schemes
/// qualify.
pub fn is_resolvable_reference(s: &str) -> bool {
    parse_reference(s).is_some_and(|(scheme, reference)| {
        !reference.is_empty()
            && (scheme == EnvSecretResolver.scheme() || RESERVED_SCHEMES.contains(&scheme))
    })
}

/// Returns the default resolver registry: a working `env://` resolver plus a
/// rejecting entry for every scheme in [`RESERVED_SCHEMES`].
pub fn default_resolvers() -> Vec<Box<dyn SecretResolver>> {
    let mut resolvers: Vec<Box<dyn SecretResolver>> = vec![Box::new(EnvSecretResolver)];
    for scheme in RESERVED_SCHEMES {
        resolvers.push(Box::new(ReservedSchemeResolver { scheme }));
    }
    resolvers
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
        // https:// has no resolver and is not reserved — must pass through
        // unchanged, or every connector URL would be mangled.
        let mut v = json!({ "url": "https://example.com/api" });
        resolve_in_place(&mut v, &stub(&[]), "test").expect("test");
        assert_eq!(v["url"], "https://example.com/api");
    }

    #[test]
    fn reserved_scheme_errors_instead_of_becoming_the_literal_password() {
        let mut v = json!({ "auth": { "password": "vault://secret/db#password" } });
        let err = resolve_in_place(&mut v, &default_resolvers(), "connector 'db'").expect_err(
            "an unimplemented scheme must fail loudly, not pass through as the password",
        );
        let OrionError::Config { message } = err else {
            unreachable!("expected Config error");
        };
        assert!(message.contains("vault"), "{message}");
        assert!(message.contains("not supported"), "{message}");
        assert!(message.contains("connector 'db'"), "{message}");
    }

    #[test]
    fn every_reserved_scheme_is_rejected() {
        for scheme in RESERVED_SCHEMES {
            let mut v = json!({ "token": format!("{scheme}://some/path") });
            assert!(
                resolve_in_place(&mut v, &default_resolvers(), "test").is_err(),
                "scheme '{scheme}' must be rejected"
            );
        }
    }

    #[test]
    fn default_resolvers_leave_connection_urls_untouched() {
        // The reserved list must not catch ordinary connector URLs.
        let mut v = json!({
            "connection_string": "postgres://user:pass@db.internal:5432/app",
            "url": "redis://cache.internal:6379",
            "brokers": ["kafka.internal:9092"]
        });
        resolve_in_place(&mut v, &default_resolvers(), "test").expect("test");
        assert_eq!(
            v["connection_string"],
            "postgres://user:pass@db.internal:5432/app"
        );
        assert_eq!(v["url"], "redis://cache.internal:6379");
        assert_eq!(v["brokers"][0], "kafka.internal:9092");
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
