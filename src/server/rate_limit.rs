use std::sync::Arc;

use axum::extract::{MatchedPath, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::channel::{KeyedLimiter, build_keyed_limiter};
use crate::config::RateLimitConfig;
use crate::metrics;

/// Holds platform-level rate limiters (global defaults + endpoint-level).
/// Per-channel rate limiters live in `ChannelRegistry` and are built from DB config.
pub struct RateLimitState {
    default_limiter: Arc<KeyedLimiter>,
    admin_limiter: Option<Arc<KeyedLimiter>>,
    data_limiter: Option<Arc<KeyedLimiter>>,
}

impl RateLimitState {
    /// Build platform-level rate limiters from config.
    pub fn from_config(config: &RateLimitConfig) -> Self {
        let default_limiter = Arc::new(build_keyed_limiter(
            config.default_rps,
            config.default_burst,
        ));

        let admin_limiter = config
            .endpoints
            .admin_rps
            .map(|rps| Arc::new(build_keyed_limiter(rps, rps / 2 + 1)));

        let data_limiter = config
            .endpoints
            .data_rps
            .map(|rps| Arc::new(build_keyed_limiter(rps, rps / 2 + 1)));

        Self {
            default_limiter,
            admin_limiter,
            data_limiter,
        }
    }
}

/// Extract client IP from proxy headers or connection info.
fn extract_client_ip(req: &Request) -> String {
    // Try X-Forwarded-For first (may contain comma-separated list)
    if let Some(xff) = req.headers().get("x-forwarded-for")
        && let Ok(val) = xff.to_str()
        && let Some(first_ip) = val.split(',').next()
    {
        let ip = first_ip.trim();
        if !ip.is_empty() {
            return ip.to_string();
        }
    }
    // Try X-Real-IP
    if let Some(xri) = req.headers().get("x-real-ip")
        && let Ok(val) = xri.to_str()
    {
        let ip = val.trim();
        if !ip.is_empty() {
            return ip.to_string();
        }
    }
    "unknown".to_string()
}

/// Extract channel name from a data route path like `/api/v1/data/{channel}`.
fn extract_channel_from_path(uri_path: &str) -> Option<&str> {
    let path = uri_path.strip_prefix("/api/v1/data/")?;
    // Take the first segment after /api/v1/data/
    let channel = path.split('/').next()?;
    if channel.is_empty() || channel == "traces" {
        return None;
    }
    Some(channel)
}

/// Determine the route group from the matched path.
enum RouteGroup {
    Admin,
    Data,
    Operational,
}

fn classify_route(path: &str) -> RouteGroup {
    if path.starts_with("/api/v1/admin") {
        RouteGroup::Admin
    } else if path.starts_with("/api/v1/data") {
        RouteGroup::Data
    } else {
        RouteGroup::Operational
    }
}

/// Rate limiting middleware.
///
/// Per-channel limits are read from the `ChannelRegistry` (DB-driven, hot-reloaded).
/// Platform-level defaults (global, admin, data endpoint) come from `RateLimitState` (config file).
pub async fn rate_limit_middleware(
    State(state): State<crate::server::state::AppState>,
    matched_path: Option<MatchedPath>,
    req: Request,
    next: Next,
) -> Response {
    let rate_limit_state = match &state.rate_limit_state {
        Some(rls) => rls,
        None => return next.run(req).await,
    };

    let client_ip = extract_client_ip(&req);
    let path = matched_path
        .as_ref()
        .map(|m: &MatchedPath| m.as_str())
        .unwrap_or(req.uri().path());
    let route_group = classify_route(path);

    // For data routes, check per-channel limiter from ChannelRegistry first
    if let RouteGroup::Data = &route_group {
        let uri_path = req.uri().path().to_string();
        if let Some(channel) = extract_channel_from_path(&uri_path)
            && let Some(channel_config) = state.channel_registry.get_by_name(channel).await
            && let Some(ref limiter) = channel_config.rate_limiter
        {
            // Compute rate limit key from key_logic or default to client_ip
            let key = if let Some(ref compiled) = channel_config.rate_limit_key_logic {
                let context = build_rate_limit_context(&client_ip, channel, &req);
                match state.datalogic.evaluate(compiled, Arc::new(context)) {
                    Ok(val) => val.as_str().map(|s| s.to_string()).unwrap_or_else(|| {
                        serde_json::to_string(&val).unwrap_or_else(|_| client_ip.clone())
                    }),
                    Err(_) => client_ip.clone(),
                }
            } else {
                client_ip.clone()
            };

            if limiter.check_key(&key).is_err() {
                metrics::record_rate_limit_rejected(&client_ip);
                return rate_limited_response();
            }
            // Channel-specific limiter passed; skip the group/default limiter
            return next.run(req).await;
        }
    }

    // Fall through to platform-level limiter
    let limiter = match route_group {
        RouteGroup::Admin => rate_limit_state
            .admin_limiter
            .as_ref()
            .unwrap_or(&rate_limit_state.default_limiter),
        RouteGroup::Data => rate_limit_state
            .data_limiter
            .as_ref()
            .unwrap_or(&rate_limit_state.default_limiter),
        RouteGroup::Operational => &rate_limit_state.default_limiter,
    };

    if limiter.check_key(&client_ip).is_err() {
        metrics::record_rate_limit_rejected(&client_ip);
        return rate_limited_response();
    }

    next.run(req).await
}

/// Build the context object available to rate limit `key_logic` expressions.
///
/// Headers are included lazily — only allocated when key_logic actually uses them.
/// For simple key_logic that only references `client_ip` or `channel`, this
/// avoids O(n) header allocations per request.
fn build_rate_limit_context(client_ip: &str, channel: &str, req: &Request) -> Value {
    // Build headers lazily only if the request has them (always true, but
    // we keep only the common ones to reduce allocations).
    let mut headers = serde_json::Map::with_capacity(8);
    // Only serialize well-known headers that key_logic is likely to use
    const COMMON_HEADERS: &[&str] = &[
        "authorization",
        "x-api-key",
        "x-forwarded-for",
        "x-real-ip",
        "user-agent",
        "content-type",
        "origin",
        "x-tenant-id",
    ];
    for &name in COMMON_HEADERS {
        if let Some(value) = req.headers().get(name)
            && let Ok(v) = value.to_str()
        {
            headers.insert(name.to_string(), Value::String(v.to_string()));
        }
    }
    json!({
        "client_ip": client_ip,
        "channel": channel,
        "headers": headers,
    })
}

fn rate_limited_response() -> Response {
    let body = json!({
        "error": {
            "code": "RATE_LIMITED",
            "message": "Too many requests"
        }
    });
    (
        StatusCode::TOO_MANY_REQUESTS,
        [("retry-after", "1")],
        axum::Json(body),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;

    #[test]
    fn test_extract_client_ip_from_xff() {
        let req = Request::builder()
            .header("x-forwarded-for", "192.168.1.1, 10.0.0.1")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_client_ip(&req), "192.168.1.1");
    }

    #[test]
    fn test_extract_client_ip_from_xff_single() {
        let req = Request::builder()
            .header("x-forwarded-for", "203.0.113.5")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_client_ip(&req), "203.0.113.5");
    }

    #[test]
    fn test_extract_client_ip_from_x_real_ip() {
        let req = Request::builder()
            .header("x-real-ip", "10.0.0.5")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_client_ip(&req), "10.0.0.5");
    }

    #[test]
    fn test_extract_client_ip_xff_takes_priority_over_x_real_ip() {
        let req = Request::builder()
            .header("x-forwarded-for", "1.2.3.4")
            .header("x-real-ip", "5.6.7.8")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_client_ip(&req), "1.2.3.4");
    }

    #[test]
    fn test_extract_client_ip_no_headers() {
        let req = Request::builder().body(Body::empty()).unwrap();
        assert_eq!(extract_client_ip(&req), "unknown");
    }

    #[test]
    fn test_extract_client_ip_empty_xff() {
        let req = Request::builder()
            .header("x-forwarded-for", "")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_client_ip(&req), "unknown");
    }

    #[test]
    fn test_extract_client_ip_empty_x_real_ip() {
        let req = Request::builder()
            .header("x-real-ip", "  ")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_client_ip(&req), "unknown");
    }

    #[test]
    fn test_extract_channel_from_path_valid() {
        assert_eq!(
            extract_channel_from_path("/api/v1/data/orders"),
            Some("orders")
        );
    }

    #[test]
    fn test_extract_channel_from_path_with_subpath() {
        assert_eq!(
            extract_channel_from_path("/api/v1/data/orders/async"),
            Some("orders")
        );
    }

    #[test]
    fn test_extract_channel_from_path_traces_excluded() {
        assert_eq!(extract_channel_from_path("/api/v1/data/traces"), None);
    }

    #[test]
    fn test_extract_channel_from_path_wrong_prefix() {
        assert_eq!(extract_channel_from_path("/api/v1/admin/workflows"), None);
    }

    #[test]
    fn test_extract_channel_from_path_empty_channel() {
        assert_eq!(extract_channel_from_path("/api/v1/data/"), None);
    }

    #[test]
    fn test_classify_route_admin() {
        assert!(matches!(
            classify_route("/api/v1/admin/workflows"),
            RouteGroup::Admin
        ));
    }

    #[test]
    fn test_classify_route_data() {
        assert!(matches!(
            classify_route("/api/v1/data/orders"),
            RouteGroup::Data
        ));
    }

    #[test]
    fn test_classify_route_operational() {
        assert!(matches!(classify_route("/health"), RouteGroup::Operational));
        assert!(matches!(
            classify_route("/metrics"),
            RouteGroup::Operational
        ));
    }

    #[test]
    fn test_from_config_default() {
        let config = RateLimitConfig {
            enabled: true,
            default_rps: 100,
            default_burst: 50,
            ..Default::default()
        };
        let state = RateLimitState::from_config(&config);
        assert!(state.admin_limiter.is_none());
        assert!(state.data_limiter.is_none());
    }

    #[test]
    fn test_from_config_with_endpoint_limiters() {
        let config = RateLimitConfig {
            enabled: true,
            default_rps: 100,
            default_burst: 50,
            endpoints: crate::config::EndpointRateLimits {
                admin_rps: Some(20),
                data_rps: Some(200),
            },
            ..Default::default()
        };
        let state = RateLimitState::from_config(&config);
        assert!(state.admin_limiter.is_some());
        assert!(state.data_limiter.is_some());
    }

    #[test]
    fn test_rate_limited_response_status() {
        let response = rate_limited_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get("retry-after")
                .unwrap()
                .to_str()
                .unwrap(),
            "1"
        );
    }
}
