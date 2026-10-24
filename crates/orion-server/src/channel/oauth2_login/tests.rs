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
        userinfo_url: None,
        identity: None,
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
        userinfo_url: None,
        identity: None,
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

    let gh = params(
        &login
            .begin(Some("github"), None, None)
            .expect("gh")
            .location,
    );
    assert_eq!(gh.get("client_id").map(String::as_str), Some("gh-client"));
    assert_eq!(
        gh.get("redirect_uri").map(String::as_str),
        Some("https://app.example.com/v1/auth/github/callback")
    );

    let acme = params(
        &login
            .begin(Some("acme"), None, None)
            .expect("acme")
            .location,
    );
    assert_eq!(
        acme.get("client_id").map(String::as_str),
        Some("acme-client")
    );
    assert_eq!(
        acme.get("redirect_uri").map(String::as_str),
        Some("https://app.example.com/v1/auth/acme/callback")
    );
    assert_eq!(
        acme.get("scope").map(String::as_str),
        Some("openid profile")
    );
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

/// The redirect URIs `compile_resolves_the_redirect_uri_and_checks_the_result`
/// resolves, set before `main()` while the process is single-threaded, in the
/// reserved `ORION_SECRET_*` namespace so a test that loads config ignores them.
#[ctor::ctor(unsafe)]
#[allow(unsafe_code)]
fn install_redirect_env() {
    // SAFETY: runs pre-main on the sole thread of the process.
    unsafe {
        std::env::set_var(
            "ORION_SECRET_TEST_OAUTH2_UNIT_REDIRECT_HTTPS",
            "https://app.example.com/v1/auth/idp/callback",
        );
        std::env::set_var(
            "ORION_SECRET_TEST_OAUTH2_UNIT_REDIRECT_HTTP",
            "http://app.example.com/v1/auth/idp/callback",
        );
    }
}

#[tokio::test]
async fn compile_resolves_the_redirect_uri_and_checks_the_result() {
    let mut cfg = config();
    cfg.redirect_uri = "env://ORION_SECRET_TEST_OAUTH2_UNIT_REDIRECT_HTTPS".to_string();
    let login = compiled(&cfg).await;
    let redirect = login.begin(None, None, None).expect("a redirect");
    let q = params(&redirect.location);
    assert_eq!(
        q.get("redirect_uri").map(String::as_str),
        Some("https://app.example.com/v1/auth/idp/callback")
    );

    let mut cfg = config();
    cfg.redirect_uri = "env://ORION_SECRET_TEST_OAUTH2_UNIT_REDIRECT_HTTP".to_string();
    let err = try_compile(&cfg)
        .await
        .expect_err("plain http after resolution");
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

    let q = params(
        &login
            .begin(Some("iitm"), None, None)
            .expect("iitm")
            .location,
    );
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
    let login = compile_with_instance(&cfg, &instance)
        .await
        .expect("compiles");
    let q = params(
        &login
            .begin(Some("github"), None, None)
            .expect("gh")
            .location,
    );
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

/// The defaults are OIDC claim names, and a subject is coerced to a string.
#[test]
fn identity_maps_oidc_claims_by_default() {
    let map = CompiledIdentityMap::resolve(None);
    let claims = json!({
        "sub": "abc-123",
        "preferred_username": "jdoe",
        "name": "J. Doe",
        "email": "j@doe.example",
        "picture": "https://cdn/x.png",
        "extra": "ignored"
    });
    let id = map.extract(&claims).expect("an identity");
    assert_eq!(id["subject"], "abc-123");
    assert_eq!(id["login"], "jdoe");
    assert_eq!(id["name"], "J. Doe");
    assert_eq!(id["email"], "j@doe.example");
    assert_eq!(id["picture"], "https://cdn/x.png");
}

/// A provider like GitHub overrides the source keys, and a numeric `id`
/// becomes a string subject.
#[test]
fn identity_map_overrides_and_coerces_the_subject() {
    let map = CompiledIdentityMap::resolve(Some(&IdentityMap {
        subject: Some("id".to_string()),
        login: Some("login".to_string()),
        picture: Some("avatar_url".to_string()),
        ..Default::default()
    }));
    let user = json!({ "id": 4210, "login": "octocat", "avatar_url": "https://gh/a.png" });
    let id = map.extract(&user).expect("an identity");
    assert_eq!(id["subject"], "4210", "a numeric id is stringified");
    assert_eq!(id["login"], "octocat");
    assert_eq!(id["picture"], "https://gh/a.png");
    // Fields the source omits are left out, not null.
    assert!(id.get("email").is_none());
}

/// No subject, no identity — an identity without one cannot be upserted on.
#[test]
fn identity_is_none_without_a_subject() {
    let map = CompiledIdentityMap::resolve(None);
    assert!(map.extract(&json!({ "name": "no sub here" })).is_none());
}

#[test]
fn value_to_string_coerces_scalars_only() {
    assert_eq!(value_to_string(&json!("s")).as_deref(), Some("s"));
    assert_eq!(value_to_string(&json!(42)).as_deref(), Some("42"));
    assert_eq!(value_to_string(&json!(true)).as_deref(), Some("true"));
    assert_eq!(value_to_string(&json!("")), None);
    assert_eq!(value_to_string(&json!(null)), None);
    assert_eq!(value_to_string(&json!([1, 2])), None);
    assert_eq!(value_to_string(&json!({"a": 1})), None);
}

/// An empty identity source key is a typo, refused at authoring.
#[test]
fn an_empty_identity_key_is_refused() {
    let mut cfg = config();
    cfg.identity = Some(IdentityMap {
        subject: Some("  ".to_string()),
        ..Default::default()
    });
    let err = validate_shape(&cfg, ShapeCheck::Authoring).expect_err("empty key");
    assert!(err.contains("identity.subject"), "{err}");
}

/// A userinfo URL is https, like the other endpoints.
#[test]
fn a_plain_http_userinfo_url_is_refused() {
    let mut cfg = config();
    cfg.userinfo_url = Some("http://api.example/user".to_string());
    let err = validate_shape(&cfg, ShapeCheck::Authoring).expect_err("http userinfo");
    assert!(
        err.contains("userinfo_url") && err.contains("https"),
        "{err}"
    );
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
