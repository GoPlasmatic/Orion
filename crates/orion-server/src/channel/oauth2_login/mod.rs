//! Inbound OAuth2: completing a browser authorization-code grant (#307).
//!
//! Orion could already *call* an OAuth2-protected API — `connector/oauth.rs`,
//! #268 — but it could not *be* the relying party, so "Sign in with
//! GitHub/Google" had to be assembled from primitives. Assembled, it is two
//! channels, two workflows and thirteen tasks for one well-specified RFC, and
//! two of those tasks are security properties an author has to know to write.
//!
//! The split this module takes is the one the codebase already makes.
//! `auth.mode` is per-request credential **verification**, and that half works:
//! `auth.mode = "jwt"` with a cookie source guards every signed-in route today.
//! Only **establishment** was missing — redirect out, callback in — and it is a
//! two-request dance rather than a credential check, so it is a `config` block
//! and not a fourth `AuthMode`.
//!
//! ## One channel, one or many providers (#355)
//!
//! A block either names one provider through its flat fields, or a `providers`
//! map selected by the `{provider}` segment of the channel's routes. Everything
//! below is written against a *selected* [`CompiledProvider`]: the single-provider
//! form is a map with one implicit entry keyed `""`, and both legs resolve it
//! through the one `select`, so selection does not fork on the form. The flat
//! form differs only in what stays backward-compatible with pre-#355 sign-ins —
//! it seals no provider into the state and stamps no `provider`/`kind` onto the
//! grant. The slug is sealed into the signed state, so a callback cannot switch
//! providers, and an unknown slug is a `404`.
//!
//! ## What this owns, and why each half is here rather than in the workflow
//!
//! - **The `302` and the state cookie.** Mechanical, identical for every
//!   provider.
//! - **The CSRF binding.** The state parameter is only a defence if a callback
//!   that fails the comparison *stops*. Spelled as a `validation` rule it does
//!   not: a failing rule returns `Status(400)` and the executor's 4xx branch
//!   continues unconditionally (see `GoPlasmatic/Orion#308`), so a callback
//!   arriving with no state cookie at all ran the exchange, wrote the user row
//!   and minted a session. Here the comparison happens before the workflow is
//!   entered and a failure is a `401` with nothing downstream of it.
//! - **The nonce's uniqueness.** `jwt_sign` alone cannot mint one: its claims
//!   are constant and `iat`/`exp` are second-granular, so two sign-ins starting
//!   in the same second produce byte-identical state tokens. The nonce here is
//!   32 bytes from the operating-system CSPRNG.
//! - **PKCE.** Inexpressible in the assembled form — a flow carrying its state
//!   in both the cookie and the query parameter cannot add a verifier without
//!   sending it to the IdP.
//! - **The `id_token`'s `nonce`.** The workflow never sees the authorize
//!   request, so it cannot hold the value the token must echo.
//!
//! The workflow keeps the half that is genuinely application-specific:
//! identify the user, upsert the row, mint the app's own session token,
//! redirect home. It receives the grant at `metadata.oauth` (and, once
//! populated, the normalised identity at `metadata.identity`).
//!
//! ## State is a signed cookie, not a stored row
//!
//! Everything the callback needs — the nonce, the PKCE verifier, the OIDC
//! nonce, the destination, the provider slug — travels in one HS256 JWT in an
//! `HttpOnly` cookie, minted with [`crate::jwt::sign`] and verified with
//! [`crate::jwt::Verifier`]. That reuses #267's core whole (the algorithm
//! allowlist, `require_exp`, the leeway, RFC 7518's key-length floor) and needs
//! no shared store, so a sign-in that begins on one node and returns to another
//! works with no coordination. The `state` query parameter **is** the nonce
//! claim; the binding is that the two match.
//!
//! The cost is that "single use" is enforced by clearing the cookie rather than
//! by a row: two concurrent replays of one callback inside the window would
//! both pass this check. The authorization code itself is single-use at the
//! IdP, which is where that defence actually lives.

use std::collections::{BTreeMap, HashMap};

use serde_json::{Value, json};

use super::config::{
    IdTokenConfig, IdentityMap, OAuth2LoginConfig, ProviderConfig, RESERVED_AUTHORIZE_PARAMS,
    ReturnToConfig, StateCookieConfig,
};
use crate::errors::{OrionError, Unavailable};

/// Which of the channel's two routes a request arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leg {
    /// The channel's own `route_pattern`: begin a sign-in.
    Authorize,
    /// `oauth2_login.callback_path`: the IdP is redirecting the browser back.
    Callback,
}

impl Leg {
    /// The metric label. Bounded by construction.
    pub const fn as_str(self) -> &'static str {
        match self {
            Leg::Authorize => "authorize",
            Leg::Callback => "callback",
        }
    }
}

/// The metric `provider` label for a single-provider (flat) block, and for any
/// failure that happens before a provider is selected. Fixed strings, never a
/// caller-supplied slug — a metric label value must come from a bounded set, or
/// a prober floods the series with junk slugs.
const PROVIDER_LABEL_SINGLE: &str = "default";
const PROVIDER_LABEL_UNKNOWN: &str = "unknown";

/// The metric `provider` label for a resolved provider: its slug, or `default`
/// for the single-provider (flat) form, whose implicit entry is keyed `""`.
fn provider_label(canonical_slug: &str) -> &str {
    if canonical_slug.is_empty() {
        PROVIDER_LABEL_SINGLE
    } else {
        canonical_slug
    }
}

/// Bytes of entropy in the CSRF nonce and the PKCE verifier. RFC 7636 §4.1
/// specifies 32 octets for the verifier; the nonce has no less to protect.
const NONCE_BYTES: usize = 32;

/// Cap on a `return_to` value. It rides in a cookie, and a cookie value is
/// capped at 4096 bytes for the whole jar entry.
const MAX_RETURN_TO_BYTES: usize = 512;

/// Cap on a userinfo response, read while streaming (#355). A profile document
/// larger than this is not one Orion needs to map five fields out of.
const MAX_USERINFO_BYTES: usize = 65_536;

/// The state token's algorithm. Fixed rather than configurable: the key is
/// Orion's own, it never leaves the instance, and nothing interoperates with
/// it, so an algorithm choice here would be a knob with no right answer other
/// than this one.
const STATE_ALG: jsonwebtoken::Algorithm = jsonwebtoken::Algorithm::HS256;

/// The `302` that begins a sign-in.
pub struct Redirect {
    pub location: String,
    /// The `Set-Cookie` value carrying the signed state.
    pub set_cookie: String,
}

impl std::fmt::Debug for Redirect {
    /// Redacts the cookie: it carries the signed state, and that seals the PKCE
    /// verifier for the duration of one sign-in.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redirect")
            .field("location", &self.location)
            .field("set_cookie", &"<redacted>")
            .finish()
    }
}

/// A verified callback: what the workflow gets, and the cookie that retires the
/// state it was verified against.
pub struct Grant {
    /// The object stamped at `metadata.oauth`.
    pub metadata: Value,
    /// The normalised identity stamped at `metadata.identity` (#355), when one
    /// could be resolved from the verified claims or the userinfo response.
    pub identity: Option<Value>,
    /// `Set-Cookie` clearing the state cookie, appended to whatever response
    /// the workflow shapes.
    pub clear_cookie: String,
}

impl std::fmt::Debug for Grant {
    /// Names the keys and prints none of the values. `metadata` holds an
    /// access token, often a refresh token and the raw `id_token`, so a
    /// derived `Debug` would put all three into any log line or test failure
    /// that formatted one.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: Vec<&str> = self
            .metadata
            .as_object()
            .map(|m| m.keys().map(String::as_str).collect())
            .unwrap_or_default();
        f.debug_struct("Grant").field("fields", &keys).finish()
    }
}

/// What [`CompiledOAuth2Login::compile`] needs beyond the config itself.
pub struct LoginDeps<'a> {
    pub http_client: &'a reqwest::Client,
    pub jwks: &'a std::sync::Arc<crate::jwt::jwks::JwksCache>,
    /// `[oauth2_login] allow_private_token_urls`. Instance-wide, never
    /// per-channel: a per-channel opt-out would let the author of a definition
    /// grant themselves the egress the flag exists to gate — the same argument
    /// recorded for `jwt.allow_private_jwks_urls`.
    pub allow_private_token_urls: bool,
    /// Deployment-supplied providers (#355), merged into a block that opts in
    /// with `providers_from_instance`. The definition's own entries win a slug
    /// clash. Empty when the deployment declares none.
    pub instance_providers:
        &'a std::collections::BTreeMap<String, crate::config::InstanceProviderConfig>,
    /// The instance's OIDC discovery cache, for a provider that names an
    /// `issuer` and no explicit endpoints (#355). Resolved at load, never per
    /// request.
    pub discovery: &'a std::sync::Arc<crate::channel::oidc_discovery::DiscoveryCache>,
}

/// One resolved identity provider: the endpoints, credentials and verifier a
/// selected provider serves with. The per-request path reads these and does no
/// resolution and no parsing.
pub struct CompiledProvider {
    /// `"oidc"` or `"oauth2"` — the establishment protocol, stamped into the
    /// identity. Bounded by construction.
    kind: &'static str,
    client_id: String,
    client_secret: String,
    authorize_url: String,
    token_url: String,
    /// The redirect URI, with `{provider}` already filled in — the value sent on
    /// both legs and registered with the IdP.
    redirect_uri: String,
    client_auth: String,
    scopes: Vec<String>,
    extra_authorize_params: BTreeMap<String, String>,
    id_token: Option<IdTokenConfig>,
    id_token_verifier: Option<crate::jwt::Verifier>,
    /// The userinfo endpoint (explicit or discovered), for a provider whose
    /// identity is not carried in an `id_token`.
    userinfo_url: Option<String>,
    /// How to read the normalised identity, with defaults resolved.
    identity: CompiledIdentityMap,
}

impl CompiledProvider {
    fn wants_oidc_nonce(&self) -> bool {
        self.id_token.as_ref().is_some_and(|id| id.nonce)
    }
}

/// The normalised-identity source keys with defaults applied — the per-request
/// form of [`IdentityMap`].
struct CompiledIdentityMap {
    subject: String,
    login: String,
    name: String,
    email: String,
    picture: String,
}

impl CompiledIdentityMap {
    /// Apply the OIDC-claim-name defaults to an authored map.
    fn resolve(map: Option<&IdentityMap>) -> Self {
        let pick = |v: Option<&str>, default: &str| {
            v.filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| default.to_string())
        };
        Self {
            subject: pick(map.and_then(|m| m.subject.as_deref()), "sub"),
            login: pick(map.and_then(|m| m.login.as_deref()), "preferred_username"),
            name: pick(map.and_then(|m| m.name.as_deref()), "name"),
            email: pick(map.and_then(|m| m.email.as_deref()), "email"),
            picture: pick(map.and_then(|m| m.picture.as_deref()), "picture"),
        }
    }

    /// Read the normalised identity out of a claims or userinfo object. Returns
    /// `None` when no `subject` is present — an identity without one cannot be
    /// upserted on, so it is not worth stamping.
    fn extract(&self, source: &Value) -> Option<Value> {
        let subject = source.get(&self.subject).and_then(value_to_string)?;
        let mut identity = json!({ "subject": subject });
        for (out, key) in [
            ("login", &self.login),
            ("name", &self.name),
            ("email", &self.email),
            ("picture", &self.picture),
        ] {
            if let Some(v) = source.get(key).and_then(value_to_string) {
                identity[out] = json!(v);
            }
        }
        Some(identity)
    }
}

/// A claim value as a string: a JSON string as-is, a number or bool stringified
/// (GitHub's `id` is a number, and `subject` is stamped as a string), anything
/// else (null, array, object) ignored.
fn value_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// A channel's `oauth2_login` block with its secrets resolved and its keys
/// built — the per-request path does no resolution and no parsing.
pub struct CompiledOAuth2Login {
    channel: String,
    callback_path: String,
    pkce: bool,
    run_workflow_on_authorize: bool,
    state_cookie: StateCookieConfig,
    return_to: Option<ReturnToConfig>,
    state_key: jsonwebtoken::EncodingKey,
    state_verifier: crate::jwt::Verifier,
    /// Every provider this block serves, by slug. The single-provider (flat)
    /// form is one entry under the empty slug with [`Self::route_selected`]
    /// false; the multi-provider form is the authored map. This is the
    /// resolver: `begin`/`complete`/the guard reach a provider only through it
    /// and never know whether the block was flat, a map, or (later) sourced
    /// elsewhere.
    providers: BTreeMap<String, CompiledProvider>,
    /// Whether a provider is chosen from the `{provider}` route slug.
    route_selected: bool,
    /// The shared client. `reqwest::Client` is an `Arc` internally, so this is
    /// a handle and not a second connection pool.
    http_client: reqwest::Client,
    allow_private_token_urls: bool,
}

impl std::fmt::Debug for CompiledOAuth2Login {
    /// Hand-written because the derive would print `client_secret` — and a
    /// `ChannelRuntimeConfig` is `Debug` and does get logged.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledOAuth2Login")
            .field("channel", &self.channel)
            .field("callback_path", &self.callback_path)
            .field("pkce", &self.pkce)
            .field("route_selected", &self.route_selected)
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl CompiledOAuth2Login {
    /// Resolve secrets, build both key sets, and check what the shape alone
    /// cannot.
    ///
    /// `Err` is a human-readable reason. The caller turns it into an F35
    /// quarantine: a channel whose sign-in flow did not compile is refused at
    /// every ingress rather than served with the CSRF binding quietly missing —
    /// which is the whole failure this block exists to prevent.
    pub async fn compile(
        cfg: &OAuth2LoginConfig,
        channel: &str,
        deps: &LoginDeps<'_>,
    ) -> Result<Self, String> {
        // Resolve before the shape check, so the check runs on what will serve.
        // A `var://` was substituted into the JSON before this block was typed;
        // the secret schemes resolve here, field by field, and the URLs are
        // among those fields because they are what differs between environments.
        let state_secret = resolve_secret(&cfg.state_secret, "oauth2_login.state_secret").await?;
        let shared_redirect =
            resolve_secret(&cfg.redirect_uri, "oauth2_login.redirect_uri").await?;

        // The effective providers: the block's own, plus (when it opts in) the
        // deployment's, with the definition winning any slug clash. The merge is
        // here, in the resolver's own constructor, so `begin`/`complete` never
        // learn a provider's source — a stored resource could become a third
        // input with no change past this point.
        let authored = merge_instance_providers(cfg, deps.instance_providers);

        // Resolve each provider's reference-bearing fields, once, up front.
        let mut resolved_entries: Vec<(String, ProviderConfig)> = Vec::new();
        for (slug, p) in authored {
            let pfx = field_prefix(&slug);
            let resolved = ProviderConfig {
                kind: p.kind.clone(),
                issuer: resolve_opt(&p.issuer, &format!("oauth2_login.{pfx}issuer")).await?,
                authorize_url: resolve_opt(
                    &p.authorize_url,
                    &format!("oauth2_login.{pfx}authorize_url"),
                )
                .await?,
                token_url: resolve_opt(&p.token_url, &format!("oauth2_login.{pfx}token_url"))
                    .await?,
                client_id: resolve_opt(&p.client_id, &format!("oauth2_login.{pfx}client_id"))
                    .await?,
                client_secret: resolve_opt(
                    &p.client_secret,
                    &format!("oauth2_login.{pfx}client_secret"),
                )
                .await?,
                client_auth: p.client_auth.clone(),
                redirect_uri: resolve_opt(
                    &p.redirect_uri,
                    &format!("oauth2_login.{pfx}redirect_uri"),
                )
                .await?,
                scopes: p.scopes.clone(),
                extra_authorize_params: p.extra_authorize_params.clone(),
                id_token: p.id_token.clone(),
                userinfo_url: resolve_opt(
                    &p.userinfo_url,
                    &format!("oauth2_login.{pfx}userinfo_url"),
                )
                .await?,
                identity: p.identity.clone(),
            };
            resolved_entries.push((slug, resolved));
        }

        // Shape-check the resolved values, so a reference that resolved to plain
        // http is refused here rather than reaching the provider.
        let resolved_cfg = assemble_resolved(cfg, shared_redirect.clone(), &resolved_entries);
        validate_shape(&resolved_cfg, ShapeCheck::Serving)?;

        // `encoding_key` enforces RFC 7518 §3.2's ≥32-byte floor for HS256, so
        // a short secret fails here rather than signing a forgeable state.
        let state_key = crate::jwt::encoding_key(STATE_ALG, &state_secret, None)
            .map_err(|e| format!("oauth2_login.state_secret: {e}"))?;
        let state_decoding = crate::jwt::decoding_key(STATE_ALG, &state_secret, None)
            .map_err(|e| format!("oauth2_login.state_secret: {e}"))?;

        let state_verifier = crate::jwt::Verifier {
            static_keys: vec![crate::jwt::StaticKey {
                kid: None,
                algorithm: STATE_ALG,
                key: state_decoding,
            }],
            jwks: None,
            algorithms: vec![STATE_ALG],
            // No issuer or audience: the token never leaves this instance, and
            // the key is what identifies it. `require_exp` is what matters —
            // the state's whole job is to be short-lived.
            issuer: Vec::new(),
            audience: Vec::new(),
            leeway_secs: crate::jwt::DEFAULT_LEEWAY_SECS,
            require_exp: true,
            max_token_bytes: crate::jwt::DEFAULT_MAX_TOKEN_BYTES,
            validations: std::sync::OnceLock::new(),
        };

        // Build the compiled providers from the resolved values, resolving OIDC
        // discovery for any provider that named an `issuer` and left its
        // endpoints out. Discovery is fetched here, at load — never per request.
        let mut providers = BTreeMap::new();
        for (slug, p) in &resolved_entries {
            let discovered = match p.issuer.as_deref() {
                Some(issuer) => Some(
                    deps.discovery
                        .resolve(issuer)
                        .await
                        .map_err(|e| format!("oauth2_login.{}issuer: {e}", field_prefix(slug)))?,
                ),
                None => None,
            };
            providers.insert(
                slug.clone(),
                build_provider(slug, p, &shared_redirect, discovered.as_deref(), deps)?,
            );
        }

        Ok(Self {
            channel: channel.to_string(),
            callback_path: cfg.callback_path.clone(),
            pkce: cfg.pkce,
            run_workflow_on_authorize: cfg.run_workflow_on_authorize,
            state_cookie: cfg.state_cookie.clone(),
            return_to: cfg.return_to.clone(),
            state_key,
            state_verifier,
            providers,
            route_selected: cfg.is_multi_provider(),
            http_client: deps.http_client.clone(),
            allow_private_token_urls: deps.allow_private_token_urls,
        })
    }

    /// The channel's callback route, as authored.
    pub fn callback_path(&self) -> &str {
        &self.callback_path
    }

    /// Whether the workflow runs on the authorize leg before the redirect is
    /// built.
    pub fn runs_workflow_on_authorize(&self) -> bool {
        self.run_workflow_on_authorize
    }

    /// The state cookie's name, for the read side.
    pub fn state_cookie_name(&self) -> &str {
        &self.state_cookie.name
    }

    /// Select the provider a request names, or `NotFound` when the slug is not
    /// one this block serves. The single-provider form ignores the slug and
    /// returns its one entry. The one lookup point — `begin`, `require_provider`
    /// and `complete` all resolve a provider through here.
    ///
    /// Returns the canonical slug (the map key) alongside the provider, so the
    /// caller seals and labels with a value from the bounded set rather than the
    /// caller's own string.
    fn select(&self, slug: Option<&str>) -> Result<(&str, &CompiledProvider), OrionError> {
        if self.route_selected {
            let slug = slug.unwrap_or_default();
            self.providers
                .get_key_value(slug)
                .map(|(k, p)| (k.as_str(), p))
                .ok_or_else(|| {
                    OrionError::NotFound("no such identity provider on this channel".to_string())
                })
        } else {
            let (k, p) = self
                .providers
                .iter()
                .next()
                .expect("a compiled block always has at least one provider");
            Ok((k.as_str(), p))
        }
    }

    /// `Ok` if the slug names a provider — used on the authorize leg that defers
    /// its redirect, so a sign-in to an unknown provider is a `404` before the
    /// workflow runs rather than after.
    pub fn require_provider(&self, slug: Option<&str>) -> Result<(), OrionError> {
        self.select(slug).map(|_| ())
    }

    // -----------------------------------------------------------------
    // The authorize leg
    // -----------------------------------------------------------------

    /// Mint the state and build the redirect to the IdP.
    ///
    /// `slug` is the `{provider}` route segment (or `None`/`""` for the
    /// single-provider form). An unknown slug is a `404`.
    ///
    /// `contributed` is `data._orion.oauth2.authorize` when the channel runs
    /// its workflow on this leg — an object that may carry `extra_params` and
    /// `scopes`. It cannot reach `state`, `nonce` or `code_challenge`:
    /// [`RESERVED_AUTHORIZE_PARAMS`] is filtered here as well as refused at
    /// create time, because config validation cannot see what a workflow
    /// computes.
    ///
    /// `return_to` arrives already checked, from [`Self::accepted_return_to`].
    pub fn begin(
        &self,
        slug: Option<&str>,
        contributed: Option<&Value>,
        return_to: Option<&str>,
    ) -> Result<Redirect, OrionError> {
        let (canonical_slug, provider) = match self.select(slug) {
            Ok(v) => v,
            Err(e) => {
                if self.route_selected {
                    crate::metrics::record_oauth_login(
                        &self.channel,
                        PROVIDER_LABEL_UNKNOWN,
                        Leg::Authorize.as_str(),
                        "unknown_provider",
                    );
                }
                return Err(e);
            }
        };

        let nonce = random_nonce();
        let oidc_nonce = provider.wants_oidc_nonce().then(random_nonce);
        let verifier = self.pkce.then(random_nonce);

        let mut url = url::Url::parse(&provider.authorize_url).map_err(|e| {
            OrionError::internal(format!("oauth2_login authorize_url does not parse: {e}"))
        })?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("response_type", "code");
            q.append_pair("client_id", &provider.client_id);
            q.append_pair("redirect_uri", &provider.redirect_uri);
            q.append_pair("state", &nonce);

            let scopes = contributed
                .and_then(|c| c.get("scopes"))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| provider.scopes.clone());
            if !scopes.is_empty() {
                q.append_pair("scope", &scopes.join(" "));
            }
            if let Some(ref n) = oidc_nonce {
                q.append_pair("nonce", n);
            }
            if let Some(ref v) = verifier {
                q.append_pair("code_challenge", &pkce_challenge(v));
                q.append_pair("code_challenge_method", "S256");
            }
            for (k, v) in &provider.extra_authorize_params {
                q.append_pair(k, v);
            }
            if let Some(extra) = contributed
                .and_then(|c| c.get("extra_params"))
                .and_then(Value::as_object)
            {
                for (k, v) in extra {
                    if RESERVED_AUTHORIZE_PARAMS.contains(&k.as_str()) {
                        tracing::warn!(
                            channel = %self.channel,
                            param = %k,
                            "Workflow tried to set an authorize parameter Orion owns; ignoring"
                        );
                        continue;
                    }
                    if let Some(v) = v.as_str() {
                        q.append_pair(k, v);
                    }
                }
            }
        }

        // Both uses of `max_age` here are total because `compile` ran
        // `validate_shape`, which caps it at `MAX_STATE_COOKIE_MAX_AGE_SECS` —
        // so the sum cannot overflow and the cast below cannot go negative.
        let now = now_secs();
        let mut claims = json!({
            "nonce": nonce,
            "iat": now,
            "exp": now + self.state_cookie.max_age,
        });
        if let Some(v) = verifier {
            claims["pkce_verifier"] = json!(v);
        }
        if let Some(n) = oidc_nonce {
            claims["oidc_nonce"] = json!(n);
        }
        if let Some(r) = return_to {
            claims["return_to"] = json!(r);
        }
        // Seal the provider slug so the callback cannot present a state minted
        // for one provider against another's callback URL.
        if self.route_selected {
            claims["provider"] = json!(canonical_slug);
        }
        let token = crate::jwt::sign(STATE_ALG, &self.state_key, None, &claims)
            .map_err(|e| OrionError::internal(format!("could not sign the OAuth2 state: {e}")))?;

        crate::metrics::record_oauth_login(
            &self.channel,
            provider_label(canonical_slug),
            Leg::Authorize.as_str(),
            "ok",
        );
        Ok(Redirect {
            location: url.into(),
            set_cookie: self
                .state_cookie(&token, self.state_cookie.max_age as i64)
                .map_err(OrionError::internal)?,
        })
    }

    // -----------------------------------------------------------------
    // The callback leg
    // -----------------------------------------------------------------

    /// Verify the callback and exchange its code.
    ///
    /// Every verification failure answers the same `401` with the same body.
    /// The reason is typed only in the log and in
    /// `orion_oauth_login_total{outcome}` — #267's rule, and it applies here
    /// for the same reason: telling a caller *which* half of the state check
    /// failed is telling a prober how to make progress.
    pub async fn complete(
        &self,
        slug: Option<&str>,
        query: &HashMap<String, String>,
        jar: &[&str],
    ) -> Result<Grant, OrionError> {
        // Every failure before a provider is selected labels the metric
        // `unknown`, never the caller's slug.
        let unknown = PROVIDER_LABEL_UNKNOWN;

        // The IdP refusing is not the same as a check failing here: the user
        // pressed "Cancel", or consent was withdrawn. Still a 401 on the wire —
        // no session was established — but named separately in the metric.
        if let Some(err) = query.get("error") {
            tracing::info!(
                channel = %self.channel,
                error = %err,
                description = query.get("error_description").map(String::as_str).unwrap_or(""),
                "OAuth2 sign-in refused at the identity provider"
            );
            return Err(self.refuse(unknown, "provider_error"));
        }

        let state = query
            .get("state")
            .ok_or_else(|| self.refuse(unknown, "state_missing"))?;
        let code = query
            .get("code")
            .ok_or_else(|| self.refuse(unknown, "code_missing"))?;

        let cookie = crate::channel::cookies::lookup(jar.iter().copied(), &self.state_cookie.name)
            .ok_or_else(|| self.refuse(unknown, "state_missing"))?;

        // Signature, algorithm and `exp` in one call — the same verifier a
        // `jwt` channel uses on a caller's token.
        let claims = self
            .state_verifier
            .verify(&cookie)
            .await
            .map_err(|reason| {
                tracing::warn!(
                    channel = %self.channel,
                    reason = reason.as_str(),
                    "OAuth2 state cookie rejected"
                );
                self.refuse(unknown, "state_invalid")
            })?;

        let minted = claims
            .get("nonce")
            .and_then(Value::as_str)
            .ok_or_else(|| self.refuse(unknown, "state_invalid"))?;
        if !secret_eq(state, minted) {
            return Err(self.refuse(unknown, "state_mismatch"));
        }

        // Confirm the sealed slug is the one whose callback URL was actually hit
        // — a state minted for one provider cannot be spent at another's — then
        // resolve through the same `select` both other legs use. `select` is
        // keyed on the slug, which equals the sealed value just checked; its
        // `NotFound` becomes the uniform `401` a callback answers.
        if self.route_selected {
            let route_slug = slug.unwrap_or_default();
            let sealed = claims
                .get("provider")
                .and_then(Value::as_str)
                .ok_or_else(|| self.refuse(unknown, "state_invalid"))?;
            if sealed != route_slug {
                return Err(self.refuse(unknown, "provider_mismatch"));
            }
        }
        let (canonical_slug, provider) = self
            .select(slug)
            .map_err(|_| self.refuse(unknown, "unknown_provider"))?;
        let label = provider_label(canonical_slug);

        let tokens = self
            .exchange(
                provider,
                label,
                code,
                claims.get("pkce_verifier").and_then(Value::as_str),
            )
            .await?;

        let mut oauth = json!({
            "access_token": tokens.access_token,
            "token_type": tokens.token_type.as_deref().unwrap_or("Bearer"),
        });
        for (key, value) in [
            ("refresh_token", tokens.refresh_token),
            ("id_token", tokens.id_token.clone()),
            ("scope", tokens.scope),
        ] {
            if let Some(v) = value {
                oauth[key] = json!(v);
            }
        }
        if let Some(expires_in) = tokens.expires_in {
            oauth["expires_in"] = json!(expires_in);
        }
        if let Some(return_to) = claims.get("return_to").and_then(Value::as_str) {
            oauth["return_to"] = json!(return_to);
        }
        // The workflow learns which provider answered, and its kind (`oidc` or
        // `oauth2`) so it can branch on how identity was established. Stamped
        // only for the multi-provider form; the single-provider form is
        // byte-for-byte as before, so existing workflows are undisturbed.
        self.stamp_provenance(&mut oauth, canonical_slug, provider.kind);

        if let Some(verifier) = provider.id_token_verifier.as_ref() {
            let id = provider.id_token.as_ref().expect("verifier implies config");
            match tokens.id_token.as_deref() {
                Some(token) => {
                    let verified = verifier.verify(token).await.map_err(|reason| {
                        tracing::warn!(
                            channel = %self.channel,
                            reason = reason.as_str(),
                            "OAuth2 id_token rejected"
                        );
                        self.refuse(label, "id_token_rejected")
                    })?;
                    if id.nonce {
                        let minted = claims.get("oidc_nonce").and_then(Value::as_str);
                        let echoed = verified.get("nonce").and_then(Value::as_str);
                        match (minted, echoed) {
                            (Some(a), Some(b)) if secret_eq(a, b) => {}
                            _ => return Err(self.refuse(label, "nonce_mismatch")),
                        }
                    }
                    oauth["claims"] = verified;
                }
                None if id.required => {
                    tracing::warn!(
                        channel = %self.channel,
                        "Token response carried no id_token, but one is required"
                    );
                    return Err(self.refuse(label, "id_token_rejected"));
                }
                None => {}
            }
        }

        // The normalised identity (#355), so one workflow upserts on
        // `(provider, subject)` whatever the provider. From the verified
        // `id_token` claims when there are any (signed, already in hand);
        // otherwise fetched from the userinfo endpoint with the access token.
        let identity = self
            .resolve_identity(provider, canonical_slug, &oauth)
            .await?;
        if let Some(ref id) = identity {
            // Mirror it under `metadata.oauth` too, the shape #355 first named;
            // `metadata.identity` (below) is the protocol-neutral home workflows
            // should key on.
            oauth["identity"] = id.clone();
        }

        crate::metrics::record_oauth_login(&self.channel, label, Leg::Callback.as_str(), "ok");
        Ok(Grant {
            metadata: oauth,
            identity,
            // Retire the state the moment it is spent. `Max-Age=0` with the
            // same name, path and attributes, or the browser keeps the old one
            // alongside.
            clear_cookie: self.state_cookie("", 0).map_err(|e| {
                OrionError::internal(format!("could not clear the state cookie: {e}"))
            })?,
        })
    }

    /// Build the normalised identity for a completed sign-in: from the verified
    /// `id_token` claims (`metadata.oauth.claims`) when present, else from a
    /// userinfo fetch, else `None`. `provider` and `kind` are stamped for the
    /// multi-provider form, matching `metadata.oauth`.
    async fn resolve_identity(
        &self,
        provider: &CompiledProvider,
        canonical_slug: &str,
        oauth: &Value,
    ) -> Result<Option<Value>, OrionError> {
        let label = provider_label(canonical_slug);
        // Prefer the signed claims; fall back to userinfo.
        let mut base = if let Some(claims) = oauth.get("claims") {
            provider.identity.extract(claims)
        } else if let Some(url) = provider.userinfo_url.as_deref() {
            let access_token = oauth
                .get("access_token")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let userinfo = self.fetch_userinfo(url, access_token, label).await?;
            provider.identity.extract(&userinfo)
        } else {
            None
        };

        if let Some(identity) = base.as_mut() {
            self.stamp_provenance(identity, canonical_slug, provider.kind);
        } else if provider.id_token_verifier.is_some() || provider.userinfo_url.is_some() {
            // A source was configured but yielded no `subject` — the mapping is
            // likely wrong. Not fatal (the workflow still has the grant), but
            // worth a line so it is not a silent absence.
            tracing::warn!(
                channel = %self.channel,
                "OAuth2 sign-in resolved no identity subject; check the provider's identity mapping"
            );
        }
        Ok(base)
    }

    /// Stamp which provider answered and its kind onto an object — for the
    /// multi-provider form only, so `metadata.oauth` and `metadata.identity`
    /// carry the provenance the same way.
    fn stamp_provenance(&self, obj: &mut Value, canonical_slug: &str, kind: &str) {
        if self.route_selected
            && let Some(map) = obj.as_object_mut()
        {
            map.insert("provider".to_string(), json!(canonical_slug));
            map.insert("kind".to_string(), json!(kind));
        }
    }

    /// Fetch the userinfo endpoint with the access token. A new egress, so it
    /// passes the same address check and byte cap the token exchange does.
    async fn fetch_userinfo(
        &self,
        url: &str,
        access_token: &str,
        label: &str,
    ) -> Result<Value, OrionError> {
        if !self.allow_private_token_urls
            && let Err(msg) = orion_common::ssrf::validate_url_not_private(url).await
        {
            tracing::warn!(channel = %self.channel, error = %msg, "userinfo URL refused");
            return Err(self.refuse(label, "userinfo_rejected"));
        }
        let response = self
            .http_client
            .get(url)
            .bearer_auth(access_token)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(channel = %self.channel, error = %e, "userinfo fetch failed");
                self.count(label, "userinfo_error");
                OrionError::unavailable(
                    Unavailable::GuardBackend,
                    "the identity provider could not be reached",
                )
            })?;
        if !response.status().is_success() {
            tracing::warn!(
                channel = %self.channel,
                status = %response.status(),
                "userinfo fetch rejected"
            );
            return Err(self.refuse(label, "userinfo_rejected"));
        }
        let body = crate::http_body::read_bounded(response, MAX_USERINFO_BYTES)
            .await
            .map_err(|e| {
                tracing::warn!(channel = %self.channel, error = %e, "userinfo body");
                self.refuse(label, "userinfo_rejected")
            })?;
        serde_json::from_slice(&body).map_err(|e| {
            tracing::warn!(channel = %self.channel, error = %e, "userinfo is not JSON");
            self.refuse(label, "userinfo_rejected")
        })
    }

    async fn exchange(
        &self,
        provider: &CompiledProvider,
        label: &str,
        code: &str,
        pkce_verifier: Option<&str>,
    ) -> Result<crate::connector::oauth::TokenResponse, OrionError> {
        let mut params = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("redirect_uri", provider.redirect_uri.clone()),
        ];
        if let Some(v) = pkce_verifier {
            params.push(("code_verifier", v.to_string()));
        }

        let endpoint = crate::connector::oauth::TokenEndpoint {
            token_url: &provider.token_url,
            client_id: &provider.client_id,
            client_secret: &provider.client_secret,
            client_auth: &provider.client_auth,
        };
        crate::connector::oauth::exchange_code(
            &self.http_client,
            &self.channel,
            endpoint,
            self.allow_private_token_urls,
            params,
        )
        .await
        .map_err(|e| {
            // The taxonomy is already right: a rejected code is the caller's
            // problem and permanent, an unreachable IdP is ours and transient.
            // Only the wire mapping is decided here.
            if e.retryable() {
                tracing::warn!(channel = %self.channel, error = %e, "OAuth2 token exchange failed");
                self.count(label, "exchange_error");
                OrionError::unavailable(
                    Unavailable::GuardBackend,
                    "the identity provider could not be reached",
                )
            } else {
                tracing::warn!(channel = %self.channel, error = %e, "OAuth2 token exchange rejected");
                self.refuse(label, "exchange_rejected")
            }
        })
    }

    // -----------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------

    /// The `return_to` a caller asked for, if the channel accepts one and the
    /// value is on the allow-list.
    ///
    /// Checked on the way **in** rather than on the way out. A value that
    /// reaches the workflow has already passed, so a workflow that redirects to
    /// `metadata.oauth.return_to` cannot be turned into an open redirect by a
    /// crafted sign-in link. A rejected value is dropped silently: a caller
    /// supplied it, and naming the refusal would only tell a probe which
    /// destinations exist.
    ///
    /// The comparison is on **origin and path segments**, not on the text — a
    /// raw `starts_with` is an open redirect (`https://app.example.com` is a
    /// prefix of `https://app.example.com.evil.test/steal`).
    pub fn accepted_return_to(&self, query: &HashMap<String, String>) -> Option<String> {
        let cfg = self.return_to.as_ref()?;
        let value = query.get(&cfg.param)?;
        if value.len() > MAX_RETURN_TO_BYTES {
            return None;
        }
        let candidate = url::Url::parse(value).ok()?;
        cfg.allow_list
            .iter()
            .any(|entry| permits_return_to(entry, &candidate))
            .then(|| value.clone())
    }

    fn state_cookie(&self, value: &str, max_age: i64) -> Result<String, String> {
        let StateCookieConfig {
            ref name,
            secure,
            ref same_site,
            ref path,
            ..
        } = self.state_cookie;
        // Through the shared formatter, so the state cookie gets the same RFC
        // 6265 spelling, `SameSite` canonicalisation and header-injection
        // refusals a workflow-declared cookie does.
        super::cookies::format_set_cookie(&json!({
            "name": name,
            "value": value,
            "path": path,
            "max_age": max_age,
            "same_site": same_site,
            "secure": secure,
            // Never readable from script. The state is a bearer value for the
            // duration of one sign-in.
            "http_only": true,
        }))
    }

    /// The uniform refusal, counted by its real reason and provider.
    fn refuse(&self, provider: &str, outcome: &'static str) -> OrionError {
        self.count(provider, outcome);
        OrionError::Unauthorized("sign-in could not be completed".to_string())
    }

    fn count(&self, provider: &str, outcome: &'static str) {
        crate::metrics::record_oauth_login(
            &self.channel,
            provider,
            Leg::Callback.as_str(),
            outcome,
        );
    }
}

/// `"providers.<slug>."` for a named provider, or `""` for the single-provider
/// (flat) form, so a diagnostic names the field an operator actually wrote.
fn field_prefix(slug: &str) -> String {
    if slug.is_empty() {
        String::new()
    } else {
        format!("providers.{slug}.")
    }
}

/// The redirect URI a provider serves: its own override, or the shared
/// template, with `{provider}` filled in with the slug.
fn effective_redirect_uri(shared: &str, slug: &str, over: Option<&str>) -> String {
    over.unwrap_or(shared).replace("{provider}", slug)
}

/// `"oidc"`, `"oauth2"`, or derived from whether id_token verification applies
/// (an explicit `id_token` block, or one auto-configured from discovery).
/// `validate_shape` has already refused any other explicit spelling.
fn effective_kind(explicit: Option<&str>, has_id_token: bool) -> &'static str {
    match explicit {
        Some("oidc") => "oidc",
        Some("oauth2") => "oauth2",
        _ if has_id_token => "oidc",
        _ => "oauth2",
    }
}

/// Reassemble a resolved config for the serving-mode shape check: the flat form
/// keeps its flat fields, the multi form its map.
fn assemble_resolved(
    o: &OAuth2LoginConfig,
    shared_redirect: String,
    entries: &[(String, ProviderConfig)],
) -> OAuth2LoginConfig {
    let mut base = OAuth2LoginConfig {
        kind: None,
        issuer: None,
        authorize_url: None,
        token_url: None,
        client_id: None,
        client_secret: None,
        client_auth: "basic".to_string(),
        providers: None,
        // The instance merge has already happened — `entries` is the final set —
        // so the resolved config carries them as its own and does not re-merge.
        providers_from_instance: false,
        redirect_uri: shared_redirect,
        callback_path: o.callback_path.clone(),
        scopes: Vec::new(),
        extra_authorize_params: BTreeMap::new(),
        pkce: o.pkce,
        state_secret: o.state_secret.clone(),
        state_cookie: o.state_cookie.clone(),
        run_workflow_on_authorize: o.run_workflow_on_authorize,
        return_to: o.return_to.clone(),
        id_token: None,
        userinfo_url: None,
        identity: None,
    };
    if o.is_multi_provider() {
        base.providers = Some(entries.iter().cloned().collect());
    } else if let Some((_, p)) = entries.first() {
        base.kind = p.kind.clone();
        base.issuer = p.issuer.clone();
        base.authorize_url = p.authorize_url.clone();
        base.token_url = p.token_url.clone();
        base.client_id = p.client_id.clone();
        base.client_secret = p.client_secret.clone();
        base.client_auth = p.client_auth.clone();
        base.scopes = p.scopes.clone();
        base.extra_authorize_params = p.extra_authorize_params.clone();
        base.id_token = p.id_token.clone();
        base.userinfo_url = p.userinfo_url.clone();
        base.identity = p.identity.clone();
    }
    base
}

/// The ceiling on `state_cookie.max_age`, and the reason the two arithmetic
/// sites in `begin` are total.
///
/// Unbounded, a large value overflowed `now + max_age` and wrapped
/// `max_age as i64` negative, which emits `Max-Age=-…` — a directive browsers
/// act on by deleting the cookie immediately. Every sign-in then failed the
/// state check with nothing in the log but a missing cookie.
///
/// A day is far past any consent screen, so the bound refuses nothing an
/// operator meant.
const MAX_STATE_COOKIE_MAX_AGE_SECS: u64 = 86_400;

mod validation;
pub use validation::*;

#[cfg(test)]
mod tests;
