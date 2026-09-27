//! OIDC discovery for inbound sign-in (#355).
//!
//! An `oauth2_login` provider that names an `issuer` and no explicit endpoints
//! self-configures from `<issuer>/.well-known/openid-configuration` (OpenID
//! Connect Discovery 1.0): Orion reads the `authorization_endpoint`,
//! `token_endpoint`, `jwks_uri` and (for the identity fetch) `userinfo_endpoint`
//! from the provider's own metadata rather than making an operator type four
//! URLs by hand.
//!
//! Modelled on [`crate::jwt::jwks::JwksCache`], and for the same reasons: one
//! cache per instance on the process's SSRF-pinned client; per-issuer
//! single-flight so a herd on a cold entry costs one fetch; a TTL; and
//! **serve-stale on refresh failure**, because a discovery document is public
//! metadata — serving the last good copy through an issuer blip is safe, while
//! refusing every sign-in because the well-known endpoint had a hiccup is not.
//!
//! The document is fetched only at channel *load* (`CompiledOAuth2Login::compile`),
//! never on the per-request path. A cold cache with an unreachable issuer fails
//! the compile and quarantines the channel (F35), exactly as a malformed block
//! does; a warm cache carries a reload through a transient outage.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// TTL when the well-known response states none. OIDC metadata changes rarely.
const DEFAULT_TTL: Duration = Duration::from_secs(3600);
const MIN_TTL: Duration = Duration::from_secs(60);
const MAX_TTL: Duration = Duration::from_secs(86_400);
/// A discovery document larger than this is not metadata, it is a problem.
const MAX_DOC_BYTES: usize = 65_536;
/// Per-request deadline on top of the shared client's own timeout: discovery
/// sits in a channel load's critical path and must not inherit a connector-shaped
/// budget.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// The endpoints Orion reads out of a discovery document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub authorize_url: String,
    pub token_url: String,
    pub jwks_url: String,
    /// The OIDC userinfo endpoint, when the provider publishes one — used by the
    /// normalised-identity fetch (#355) for a provider whose identity is not in
    /// the `id_token`.
    pub userinfo_url: Option<String>,
    /// The `issuer` the document declares — verified to equal the configured one.
    pub issuer: String,
}

/// The subset of the well-known document Orion consumes.
#[derive(serde::Deserialize)]
struct DiscoveryDoc {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    #[serde(default)]
    userinfo_endpoint: Option<String>,
}

struct Entry {
    doc: Arc<Discovered>,
    fetched_at: Instant,
    ttl: Duration,
}

/// The per-instance OIDC discovery cache, built at startup on the pinned client.
pub struct DiscoveryCache {
    entries: tokio::sync::RwLock<HashMap<String, Arc<Entry>>>,
    /// Per-issuer single-flight, so concurrent misses for one issuer collapse
    /// into one fetch and issuers do not queue behind each other.
    flights: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    client: reqwest::Client,
    /// `[oauth2_login] allow_private_token_urls`: the same gate the token
    /// exchange uses, because discovery is the same class of authored egress.
    allow_private: bool,
}

impl DiscoveryCache {
    pub fn new(client: reqwest::Client, allow_private: bool) -> Self {
        Self {
            entries: tokio::sync::RwLock::new(HashMap::new()),
            flights: tokio::sync::Mutex::new(HashMap::new()),
            client,
            allow_private,
        }
    }

    /// A cached entry for `issuer` that is still within its TTL, if any.
    async fn fresh(&self, issuer: &str) -> Option<Arc<Discovered>> {
        self.entries
            .read()
            .await
            .get(issuer)
            .filter(|e| e.fetched_at.elapsed() < e.ttl)
            .map(|e| Arc::clone(&e.doc))
    }

    /// The endpoints for `issuer`, cached. On a refresh failure the last good
    /// copy is served; only a *cold* miss that also fails to fetch is an error.
    pub async fn resolve(&self, issuer: &str) -> Result<Arc<Discovered>, String> {
        if let Some(doc) = self.fresh(issuer).await {
            return Ok(doc);
        }

        // Single-flight per issuer. `get` before `entry` so the hot path (an
        // issuer already seen) does not allocate a `String` key. The map is not
        // pruned: issuers are authored config — a small, bounded set — and this
        // runs only at channel load, so it plateaus at the distinct-issuer count.
        let flight = {
            let mut flights = self.flights.lock().await;
            if let Some(flight) = flights.get(issuer) {
                Arc::clone(flight)
            } else {
                let flight = Arc::new(tokio::sync::Mutex::new(()));
                flights.insert(issuer.to_string(), Arc::clone(&flight));
                flight
            }
        };
        let _guard = flight.lock().await;

        // Re-check: another task may have refreshed while we waited.
        if let Some(doc) = self.fresh(issuer).await {
            return Ok(doc);
        }

        match self.fetch(issuer).await {
            Ok((doc, ttl)) => {
                let doc = Arc::new(doc);
                self.entries.write().await.insert(
                    issuer.to_string(),
                    Arc::new(Entry {
                        doc: Arc::clone(&doc),
                        fetched_at: Instant::now(),
                        ttl,
                    }),
                );
                Ok(doc)
            }
            Err(e) => {
                // Serve stale if we have any prior copy; a cold miss is fatal.
                if let Some(entry) = self.entries.read().await.get(issuer).cloned() {
                    tracing::warn!(
                        issuer = %issuer,
                        error = %e,
                        "OIDC discovery refresh failed; serving the last good document"
                    );
                    return Ok(Arc::clone(&entry.doc));
                }
                Err(e)
            }
        }
    }

    async fn fetch(&self, issuer: &str) -> Result<(Discovered, Duration), String> {
        let url = well_known_url(issuer)?;
        // Address-checked at the moment of egress, like the JWKS and token
        // fetches — a host public when the channel was stored can be private by
        // the time it is dialled.
        if !self.allow_private {
            crate::validation::validate_url_not_private(&url).await?;
        }
        let response = self
            .client
            .get(&url)
            .timeout(FETCH_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("discovery fetch failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("discovery HTTP {}", response.status()));
        }
        let ttl = ttl_from_cache_control(
            response
                .headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
        );
        // Bounded while streaming, so a well-known endpoint cannot hand this
        // cache an unbounded body by omitting `Content-Length`.
        let body = crate::http_body::read_bounded(response, MAX_DOC_BYTES)
            .await
            .map_err(|e| format!("discovery document {e}"))?;
        let doc: DiscoveryDoc = serde_json::from_slice(&body)
            .map_err(|e| format!("discovery document is not valid metadata: {e}"))?;

        // OIDC Discovery §4.3: the `issuer` in the document MUST equal the one
        // used to build the request. Skipping this lets a redirector at the
        // well-known path point Orion's key and token fetches anywhere.
        if doc.issuer != issuer {
            return Err(format!(
                "discovery document issuer '{}' does not match the configured issuer '{issuer}'",
                doc.issuer
            ));
        }
        // The endpoints Orion will POST a secret to, fetch keys from, or send a
        // browser to must all be https (loopback carve-out for local dev), the
        // same rule the hand-typed URLs pass.
        require_https("authorization_endpoint", &doc.authorization_endpoint)?;
        require_https("token_endpoint", &doc.token_endpoint)?;
        require_https("jwks_uri", &doc.jwks_uri)?;
        if let Some(ref u) = doc.userinfo_endpoint {
            require_https("userinfo_endpoint", u)?;
        }

        Ok((
            Discovered {
                authorize_url: doc.authorization_endpoint,
                token_url: doc.token_endpoint,
                jwks_url: doc.jwks_uri,
                userinfo_url: doc.userinfo_endpoint,
                issuer: doc.issuer,
            },
            ttl,
        ))
    }
}

/// `<issuer>/.well-known/openid-configuration`, joined per OIDC Discovery §4: the
/// well-known suffix is appended to the issuer, whose own path (a tenant path,
/// say) is preserved.
fn well_known_url(issuer: &str) -> Result<String, String> {
    let trimmed = issuer.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("issuer must not be empty".to_string());
    }
    Ok(format!("{trimmed}/.well-known/openid-configuration"))
}

/// `https`, or `http` on a loopback host. The loopback rule is
/// [`super::oauth2_login::is_loopback_host`], shared so the gate on the URLs
/// Orion fetches has one definition.
fn require_https(field: &str, value: &str) -> Result<(), String> {
    let url = url::Url::parse(value)
        .map_err(|e| format!("discovery {field} '{value}' is not a URL: {e}"))?;
    if url.scheme() == "https"
        || (url.scheme() == "http" && super::oauth2_login::is_loopback_host(&url))
    {
        return Ok(());
    }
    Err(format!(
        "discovery {field} must be https — '{value}' is {}",
        url.scheme()
    ))
}

fn ttl_from_cache_control(header: Option<&str>) -> Duration {
    let Some(header) = header else {
        return DEFAULT_TTL;
    };
    header
        .split(',')
        .filter_map(|d| {
            let d = d.trim();
            d.strip_prefix("max-age=")
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
        })
        .next()
        .map(|d| d.clamp(MIN_TTL, MAX_TTL))
        .unwrap_or(DEFAULT_TTL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_url_appends_and_preserves_the_issuer_path() {
        assert_eq!(
            well_known_url("https://issuer.example").expect("ok"),
            "https://issuer.example/.well-known/openid-configuration"
        );
        // A trailing slash does not double up.
        assert_eq!(
            well_known_url("https://issuer.example/").expect("ok"),
            "https://issuer.example/.well-known/openid-configuration"
        );
        // A tenant path on the issuer is preserved (Entra, Keycloak realms).
        assert_eq!(
            well_known_url("https://login.example/tenant-123/v2.0").expect("ok"),
            "https://login.example/tenant-123/v2.0/.well-known/openid-configuration"
        );
    }

    #[test]
    fn require_https_matches_the_hand_typed_rule() {
        assert!(require_https("token_endpoint", "https://idp.example/token").is_ok());
        assert!(require_https("token_endpoint", "http://127.0.0.1:9/token").is_ok());
        assert!(require_https("token_endpoint", "http://idp.example/token").is_err());
    }

    #[test]
    fn ttl_is_clamped() {
        assert_eq!(ttl_from_cache_control(None), DEFAULT_TTL);
        assert_eq!(
            ttl_from_cache_control(Some("max-age=30")),
            MIN_TTL,
            "below the floor clamps up"
        );
        assert_eq!(
            ttl_from_cache_control(Some("public, max-age=7200")),
            Duration::from_secs(7200)
        );
        assert_eq!(
            ttl_from_cache_control(Some("max-age=999999")),
            MAX_TTL,
            "above the ceiling clamps down"
        );
    }
}
