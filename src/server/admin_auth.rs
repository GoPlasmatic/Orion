//! Admin API authentication middleware.
//!
//! When enabled, requires a valid API key for all `/api/v1/admin/*` endpoints
//! and the `/metrics` endpoint.
//! Supports `Authorization: Bearer <token>` or custom header (e.g. `X-API-Key: <token>`).

use axum::extract::{MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;

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

    if !path.starts_with("/api/v1/admin") && path != "/metrics" {
        return Ok(next.run(req).await);
    }

    let token = extract_api_key(&req, &state.config.admin_auth)?;

    let matched_key = state
        .config
        .admin_auth
        .effective_keys()
        .into_iter()
        .find(|key| constant_time_eq(token.as_bytes(), key.as_bytes()));

    if matched_key.is_none() {
        metrics::record_error("auth_failure");
        tracing::warn!(
            path = %req.uri().path(),
            "Admin API authentication failed: invalid API key"
        );
        return Err(OrionError::Unauthorized("Invalid API key".into()));
    }

    // Store principal identity in request extensions for audit logging
    req.extensions_mut()
        .insert(AdminPrincipal::from_token(&token));

    Ok(next.run(req).await)
}

/// Extract the API key from the request based on the configured header.
fn extract_api_key(req: &Request, config: &AdminAuthConfig) -> Result<String, OrionError> {
    let header_value = req
        .headers()
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

/// Constant-time string comparison to prevent timing attacks.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constant_time_eq_same() {
        assert!(constant_time_eq(b"secret", b"secret"));
    }

    #[test]
    fn test_constant_time_eq_different() {
        assert!(!constant_time_eq(b"secret", b"wrong!"));
    }

    #[test]
    fn test_constant_time_eq_different_length() {
        assert!(!constant_time_eq(b"short", b"longer"));
    }

    #[test]
    fn test_constant_time_eq_empty() {
        assert!(constant_time_eq(b"", b""));
    }
}
