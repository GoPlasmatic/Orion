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
    IdTokenConfig, OAuth2LoginConfig, ProviderConfig, RESERVED_AUTHORIZE_PARAMS, ReturnToConfig,
    StateCookieConfig,
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
    pub instance_providers: &'a std::collections::BTreeMap<String, crate::config::InstanceProviderConfig>,
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
}

impl CompiledProvider {
    fn wants_oidc_nonce(&self) -> bool {
        self.id_token.as_ref().is_some_and(|id| id.nonce)
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
                authorize_url: resolve_opt(&p.authorize_url, &format!("oauth2_login.{pfx}authorize_url"))
                    .await?,
                token_url: resolve_opt(&p.token_url, &format!("oauth2_login.{pfx}token_url")).await?,
                client_id: resolve_opt(&p.client_id, &format!("oauth2_login.{pfx}client_id")).await?,
                client_secret: resolve_opt(
                    &p.client_secret,
                    &format!("oauth2_login.{pfx}client_secret"),
                )
                .await?,
                client_auth: p.client_auth.clone(),
                redirect_uri: resolve_opt(&p.redirect_uri, &format!("oauth2_login.{pfx}redirect_uri"))
                    .await?,
                scopes: p.scopes.clone(),
                extra_authorize_params: p.extra_authorize_params.clone(),
                id_token: p.id_token.clone(),
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
                Some(issuer) => Some(deps.discovery.resolve(issuer).await.map_err(|e| {
                    format!("oauth2_login.{}issuer: {e}", field_prefix(slug))
                })?),
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
                        Leg::Authorize,
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
        let token = crate::jwt::sign(STATE_ALG, &self.state_key, None, &claims).map_err(|e| {
            OrionError::internal(format!("could not sign the OAuth2 state: {e}"))
        })?;

        crate::metrics::record_oauth_login(
            &self.channel,
            provider_label(canonical_slug),
            Leg::Authorize,
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
        let claims = self.state_verifier.verify(&cookie).await.map_err(|reason| {
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
        if self.route_selected {
            oauth["provider"] = json!(canonical_slug);
            oauth["kind"] = json!(provider.kind);
        }

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

        crate::metrics::record_oauth_login(&self.channel, label, Leg::Callback, "ok");
        Ok(Grant {
            metadata: oauth,
            // Retire the state the moment it is spent. `Max-Age=0` with the
            // same name, path and attributes, or the browser keeps the old one
            // alongside.
            clear_cookie: self.state_cookie("", 0).map_err(|e| {
                OrionError::internal(format!("could not clear the state cookie: {e}"))
            })?,
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
        crate::metrics::record_oauth_login(&self.channel, provider, Leg::Callback, outcome);
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

/// Where [`validate_shape`] runs, which decides what a reference string means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeCheck {
    /// `POST /channels`, `orion-server lint`, `package lint`: the deployment's
    /// values are not at hand, so a reference is accepted where a value would
    /// be checked. It is checked at load, against the instance that resolves
    /// it — by this same function, in the other mode.
    Authoring,
    /// [`CompiledOAuth2Login::compile`], on the block the channel will serve
    /// with. Every `var://` was substituted before the block was typed and
    /// [`SECRET_RESOLVED_FIELDS`] were resolved, so a reference still present
    /// is one nothing resolves — refused, or its text would reach the provider.
    Serving,
}

/// The fields `compile` hands to the secret resolver. A `var://` may sit in any
/// field of the block — the loader substitutes it into the JSON before typing —
/// but `env://` and the vault schemes resolve field by field, and only in these
/// (matched by their final segment, so a per-provider `providers.<slug>.client_id`
/// counts the same as a flat `client_id`).
pub const SECRET_RESOLVED_FIELDS: &[&str] = &[
    "client_id",
    "client_secret",
    "state_secret",
    "issuer",
    "authorize_url",
    "token_url",
    "redirect_uri",
];

/// Whether `value` is a reference rather than a value.
///
/// `Ok(true)`: deferred — the load path resolves it and checks the result.
/// `Ok(false)`: a value; check it. `Err`: a reference nothing resolves in this
/// field, or one that survived resolution.
///
/// `field` may be prefixed (`providers.iitm.client_id`); the resolvable-field
/// test is on its final segment.
fn deferred(mode: ShapeCheck, field: &str, value: &str) -> Result<bool, String> {
    let is_var = value.starts_with(crate::config::vars::VAR_SCHEME);
    if !is_var && !crate::connector::secrets::is_resolvable_reference(value) {
        return Ok(false);
    }
    let bare = field.rsplit('.').next().unwrap_or(field);
    match mode {
        ShapeCheck::Serving => Err(format!(
            "oauth2_login.{field} still holds '{value}' after resolution; nothing resolves a \
             reference in this field, so its text would reach the identity provider"
        )),
        ShapeCheck::Authoring if is_var || SECRET_RESOLVED_FIELDS.contains(&bare) => Ok(true),
        ShapeCheck::Authoring => Err(format!(
            "oauth2_login.{field} holds '{value}', but a secret reference is resolved only in \
             {}; for a per-environment value here use var://name",
            SECRET_RESOLVED_FIELDS.join(", ")
        )),
    }
}

/// Everything about the block that can be judged without resolving a secret.
///
/// Shared with `validation::channels` so a create or update answers `400`
/// naming the field, rather than storing a definition that quarantines its
/// channel at the next reload. Secret *resolution* deliberately stays in
/// [`CompiledOAuth2Login::compile`]: a bundle has to validate on a host that
/// holds none of the production secrets.
pub fn validate_shape(cfg: &OAuth2LoginConfig, mode: ShapeCheck) -> Result<(), String> {
    validate_shared(cfg, mode)?;
    let entries = cfg.provider_entries();
    // A block that declares none of its own is only empty legitimately when it
    // relies on the deployment (`providers_from_instance`); the serving-mode
    // check runs after the merge, so a block that ends up with none is refused
    // there.
    if cfg.is_multi_provider() && entries.is_empty() && !cfg.providers_from_instance {
        return Err("oauth2_login.providers must name at least one provider".to_string());
    }
    for (slug, provider) in &entries {
        validate_provider(cfg, slug, provider, mode)?;
    }
    Ok(())
}

/// The effective providers a block serves: the ones it declares, plus (when it
/// opts in with `providers_from_instance`) the deployment's, with the
/// definition's own entries winning any slug clash. The single merge point, so
/// nothing downstream learns a provider's source.
fn merge_instance_providers(
    cfg: &OAuth2LoginConfig,
    instance: &BTreeMap<String, crate::config::InstanceProviderConfig>,
) -> Vec<(String, ProviderConfig)> {
    let mut entries = cfg.provider_entries();
    if cfg.providers_from_instance {
        let declared: std::collections::HashSet<&str> =
            entries.iter().map(|(s, _)| s.as_str()).collect();
        let extra: Vec<(String, ProviderConfig)> = instance
            .iter()
            .filter(|(slug, _)| !declared.contains(slug.as_str()))
            .map(|(slug, p)| (slug.clone(), ProviderConfig::from(p)))
            .collect();
        entries.extend(extra);
    }
    entries
}

/// The fields shared by every provider.
fn validate_shared(cfg: &OAuth2LoginConfig, mode: ShapeCheck) -> Result<(), String> {
    let multi = cfg.is_multi_provider();

    // The two forms are mutually exclusive: a `providers` map alongside the flat
    // per-provider fields is ambiguous about which one serves.
    if multi {
        let flat_set = cfg.kind.is_some()
            || cfg.authorize_url.is_some()
            || cfg.token_url.is_some()
            || cfg.client_id.is_some()
            || cfg.client_secret.is_some()
            || cfg.id_token.is_some()
            || !cfg.scopes.is_empty()
            || !cfg.extra_authorize_params.is_empty();
        if flat_set {
            return Err(
                "oauth2_login sets both `providers` and the flat provider fields \
                 (authorize_url, client_id, …); use one form or the other — the per-provider \
                 fields belong inside each `providers` entry"
                    .to_string(),
            );
        }
    }

    // callback_path: absolute, and its parameter shape must match the form.
    if !deferred(mode, "callback_path", &cfg.callback_path)? {
        if cfg.callback_path.trim().is_empty() || !cfg.callback_path.starts_with('/') {
            return Err("oauth2_login.callback_path must be an absolute path, e.g. \
                        /v1/auth/github/callback"
                .to_string());
        }
        validate_provider_param("callback_path", &cfg.callback_path, multi)?;
    }

    // redirect_uri: with `providers`, it is a template and must carry
    // `{provider}` (a provider may still override it, but the shared value is
    // what fills in for the rest); without, it is a static URL.
    if !deferred(mode, "redirect_uri", &cfg.redirect_uri)? {
        if multi {
            if !cfg.redirect_uri.contains("{provider}") {
                return Err(
                    "oauth2_login.redirect_uri is a template when `providers` is set and must \
                     contain {provider}, e.g. https://app.example.com/v1/auth/{provider}/callback"
                        .to_string(),
                );
            }
        } else if cfg.redirect_uri.contains('{') {
            return Err(format!(
                "oauth2_login.redirect_uri '{}' carries a path parameter, but this block names a \
                 single provider; only a `providers` block substitutes {{provider}}",
                cfg.redirect_uri
            ));
        }
    }

    if cfg.state_cookie.max_age == 0 {
        return Err(
            "oauth2_login.state_cookie.max_age must be greater than zero — it is also the state \
             token's expiry"
                .to_string(),
        );
    }
    if cfg.state_cookie.max_age > MAX_STATE_COOKIE_MAX_AGE_SECS {
        return Err(format!(
            "oauth2_login.state_cookie.max_age is {} seconds, above the {MAX_STATE_COOKIE_MAX_AGE_SECS} \
             second ceiling ({} days). It is the window to finish one consent screen, not a \
             session lifetime, and a long one keeps a replayable state token valid for as long \
             as it lasts",
            cfg.state_cookie.max_age,
            MAX_STATE_COOKIE_MAX_AGE_SECS / 86_400,
        ));
    }
    if !deferred(mode, "state_cookie.same_site", &cfg.state_cookie.same_site)? {
        match cfg.state_cookie.same_site.to_ascii_lowercase().as_str() {
            "lax" | "none" => {}
            // The callback is a top-level cross-site GET from the IdP, and a
            // `Strict` cookie is withheld on exactly that request.
            "strict" => {
                return Err(
                    "oauth2_login.state_cookie.same_site = \"strict\" would withhold the \
                    cookie on the callback, which is a top-level cross-site GET from the \
                    identity provider — every sign-in would fail the state check. Use \
                    \"lax\"."
                        .to_string(),
                );
            }
            _ => {
                return Err(format!(
                    "oauth2_login.state_cookie.same_site '{}' is not valid — Lax or None",
                    cfg.state_cookie.same_site
                ));
            }
        }
    }

    if let Some(ref rt) = cfg.return_to {
        if rt.param.trim().is_empty() {
            return Err("oauth2_login.return_to.param must not be empty".to_string());
        }
        if rt.allow_list.is_empty() {
            return Err(
                "oauth2_login.return_to.allow_list must list at least one permitted \
                        destination prefix — an empty list accepts nothing, so omit the \
                        whole block instead"
                    .to_string(),
            );
        }
        for prefix in &rt.allow_list {
            if !deferred(mode, "return_to.allow_list", prefix)? {
                require_https("return_to.allow_list entry", prefix)?;
            }
        }
    }
    Ok(())
}

/// A `callback_path`/`route_pattern` must carry exactly one `{provider}` segment
/// when the block is multi-provider, and no path parameter at all when it is not.
fn validate_provider_param(field: &str, path: &str, multi: bool) -> Result<(), String> {
    let params = crate::channel::routing::route_param_names(path);
    if multi {
        if params != ["provider"] {
            return Err(format!(
                "oauth2_login.{field} '{path}' must carry exactly one {{provider}} segment when \
                 `providers` is set, and no other path parameter"
            ));
        }
    } else if !params.is_empty() {
        return Err(format!(
            "oauth2_login.{field} '{path}' carries a path parameter; a single-provider callback \
             is a fixed URL registered with the identity provider, so it must be static"
        ));
    }
    Ok(())
}

/// One provider's fields. `slug` is the map key (or `""` for the flat form),
/// used to prefix diagnostics and to fill `{provider}` in the redirect.
fn validate_provider(
    cfg: &OAuth2LoginConfig,
    slug: &str,
    p: &ProviderConfig,
    mode: ShapeCheck,
) -> Result<(), String> {
    let pfx = field_prefix(slug);

    if let Some(kind) = p.kind.as_deref() {
        match kind {
            "oidc" | "oauth2" => {}
            other => {
                return Err(format!(
                    "oauth2_login.{pfx}kind '{other}' is not supported — expected oidc or oauth2"
                ));
            }
        }
    }

    // With an `issuer`, the endpoints are discovered at load, so they may be
    // omitted here; an explicit one still has to be https. Without an issuer,
    // both are required — an authorize/token URL has to come from somewhere.
    let discovers = p.issuer.as_deref().is_some_and(|s| !s.trim().is_empty());
    if let Some(issuer) = p.issuer.as_deref() {
        let field = format!("{pfx}issuer");
        if !deferred(mode, &field, issuer)? {
            require_https(&field, issuer)?;
        }
    }
    for (name, value) in [
        ("authorize_url", &p.authorize_url),
        ("token_url", &p.token_url),
    ] {
        let field = format!("{pfx}{name}");
        match value.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(present) => {
                if !deferred(mode, &field, present)? {
                    require_https(&field, present)?;
                }
            }
            None if discovers => {}
            None => return Err(format!("oauth2_login.{field} is required")),
        }
    }
    for name in ["client_id", "client_secret"] {
        let value = if name == "client_id" {
            &p.client_id
        } else {
            &p.client_secret
        };
        if value
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none()
        {
            return Err(format!("oauth2_login.{pfx}{name} is required"));
        }
    }

    let redirect = effective_redirect_uri(&cfg.redirect_uri, slug, p.redirect_uri.as_deref());
    let redirect_field = format!("{pfx}redirect_uri");
    if !deferred(mode, &redirect_field, &redirect)? {
        require_https(&redirect_field, &redirect)?;
    }

    if !deferred(mode, &format!("{pfx}client_auth"), &p.client_auth)?
        && crate::connector::OAuth2ClientAuth::parse(&p.client_auth).is_none()
    {
        return Err(format!(
            "oauth2_login.{pfx}client_auth '{}' is not supported — expected {}",
            p.client_auth,
            crate::connector::OAuth2ClientAuth::VALUES
        ));
    }

    for name in p.extra_authorize_params.keys() {
        if RESERVED_AUTHORIZE_PARAMS.contains(&name.as_str()) {
            return Err(format!(
                "oauth2_login.{pfx}extra_authorize_params sets '{name}', which Orion owns. \
                 Overriding it would disable the protection it carries — the reserved \
                 set is: {}",
                RESERVED_AUTHORIZE_PARAMS.join(", ")
            ));
        }
    }

    if let Some(ref id) = p.id_token {
        if !deferred(mode, &format!("{pfx}id_token.jwks_url"), &id.jwks_url)? {
            crate::jwt::validate_jwks_url(&id.jwks_url)
                .map_err(|e| format!("oauth2_login.{pfx}id_token.jwks_url: {e}"))?;
        }
        if id.issuer.is_empty() {
            return Err(format!(
                "oauth2_login.{pfx}id_token.issuer must list at least one accepted issuer — an \
                 unchecked `iss` accepts a token from any provider whose key happens to be in \
                 the JWKS"
            ));
        }
        if id.algorithms.is_empty() {
            return Err(format!(
                "oauth2_login.{pfx}id_token.algorithms must not be empty"
            ));
        }
        for alg in &id.algorithms {
            if !deferred(mode, &format!("{pfx}id_token.algorithms"), alg)? {
                crate::jwt::parse_algorithm(alg)
                    .map_err(|e| format!("oauth2_login.{pfx}id_token.algorithms: {e}"))?;
            }
        }
    }
    Ok(())
}

/// `https`, or `http` on a loopback host.
///
/// The carve-out is the rule browsers already use for secure contexts: an IdP
/// will not issue a certificate for your laptop, and GitHub, Google and Entra
/// all accept a plain-`http` loopback redirect URI (RFC 8252 §7.3). It grants
/// nothing on its own — `token_url` still passes the private-address check at
/// every exchange unless the operator sets `allow_private_token_urls`.
fn require_https(field: &str, value: &str) -> Result<(), String> {
    let url = url::Url::parse(value)
        .map_err(|e| format!("oauth2_login.{field} '{value}' is not a URL: {e}"))?;
    if url.scheme() == "https" {
        return Ok(());
    }
    if url.scheme() == "http" && is_loopback_host(&url) {
        return Ok(());
    }
    Err(format!(
        "oauth2_login.{field} must be https — '{value}' is {}. The client secret, the \
         authorization code and the session that follows all travel over it. Plain http \
         is accepted only on a loopback host (localhost, 127.0.0.1, [::1]) for local \
         development",
        url.scheme()
    ))
}

/// Whether a URL's host is the local machine, by literal address or by the one
/// name that is reserved for it. Name resolution is deliberately not consulted:
/// this runs against a definition that will be promoted to other instances.
fn is_loopback_host(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        // RFC 6761 §6.3 reserves `localhost` and its subdomains for loopback.
        Some(url::Host::Domain(host)) => host == "localhost" || host.ends_with(".localhost"),
        None => false,
    }
}

/// Whether one allow-list entry admits a candidate `return_to` — same origin and
/// a path-segment prefix, so `app.example.com.evil.test` and `/application` are
/// both refused.
fn permits_return_to(entry: &str, candidate: &url::Url) -> bool {
    let Ok(allowed) = url::Url::parse(entry) else {
        return false;
    };
    if allowed.origin() != candidate.origin() {
        return false;
    }
    let (allowed_path, candidate_path) = (allowed.path(), candidate.path());
    if let Some(base) = allowed_path.strip_suffix('/') {
        return candidate_path == base || candidate_path.starts_with(allowed_path);
    }
    candidate_path == allowed_path || candidate_path.starts_with(&format!("{allowed_path}/"))
}

fn build_provider(
    slug: &str,
    resolved: &ProviderConfig,
    shared_redirect: &str,
    discovered: Option<&crate::channel::oidc_discovery::Discovered>,
    deps: &LoginDeps<'_>,
) -> Result<CompiledProvider, String> {
    let pfx = field_prefix(slug);
    // Explicit endpoints win; discovery fills whatever the block left out.
    let pick = |explicit: &Option<String>, from_discovery: Option<&str>, name: &str| {
        explicit
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| from_discovery.map(str::to_string))
            .ok_or_else(|| format!("oauth2_login.{pfx}{name} is required"))
    };
    let client_id = resolved
        .client_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("oauth2_login.{pfx}client_id is required"))?;
    let client_secret = resolved
        .client_secret
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("oauth2_login.{pfx}client_secret is required"))?;
    let authorize_url = pick(
        &resolved.authorize_url,
        discovered.map(|d| d.authorize_url.as_str()),
        "authorize_url",
    )?;
    let token_url = pick(
        &resolved.token_url,
        discovered.map(|d| d.token_url.as_str()),
        "token_url",
    )?;
    let redirect_uri = effective_redirect_uri(shared_redirect, slug, resolved.redirect_uri.as_deref());

    // OIDC id_token verification: an explicit `id_token` block wins; otherwise
    // discovery auto-configures one, so naming an `issuer` alone means "verify
    // the id_token" against the discovered keys — no block to hand-write.
    let id_token: Option<IdTokenConfig> = match (&resolved.id_token, discovered) {
        (Some(id), _) => Some(id.clone()),
        (None, Some(d)) => Some(IdTokenConfig {
            required: true,
            issuer: vec![d.issuer.clone()],
            audience: None,
            jwks_url: d.jwks_url.clone(),
            algorithms: vec!["RS256".to_string()],
            nonce: true,
        }),
        (None, None) => None,
    };
    let id_token_verifier = match id_token {
        Some(ref id) => Some(build_id_token_verifier(&pfx, id, &client_id, deps)?),
        None => None,
    };

    Ok(CompiledProvider {
        kind: effective_kind(resolved.kind.as_deref(), id_token.is_some()),
        client_id,
        client_secret,
        authorize_url,
        token_url,
        redirect_uri,
        client_auth: resolved.client_auth.clone(),
        scopes: resolved.scopes.clone(),
        extra_authorize_params: resolved.extra_authorize_params.clone(),
        id_token,
        id_token_verifier,
    })
}

fn build_id_token_verifier(
    pfx: &str,
    cfg: &IdTokenConfig,
    client_id: &str,
    deps: &LoginDeps<'_>,
) -> Result<crate::jwt::Verifier, String> {
    let algorithms = cfg
        .algorithms
        .iter()
        .map(|a| crate::jwt::parse_algorithm(a))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("oauth2_login.{pfx}id_token.algorithms: {e}"))?;

    Ok(crate::jwt::Verifier {
        static_keys: Vec::new(),
        jwks: Some(crate::jwt::JwksSource {
            url: cfg.jwks_url.clone(),
            cache: std::sync::Arc::clone(deps.jwks),
        }),
        algorithms,
        issuer: cfg.issuer.clone(),
        // OIDC Core §3.1.3.7: the audience of an id_token from the
        // authorization-code flow is the client that asked for it.
        audience: cfg
            .audience
            .clone()
            .unwrap_or_else(|| vec![client_id.to_string()]),
        leeway_secs: crate::jwt::DEFAULT_LEEWAY_SECS,
        require_exp: true,
        max_token_bytes: crate::jwt::DEFAULT_MAX_TOKEN_BYTES,
        validations: std::sync::OnceLock::new(),
    })
}

/// The same resolver `auth.secret` and `auth.jwt_keys[].key` use, so an operator
/// has one mechanism rather than one per block.
async fn resolve_secret(value: &str, field: &str) -> Result<String, String> {
    let resolved = crate::connector::secrets::resolve_secret_string(value, field).await?;
    if resolved.is_empty() {
        return Err(format!("{field} resolved to an empty value"));
    }
    Ok(resolved)
}

/// [`resolve_secret`] for an optional field: `None` stays `None`.
async fn resolve_opt(value: &Option<String>, field: &str) -> Result<Option<String>, String> {
    match value {
        Some(v) => Ok(Some(resolve_secret(v, field).await?)),
        None => Ok(None),
    }
}

/// 32 CSPRNG bytes, base64url-unpadded — safe in a query string and in a JWT
/// claim without further escaping.
fn random_nonce() -> String {
    crate::crypto::encode_bytes(
        crate::crypto::Codec::Base64Url,
        &crate::crypto::random_bytes(NONCE_BYTES),
    )
}

/// RFC 7636 §4.2: `BASE64URL-NOPAD(SHA256(ASCII(verifier)))`.
fn pkce_challenge(verifier: &str) -> String {
    use sha2::Digest as _;
    crate::crypto::encode_bytes(
        crate::crypto::Codec::Base64Url,
        &sha2::Sha256::digest(verifier.as_bytes()),
    )
}

/// Constant-time comparison of two nonces, via SHA-256 so the shared helper's
/// fixed width applies and the inputs' length is not a signal.
fn secret_eq(a: &str, b: &str) -> bool {
    use sha2::Digest as _;
    let da: [u8; 32] = sha2::Sha256::digest(a.as_bytes()).into();
    let db: [u8; 32] = sha2::Sha256::digest(b.as_bytes()).into();
    crate::config::constant_time_eq(&da, &db)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::config::{OAuth2LoginConfig, ProviderConfig, StateCookieConfig};

    /// 32 bytes, so `encoding_key` accepts it as an HS256 secret.
    const STATE_SECRET: &str = "0123456789abcdef0123456789abcdef";

    /// A single-provider (flat) block.
    fn config() -> OAuth2LoginConfig {
        OAuth2LoginConfig {
            kind: None,
            issuer: None,
            authorize_url: Some("https://idp.example.com/authorize".to_string()),
            token_url: Some("https://idp.example.com/token".to_string()),
            client_id: Some("client-123".to_string()),
            client_secret: Some("shhh".to_string()),
            client_auth: "basic".to_string(),
            providers: None,
            providers_from_instance: false,
            redirect_uri: "https://app.example.com/v1/auth/idp/callback".to_string(),
            callback_path: "/v1/auth/idp/callback".to_string(),
            scopes: vec!["read:user".to_string()],
            extra_authorize_params: Default::default(),
            pkce: true,
            state_secret: STATE_SECRET.to_string(),
            state_cookie: StateCookieConfig::default(),
            run_workflow_on_authorize: false,
            return_to: None,
            id_token: None,
        }
    }

    /// A two-provider block selected by `{provider}`.
    fn multi_config() -> OAuth2LoginConfig {
        let github = ProviderConfig {
            authorize_url: Some("https://github.com/login/oauth/authorize".to_string()),
            token_url: Some("https://github.com/login/oauth/access_token".to_string()),
            client_id: Some("gh-client".to_string()),
            client_secret: Some("gh-secret".to_string()),
            client_auth: "body".to_string(),
            scopes: vec!["read:user".to_string()],
            ..Default::default()
        };
        let acme = ProviderConfig {
            authorize_url: Some("https://acme.example.com/authorize".to_string()),
            token_url: Some("https://acme.example.com/token".to_string()),
            client_id: Some("acme-client".to_string()),
            client_secret: Some("acme-secret".to_string()),
            client_auth: "basic".to_string(),
            scopes: vec!["openid".to_string(), "profile".to_string()],
            ..Default::default()
        };
        OAuth2LoginConfig {
            kind: None,
            issuer: None,
            authorize_url: None,
            token_url: None,
            client_id: None,
            client_secret: None,
            client_auth: "basic".to_string(),
            providers: Some(BTreeMap::from([
                ("github".to_string(), github),
                ("acme".to_string(), acme),
            ])),
            providers_from_instance: false,
            redirect_uri: "https://app.example.com/v1/auth/{provider}/callback".to_string(),
            callback_path: "/v1/auth/{provider}/callback".to_string(),
            scopes: Vec::new(),
            extra_authorize_params: Default::default(),
            pkce: true,
            state_secret: STATE_SECRET.to_string(),
            state_cookie: StateCookieConfig::default(),
            run_workflow_on_authorize: false,
            return_to: None,
            id_token: None,
        }
    }

    /// No deployment-supplied providers, for the unit tests.
    fn no_instance() -> &'static BTreeMap<String, crate::config::InstanceProviderConfig> {
        static EMPTY: std::sync::LazyLock<BTreeMap<String, crate::config::InstanceProviderConfig>> =
            std::sync::LazyLock::new(BTreeMap::new);
        &EMPTY
    }

    /// A discovery cache the unit tests never reach over the network (no provider
    /// here names an `issuer`).
    fn no_discovery() -> &'static std::sync::Arc<crate::channel::oidc_discovery::DiscoveryCache> {
        static CACHE: std::sync::LazyLock<
            std::sync::Arc<crate::channel::oidc_discovery::DiscoveryCache>,
        > = std::sync::LazyLock::new(|| {
            std::sync::Arc::new(crate::channel::oidc_discovery::DiscoveryCache::new(
                reqwest::Client::new(),
                false,
            ))
        });
        &CACHE
    }

    /// A single deployment-supplied provider keyed `iitm`, for the merge tests.
    fn instance_with_iitm() -> BTreeMap<String, crate::config::InstanceProviderConfig> {
        BTreeMap::from([(
            "iitm".to_string(),
            crate::config::InstanceProviderConfig {
                authorize_url: Some("https://login.iitm.example/authorize".to_string()),
                token_url: Some("https://login.iitm.example/token".to_string()),
                client_id: Some("iitm-client".to_string()),
                client_secret: Some("iitm-secret".to_string()),
                scopes: vec!["openid".to_string()],
                ..Default::default()
            },
        )])
    }

    async fn compile_with_instance(
        cfg: &OAuth2LoginConfig,
        instance: &BTreeMap<String, crate::config::InstanceProviderConfig>,
    ) -> Result<CompiledOAuth2Login, String> {
        let jwks = std::sync::Arc::new(crate::jwt::jwks::JwksCache::new(
            reqwest::Client::new(),
            false,
        ));
        CompiledOAuth2Login::compile(
            cfg,
            "signin",
            &LoginDeps {
                http_client: &reqwest::Client::new(),
                jwks: &jwks,
                allow_private_token_urls: false,
                instance_providers: instance,
                discovery: no_discovery(),
            },
        )
        .await
    }

    async fn compiled(cfg: &OAuth2LoginConfig) -> CompiledOAuth2Login {
        let jwks = std::sync::Arc::new(crate::jwt::jwks::JwksCache::new(
            reqwest::Client::new(),
            false,
        ));
        CompiledOAuth2Login::compile(
            cfg,
            "signin",
            &LoginDeps {
                http_client: &reqwest::Client::new(),
                jwks: &jwks,
                allow_private_token_urls: false,
                instance_providers: no_instance(),
                discovery: no_discovery(),
            },
        )
        .await
        .expect("compiles")
    }

    async fn try_compile(cfg: &OAuth2LoginConfig) -> Result<CompiledOAuth2Login, String> {
        let jwks = std::sync::Arc::new(crate::jwt::jwks::JwksCache::new(
            reqwest::Client::new(),
            false,
        ));
        CompiledOAuth2Login::compile(
            cfg,
            "signin",
            &LoginDeps {
                http_client: &reqwest::Client::new(),
                jwks: &jwks,
                allow_private_token_urls: false,
                instance_providers: no_instance(),
                discovery: no_discovery(),
            },
        )
        .await
    }

    fn params(location: &str) -> HashMap<String, String> {
        url::Url::parse(location)
            .expect("a URL")
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    fn cookie_value(set_cookie: &str) -> String {
        set_cookie
            .split(';')
            .next()
            .and_then(|p| p.split_once('='))
            .map(|(_, v)| v.to_string())
            .expect("a cookie value")
    }

    #[tokio::test]
    async fn the_authorize_url_carries_what_the_rfc_requires() {
        let login = compiled(&config()).await;
        let redirect = login.begin(None, None, None).expect("a redirect");
        let q = params(&redirect.location);

        assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(q.get("client_id").map(String::as_str), Some("client-123"));
        assert_eq!(
            q.get("redirect_uri").map(String::as_str),
            Some("https://app.example.com/v1/auth/idp/callback")
        );
        assert_eq!(q.get("scope").map(String::as_str), Some("read:user"));
        assert!(q.contains_key("state"));
        assert_eq!(
            q.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );

        assert!(redirect.set_cookie.contains("HttpOnly"));
        assert!(redirect.set_cookie.contains("Secure"));
        assert!(redirect.set_cookie.contains("SameSite=Lax"));
        assert!(redirect.set_cookie.contains("Max-Age=600"));
    }

    /// The `{provider}` slug picks the entry: two providers on one block send
    /// the browser to two different IdPs with two different clients, and the
    /// slug fills in the redirect URI template.
    #[tokio::test]
    async fn a_slug_selects_its_provider_on_the_authorize_leg() {
        let login = compiled(&multi_config()).await;

        let gh = params(&login.begin(Some("github"), None, None).expect("gh").location);
        assert_eq!(gh.get("client_id").map(String::as_str), Some("gh-client"));
        assert_eq!(
            gh.get("redirect_uri").map(String::as_str),
            Some("https://app.example.com/v1/auth/github/callback")
        );

        let acme = params(&login.begin(Some("acme"), None, None).expect("acme").location);
        assert_eq!(acme.get("client_id").map(String::as_str), Some("acme-client"));
        assert_eq!(
            acme.get("redirect_uri").map(String::as_str),
            Some("https://app.example.com/v1/auth/acme/callback")
        );
        assert_eq!(acme.get("scope").map(String::as_str), Some("openid profile"));
    }

    /// An unknown slug is a 404 on both legs — before the exchange, before any
    /// workflow.
    #[tokio::test]
    async fn an_unknown_slug_is_not_found() {
        let login = compiled(&multi_config()).await;
        let err = login.begin(Some("nope"), None, None).expect_err("must 404");
        assert!(matches!(err, OrionError::NotFound(_)), "{err:?}");
        assert!(login.require_provider(Some("nope")).is_err());
        assert!(login.require_provider(Some("github")).is_ok());
    }

    /// The slug is sealed into the signed state, so a state minted for one
    /// provider cannot be redeemed at another's callback URL.
    #[tokio::test]
    async fn the_state_seals_the_provider_and_a_mismatch_is_refused() {
        let login = compiled(&multi_config()).await;
        let redirect = login.begin(Some("github"), None, None).expect("a redirect");
        let state = params(&redirect.location)
            .get("state")
            .expect("a state")
            .clone();
        let jar = redirect
            .set_cookie
            .split(';')
            .next()
            .expect("a cookie pair")
            .to_string();

        let query = HashMap::from([
            ("state".to_string(), state),
            ("code".to_string(), "whatever".to_string()),
        ]);
        // Presented at `acme`'s callback with `github`'s state.
        let err = login
            .complete(Some("acme"), &query, &[jar.as_str()])
            .await
            .expect_err("must refuse");
        assert!(matches!(err, OrionError::Unauthorized(_)), "{err:?}");
    }

    /// #307's second trap: two sign-ins in the same second get different states.
    #[tokio::test]
    async fn two_sign_ins_in_one_second_get_different_states() {
        let login = compiled(&config()).await;
        let a = login.begin(None, None, None).expect("a redirect");
        let b = login.begin(None, None, None).expect("a redirect");

        assert_ne!(
            params(&a.location).get("state"),
            params(&b.location).get("state")
        );
        assert_ne!(
            params(&a.location).get("code_challenge"),
            params(&b.location).get("code_challenge")
        );
        assert_ne!(a.set_cookie, b.set_cookie);
    }

    /// RFC 7636 Appendix B's published vector.
    #[test]
    fn the_pkce_challenge_matches_the_rfc_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    /// Every property of a `Verifier` is what stops a forged state being taken.
    #[tokio::test]
    async fn a_state_token_from_another_key_is_rejected() {
        let login = compiled(&config()).await;
        let mut other = config();
        other.state_secret = "fedcba9876543210fedcba9876543210".to_string();
        let attacker = compiled(&other).await;

        let forged = attacker.begin(None, None, None).expect("a redirect");
        let cookie = cookie_value(&forged.set_cookie);

        let claims = login.state_verifier.verify(&cookie).await;
        assert!(
            claims.is_err(),
            "a state signed with another key must not verify"
        );
    }

    /// A callback that presents one half of the binding without the other is the
    /// login-CSRF the parameter exists to prevent.
    #[tokio::test]
    async fn a_callback_without_the_cookie_is_refused_before_the_exchange() {
        let login = compiled(&config()).await;
        let redirect = login.begin(None, None, None).expect("a redirect");
        let state = params(&redirect.location)
            .get("state")
            .expect("a state")
            .clone();

        let query = HashMap::from([
            ("state".to_string(), state),
            ("code".to_string(), "whatever".to_string()),
        ]);
        let err = login
            .complete(None, &query, &[])
            .await
            .expect_err("must refuse");
        assert!(
            matches!(err, OrionError::Unauthorized(_)),
            "expected a 401, got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_state_that_does_not_match_the_cookie_is_refused() {
        let login = compiled(&config()).await;
        let redirect = login.begin(None, None, None).expect("a redirect");
        let jar = redirect
            .set_cookie
            .split(';')
            .next()
            .expect("a cookie pair")
            .to_string();

        let query = HashMap::from([
            ("state".to_string(), "not-the-minted-one".to_string()),
            ("code".to_string(), "whatever".to_string()),
        ]);
        let err = login
            .complete(None, &query, &[jar.as_str()])
            .await
            .expect_err("must refuse");
        assert!(matches!(err, OrionError::Unauthorized(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_provider_error_is_refused_without_looking_at_the_state() {
        let login = compiled(&config()).await;
        let query = HashMap::from([("error".to_string(), "access_denied".to_string())]);
        let err = login
            .complete(None, &query, &[])
            .await
            .expect_err("must refuse");
        assert!(matches!(err, OrionError::Unauthorized(_)), "{err:?}");
    }

    #[test]
    fn a_reserved_authorize_parameter_is_refused_at_the_door() {
        let mut cfg = config();
        cfg.extra_authorize_params
            .insert("state".to_string(), "attacker-chosen".to_string());
        let err = validate_shape(&cfg, ShapeCheck::Authoring).expect_err("must refuse");
        assert!(err.contains("state"), "{err}");
    }

    #[tokio::test]
    async fn a_workflow_cannot_contribute_a_reserved_parameter() {
        let mut cfg = config();
        cfg.run_workflow_on_authorize = true;
        let login = compiled(&cfg).await;
        let contributed = json!({
            "extra_params": { "state": "attacker-chosen", "login_hint": "a@b.com" }
        });
        let redirect = login
            .begin(None, Some(&contributed), None)
            .expect("a redirect");
        let q = params(&redirect.location);
        assert_eq!(q.get("login_hint").map(String::as_str), Some("a@b.com"));
        assert_ne!(q.get("state").map(String::as_str), Some("attacker-chosen"));
    }

    #[test]
    fn http_endpoints_are_refused() {
        for field in ["authorize_url", "token_url", "redirect_uri"] {
            let mut cfg = config();
            let value = "http://idp.example.com/x".to_string();
            match field {
                "authorize_url" => cfg.authorize_url = Some(value),
                "token_url" => cfg.token_url = Some(value),
                _ => cfg.redirect_uri = value,
            }
            let err = validate_shape(&cfg, ShapeCheck::Authoring).expect_err(field);
            assert!(err.contains("https"), "{field}: {err}");
        }
    }

    #[test]
    fn plain_http_is_accepted_only_on_loopback() {
        for host in ["localhost", "127.0.0.1", "[::1]", "app.localhost"] {
            let mut cfg = config();
            cfg.token_url = Some(format!("http://{host}:8080/token"));
            assert!(
                validate_shape(&cfg, ShapeCheck::Authoring).is_ok(),
                "{host} should be accepted"
            );
        }
        for host in ["localhost.evil.test", "127.0.0.1.evil.test", "10.0.0.1"] {
            let mut cfg = config();
            cfg.token_url = Some(format!("http://{host}/token"));
            assert!(
                validate_shape(&cfg, ShapeCheck::Authoring).is_err(),
                "{host} should be refused"
            );
        }
    }

    #[test]
    fn a_reference_is_deferred_at_authoring_and_refused_when_serving() {
        for value in [
            "var://redirect",
            "env://OAUTH_REDIRECT_URI",
            "vault://kv/app#redirect",
        ] {
            let mut cfg = config();
            cfg.redirect_uri = value.to_string();
            assert!(
                validate_shape(&cfg, ShapeCheck::Authoring).is_ok(),
                "{value} is deferred at authoring"
            );
            let err = validate_shape(&cfg, ShapeCheck::Serving).expect_err(value);
            assert!(err.contains("redirect_uri"), "{value}: {err}");
        }

        // A var may stand in any field; a secret reference only where `compile`
        // resolves one.
        let mut cfg = config();
        cfg.callback_path = "var://callback".to_string();
        cfg.client_auth = "var://client_auth".to_string();
        cfg.state_cookie.same_site = "var://same_site".to_string();
        assert!(validate_shape(&cfg, ShapeCheck::Authoring).is_ok());
        let mut cfg = config();
        cfg.callback_path = "env://CALLBACK_PATH".to_string();
        let err = validate_shape(&cfg, ShapeCheck::Authoring).expect_err("nothing resolves it");
        assert!(
            err.contains("callback_path") && err.contains("var://"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn compile_resolves_the_redirect_uri_and_checks_the_result() {
        // SAFETY: names no other test reads, set before anything resolves them.
        unsafe {
            std::env::set_var(
                "ORION_TEST_OAUTH2_UNIT_REDIRECT_HTTPS",
                "https://app.example.com/v1/auth/idp/callback",
            );
            std::env::set_var(
                "ORION_TEST_OAUTH2_UNIT_REDIRECT_HTTP",
                "http://app.example.com/v1/auth/idp/callback",
            );
        }
        let mut cfg = config();
        cfg.redirect_uri = "env://ORION_TEST_OAUTH2_UNIT_REDIRECT_HTTPS".to_string();
        let login = compiled(&cfg).await;
        let redirect = login.begin(None, None, None).expect("a redirect");
        let q = params(&redirect.location);
        assert_eq!(
            q.get("redirect_uri").map(String::as_str),
            Some("https://app.example.com/v1/auth/idp/callback")
        );

        let mut cfg = config();
        cfg.redirect_uri = "env://ORION_TEST_OAUTH2_UNIT_REDIRECT_HTTP".to_string();
        let err = try_compile(&cfg).await.expect_err("plain http after resolution");
        assert!(err.contains("https"), "{err}");
    }

    #[test]
    fn a_strict_state_cookie_is_refused_with_the_reason() {
        let mut cfg = config();
        cfg.state_cookie.same_site = "strict".to_string();
        let err = validate_shape(&cfg, ShapeCheck::Authoring).expect_err("must refuse");
        assert!(err.contains("cross-site"), "{err}");
    }

    #[test]
    fn a_parameterised_or_self_referencing_callback_is_refused() {
        // A single-provider callback must be static.
        let mut cfg = config();
        cfg.callback_path = "/v1/auth/{provider}/callback".to_string();
        assert!(validate_shape(&cfg, ShapeCheck::Authoring).is_err());

        let mut cfg = config();
        cfg.callback_path = "v1/auth/idp/callback".to_string();
        assert!(
            validate_shape(&cfg, ShapeCheck::Authoring).is_err(),
            "must be absolute"
        );
    }

    /// The multi-provider form requires exactly one `{provider}` in the callback
    /// and a `{provider}` template in the redirect URI, and refuses the flat
    /// fields alongside the map.
    #[test]
    fn the_multi_provider_shape_is_checked() {
        assert!(validate_shape(&multi_config(), ShapeCheck::Authoring).is_ok());

        let mut cfg = multi_config();
        cfg.callback_path = "/v1/auth/callback".to_string();
        let err = validate_shape(&cfg, ShapeCheck::Authoring).expect_err("no {provider}");
        assert!(err.contains("{provider}"), "{err}");

        let mut cfg = multi_config();
        cfg.redirect_uri = "https://app.example.com/callback".to_string();
        let err = validate_shape(&cfg, ShapeCheck::Authoring).expect_err("static redirect");
        assert!(err.contains("{provider}"), "{err}");

        let mut cfg = multi_config();
        cfg.client_id = Some("stray".to_string());
        let err = validate_shape(&cfg, ShapeCheck::Authoring).expect_err("both forms");
        assert!(err.contains("both"), "{err}");

        let mut cfg = multi_config();
        cfg.providers = Some(BTreeMap::new());
        let err = validate_shape(&cfg, ShapeCheck::Authoring).expect_err("empty map");
        assert!(err.contains("at least one"), "{err}");
    }

    /// A per-provider diagnostic names the provider whose field is wrong.
    #[test]
    fn a_bad_provider_field_names_the_provider() {
        let mut cfg = multi_config();
        if let Some(p) = cfg.providers.as_mut().and_then(|m| m.get_mut("github")) {
            p.token_url = Some("http://github.example/token".to_string());
        }
        let err = validate_shape(&cfg, ShapeCheck::Authoring).expect_err("http token_url");
        assert!(err.contains("providers.github.token_url"), "{err}");
    }

    /// A block that opts in merges the deployment's providers under its own; the
    /// instance-supplied one is selectable and fills the redirect template.
    #[tokio::test]
    async fn instance_providers_are_merged_when_opted_in() {
        let mut cfg = multi_config();
        cfg.providers_from_instance = true;
        let login = compile_with_instance(&cfg, &instance_with_iitm())
            .await
            .expect("compiles");

        let q = params(&login.begin(Some("iitm"), None, None).expect("iitm").location);
        assert_eq!(q.get("client_id").map(String::as_str), Some("iitm-client"));
        assert_eq!(
            q.get("redirect_uri").map(String::as_str),
            Some("https://app.example.com/v1/auth/iitm/callback")
        );
        // The definition's own providers still resolve.
        assert!(login.begin(Some("github"), None, None).is_ok());
    }

    /// The definition's own entry wins a slug clash with the deployment's.
    #[tokio::test]
    async fn the_definition_wins_a_slug_clash() {
        let mut cfg = multi_config(); // github → gh-client
        cfg.providers_from_instance = true;
        let mut instance = instance_with_iitm();
        instance.insert(
            "github".to_string(),
            crate::config::InstanceProviderConfig {
                authorize_url: Some("https://github.com/login/oauth/authorize".to_string()),
                token_url: Some("https://github.com/login/oauth/access_token".to_string()),
                client_id: Some("instance-gh".to_string()),
                client_secret: Some("x".to_string()),
                ..Default::default()
            },
        );
        let login = compile_with_instance(&cfg, &instance).await.expect("compiles");
        let q = params(&login.begin(Some("github"), None, None).expect("gh").location);
        assert_eq!(
            q.get("client_id").map(String::as_str),
            Some("gh-client"),
            "the definition's own github entry wins"
        );
    }

    /// Opting into instance providers but supplying none compiles to zero
    /// providers, which is refused at load rather than served empty.
    #[tokio::test]
    async fn instance_only_with_none_supplied_is_refused() {
        let mut cfg = multi_config();
        cfg.providers = None;
        cfg.providers_from_instance = true;
        let err = compile_with_instance(&cfg, no_instance())
            .await
            .expect_err("no providers");
        assert!(err.contains("at least one"), "{err}");
    }

    /// `providers_from_instance` is multi-provider, so it cannot be combined with
    /// the flat single-provider fields.
    #[test]
    fn instance_opt_in_refuses_the_flat_fields() {
        let mut cfg = config(); // flat fields set
        cfg.providers_from_instance = true;
        let err = validate_shape(&cfg, ShapeCheck::Authoring).expect_err("both forms");
        assert!(err.contains("both"), "{err}");
    }

    #[tokio::test]
    async fn a_short_state_secret_is_refused_at_compile() {
        let mut cfg = config();
        cfg.state_secret = "too-short".to_string();
        let err = try_compile(&cfg).await.expect_err("must refuse");
        assert!(err.contains("RFC 7518"), "{err}");
    }

    #[tokio::test]
    async fn state_cookie_max_age_is_bounded_at_both_ends() {
        let mut cfg = config();

        cfg.state_cookie.max_age = 0;
        let err = validate_shape(&cfg, ShapeCheck::Authoring).expect_err("zero must be refused");
        assert!(err.contains("greater than zero"), "{err}");

        for absurd in [u64::MAX, u64::MAX / 2, MAX_STATE_COOKIE_MAX_AGE_SECS + 1] {
            cfg.state_cookie.max_age = absurd;
            let err =
                validate_shape(&cfg, ShapeCheck::Authoring).expect_err("{absurd} must be refused");
            assert!(err.contains("max_age"), "{err}");
            assert!(err.contains("ceiling"), "{err}");
        }

        for ok in [1, 600, MAX_STATE_COOKIE_MAX_AGE_SECS] {
            cfg.state_cookie.max_age = ok;
            assert!(
                validate_shape(&cfg, ShapeCheck::Authoring).is_ok(),
                "{ok} should be accepted"
            );
        }
    }

    #[tokio::test]
    async fn an_out_of_range_max_age_is_refused_at_compile() {
        let mut cfg = config();
        cfg.state_cookie.max_age = u64::MAX;
        let err = try_compile(&cfg).await.expect_err("must refuse");
        assert!(err.contains("max_age"), "{err}");
    }

    #[tokio::test]
    async fn return_to_is_filtered_against_the_allow_list() {
        let mut cfg = config();
        cfg.return_to = Some(crate::channel::ReturnToConfig {
            param: "next".to_string(),
            allow_list: vec!["https://app.example.com".to_string()],
        });
        let login = compiled(&cfg).await;

        for value in [
            "https://app.example.com/dashboard",
            "https://app.example.com/",
            "https://app.example.com",
            "https://app.example.com/a/b?q=1#frag",
        ] {
            let permitted = HashMap::from([("next".to_string(), value.to_string())]);
            assert_eq!(
                login.accepted_return_to(&permitted).as_deref(),
                Some(value),
                "{value}"
            );
        }

        for value in [
            "https://evil.example.com/",
            "https://app.example.com.evil.test/steal",
            "https://app.example.com.evil.test",
            "https://app.example.com@evil.test/steal",
            "http://app.example.com/dashboard",
            "https://app.example.com:8443/dashboard",
            "/dashboard",
            "javascript:alert(1)",
            "",
        ] {
            let refused = HashMap::from([("next".to_string(), value.to_string())]);
            assert_eq!(login.accepted_return_to(&refused), None, "{value}");
        }
    }

    #[tokio::test]
    async fn return_to_path_matching_cuts_at_a_segment_boundary() {
        let mut cfg = config();
        cfg.return_to = Some(crate::channel::ReturnToConfig {
            param: "next".to_string(),
            allow_list: vec!["https://app.example.com/app".to_string()],
        });
        let login = compiled(&cfg).await;

        for value in [
            "https://app.example.com/app",
            "https://app.example.com/app/",
            "https://app.example.com/app/home",
        ] {
            let permitted = HashMap::from([("next".to_string(), value.to_string())]);
            assert_eq!(
                login.accepted_return_to(&permitted).as_deref(),
                Some(value),
                "{value}"
            );
        }

        for value in [
            "https://app.example.com/application",
            "https://app.example.com/appliance/x",
            "https://app.example.com/other",
            "https://app.example.com/",
        ] {
            let refused = HashMap::from([("next".to_string(), value.to_string())]);
            assert_eq!(login.accepted_return_to(&refused), None, "{value}");
        }
    }

    #[tokio::test]
    async fn the_debug_rendering_does_not_carry_the_client_secret() {
        let login = compiled(&config()).await;
        let rendered = format!("{login:?}");
        assert!(!rendered.contains("shhh"), "{rendered}");
    }
}
