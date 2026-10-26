use super::*;

/// Where [`validate_shape`] runs, which decides what a reference string means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeCheck {
    /// `POST /channels`, `orion-cli lint`, `package lint`: the deployment's
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
    "userinfo_url",
];

/// Whether `value` is a reference rather than a value.
///
/// `Ok(true)`: deferred — the load path resolves it and checks the result.
/// `Ok(false)`: a value; check it. `Err`: a reference nothing resolves in this
/// field, or one that survived resolution.
///
/// `field` may be prefixed (`providers.iitm.client_id`); the resolvable-field
/// test is on its final segment.
pub(super) fn deferred(mode: ShapeCheck, field: &str, value: &str) -> Result<bool, String> {
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
pub(super) fn merge_instance_providers(
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
pub(super) fn validate_shared(cfg: &OAuth2LoginConfig, mode: ShapeCheck) -> Result<(), String> {
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
pub(super) fn validate_provider_param(field: &str, path: &str, multi: bool) -> Result<(), String> {
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
pub(super) fn validate_provider(
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

    // The userinfo endpoint is server-side egress like `token_url`: https, and
    // address-checked at fetch time under the same gate.
    if let Some(userinfo) = p
        .userinfo_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let field = format!("{pfx}userinfo_url");
        if !deferred(mode, &field, userinfo)? {
            require_https(&field, userinfo)?;
        }
    }

    // An identity mapping names source keys; an empty one is a typo, not a
    // default (the default is the whole map being absent).
    if let Some(ref map) = p.identity {
        for (label, value) in [
            ("subject", &map.subject),
            ("login", &map.login),
            ("name", &map.name),
            ("email", &map.email),
            ("picture", &map.picture),
        ] {
            if value.as_deref().is_some_and(|s| s.trim().is_empty()) {
                return Err(format!(
                    "oauth2_login.{pfx}identity.{label} must not be empty — omit it to use the \
                     default claim name"
                ));
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
pub(super) fn require_https(field: &str, value: &str) -> Result<(), String> {
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
///
/// Shared with `oidc_discovery` so the "https, or http on loopback" gate on the
/// URLs Orion fetches has one definition — the `localhost.evil.test` trap is
/// exactly the sort of rule that must not be written twice.
pub(crate) fn is_loopback_host(url: &url::Url) -> bool {
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
pub(super) fn permits_return_to(entry: &str, candidate: &url::Url) -> bool {
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

pub(super) fn build_provider(
    slug: &str,
    resolved: &ProviderConfig,
    shared_redirect: &str,
    discovered: Option<&crate::channel::oidc_discovery::Discovered>,
    deps: &LoginDeps<'_>,
) -> Result<CompiledProvider, String> {
    let pfx = field_prefix(slug);
    // Explicit value wins; discovery fills whatever the block left out.
    let merge_opt = |explicit: &Option<String>, from_discovery: Option<&str>| {
        explicit
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| from_discovery.map(str::to_string))
    };
    let pick = |explicit: &Option<String>, from_discovery: Option<&str>, name: &str| {
        merge_opt(explicit, from_discovery)
            .ok_or_else(|| format!("oauth2_login.{pfx}{name} is required"))
    };
    let client_id = pick(&resolved.client_id, None, "client_id")?;
    let client_secret = pick(&resolved.client_secret, None, "client_secret")?;
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
    let userinfo_url = merge_opt(
        &resolved.userinfo_url,
        discovered.and_then(|d| d.userinfo_url.as_deref()),
    );
    let redirect_uri =
        effective_redirect_uri(shared_redirect, slug, resolved.redirect_uri.as_deref());

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
        userinfo_url,
        identity: CompiledIdentityMap::resolve(resolved.identity.as_ref()),
    })
}

pub(super) fn build_id_token_verifier(
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
pub(super) async fn resolve_secret(value: &str, field: &str) -> Result<String, String> {
    let resolved = crate::connector::secrets::resolve_secret_string(value, field).await?;
    if resolved.is_empty() {
        return Err(format!("{field} resolved to an empty value"));
    }
    Ok(resolved)
}

/// [`resolve_secret`] for an optional field: `None` stays `None`.
pub(super) async fn resolve_opt(
    value: &Option<String>,
    field: &str,
) -> Result<Option<String>, String> {
    match value {
        Some(v) => Ok(Some(resolve_secret(v, field).await?)),
        None => Ok(None),
    }
}

/// 32 CSPRNG bytes, base64url-unpadded — safe in a query string and in a JWT
/// claim without further escaping.
pub(super) fn random_nonce() -> String {
    crate::crypto::encode_bytes(
        crate::crypto::Codec::Base64Url,
        &crate::crypto::random_bytes(NONCE_BYTES),
    )
}

/// RFC 7636 §4.2: `BASE64URL-NOPAD(SHA256(ASCII(verifier)))`.
pub(super) fn pkce_challenge(verifier: &str) -> String {
    use sha2::Digest as _;
    crate::crypto::encode_bytes(
        crate::crypto::Codec::Base64Url,
        &sha2::Sha256::digest(verifier.as_bytes()),
    )
}

/// Constant-time comparison of two nonces, via SHA-256 so the shared helper's
/// fixed width applies and the inputs' length is not a signal.
pub(super) fn secret_eq(a: &str, b: &str) -> bool {
    use sha2::Digest as _;
    let da: [u8; 32] = sha2::Sha256::digest(a.as_bytes()).into();
    let db: [u8; 32] = sha2::Sha256::digest(b.as_bytes()).into();
    crate::config::constant_time_eq(&da, &db)
}

pub(super) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
