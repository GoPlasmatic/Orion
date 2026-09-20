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

/// The default resolver registry: a working `env://` resolver, a `vault://`
/// resolver that reads the standard `VAULT_ADDR`/`VAULT_TOKEN` environment
/// (H3c), and a rejecting entry for every remaining scheme in
/// [`RESERVED_SCHEMES`] — so an unresolvable reference fails loudly instead of
/// being handed to a backend as the literal credential.
///
/// **Built once for the process.** This is the registry every resolution site
/// reaches for — connector load, channel-auth compile per request, the
/// `crypto` task function, `[secrets]` startup resolution, admin
/// validate/test — and it used to be rebuilt at each of them: a `Vec`, six
/// boxed resolvers, two `env::var` reads and a client clone, per call.
///
/// Building it once is only sound because no entry closes over environment
/// state any more. That was the previous shape's reason for existing: the
/// `vault` entry captured `VAULT_ADDR`/`VAULT_TOKEN` at construction, so a
/// renewed token needed a rebuilt registry. [`VaultSecretResolver`] now reads
/// them *per resolution*, which keeps that property and adds one the rebuild
/// never had — a token that appears after startup works without a reload.
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

/// HashiCorp Vault KV resolver (H3c).
///
/// Reference form: `vault://<api-path>#<field>` — the api-path is exactly
/// what follows `/v1/` in Vault's HTTP API, so a KV **v2** secret reads as
/// `vault://secret/data/db#password` (the `data/` segment is v2's, not
/// Orion's). Field lookup understands both KV shapes: v2's nested
/// `data.data.<field>` first, then v1's flat `data.<field>`.
///
/// Configuration is the standard Vault client environment — `VAULT_ADDR`
/// plus `VAULT_TOKEN` — read **per resolution**, so a renewed token is picked
/// up by the next `vault://` lookup without a restart or a reload. Errors
/// never include the token, and never include response bodies (a Vault error
/// body can echo the request path, which is fine, but a success body is the
/// secret — so bodies are parsed, not quoted).
pub struct VaultSecretResolver {
    endpoint: VaultEndpoint,
    client: reqwest::Client,
}

/// Where a [`VaultSecretResolver`] gets its address and token.
///
/// The environment variant reads at resolution rather than at construction.
/// That is what lets [`default_resolvers`] be a process-wide singleton: with
/// the read deferred, the registry holds no snapshot of the environment to go
/// stale, so rebuilding it buys nothing.
enum VaultEndpoint {
    /// Read `VAULT_ADDR` / `VAULT_TOKEN` at each resolution.
    Environment,
    /// Settings supplied by the caller — tests, and any embedder managing its
    /// own Vault configuration.
    Fixed { addr: String, token: String },
}

impl VaultEndpoint {
    /// The address and token to use for one lookup.
    ///
    /// Absent environment is an error, not a pass-through: `vault://` reaching
    /// a backend as its own literal text is exactly what this module exists to
    /// prevent, and an unconfigured process is the likeliest way to get there.
    fn settings(&self) -> Result<(String, String), OrionError> {
        match self {
            VaultEndpoint::Fixed { addr, token } => Ok((addr.clone(), token.clone())),
            VaultEndpoint::Environment => {
                let addr = std::env::var("VAULT_ADDR").map_err(|_| unconfigured("VAULT_ADDR"))?;
                let token =
                    std::env::var("VAULT_TOKEN").map_err(|_| unconfigured("VAULT_TOKEN"))?;
                Ok((addr.trim_end_matches('/').to_string(), token))
            }
        }
    }
}

/// The refusal when `vault://` is used in a process that has no Vault
/// environment — the fail-closed answer [`ReservedSchemeResolver`] gives for
/// the schemes that have no implementation at all.
fn unconfigured(var: &str) -> OrionError {
    OrionError::Config {
        message: format!(
            "vault:// is not usable in this process: {var} is not set. \
             Set VAULT_ADDR and VAULT_TOKEN, or supply the value via env:// \
             or a literal instead"
        ),
    }
}

impl VaultSecretResolver {
    /// The resolver backed by `VAULT_ADDR` + `VAULT_TOKEN`, read at each
    /// resolution rather than here — so this is infallible and a process that
    /// gains a Vault environment later needs no rebuild.
    pub fn from_env() -> Self {
        Self {
            endpoint: VaultEndpoint::Environment,
            client: vault_http_client(),
        }
    }

    /// Explicit construction, for tests and for callers that manage their own
    /// Vault settings.
    pub fn new(addr: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            endpoint: VaultEndpoint::Fixed {
                addr: addr.into().trim_end_matches('/').to_string(),
                token: token.into(),
            },
            client: vault_http_client(),
        }
    }
}

/// One HTTP client shared by every [`VaultSecretResolver`] instance.
///
/// [`default_resolvers`] is itself a singleton now, so this mostly matters for
/// the explicitly-constructed resolvers; it stays a `OnceLock` because a fresh
/// pool per resolver means a new TLS handshake per resolved secret.
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

        // Read before the request, not at construction: this is where a
        // renewed `VAULT_TOKEN` is picked up (see [`VaultEndpoint`]).
        let (addr, token) = self.endpoint.settings()?;
        let url = format!("{addr}/v1/{path}");
        let response = self
            .client
            .get(&url)
            .header("X-Vault-Token", &token)
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
    resolve_in_place(&mut json, default_resolvers(), field)
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

/// A reference that sits *inside* a longer string — `"Bearer env://API_KEY"`
/// — and is therefore not a reference at all: a reference is the whole
/// value, so this text is sent literally. `Some(scheme)` names the scheme
/// that was meant.
///
/// A scheme only counts where it starts a word (`someenv://k` is not a hit)
/// and is followed by a name. A string that is one `${…}` placeholder is
/// skipped: `${X:-env://Y}` becomes a whole-string reference after
/// substitution.
pub fn embedded_reference(s: &str) -> Option<&'static str> {
    if s.starts_with("${") && s.ends_with('}') {
        return None;
    }
    std::iter::once("env")
        .chain(RESERVED_SCHEMES.iter().copied())
        .find(|scheme| {
            let needle = format!("{scheme}://");
            s.match_indices(&needle).any(|(at, _)| {
                at > 0
                    && !s[..at]
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-'))
                    && !s[at + needle.len()..].trim().is_empty()
            })
        })
}

/// One string in a connector config with a reference inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedReference {
    /// Dotted path from the config root, e.g. `headers.Authorization`.
    pub path: String,
    /// The scheme that was meant, e.g. `env`.
    pub scheme: &'static str,
    /// The string sits under `headers` as an `Authorization` header, where
    /// `auth` is the field that takes a reference.
    pub authorization_header: bool,
}

impl EmbeddedReference {
    /// What an author should do instead.
    pub fn remedy(&self) -> &'static str {
        if self.authorization_header {
            "use \"auth\": {\"type\": \"bearer\", \"token\": \"env://API_KEY\"} instead of an \
             Authorization header"
        } else {
            "make the reference the whole value, or build the string in the deployment \
             environment"
        }
    }

    /// The finding, in one sentence.
    pub fn message(&self) -> String {
        format!(
            "`{}://…` sits inside a longer string, so it is not a reference: a reference must be \
             the whole value, and this text will be sent literally",
            self.scheme
        )
    }
}

/// Every string in `config` with [`embedded_reference`] in it.
pub fn embedded_references(config: &Value) -> Vec<EmbeddedReference> {
    fn walk(value: &Value, path: &mut Vec<String>, out: &mut Vec<EmbeddedReference>) {
        match value {
            Value::String(s) => {
                if let Some(scheme) = embedded_reference(s) {
                    out.push(EmbeddedReference {
                        path: path.join("."),
                        scheme,
                        authorization_header: matches!(
                            path.as_slice(),
                            [headers, name] if headers == "headers"
                                && name.eq_ignore_ascii_case("authorization")
                        ),
                    });
                }
            }
            Value::Object(map) => {
                for (key, v) in map {
                    path.push(key.clone());
                    walk(v, path, out);
                    path.pop();
                }
            }
            Value::Array(items) => {
                for (index, v) in items.iter().enumerate() {
                    path.push(index.to_string());
                    walk(v, path, out);
                    path.pop();
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(config, &mut Vec::new(), &mut out);
    out
}

/// Whether `s` stands for a value resolved at load — a `var://` or a
/// resolvable secret reference — and so may sit in a field of any type in an
/// authored connector config.
pub fn is_load_time_reference(s: &str) -> bool {
    s.starts_with(crate::config::vars::VAR_SCHEME) || is_resolvable_reference(s)
}

/// The authoring parse of a connector config: a reference standing in a
/// field that is not a string is replaced by a placeholder of the kind the
/// field wants, so a document the load path can type types here too — on a
/// host that holds none of the deployment's values. A check that reads such
/// a field reads the placeholder; see
/// [`crate::config::vars::parse_with_unresolved_vars`].
pub struct UnresolvedReferences;

impl super::VariantParse for UnresolvedReferences {
    type Error = String;
    fn parse<T: serde::de::DeserializeOwned>(&self, value: &Value) -> Result<T, String> {
        crate::config::vars::parse_with_unresolved_refs(value, &|_| false, &is_load_time_reference)
    }
}

/// Paths of every string [`resolve_in_place`] will replace, collected before
/// it runs — so the typed parse after it knows which values came from a
/// reference.
pub(crate) fn reference_sites(
    value: &Value,
    resolvers: &[Box<dyn SecretResolver>],
) -> Vec<Vec<crate::config::vars::Seg>> {
    use crate::config::vars::Seg;
    fn walk(
        value: &Value,
        resolvers: &[Box<dyn SecretResolver>],
        path: &mut Vec<Seg>,
        out: &mut Vec<Vec<Seg>>,
    ) {
        match value {
            Value::String(s) => {
                if let Some((scheme, _)) = parse_reference(s)
                    && resolvers.iter().any(|r| r.scheme() == scheme)
                {
                    out.push(path.clone());
                }
            }
            Value::Object(map) => {
                for (key, v) in map {
                    path.push(Seg::Key(key.clone()));
                    walk(v, resolvers, path, out);
                    path.pop();
                }
            }
            Value::Array(items) => {
                for (index, v) in items.iter().enumerate() {
                    path.push(Seg::Index(index));
                    walk(v, resolvers, path, out);
                    path.pop();
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(value, resolvers, &mut Vec::new(), &mut out);
    out
}

/// Why a resolved connector config did not type. None of these quotes a
/// resolved value: a mis-pointed reference may hold a credential (S21).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedParseError {
    /// A boolean field's reference resolved to text other than `true` or
    /// `false`. `field` is the dotted path.
    NotABoolean { field: String },
    /// Any other refusal at a reference site; the value is withheld.
    ShapeAtSite { field: String },
    /// A refusal away from every reference site — the author's own value,
    /// safe to show.
    Shape(String),
}

impl std::fmt::Display for ResolvedParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotABoolean { field } => write!(
                f,
                "{field}: the reference resolved to something other than true or false"
            ),
            Self::ShapeAtSite { field } => write!(
                f,
                "{field}: the reference resolved to a value this field cannot take (the value \
                 is withheld: it may be a secret)"
            ),
            Self::Shape(message) => f.write_str(message),
        }
    }
}

/// The load parse of a connector config whose references [`resolve_in_place`]
/// has replaced. A reference resolves to a string, always; where the shape
/// wants a boolean, exactly `true` or `false` (trimmed, any case) at a
/// reference site becomes that boolean. Nothing else is coerced, and nothing
/// away from a site is — a literal `"true"` the author wrote in a boolean
/// field is still their error.
pub struct ResolvedReferences<'a> {
    pub(crate) sites: &'a [Vec<crate::config::vars::Seg>],
}

impl super::VariantParse for ResolvedReferences<'_> {
    type Error = ResolvedParseError;
    fn parse<T: serde::de::DeserializeOwned>(
        &self,
        value: &Value,
    ) -> Result<T, ResolvedParseError> {
        use crate::config::vars::{Seg, display_path, slot_at};
        let mut doc = value.clone();
        // Each site is coerced at most once, so the loop is bounded.
        let mut coerced: Vec<Vec<Seg>> = Vec::new();
        loop {
            let err = match serde_path_to_error::deserialize::<_, T>(doc.clone()) {
                Ok(typed) => return Ok(typed),
                Err(err) => err,
            };
            let at: Option<Vec<Seg>> = err.path().iter().map(Seg::from_segment).collect();
            let Some(at) = at.filter(|at| self.sites.contains(at)) else {
                return Err(ResolvedParseError::Shape(err.to_string()));
            };
            let field = display_path(&at);
            let text = slot_at(&mut doc, &at)
                .and_then(|slot| slot.as_str())
                .map(|s| s.trim().to_ascii_lowercase());
            let wants_bool = err.inner().to_string().contains("expected a boolean");
            match text.as_deref() {
                Some(b @ ("true" | "false")) if !coerced.contains(&at) => {
                    let b = b == "true";
                    if let Some(slot) = slot_at(&mut doc, &at) {
                        *slot = Value::Bool(b);
                    }
                    coerced.push(at);
                }
                _ if wants_bool => return Err(ResolvedParseError::NotABoolean { field }),
                _ => return Err(ResolvedParseError::ShapeAtSite { field }),
            }
        }
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
        // `aws-sm` rather than `vault`: vault has a real resolver now, so its
        // refusal is about configuration, not about the scheme. This test is
        // about the schemes with no implementation at all.
        let mut v = json!({ "auth": { "password": "aws-sm://prod/db#password" } });
        let err = resolve_in_place(&mut v, default_resolvers(), "connector 'db'")
            .await
            .expect_err(
                "an unimplemented scheme must fail loudly, not pass through as the password",
            );
        let OrionError::Config { message } = err else {
            unreachable!("expected Config error");
        };
        assert!(message.contains("aws-sm"), "{message}");
        assert!(message.contains("not supported"), "{message}");
        assert!(message.contains("connector 'db'"), "{message}");
    }

    /// The other half of the same guarantee, for the one reserved scheme that
    /// *is* implemented: a process without `VAULT_ADDR`/`VAULT_TOKEN` must
    /// refuse a `vault://` reference rather than hand it on as the credential.
    ///
    /// Asserted against the refusal itself rather than by resolving through
    /// the registry, because mutating the process environment mid-test is
    /// unsound in a multi-threaded binary and a developer machine may well
    /// have a live Vault environment.
    #[test]
    fn vault_without_an_environment_refuses_rather_than_passing_through() {
        let OrionError::Config { message } = unconfigured("VAULT_ADDR") else {
            unreachable!("expected Config error");
        };
        assert!(message.contains("vault://"), "{message}");
        assert!(message.contains("VAULT_ADDR"), "{message}");
    }

    /// The registry is the same one every call site sees — the property that
    /// makes borrowing it at five call sites sound (§3.7).
    #[test]
    fn the_default_registry_is_built_once() {
        assert!(
            std::ptr::eq(default_resolvers(), default_resolvers()),
            "default_resolvers() must hand back one process-wide registry"
        );
    }

    #[tokio::test]
    async fn every_reserved_scheme_is_rejected() {
        for scheme in RESERVED_SCHEMES {
            let mut v = json!({ "token": format!("{scheme}://some/path") });
            assert!(
                resolve_in_place(&mut v, default_resolvers(), "test")
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
        resolve_in_place(&mut v, default_resolvers(), "test")
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

#[cfg(test)]
mod reference_parse_tests {
    use super::*;
    use crate::connector::{ConnectorConfig, ConnectorType};
    use serde_json::json;

    fn env_resolvers() -> Vec<Box<dyn SecretResolver>> {
        vec![Box::new(EnvSecretResolver)]
    }

    fn load(stored: Value, resolved: Value) -> Result<ConnectorConfig, ResolvedParseError> {
        let sites = reference_sites(&stored, &env_resolvers());
        ConnectorConfig::parse_variant(
            ConnectorType::Http,
            &resolved,
            &ResolvedReferences { sites: &sites },
        )
    }

    #[test]
    fn parse_resolved_coerces_true_and_false_at_sites_only() {
        let stored = json!({"url": "env://URL", "allow_private_urls": "env://PRIVATE"});
        for (text, expected) in [("true", true), (" FALSE\n", false), ("True", true)] {
            let resolved = json!({"url": "http://peer:8080", "allow_private_urls": text});
            match load(stored.clone(), resolved).expect("coerced") {
                ConnectorConfig::Http(http) => {
                    assert_eq!(http.allow_private_urls, expected, "{text:?}");
                    assert_eq!(http.url, "http://peer:8080");
                }
                other => unreachable!("{other:?}"),
            }
        }
        // A literal the author wrote as a string is theirs, not a reference.
        let literal = json!({"url": "http://peer", "allow_private_urls": "true"});
        let err = load(literal.clone(), literal).expect_err("not a site");
        assert!(matches!(err, ResolvedParseError::Shape(_)), "{err:?}");
    }

    #[test]
    fn a_non_boolean_resolution_names_the_field_and_not_the_value() {
        let stored = json!({"url": "http://peer", "allow_private_urls": "env://PRIVATE"});
        let resolved = json!({"url": "http://peer", "allow_private_urls": "s3cr3t-token"});
        let err = load(stored, resolved).expect_err("garbage");
        assert_eq!(
            err,
            ResolvedParseError::NotABoolean {
                field: "allow_private_urls".to_string()
            }
        );
        let message = err.to_string();
        assert!(message.contains("allow_private_urls"), "{message}");
        assert!(!message.contains("s3cr3t"), "{message}");
    }

    #[test]
    fn embedded_references_are_found_and_whole_ones_are_not() {
        for (text, expected) in [
            ("Bearer env://API_KEY", Some("env")),
            ("x vault://secret/a#b", Some("vault")),
            ("token=(env://K)", Some("env")),
            ("env://API_KEY", None),
            ("someenv://k", None),
            ("${X:-env://Y}", None),
            ("https://h/p", None),
            ("see env:// ", None),
        ] {
            assert_eq!(embedded_reference(text), expected, "{text:?}");
        }
        let found = embedded_references(&json!({
            "url": "https://api.example.com",
            "headers": {"Authorization": "Bearer env://API_KEY", "X-Other": "k env://K"},
            "auth": {"type": "bearer", "token": "env://API_KEY"},
        }));
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].path, "headers.Authorization");
        assert!(found[0].authorization_header);
        assert!(found[0].remedy().contains("\"auth\""));
        assert_eq!(found[1].path, "headers.X-Other");
        assert!(!found[1].authorization_header);
    }
}
