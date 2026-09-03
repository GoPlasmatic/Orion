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
//! redirect home. It receives the grant at `metadata.oauth`.
//!
//! ## State is a signed cookie, not a stored row
//!
//! Everything the callback needs — the nonce, the PKCE verifier, the OIDC
//! nonce, the destination — travels in one HS256 JWT in an `HttpOnly` cookie,
//! minted with [`crate::jwt::sign`] and verified with [`crate::jwt::Verifier`].
//! That reuses #267's core whole (the algorithm allowlist, `require_exp`, the
//! leeway, RFC 7518's key-length floor) and needs no shared store, so a sign-in
//! that begins on one node and returns to another works with no coordination.
//! The `state` query parameter **is** the nonce claim; the binding is that the
//! two match.
//!
//! The cost is that "single use" is enforced by clearing the cookie rather than
//! by a row: two concurrent replays of one callback inside the window would
//! both pass this check. The authorization code itself is single-use at the
//! IdP, which is where that defence actually lives.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::config::{
    IdTokenConfig, OAuth2LoginConfig, RESERVED_AUTHORIZE_PARAMS, StateCookieConfig,
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
}

/// A channel's `oauth2_login` block with its secrets resolved and its keys
/// built — the per-request path does no resolution and no parsing.
pub struct CompiledOAuth2Login {
    cfg: OAuth2LoginConfig,
    channel: String,
    client_id: String,
    client_secret: String,
    state_key: jsonwebtoken::EncodingKey,
    state_verifier: crate::jwt::Verifier,
    id_token_verifier: Option<crate::jwt::Verifier>,
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
            .field("authorize_url", &self.cfg.authorize_url)
            .field("callback_path", &self.cfg.callback_path)
            .field("pkce", &self.cfg.pkce)
            .field("oidc", &self.id_token_verifier.is_some())
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
        validate_shape(cfg)?;

        let client_id = resolve_secret(&cfg.client_id, "oauth2_login.client_id").await?;
        let client_secret =
            resolve_secret(&cfg.client_secret, "oauth2_login.client_secret").await?;
        let state_secret = resolve_secret(&cfg.state_secret, "oauth2_login.state_secret").await?;

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

        let id_token_verifier = match cfg.id_token {
            Some(ref id) => Some(build_id_token_verifier(id, &client_id, deps)?),
            None => None,
        };

        Ok(Self {
            cfg: cfg.clone(),
            channel: channel.to_string(),
            client_id,
            client_secret,
            state_key,
            state_verifier,
            id_token_verifier,
            http_client: deps.http_client.clone(),
            allow_private_token_urls: deps.allow_private_token_urls,
        })
    }

    /// The channel's callback route, as authored.
    pub fn callback_path(&self) -> &str {
        &self.cfg.callback_path
    }

    /// Whether the workflow runs on the authorize leg before the redirect is
    /// built.
    pub fn runs_workflow_on_authorize(&self) -> bool {
        self.cfg.run_workflow_on_authorize
    }

    /// The state cookie's name, for the read side.
    pub fn state_cookie_name(&self) -> &str {
        &self.cfg.state_cookie.name
    }

    // -----------------------------------------------------------------
    // The authorize leg
    // -----------------------------------------------------------------

    /// Mint the state and build the redirect to the IdP.
    ///
    /// `contributed` is `data._orion.oauth2.authorize` when the channel runs
    /// its workflow on this leg — an object that may carry `extra_params` and
    /// `scopes`. It cannot reach `state`, `nonce` or `code_challenge`:
    /// [`RESERVED_AUTHORIZE_PARAMS`] is filtered here as well as refused at
    /// create time, because config validation cannot see what a workflow
    /// computes.
    ///
    /// `return_to` arrives already checked, from
    /// [`Self::accepted_return_to`]. It is a parameter rather than something
    /// this reads from the query itself because the two authorize paths run at
    /// different times: without the workflow the redirect is built in the guard
    /// chain, and with it, after the workflow has run — by which point the
    /// request's real query string is no longer at hand, and the only copy
    /// within reach would be the one in `metadata`, where a caller's envelope
    /// can survive.
    pub fn begin(
        &self,
        contributed: Option<&Value>,
        return_to: Option<&str>,
    ) -> Result<Redirect, String> {
        let nonce = random_nonce();
        let oidc_nonce = self.wants_oidc_nonce().then(random_nonce);
        let verifier = self.cfg.pkce.then(random_nonce);

        let mut url = url::Url::parse(&self.cfg.authorize_url)
            .map_err(|e| format!("authorize_url does not parse: {e}"))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("response_type", "code");
            q.append_pair("client_id", &self.client_id);
            q.append_pair("redirect_uri", &self.cfg.redirect_uri);
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
                .unwrap_or_else(|| self.cfg.scopes.clone());
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
            for (k, v) in &self.cfg.extra_authorize_params {
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

        let now = now_secs();
        let mut claims = json!({
            "nonce": nonce,
            "iat": now,
            "exp": now + self.cfg.state_cookie.max_age,
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
        let token = crate::jwt::sign(STATE_ALG, &self.state_key, None, &claims)
            .map_err(|e| format!("could not sign the OAuth2 state: {e}"))?;

        Ok(Redirect {
            location: url.into(),
            set_cookie: self.state_cookie(&token, self.cfg.state_cookie.max_age as i64)?,
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
        query: &HashMap<String, String>,
        jar: &[&str],
    ) -> Result<Grant, OrionError> {
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
            return Err(self.refuse("provider_error"));
        }

        let state = query
            .get("state")
            .ok_or_else(|| self.refuse("state_missing"))?;
        let code = query
            .get("code")
            .ok_or_else(|| self.refuse("code_missing"))?;

        let cookie =
            crate::channel::cookies::lookup(jar.iter().copied(), &self.cfg.state_cookie.name)
                .ok_or_else(|| self.refuse("state_missing"))?;

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
                self.refuse("state_invalid")
            })?;

        let minted = claims
            .get("nonce")
            .and_then(Value::as_str)
            .ok_or_else(|| self.refuse("state_invalid"))?;
        if !secret_eq(state, minted) {
            return Err(self.refuse("state_mismatch"));
        }

        let tokens = self
            .exchange(code, claims.get("pkce_verifier").and_then(Value::as_str))
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

        if let Some(verifier) = self.id_token_verifier.as_ref() {
            let id = self.cfg.id_token.as_ref().expect("verifier implies config");
            match tokens.id_token.as_deref() {
                Some(token) => {
                    let verified = verifier.verify(token).await.map_err(|reason| {
                        tracing::warn!(
                            channel = %self.channel,
                            reason = reason.as_str(),
                            "OAuth2 id_token rejected"
                        );
                        self.refuse("id_token_rejected")
                    })?;
                    if id.nonce {
                        let minted = claims.get("oidc_nonce").and_then(Value::as_str);
                        let echoed = verified.get("nonce").and_then(Value::as_str);
                        match (minted, echoed) {
                            (Some(a), Some(b)) if secret_eq(a, b) => {}
                            _ => return Err(self.refuse("nonce_mismatch")),
                        }
                    }
                    oauth["claims"] = verified;
                }
                None if id.required => {
                    tracing::warn!(
                        channel = %self.channel,
                        "Token response carried no id_token, but one is required"
                    );
                    return Err(self.refuse("id_token_rejected"));
                }
                None => {}
            }
        }

        crate::metrics::record_oauth_login(&self.channel, Leg::Callback, "ok");
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
        code: &str,
        pkce_verifier: Option<&str>,
    ) -> Result<crate::connector::oauth::TokenResponse, OrionError> {
        let mut params = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("redirect_uri", self.cfg.redirect_uri.clone()),
        ];
        if let Some(v) = pkce_verifier {
            params.push(("code_verifier", v.to_string()));
        }

        let endpoint = crate::connector::oauth::TokenEndpoint {
            token_url: &self.cfg.token_url,
            client_id: &self.client_id,
            client_secret: &self.client_secret,
            client_auth: &self.cfg.client_auth,
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
                self.count("exchange_error");
                OrionError::unavailable(
                    Unavailable::GuardBackend,
                    "the identity provider could not be reached",
                )
            } else {
                tracing::warn!(channel = %self.channel, error = %e, "OAuth2 token exchange rejected");
                self.refuse("exchange_rejected")
            }
        })
    }

    // -----------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------

    fn wants_oidc_nonce(&self) -> bool {
        self.cfg.id_token.as_ref().is_some_and(|id| id.nonce)
    }

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
    /// The comparison is on **origin and path segments**, not on the text. A
    /// raw `starts_with` reads like the obvious implementation and is an open
    /// redirect: the natural entry `https://app.example.com` is a string prefix
    /// of `https://app.example.com.evil.test/steal`, so the crafted host is
    /// admitted, sealed into the signed state, and handed to the workflow as a
    /// *vetted* value — which is precisely the moment the workflow stops being
    /// able to defend itself. Requiring the entry to end in `/` would close
    /// that one hole and leave the shape of the rule depending on a trailing
    /// character an operator cannot be expected to know is load-bearing.
    pub fn accepted_return_to(&self, query: &HashMap<String, String>) -> Option<String> {
        let cfg = self.cfg.return_to.as_ref()?;
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
        } = self.cfg.state_cookie;
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

    /// The uniform refusal, counted by its real reason.
    fn refuse(&self, outcome: &'static str) -> OrionError {
        self.count(outcome);
        OrionError::Unauthorized("sign-in could not be completed".to_string())
    }

    fn count(&self, outcome: &'static str) {
        crate::metrics::record_oauth_login(&self.channel, Leg::Callback, outcome);
    }
}

/// Everything about the block that can be judged without resolving a secret.
///
/// Shared with `validation::channels` so a create or update answers `400`
/// naming the field, rather than storing a definition that quarantines its
/// channel at the next reload. Secret *resolution* deliberately stays in
/// [`CompiledOAuth2Login::compile`]: a bundle has to validate on a host that
/// holds none of the production secrets.
pub fn validate_shape(cfg: &OAuth2LoginConfig) -> Result<(), String> {
    for (field, value) in [
        ("authorize_url", &cfg.authorize_url),
        ("token_url", &cfg.token_url),
        ("redirect_uri", &cfg.redirect_uri),
    ] {
        require_https(field, value)?;
    }

    if cfg.callback_path.trim().is_empty() || !cfg.callback_path.starts_with('/') {
        return Err("oauth2_login.callback_path must be an absolute path, e.g. \
                    /v1/auth/github/callback"
            .to_string());
    }
    if cfg.callback_path.contains('{') {
        return Err(format!(
            "oauth2_login.callback_path '{}' carries a path parameter; the callback is a \
             fixed URL registered with the identity provider, so it must be static",
            cfg.callback_path
        ));
    }

    if crate::connector::OAuth2ClientAuth::parse(&cfg.client_auth).is_none() {
        return Err(format!(
            "oauth2_login.client_auth '{}' is not supported — expected {}",
            cfg.client_auth,
            crate::connector::OAuth2ClientAuth::VALUES
        ));
    }

    for name in cfg.extra_authorize_params.keys() {
        if RESERVED_AUTHORIZE_PARAMS.contains(&name.as_str()) {
            return Err(format!(
                "oauth2_login.extra_authorize_params sets '{name}', which Orion owns. \
                 Overriding it would disable the protection it carries — the reserved \
                 set is: {}",
                RESERVED_AUTHORIZE_PARAMS.join(", ")
            ));
        }
    }

    if cfg.state_cookie.max_age == 0 {
        return Err(
            "oauth2_login.state_cookie.max_age must be greater than zero — it is \
                    also the state token's expiry"
                .to_string(),
        );
    }
    // Canonicalised by the formatter, which refuses anything else — but a
    // create-time message naming the field beats a reload-time quarantine.
    //
    // One match, so the set this field actually accepts — `lax` or `none` — is
    // stated once. Spelling `strict` as a valid value and then refusing it in a
    // second `if` advertised a setting no configuration can hold.
    match cfg.state_cookie.same_site.to_ascii_lowercase().as_str() {
        "lax" | "none" => {}
        // Not merely unusual: the callback is a top-level cross-site GET from
        // the IdP, and a `Strict` cookie is withheld on exactly that request,
        // so every sign-in would fail the state check with nothing to see in
        // the logs but a missing cookie.
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
            require_https("return_to.allow_list entry", prefix)?;
        }
    }

    if let Some(ref id) = cfg.id_token {
        crate::jwt::validate_jwks_url(&id.jwks_url)
            .map_err(|e| format!("oauth2_login.id_token.jwks_url: {e}"))?;
        if id.issuer.is_empty() {
            return Err(
                "oauth2_login.id_token.issuer must list at least one accepted \
                        issuer — an unchecked `iss` accepts a token from any provider \
                        whose key happens to be in the JWKS"
                    .to_string(),
            );
        }
        if id.algorithms.is_empty() {
            return Err("oauth2_login.id_token.algorithms must not be empty".to_string());
        }
        for alg in &id.algorithms {
            crate::jwt::parse_algorithm(alg)
                .map_err(|e| format!("oauth2_login.id_token.algorithms: {e}"))?;
        }
    }
    Ok(())
}

/// `https`, or `http` on a loopback host.
///
/// The carve-out is the rule browsers already use for secure contexts, and it
/// is what makes the flow developable: an identity provider will not issue a
/// certificate for your laptop, and GitHub, Google and Entra all accept a
/// plain-`http` loopback redirect URI for exactly this reason (RFC 8252 §7.3
/// says so in as many words). Without it, "run the sign-in locally" means
/// terminating TLS in front of a development server, which nobody does, so the
/// first time the flow is exercised end to end is in staging.
///
/// It grants nothing on its own. `token_url` still has to pass
/// [`crate::validation::validate_url_not_private`] at every exchange unless the
/// operator sets `[oauth2_login] allow_private_token_urls`, and that flag is
/// instance-wide. Two independent gates; this relaxes one of them, for a class
/// of host that is not reachable from anywhere else.
/// Whether one allow-list entry admits a candidate `return_to`.
///
/// Two conditions, and the second is the one a string prefix cannot express:
///
/// 1. **Same origin.** Scheme, host and port must match exactly, so a host that
///    merely *starts with* the permitted one — `app.example.com.evil.test` — is
///    a different origin and is refused. `Url::origin` also disregards
///    userinfo, which is what stops `https://app.example.com@evil.test/` from
///    reading as the permitted host; that URL's host is `evil.test`.
/// 2. **Path-segment prefix.** The candidate's path must be the entry's path or
///    live beneath it, cut at a `/`. So an entry of `/app` admits `/app` and
///    `/app/home` but not `/application`. An entry with no path parses as `/`
///    and therefore admits the whole origin, which is what writing a bare
///    origin plainly means.
///
/// An entry that does not parse admits nothing. `validate_shape` already
/// refuses those through [`require_https`], so this is unreachable rather than
/// lenient — failing closed is simply the right answer for the case.
fn permits_return_to(entry: &str, candidate: &url::Url) -> bool {
    let Ok(allowed) = url::Url::parse(entry) else {
        return false;
    };
    if allowed.origin() != candidate.origin() {
        return false;
    }
    let (allowed_path, candidate_path) = (allowed.path(), candidate.path());
    if let Some(base) = allowed_path.strip_suffix('/') {
        // `/app/` — the trailing slash is already the boundary.
        return candidate_path == base || candidate_path.starts_with(allowed_path);
    }
    candidate_path == allowed_path || candidate_path.starts_with(&format!("{allowed_path}/"))
}

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
/// name that is reserved for it.
///
/// Name resolution is deliberately not consulted: a host that resolves to
/// `127.0.0.1` today can resolve elsewhere tomorrow, and this runs at authoring
/// time against a definition that will be promoted to other instances. Only
/// spellings that cannot mean anything else are accepted.
fn is_loopback_host(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        // RFC 6761 §6.3 reserves `localhost` and its subdomains for the loopback
        // interface; a resolver is not permitted to answer them otherwise.
        Some(url::Host::Domain(host)) => host == "localhost" || host.ends_with(".localhost"),
        None => false,
    }
}

fn build_id_token_verifier(
    cfg: &IdTokenConfig,
    client_id: &str,
    deps: &LoginDeps<'_>,
) -> Result<crate::jwt::Verifier, String> {
    let algorithms = cfg
        .algorithms
        .iter()
        .map(|a| crate::jwt::parse_algorithm(a))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("oauth2_login.id_token.algorithms: {e}"))?;

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

/// The same resolver `auth.secret` and `auth.jwt_keys[].key` use, so an
/// operator has one mechanism rather than one per block.
async fn resolve_secret(value: &str, field: &str) -> Result<String, String> {
    let resolved = crate::connector::secrets::resolve_secret_string(value, field).await?;
    if resolved.is_empty() {
        return Err(format!("{field} resolved to an empty value"));
    }
    Ok(resolved)
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
///
/// [`crate::crypto::Codec::Base64Url`] is already the unpadded alphabet, which
/// is the half of this that is easy to get wrong — a padded challenge is
/// rejected by every conforming IdP.
fn pkce_challenge(verifier: &str) -> String {
    use sha2::Digest as _;
    crate::crypto::encode_bytes(
        crate::crypto::Codec::Base64Url,
        &sha2::Sha256::digest(verifier.as_bytes()),
    )
}

/// Constant-time comparison of two nonces.
///
/// Via SHA-256 because the shared helper is fixed-width (`&[u8; 32]`), and
/// because hashing first also removes the length of the inputs as a signal.
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
    use crate::channel::config::{OAuth2LoginConfig, StateCookieConfig};

    /// 32 bytes, so `encoding_key` accepts it as an HS256 secret.
    const STATE_SECRET: &str = "0123456789abcdef0123456789abcdef";

    fn config() -> OAuth2LoginConfig {
        OAuth2LoginConfig {
            authorize_url: "https://idp.example.com/authorize".to_string(),
            token_url: "https://idp.example.com/token".to_string(),
            client_id: "client-123".to_string(),
            client_secret: "shhh".to_string(),
            client_auth: "basic".to_string(),
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
            },
        )
        .await
        .expect("compiles")
    }

    fn params(location: &str) -> HashMap<String, String> {
        url::Url::parse(location)
            .expect("a URL")
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    #[tokio::test]
    async fn the_authorize_url_carries_what_the_rfc_requires() {
        let login = compiled(&config()).await;
        let redirect = login.begin(None, None).expect("a redirect");
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

    /// #307's second trap, asserted. `jwt_sign` alone cannot mint a state: its
    /// claims are constant and `iat`/`exp` are second-granular, so two sign-ins
    /// beginning in the same second produced byte-identical tokens — a state
    /// parameter that identifies nothing. These two calls are the same second.
    #[tokio::test]
    async fn two_sign_ins_in_one_second_get_different_states() {
        let login = compiled(&config()).await;
        let a = login.begin(None, None).expect("a redirect");
        let b = login.begin(None, None).expect("a redirect");

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

    /// RFC 7636 Appendix B's published vector: the verifier and the challenge
    /// it must produce. Unpadded base64url is the half that is easy to get
    /// wrong, and a padded challenge is refused by every conforming IdP.
    #[test]
    fn the_pkce_challenge_matches_the_rfc_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    /// The state cookie is a bearer value for one sign-in, and every property
    /// of a `Verifier` is what stops a forged one being accepted.
    #[tokio::test]
    async fn a_state_token_from_another_key_is_rejected() {
        let login = compiled(&config()).await;
        let mut other = config();
        other.state_secret = "fedcba9876543210fedcba9876543210".to_string();
        let attacker = compiled(&other).await;

        let forged = attacker.begin(None, None).expect("a redirect");
        let cookie = forged
            .set_cookie
            .split(';')
            .next()
            .and_then(|p| p.split_once('='))
            .map(|(_, v)| v.to_string())
            .expect("a cookie value");

        let claims = login.state_verifier.verify(&cookie).await;
        assert!(
            claims.is_err(),
            "a state signed with another key must not verify"
        );
    }

    /// The state the browser carries and the state in the cookie are two
    /// halves of one binding; a callback that presents one without the other
    /// is the login-CSRF the parameter exists to prevent.
    #[tokio::test]
    async fn a_callback_without_the_cookie_is_refused_before_the_exchange() {
        let login = compiled(&config()).await;
        let redirect = login.begin(None, None).expect("a redirect");
        let state = params(&redirect.location)
            .get("state")
            .expect("a state")
            .clone();

        let query = HashMap::from([
            ("state".to_string(), state),
            ("code".to_string(), "whatever".to_string()),
        ]);
        // No cookie header at all. The token URL is unreachable in tests, so
        // reaching the exchange would surface as a *different* error — which
        // is exactly what this asserts did not happen.
        let err = login.complete(&query, &[]).await.expect_err("must refuse");
        assert!(
            matches!(err, OrionError::Unauthorized(_)),
            "expected a 401, got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_state_that_does_not_match_the_cookie_is_refused() {
        let login = compiled(&config()).await;
        let redirect = login.begin(None, None).expect("a redirect");
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
            .complete(&query, &[jar.as_str()])
            .await
            .expect_err("must refuse");
        assert!(matches!(err, OrionError::Unauthorized(_)), "{err:?}");
    }

    /// The IdP refusing is a different event from a check failing here, and
    /// both are the same `401` on the wire.
    #[tokio::test]
    async fn a_provider_error_is_refused_without_looking_at_the_state() {
        let login = compiled(&config()).await;
        let query = HashMap::from([("error".to_string(), "access_denied".to_string())]);
        let err = login.complete(&query, &[]).await.expect_err("must refuse");
        assert!(matches!(err, OrionError::Unauthorized(_)), "{err:?}");
    }

    #[test]
    fn a_reserved_authorize_parameter_is_refused_at_the_door() {
        let mut cfg = config();
        cfg.extra_authorize_params
            .insert("state".to_string(), "attacker-chosen".to_string());
        let err = validate_shape(&cfg).expect_err("must refuse");
        assert!(err.contains("state"), "{err}");
    }

    /// A workflow contributing under `run_workflow_on_authorize` is filtered
    /// too: config validation cannot see what a workflow computes.
    #[tokio::test]
    async fn a_workflow_cannot_contribute_a_reserved_parameter() {
        let mut cfg = config();
        cfg.run_workflow_on_authorize = true;
        let login = compiled(&cfg).await;
        let contributed = json!({
            "extra_params": { "state": "attacker-chosen", "login_hint": "a@b.com" }
        });
        let redirect = login.begin(Some(&contributed), None).expect("a redirect");
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
                "authorize_url" => cfg.authorize_url = value,
                "token_url" => cfg.token_url = value,
                _ => cfg.redirect_uri = value,
            }
            let err = validate_shape(&cfg).expect_err(field);
            assert!(err.contains("https"), "{field}: {err}");
        }
    }

    /// The carve-out, and its edges. `localhost.evil.test` is the one that
    /// matters: a suffix check written the other way round would accept it.
    #[test]
    fn plain_http_is_accepted_only_on_loopback() {
        for host in ["localhost", "127.0.0.1", "[::1]", "app.localhost"] {
            let mut cfg = config();
            cfg.token_url = format!("http://{host}:8080/token");
            assert!(validate_shape(&cfg).is_ok(), "{host} should be accepted");
        }
        for host in ["localhost.evil.test", "127.0.0.1.evil.test", "10.0.0.1"] {
            let mut cfg = config();
            cfg.token_url = format!("http://{host}/token");
            assert!(validate_shape(&cfg).is_err(), "{host} should be refused");
        }
    }

    /// Not a style preference: `Strict` withholds the cookie on the callback,
    /// which is a top-level cross-site GET, so every sign-in would fail the
    /// state check with nothing in the logs but an absent cookie.
    #[test]
    fn a_strict_state_cookie_is_refused_with_the_reason() {
        let mut cfg = config();
        cfg.state_cookie.same_site = "strict".to_string();
        let err = validate_shape(&cfg).expect_err("must refuse");
        assert!(err.contains("cross-site"), "{err}");
    }

    #[test]
    fn a_parameterised_or_self_referencing_callback_is_refused() {
        let mut cfg = config();
        cfg.callback_path = "/v1/auth/{provider}/callback".to_string();
        assert!(validate_shape(&cfg).is_err());

        let mut cfg = config();
        cfg.callback_path = "v1/auth/idp/callback".to_string();
        assert!(validate_shape(&cfg).is_err(), "must be absolute");
    }

    #[tokio::test]
    async fn a_short_state_secret_is_refused_at_compile() {
        let mut cfg = config();
        cfg.state_secret = "too-short".to_string();
        let jwks = std::sync::Arc::new(crate::jwt::jwks::JwksCache::new(
            reqwest::Client::new(),
            false,
        ));
        let err = CompiledOAuth2Login::compile(
            &cfg,
            "signin",
            &LoginDeps {
                http_client: &reqwest::Client::new(),
                jwks: &jwks,
                allow_private_token_urls: false,
            },
        )
        .await
        .expect_err("must refuse");
        assert!(err.contains("RFC 7518"), "{err}");
    }

    /// Checked on the way *in*, so a value that reaches the workflow has
    /// already passed and cannot turn a workflow redirect into an open one.
    ///
    /// The entry here carries **no trailing slash**, which is what an operator
    /// naturally writes and what the previous string-prefix implementation got
    /// wrong: `https://app.example.com` is a textual prefix of
    /// `https://app.example.com.evil.test/steal`, so the crafted host was
    /// admitted and sealed into the signed state as a vetted destination. The
    /// old test passed only because it configured the trailing slash.
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
            // A bare origin entry admits the whole origin, which is what
            // writing a bare origin plainly means.
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
            // The open redirect: a longer host that starts with the permitted
            // one. This is the case the trailing slash used to be load-bearing
            // for, and it is now refused however the entry is written.
            "https://app.example.com.evil.test/steal",
            "https://app.example.com.evil.test",
            // Userinfo cannot be used to make the host read as the permitted
            // one — this URL's host is `evil.test`.
            "https://app.example.com@evil.test/steal",
            // A different scheme or port is a different origin.
            "http://app.example.com/dashboard",
            "https://app.example.com:8443/dashboard",
            // Not a URL at all, and a relative path, are both refused: the
            // allow-list is written in absolute URLs.
            "/dashboard",
            "javascript:alert(1)",
            "",
        ] {
            let refused = HashMap::from([("next".to_string(), value.to_string())]);
            assert_eq!(login.accepted_return_to(&refused), None, "{value}");
        }
    }

    /// A path on an entry is a boundary, not a substring: `/app` must not admit
    /// `/application`, which is the same mistake as the host case one level
    /// down.
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

    /// A trailing slash on the entry means the same thing as none, so an
    /// operator cannot get this wrong by writing it either way.
    #[tokio::test]
    async fn return_to_entry_means_the_same_with_or_without_a_trailing_slash() {
        for entry in [
            "https://app.example.com/app",
            "https://app.example.com/app/",
        ] {
            let mut cfg = config();
            cfg.return_to = Some(crate::channel::ReturnToConfig {
                param: "next".to_string(),
                allow_list: vec![entry.to_string()],
            });
            let login = compiled(&cfg).await;

            for (value, admitted) in [
                ("https://app.example.com/app", true),
                ("https://app.example.com/app/home", true),
                ("https://app.example.com/application", false),
            ] {
                let q = HashMap::from([("next".to_string(), value.to_string())]);
                assert_eq!(
                    login.accepted_return_to(&q).is_some(),
                    admitted,
                    "entry {entry} / value {value}"
                );
            }
        }
    }

    #[tokio::test]
    async fn the_debug_rendering_does_not_carry_the_client_secret() {
        let login = compiled(&config()).await;
        let rendered = format!("{login:?}");
        assert!(!rendered.contains("shhh"), "{rendered}");
    }
}
