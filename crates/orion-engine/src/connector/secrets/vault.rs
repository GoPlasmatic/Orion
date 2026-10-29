use serde_json::Value;

use super::SecretResolver;
use crate::errors::OrionError;

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

/// The most a Vault response body may be before it is refused. A KV secret is
/// small JSON — a value or two plus KV v2's metadata envelope — so 256 KiB is
/// generous headroom for any legitimate secret (a PEM bundle is a few KB)
/// while bounding a compromised or misconfigured endpoint. The same magnitude
/// as the JWKS cap, and enforced the same way: `http_body::read_bounded` hangs
/// up on an oversized body mid-stream rather than reading it whole and then
/// rejecting it.
const MAX_VAULT_RESPONSE_BYTES: usize = 262_144;

/// Where a [`VaultSecretResolver`] gets its address and token.
///
/// The environment variant reads at resolution rather than at construction.
/// That is what lets [`super::default_resolvers`] be a process-wide singleton: with
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
/// environment — the fail-closed answer [`super::ReservedSchemeResolver`] gives for
/// the schemes that have no implementation at all.
pub(super) fn unconfigured(var: &str) -> OrionError {
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
/// [`super::default_resolvers`] is itself a singleton now, so this mostly matters for
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
                // Separate for its SSRF policy, not to be anonymous (#356).
                .user_agent(crate::version::USER_AGENT)
                .timeout(std::time::Duration::from_secs(5))
                // `X-Vault-Token` is a custom header, which reqwest does not strip
                // on a cross-host redirect, so a redirecting `VAULT_ADDR` would
                // receive the token. Vault answers directly; a redirect is a fault.
                .redirect(reqwest::redirect::Policy::none())
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
        // Bounded *while streaming* (`http_body`), like every other egress in
        // the crate: `response.json()` reads to the end of the body first, so a
        // Vault endpoint that omits `Content-Length` and streams without limit
        // was read whole into memory at load time. The cap is enforced before
        // each chunk lands, so an oversized body is refused mid-stream.
        let bytes = crate::http_body::read_bounded(response, MAX_VAULT_RESPONSE_BYTES)
            .await
            .map_err(|e| OrionError::Config {
                message: format!("vault://{path}: reading the Vault response failed: {e}"),
            })?;
        let body: Value = serde_json::from_slice(&bytes).map_err(|_| OrionError::Config {
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
