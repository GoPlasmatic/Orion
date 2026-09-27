use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Instance-wide policy for the inbound OAuth2 sign-in flow (#307).
///
/// Everything that describes *one* identity-provider relationship — the
/// endpoints, the client credentials, the scopes, PKCE, the state cookie —
/// belongs to the channel's `oauth2_login` block, because it is part of the
/// definition and is promoted with it. What lives here is the operator's egress
/// policy, and (#355) the set of **deployment-supplied providers** — the
/// deliberate exception, for the case the definition cannot express: *which*
/// identity providers exist differs per deployment, and that is a property of
/// the deployment, not the promoted definition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OAuth2LoginConfig {
    /// Allow a channel's `oauth2_login.token_url` to resolve to a private or
    /// link-local address.
    ///
    /// Off by default. The token URL is authored input that Orion POSTs a
    /// client secret to, so with this off
    /// [`crate::validation::validate_url_not_private`] runs on every exchange —
    /// the same treatment a connector's token endpoint gets without
    /// `allow_private_urls`.
    ///
    /// Turn it on for an in-cluster issuer (a Keycloak on a service address, a
    /// mock IdP in a test harness). Instance-wide rather than per channel for
    /// the same reason `jwt.allow_private_jwks_urls` is: a per-channel opt-out
    /// would let the author of a definition grant themselves the egress the
    /// flag exists to gate.
    pub allow_private_token_urls: bool,

    /// Deployment-supplied identity providers, keyed by slug (#355).
    ///
    /// A channel whose `oauth2_login` block sets `providers_from_instance = true`
    /// merges these under its own `providers` map — the definition's own entries
    /// win a slug clash — so a deployment adds an identity provider by config and
    /// the definition promotes unchanged.
    ///
    /// **File-only.** A nested, arbitrary-key map does not fit the
    /// `ORION_SECTION__KEY` environment scheme, so an individual provider cannot
    /// be set through an environment variable (the same limit `models.runtimes`
    /// and `plugins.overrides` have). Values inside still take `${VAR}`
    /// substitution (applied to the whole file before it is parsed) and
    /// `env://NAME` references (resolved when the channel loads), which is how a
    /// per-deployment endpoint or secret is supplied.
    ///
    /// **Read once, at startup.** `state.config` is loaded at boot and is not
    /// re-read on an engine reload, so adding or changing a provider here takes
    /// effect on a process restart, not on a reload. (A channel's own
    /// `providers` map is reload-scoped, as always.)
    pub providers: BTreeMap<String, InstanceProviderConfig>,
}

/// One deployment-supplied identity provider (#355).
///
/// The per-provider half of a channel `oauth2_login` block, expressed in
/// instance TOML. It converts to the one compiled provider shape
/// (`channel::config::ProviderConfig`) at the merge point, so validation and
/// compilation run on a single shape whatever the source.
///
/// OIDC by explicit `id_token` config is not expressible here yet; a
/// deployment-supplied OIDC provider is configured through OIDC discovery
/// (`issuer`) once that lands. `env://NAME` in `client_secret` (and the URL
/// fields) resolves when the channel loads; `${VAR}` anywhere resolves before
/// the file is parsed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InstanceProviderConfig {
    /// `oidc` or `oauth2`; absent derives from whether OIDC applies (an
    /// `issuer`, or — at channel level — an `id_token` block).
    pub kind: Option<String>,
    /// The OIDC issuer. Set it (and omit the endpoints below) to have Orion
    /// discover them from `<issuer>/.well-known/openid-configuration` and verify
    /// the `id_token` — the usual shape for a deployment-supplied OIDC directory
    /// (Entra, Keycloak, Google Workspace).
    pub issuer: Option<String>,
    /// The provider's authorization endpoint (`https`).
    pub authorize_url: Option<String>,
    /// The provider's token endpoint (`https`).
    pub token_url: Option<String>,
    /// The OAuth2 client identifier.
    pub client_id: Option<String>,
    /// The OAuth2 client secret. `env://NAME` keeps it out of the config file.
    pub client_secret: Option<String>,
    /// How credentials are presented at the token endpoint: `basic` or `body`.
    /// Absent means `basic`.
    pub client_auth: Option<String>,
    /// A redirect-URI override; absent uses the channel block's shared
    /// `redirect_uri` template with `{provider}` filled in.
    pub redirect_uri: Option<String>,
    /// Requested scopes.
    pub scopes: Vec<String>,
    /// Extra authorize-URL parameters.
    pub extra_authorize_params: BTreeMap<String, String>,
}
