use serde::{Deserialize, Serialize};

/// JWT verification settings that are instance-wide rather than per channel or
/// per task.
///
/// The per-surface knobs (algorithms, issuer, audience, leeway, the JWKS URL
/// itself) belong to the channel's `auth` block or the `jwt_verify` task,
/// because they describe *one* issuer relationship. What lives here is the
/// operator's egress policy for fetching keys, which is a property of the
/// deployment.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JwtConfig {
    /// Allow a `jwks_url` that resolves to a private or link-local address.
    ///
    /// Off by default: `jwks_url` is authored input — a channel's `auth` block
    /// or a `jwt_verify` task field — and it is the only egress path in the
    /// runtime that does not go through an operator-configured connector. With
    /// this off, [`crate::validation::validate_url_not_private`] runs on every
    /// fetch, exactly as it does for `http` connectors without
    /// `allow_private_urls`.
    ///
    /// Turn it on for an in-cluster issuer (a Keycloak on a service address, a
    /// sidecar), which is a legitimate and common shape. It is instance-wide
    /// because the alternative — a per-channel opt-out — would let the author
    /// of a definition grant themselves the egress the flag exists to gate.
    pub allow_private_jwks_urls: bool,
}
