//! Admin API authentication middleware.
//!
//! When enabled, requires a valid API key for all `/api/v1/admin/*` endpoints,
//! the `/metrics` endpoint, and the trace read endpoints under
//! `/api/v1/data/traces` (traces expose full request/response payloads).
//! Supports `Authorization: Bearer <token>` or custom header (e.g. `X-API-Key: <token>`).

use axum::extract::{MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use sha2::{Digest, Sha256};

use crate::config::AdminAuthConfig;
use crate::errors::OrionError;
use crate::metrics;
use crate::server::state::AppState;

/// Identity of the authenticated admin principal, stored in request extensions.
#[derive(Debug, Clone)]
pub struct AdminPrincipal {
    /// Truncated key prefix for audit logging (never the full key).
    pub key_prefix: String,
}

impl AdminPrincipal {
    fn from_token(token: &str) -> Self {
        let prefix_len = token.len().min(8);
        Self {
            key_prefix: format!("{}...", &token[..prefix_len]),
        }
    }

    /// For keys configured in the `sha256:` hash-at-rest form: identify by a
    /// prefix of the (already public-at-rest) hash rather than leak plaintext
    /// characters of the presented token into audit logs.
    fn from_digest(digest: &[u8; 32]) -> Self {
        Self {
            key_prefix: format!("sha256:{}...", hex::encode(&digest[..4])),
        }
    }
}

/// True when `path` — an Axum `MatchedPath` template such as
/// `/api/v1/admin/channels/{id}` — is behind admin authentication.
///
/// The trace *list* returns rows for every caller, so it is guarded like
/// admin routes. The single-trace GET is **not** in this set (R12): its
/// handler enforces its own two-lane rule — a valid admin credential, or the
/// per-submission capability token returned with the async 202 — so a
/// data-plane caller can poll its own result without holding an admin key.
/// Channel traffic cannot collide with the traces prefix: its `MatchedPath`
/// is always the `/api/v1/data/{*path}` catch-all template.
///
/// The OpenAPI `SecurityAddon` (`server::routes::openapi`) applies the spec's
/// `security` requirement through this same predicate, so the documented
/// surface cannot drift from what the middleware enforces. The templates it
/// feeds in are OpenAPI path keys, which are byte-identical to Axum's.
pub(crate) fn is_guarded_path(path: &str) -> bool {
    path.starts_with("/api/v1/admin") || path == "/metrics" || path == "/api/v1/data/traces"
}

/// Middleware that authenticates admin API requests.
///
/// Skips authentication for non-admin routes and when auth is disabled.
pub async fn admin_auth_middleware(
    State(state): State<AppState>,
    matched_path: Option<MatchedPath>,
    mut req: Request,
    next: Next,
) -> Result<Response, OrionError> {
    if !state.config.admin_auth.enabled {
        return Ok(next.run(req).await);
    }

    let path = matched_path
        .as_ref()
        .map(|m| m.as_str())
        .unwrap_or(req.uri().path());

    if !is_guarded_path(path) {
        return Ok(next.run(req).await);
    }

    let token = extract_api_key(req.headers(), &state.config.admin_auth)?;

    // Compare SHA-256 digests instead of raw keys: fixed width, so timing
    // reveals neither key length nor content (S11).
    let presented: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    let matched_key = state
        .config
        .admin_auth
        .admin_keys()
        .into_iter()
        .find(|key| constant_time_eq(&presented, &key.digest));

    let Some(matched_key) = matched_key else {
        metrics::record_error("auth_failure");
        tracing::warn!(
            path = %req.uri().path(),
            "Admin API authentication failed: invalid API key"
        );
        return Err(OrionError::Unauthorized("Invalid API key".into()));
    };

    // Store principal identity in request extensions for audit logging
    let principal = if matched_key.hashed {
        AdminPrincipal::from_digest(&matched_key.digest)
    } else {
        AdminPrincipal::from_token(&token)
    };
    req.extensions_mut().insert(principal);

    Ok(next.run(req).await)
}

/// True when the request headers present a valid admin credential. For
/// surfaces that stay reachable without auth but serve a reduced body to
/// anonymous callers (O9: `/health`'s topology detail). Failures are not an
/// error here — they just mean "anonymous" — so nothing is logged or counted.
pub(crate) fn headers_present_valid_key(
    headers: &axum::http::HeaderMap,
    config: &AdminAuthConfig,
) -> bool {
    let Ok(token) = extract_api_key(headers, config) else {
        return false;
    };
    let presented: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    config
        .admin_keys()
        .into_iter()
        .any(|key| constant_time_eq(&presented, &key.digest))
}

/// SHA-256 hex of an async-trace capability token (R12). The trace row
/// stores this instead of the token itself.
pub(crate) fn hash_trace_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Constant-time check of a presented trace token against the stored hash.
pub(crate) fn trace_token_matches(presented: &str, stored_hash: &str) -> bool {
    let presented: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
    let Ok(decoded) = hex::decode(stored_hash) else {
        return false;
    };
    let Ok(stored) = <[u8; 32]>::try_from(decoded) else {
        return false;
    };
    constant_time_eq(&presented, &stored)
}

/// Extract the API key from the request headers based on the configured header.
fn extract_api_key(
    headers: &axum::http::HeaderMap,
    config: &AdminAuthConfig,
) -> Result<String, OrionError> {
    let header_value = headers
        .get(&config.header)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| OrionError::Unauthorized(format!("Missing {} header", config.header)))?;

    if config.header.eq_ignore_ascii_case("authorization") {
        // Expect "Bearer <token>" format
        header_value
            .strip_prefix("Bearer ")
            .or_else(|| header_value.strip_prefix("bearer "))
            .map(|t| t.to_string())
            .ok_or_else(|| {
                OrionError::Unauthorized(
                    "Authorization header must use 'Bearer <token>' format".into(),
                )
            })
    } else {
        // Custom header — use raw value
        Ok(header_value.to_string())
    }
}

/// Constant-time comparison of two SHA-256 digests. Digests are fixed width,
/// so there is no length branch for a timing side channel to observe (S11).
fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(s: &str) -> [u8; 32] {
        Sha256::digest(s.as_bytes()).into()
    }

    #[test]
    fn test_digest_compare_equal() {
        assert!(constant_time_eq(&digest("secret"), &digest("secret")));
    }

    #[test]
    fn test_digest_compare_unequal() {
        assert!(!constant_time_eq(&digest("secret"), &digest("wrong!")));
    }

    #[test]
    fn test_digest_compare_length_differs() {
        // Tokens of different lengths still produce fixed-width digests, so
        // the comparison runs to completion instead of early-returning.
        assert!(!constant_time_eq(
            &digest("short"),
            &digest("a-much-longer-candidate-key")
        ));
    }

    #[test]
    fn test_digest_compare_empty_token() {
        assert!(constant_time_eq(&digest(""), &digest("")));
        assert!(!constant_time_eq(&digest(""), &digest("secret")));
    }

    #[test]
    fn test_sha256_config_form_matches_presented_plaintext() {
        // Operator stores sha256:<hex>; client presents the plaintext key.
        let config = AdminAuthConfig {
            enabled: true,
            api_keys: vec![format!("sha256:{}", hex::encode(digest("the-real-key")))],
            header: "Authorization".to_string(),
        };
        let presented = digest("the-real-key");
        let keys = config.admin_keys();
        assert!(keys.iter().any(|k| constant_time_eq(&presented, &k.digest)));
        let wrong = digest("not-the-key");
        assert!(!keys.iter().any(|k| constant_time_eq(&wrong, &k.digest)));
    }

    #[test]
    fn test_principal_prefix_for_hashed_key_uses_hash() {
        let d = digest("the-real-key");
        let principal = AdminPrincipal::from_digest(&d);
        assert!(principal.key_prefix.starts_with("sha256:"));
        assert!(!principal.key_prefix.contains("the-real"));
    }
}
