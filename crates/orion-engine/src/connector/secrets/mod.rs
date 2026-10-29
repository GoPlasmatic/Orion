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
//! for later backends (`RESERVED_SCHEMES`) are registered too, but resolve
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

mod resolve;
#[cfg(test)]
mod tests;
mod vault;

use crate::errors::OrionError;
pub use orion_common::config::references::{
    RESERVED_SCHEMES, is_resolvable_reference, parse_reference,
};
pub(crate) use resolve::reference_sites;
pub use resolve::{
    EmbeddedReference, ResolvedParseError, ResolvedReferences, UnresolvedReferences,
    embedded_reference, embedded_references, is_load_time_reference, resolve_in_place,
    resolve_secret_string,
};
pub use vault::VaultSecretResolver;

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

/// Whether `s` is, as a whole, a reference to a secret: a `scheme://…`
/// whose scheme a [`default_resolvers`] entry serves. What a connection
/// revision's `secret_locators` values must be, and what
/// `compile --format deployment` moves out of a connector's settings.
pub fn is_secret_reference(s: &str) -> bool {
    parse_reference(s).is_some_and(|(scheme, _)| {
        default_resolvers()
            .iter()
            .any(|resolver| resolver.scheme() == scheme)
    })
}

/// The default resolver registry: a working `env://` resolver, a `vault://`
/// resolver that reads the standard `VAULT_ADDR`/`VAULT_TOKEN` environment
/// (H3c), and a rejecting entry for every remaining scheme in
/// [`RESERVED_SCHEMES`] — so an unresolvable reference fails loudly instead of
/// being handed to a backend as the literal credential.
///
/// **Built once for the process.** This is the registry every resolution site
/// reaches for — connector load, channel-auth compile per request, the
/// `crypto` task function, `[secrets]` startup resolution, admin
/// validate/test — so rebuilding it at each would cost a `Vec`, six boxed
/// resolvers, two `env::var` reads and a client clone, per call.
///
/// Building it once is only sound because no entry closes over environment
/// state: an entry that captured `VAULT_ADDR`/`VAULT_TOKEN` at construction
/// would need a rebuilt registry for a renewed token. [`VaultSecretResolver`]
/// reads them *per resolution*, so a renewed token, or one that appears after
/// startup, works without a rebuild or a reload.
pub fn default_resolvers() -> &'static [Box<dyn SecretResolver>] {
    static RESOLVERS: std::sync::OnceLock<Vec<Box<dyn SecretResolver>>> =
        std::sync::OnceLock::new();
    RESOLVERS.get_or_init(|| {
        let mut resolvers: Vec<Box<dyn SecretResolver>> = vec![
            Box::new(EnvSecretResolver),
            Box::new(VaultSecretResolver::from_env()),
        ];
        // `vault` is served by the resolver above, whose own error covers the
        // unconfigured case; the rest have no implementation to reach.
        for scheme in RESERVED_SCHEMES {
            if *scheme == "vault" {
                continue;
            }
            resolvers.push(Box::new(ReservedSchemeResolver { scheme }));
        }
        resolvers
    })
}
