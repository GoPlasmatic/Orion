use serde::{Deserialize, Serialize};

/// Instance-wide policy for the inbound OAuth2 sign-in flow (#307).
///
/// Everything that describes *one* identity-provider relationship — the
/// endpoints, the client credentials, the scopes, PKCE, the state cookie —
/// belongs to the channel's `oauth2_login` block, because it is part of the
/// definition and is promoted with it. What lives here is the operator's egress
/// policy, which is a property of the deployment.
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
}
