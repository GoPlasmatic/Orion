//! Secret-reference resolvers for connector configs.
//!
//! Each string field in a connector's `config_json` may be a `scheme://value`
//! reference instead of a literal value. A registered `SecretResolver` for
//! that scheme replaces the string with the resolved secret before the
//! connector config is deserialized into its typed form.
//!
//! v1.0 ships two working resolvers: `env://VAR_NAME` reads from the process
//! environment, and `vault://` reads from HashiCorp Vault when the standard
//! `VAULT_ADDR`/`VAULT_TOKEN` environment is present. The schemes reserved
//! for later backends ([`RESERVED_SCHEMES`]) are registered too, but resolve
//! to a hard error — a reference that cannot be resolved must never reach the
//! remote system as its own literal text. The cloud backends (`aws-sm://`,
//! `gcp-sm://`, `azure-kv://`) are deliberately deferred: each means adopting
//! that vendor's SDK tree, a dependency-policy decision (2026-08-01) rather
//! than missing code — [`SecretResolver`] being async makes any of them a
//! drop-in when that decision changes.
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
///
/// Async (H3c): the Vault resolver is an HTTP call, and every resolution
/// site — connector load, channel-auth compile, the admin validate paths —
/// already runs in async context. `env://` resolution is trivially async.
#[async_trait::async_trait]
pub trait SecretResolver: Send + Sync {
    /// The URI scheme this resolver handles, without the `://` (e.g. `"env"`).
    fn scheme(&self) -> &'static str;

    /// Resolve the part of the reference after `scheme://`. Returns the
    /// secret value or an error describing why resolution failed.
    async fn resolve(&self, reference: &str) -> Result<String, OrionError>;
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

#[async_trait::async_trait]
impl SecretResolver for EnvSecretResolver {
    fn scheme(&self) -> &'static str {
        "env"
    }
    async fn resolve(&self, reference: &str) -> Result<String, OrionError> {
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

#[async_trait::async_trait]
impl SecretResolver for ReservedSchemeResolver {
    fn scheme(&self) -> &'static str {
        self.scheme
    }
    async fn resolve(&self, _reference: &str) -> Result<String, OrionError> {
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

/// Returns the default resolver registry: a working `env://` resolver, a
/// working `vault://` resolver when the standard `VAULT_ADDR`/`VAULT_TOKEN`
/// environment is present (H3c), and a rejecting entry for every remaining
/// scheme in [`RESERVED_SCHEMES`] — so an unresolvable reference fails
/// loudly instead of being handed to a backend as the literal credential.
pub fn default_resolvers() -> Vec<Box<dyn SecretResolver>> {
    let mut resolvers: Vec<Box<dyn SecretResolver>> = vec![Box::new(EnvSecretResolver)];
    let vault = VaultSecretResolver::from_env();
    let vault_live = vault.is_some();
    if let Some(v) = vault {
        resolvers.push(Box::new(v));
    }
    for scheme in RESERVED_SCHEMES {
        if *scheme == "vault" && vault_live {
            continue;
        }
        resolvers.push(Box::new(ReservedSchemeResolver { scheme }));
    }
    resolvers
}

/// HashiCorp Vault KV resolver (H3c).
///
/// Reference form: `vault://<api-path>#<field>` — the api-path is exactly
/// what follows `/v1/` in Vault's HTTP API, so a KV **v2** secret reads as
/// `vault://secret/data/db#password` (the `data/` segment is v2's, not
/// Orion's). Field lookup understands both KV shapes: v2's nested
/// `data.data.<field>` first, then v1's flat `data.<field>`.
///
/// Configuration is the standard Vault client environment — `VAULT_ADDR`
/// plus `VAULT_TOKEN` — read at resolver construction, i.e. per load, so a
/// renewed token is picked up by the next reload without a restart. Errors
/// never include the token, and never include response bodies (a Vault error
/// body can echo the request path, which is fine, but a success body is the
/// secret — so bodies are parsed, not quoted).
pub struct VaultSecretResolver {
    addr: String,
    token: String,
    client: reqwest::Client,
}

impl VaultSecretResolver {
    /// Build from `VAULT_ADDR` + `VAULT_TOKEN`; `None` when either is absent
    /// (the fail-closed [`ReservedSchemeResolver`] then covers `vault://`).
    pub fn from_env() -> Option<Self> {
        let addr = std::env::var("VAULT_ADDR").ok()?;
        let token = std::env::var("VAULT_TOKEN").ok()?;
        Some(Self::new(addr, token))
    }

    /// Explicit construction, for tests and for callers that manage their own
    /// Vault settings.
    pub fn new(addr: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            addr: addr.into().trim_end_matches('/').to_string(),
            token: token.into(),
            client: vault_http_client(),
        }
    }
}

/// One HTTP client shared by every [`VaultSecretResolver`] instance.
///
/// Resolvers are rebuilt per load so a renewed token is picked up, but the
/// connection pool has no reason to follow: `default_resolvers()` runs on
/// every connector load, channel-auth compile and admin validate/test call,
/// and a fresh pool each time means a new TLS handshake per resolved secret.
/// Deliberately *not* the engine's shared client (`bootstrap`): that one
/// carries the SSRF pinning and no-redirect policy for user-supplied URLs,
/// and `VAULT_ADDR` is operator config that legitimately points at private
/// addresses the SSRF rules exist to block.
fn vault_http_client() -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("reqwest client with static config")
        })
        .clone()
}

#[async_trait::async_trait]
impl SecretResolver for VaultSecretResolver {
    fn scheme(&self) -> &'static str {
        "vault"
    }
    async fn resolve(&self, reference: &str) -> Result<String, OrionError> {
        let (path, field) = reference
            .split_once('#')
            .ok_or_else(|| OrionError::Config {
                message: format!(
                    "vault:// reference '{reference}' must name a field: \
                 vault://<api-path>#<field> (e.g. vault://secret/data/db#password)"
                ),
            })?;
        if path.is_empty() || field.is_empty() {
            return Err(OrionError::Config {
                message: format!("vault:// reference '{reference}' has an empty path or field"),
            });
        }

        let url = format!("{}/v1/{}", self.addr, path);
        let response = self
            .client
            .get(&url)
            .header("X-Vault-Token", &self.token)
            .send()
            .await
            .map_err(|e| OrionError::Config {
                message: format!("vault://{path}: request to Vault failed: {e}"),
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(OrionError::Config {
                message: format!(
                    "vault://{path}: Vault answered {status} (check VAULT_ADDR, \
                     VAULT_TOKEN and the secret path)"
                ),
            });
        }
        let body: Value = response.json().await.map_err(|_| OrionError::Config {
            message: format!("vault://{path}: Vault response was not JSON"),
        })?;

        // KV v2 nests the secret under data.data; KV v1 is flat under data.
        let secret = body
            .get("data")
            .and_then(|d| d.get("data"))
            .and_then(|d| d.get(field))
            .or_else(|| body.get("data").and_then(|d| d.get(field)));
        match secret {
            Some(Value::String(v)) => Ok(v.clone()),
            Some(other) => Ok(other.to_string()),
            None => Err(OrionError::Config {
                message: format!(
                    "vault://{path}#{field}: the secret exists but carries no \
                     field '{field}'"
                ),
            }),
        }
    }
}

/// Resolve one string that may be a secret reference (`env://VAR`, …); a
/// literal passes through unchanged.
///
/// The single-value convenience over [`resolve_in_place`] with the default
/// resolver registry — channel auth and the `crypto` task function both drive
/// it, so a workflow author has exactly one reference mechanism to learn and
/// production key material never has to sit in a stored definition.
/// `field` names the referencing field in error messages; the resolved value
/// itself never appears in one.
pub async fn resolve_secret_string(value: &str, field: &str) -> Result<String, String> {
    let mut json = Value::String(value.to_string());
    resolve_in_place(&mut json, &default_resolvers(), field)
        .await
        .map_err(|e| e.to_string())?;
    json.as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{field} did not resolve to a string"))
}

/// Walk `value`, replacing each `scheme://reference` string with the value
/// from the matching resolver. Strings without a recognized scheme pass
/// through unchanged. Other JSON types (numbers, bools, null) are never
/// modified.
///
/// Three phases rather than one recursive walk (H3c): collect the distinct
/// references synchronously, resolve each **once** (a document repeating a
/// reference costs one lookup — which matters now that a lookup can be an
/// HTTP round trip), then substitute synchronously. It also sidesteps async
/// recursion, which would otherwise need per-level boxing.
pub async fn resolve_in_place(
    value: &mut Value,
    resolvers: &[Box<dyn SecretResolver>],
    source_label: &str,
) -> Result<(), OrionError> {
    let mut wanted: Vec<String> = Vec::new();
    collect_references(value, resolvers, &mut wanted);
    if wanted.is_empty() {
        return Ok(());
    }

    let mut resolved: std::collections::HashMap<String, String> = Default::default();
    for reference_string in wanted {
        let (scheme, reference) =
            parse_reference(&reference_string).expect("collected as a reference");
        let resolver = resolvers
            .iter()
            .find(|r| r.scheme() == scheme)
            .expect("collected against this registry");
        let secret = resolver.resolve(reference).await.map_err(|e| match e {
            OrionError::Config { message } => OrionError::Config {
                message: format!("{source_label}: {message}"),
            },
            other => other,
        })?;
        resolved.insert(reference_string, secret);
    }

    substitute(value, &resolved);
    Ok(())
}

/// Phase 1 of [`resolve_in_place`]: every distinct string in the tree whose
/// scheme matches a registered resolver, in first-seen order.
fn collect_references(value: &Value, resolvers: &[Box<dyn SecretResolver>], out: &mut Vec<String>) {
    match value {
        Value::String(s) => {
            if let Some((scheme, _)) = parse_reference(s)
                && resolvers.iter().any(|r| r.scheme() == scheme)
                && !out.iter().any(|seen| seen == s)
            {
                out.push(s.clone());
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                collect_references(v, resolvers, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_references(v, resolvers, out);
            }
        }
        _ => {}
    }
}

/// Phase 3 of [`resolve_in_place`].
fn substitute(value: &mut Value, resolved: &std::collections::HashMap<String, String>) {
    match value {
        Value::String(s) => {
            if let Some(secret) = resolved.get(s.as_str()) {
                *s = secret.clone();
            }
        }
        Value::Object(map) => {
            for v in map.values_mut() {
                substitute(v, resolved);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                substitute(v, resolved);
            }
        }
        _ => {}
    }
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
pub(crate) fn parse_reference(s: &str) -> Option<(&str, &str)> {
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
    #[async_trait::async_trait]
    impl SecretResolver for StubResolver {
        fn scheme(&self) -> &'static str {
            self.scheme
        }
        async fn resolve(&self, reference: &str) -> Result<String, OrionError> {
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

    #[tokio::test]
    async fn parse_reference_returns_none_for_plain_string() {
        assert_eq!(parse_reference("plain text"), None);
        assert_eq!(parse_reference(""), None);
    }

    #[tokio::test]
    async fn resolve_in_place_replaces_string() {
        let mut v = json!({ "token": "env://API_TOKEN" });
        resolve_in_place(&mut v, &stub(&[("API_TOKEN", "s3cret")]), "test")
            .await
            .expect("test");
        assert_eq!(v["token"], "s3cret");
    }

    #[tokio::test]
    async fn resolve_in_place_leaves_unknown_schemes_alone() {
        // https:// has no resolver and is not reserved — must pass through
        // unchanged, or every connector URL would be mangled.
        let mut v = json!({ "url": "https://example.com/api" });
        resolve_in_place(&mut v, &stub(&[]), "test")
            .await
            .expect("test");
        assert_eq!(v["url"], "https://example.com/api");
    }

    #[tokio::test]
    async fn reserved_scheme_errors_instead_of_becoming_the_literal_password() {
        let mut v = json!({ "auth": { "password": "vault://secret/db#password" } });
        let err = resolve_in_place(&mut v, &default_resolvers(), "connector 'db'")
            .await
            .expect_err(
                "an unimplemented scheme must fail loudly, not pass through as the password",
            );
        let OrionError::Config { message } = err else {
            unreachable!("expected Config error");
        };
        assert!(message.contains("vault"), "{message}");
        assert!(message.contains("not supported"), "{message}");
        assert!(message.contains("connector 'db'"), "{message}");
    }

    #[tokio::test]
    async fn every_reserved_scheme_is_rejected() {
        for scheme in RESERVED_SCHEMES {
            let mut v = json!({ "token": format!("{scheme}://some/path") });
            assert!(
                resolve_in_place(&mut v, &default_resolvers(), "test")
                    .await
                    .is_err(),
                "scheme '{scheme}' must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn default_resolvers_leave_connection_urls_untouched() {
        // The reserved list must not catch ordinary connector URLs.
        let mut v = json!({
            "connection_string": "postgres://user:pass@db.internal:5432/app",
            "url": "redis://cache.internal:6379",
            "brokers": ["kafka.internal:9092"]
        });
        resolve_in_place(&mut v, &default_resolvers(), "test")
            .await
            .expect("test");
        assert_eq!(
            v["connection_string"],
            "postgres://user:pass@db.internal:5432/app"
        );
        assert_eq!(v["url"], "redis://cache.internal:6379");
        assert_eq!(v["brokers"][0], "kafka.internal:9092");
    }

    #[tokio::test]
    async fn resolve_in_place_recurses_into_objects() {
        let mut v = json!({
            "auth": { "type": "bearer", "token": "env://TOK" },
            "max_retries": 3
        });
        resolve_in_place(&mut v, &stub(&[("TOK", "abc")]), "test")
            .await
            .expect("test");
        assert_eq!(v["auth"]["token"], "abc");
        assert_eq!(v["max_retries"], 3);
    }

    #[tokio::test]
    async fn resolve_in_place_recurses_into_arrays() {
        let mut v = json!({ "brokers": ["env://B1", "literal:9092"] });
        resolve_in_place(&mut v, &stub(&[("B1", "broker.local:9092")]), "test")
            .await
            .expect("test");
        assert_eq!(v["brokers"][0], "broker.local:9092");
        assert_eq!(v["brokers"][1], "literal:9092");
    }

    #[tokio::test]
    async fn missing_env_var_errors_with_source_label() {
        let mut v = json!({ "token": "env://NOPE" });
        let err = resolve_in_place(&mut v, &stub(&[]), "connector 'foo'")
            .await
            .expect_err("test");
        let OrionError::Config { message } = err else {
            unreachable!("expected Config error");
        };
        assert!(message.contains("NOPE"));
        assert!(message.contains("connector 'foo'"));
    }

    #[tokio::test]
    async fn env_resolver_rejects_invalid_var_name() {
        let r = EnvSecretResolver;
        assert!(r.resolve("").await.is_err());
        assert!(r.resolve("has-hyphen").await.is_err());
        assert!(r.resolve("with space").await.is_err());
    }
}

#[cfg(test)]
mod vault_tests {
    use super::*;
    use axum::Json;
    use axum::http::{HeaderMap, StatusCode};
    use serde_json::json;

    /// An in-process fake Vault speaking just enough of the KV HTTP API:
    /// token-checked, one KV v2 path, one KV v1 path. The resolver is plain
    /// HTTP, so nothing about this test needs a real Vault.
    async fn fake_vault() -> String {
        fn authed(headers: &HeaderMap) -> bool {
            headers.get("X-Vault-Token").is_some_and(|v| v == "t0ken")
        }
        let app = axum::Router::new()
            .route(
                "/v1/secret/data/db",
                axum::routing::get(|headers: HeaderMap| async move {
                    if !authed(&headers) {
                        return (StatusCode::FORBIDDEN, Json(json!({"errors": ["denied"]})));
                    }
                    // KV v2 nests under data.data.
                    (
                        StatusCode::OK,
                        Json(json!({"data": {"data": {"password": "hunter2"}}})),
                    )
                }),
            )
            .route(
                "/v1/legacy/db",
                axum::routing::get(|headers: HeaderMap| async move {
                    if !authed(&headers) {
                        return (StatusCode::FORBIDDEN, Json(json!({"errors": ["denied"]})));
                    }
                    // KV v1 is flat under data.
                    (
                        StatusCode::OK,
                        Json(json!({"data": {"password": "legacy2"}})),
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn resolves_kv2_and_kv1_shapes() {
        let addr = fake_vault().await;
        let r = VaultSecretResolver::new(&addr, "t0ken");
        assert_eq!(
            r.resolve("secret/data/db#password").await.expect("kv2"),
            "hunter2"
        );
        assert_eq!(
            r.resolve("legacy/db#password").await.expect("kv1"),
            "legacy2"
        );
    }

    #[tokio::test]
    async fn failures_are_loud_and_tokenless() {
        let addr = fake_vault().await;

        // Wrong token: the status reaches the message, the token never does.
        let r = VaultSecretResolver::new(&addr, "wrong");
        let err = r
            .resolve("secret/data/db#password")
            .await
            .expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("403"), "{msg}");
        assert!(!msg.contains("wrong"), "token must not leak: {msg}");

        // A field the secret does not carry.
        let r = VaultSecretResolver::new(&addr, "t0ken");
        let err = r
            .resolve("secret/data/db#missing_field")
            .await
            .expect_err("must refuse");
        assert!(err.to_string().contains("missing_field"));

        // A reference with no field designator.
        let err = r.resolve("secret/data/db").await.expect_err("must refuse");
        assert!(err.to_string().contains("must name a field"));
    }

    /// End to end through the tree walk: a vault:// reference inside a config
    /// resolves like env:// always has.
    #[tokio::test]
    async fn resolve_in_place_uses_the_vault_resolver() {
        let addr = fake_vault().await;
        let resolvers: Vec<Box<dyn SecretResolver>> = vec![
            Box::new(EnvSecretResolver),
            Box::new(VaultSecretResolver::new(&addr, "t0ken")),
        ];
        let mut v = serde_json::json!({
            "auth": {"password": "vault://secret/data/db#password"},
            "url": "https://db.example.com"
        });
        resolve_in_place(&mut v, &resolvers, "connector 'db'")
            .await
            .expect("resolves");
        assert_eq!(v["auth"]["password"], "hunter2");
        assert_eq!(v["url"], "https://db.example.com");
    }
}
