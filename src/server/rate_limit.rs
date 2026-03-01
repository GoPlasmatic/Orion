use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use axum::extract::{MatchedPath, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use governor::clock::DefaultClock;
use governor::state::keyed::DashMapStateStore;
use governor::{Quota, RateLimiter};
use serde_json::json;

use crate::config::RateLimitConfig;
use crate::metrics;

type KeyedLimiter = RateLimiter<String, DashMapStateStore<String>, DefaultClock>;

/// Holds all rate limiters for the application.
pub struct RateLimitState {
    default_limiter: Arc<KeyedLimiter>,
    admin_limiter: Option<Arc<KeyedLimiter>>,
    data_limiter: Option<Arc<KeyedLimiter>>,
    channel_limiters: HashMap<String, Arc<KeyedLimiter>>,
}

impl RateLimitState {
    /// Build rate limiters from config.
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

        let mut channel_limiters = HashMap::new();
        for (channel, limit) in &config.channels {
            let burst = limit.burst.unwrap_or(limit.rps / 2 + 1);
            channel_limiters.insert(
                channel.clone(),
                Arc::new(build_keyed_limiter(limit.rps, burst)),
            );
        }

        Self {
            default_limiter,
            admin_limiter,
            data_limiter,
            channel_limiters,
        }
    }
}

fn build_keyed_limiter(rps: u32, burst: u32) -> KeyedLimiter {
    let quota = Quota::per_second(NonZeroU32::new(rps).unwrap_or(NonZeroU32::MIN))
        .allow_burst(NonZeroU32::new(burst).unwrap_or(NonZeroU32::MIN));
    RateLimiter::dashmap(quota)
}

/// Extract client IP from proxy headers or connection info.
fn extract_client_ip(req: &Request) -> String {
    // Try X-Forwarded-For first (may contain comma-separated list)
    if let Some(xff) = req.headers().get("x-forwarded-for") {
        if let Ok(val) = xff.to_str() {
            if let Some(first_ip) = val.split(',').next() {
                let ip = first_ip.trim();
                if !ip.is_empty() {
                    return ip.to_string();
                }
            }
        }
    }
    // Try X-Real-IP
    if let Some(xri) = req.headers().get("x-real-ip") {
        if let Ok(val) = xri.to_str() {
            let ip = val.trim();
            if !ip.is_empty() {
                return ip.to_string();
            }
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

    // For data routes, check channel-specific limiter first
    if let RouteGroup::Data = &route_group {
        let uri_path = req.uri().path().to_string();
        if let Some(channel) = extract_channel_from_path(&uri_path) {
            if let Some(channel_limiter) = rate_limit_state.channel_limiters.get(channel) {
                let key = format!("{}:{}", client_ip, channel);
                if channel_limiter.check_key(&key).is_err() {
                    metrics::record_rate_limit_rejected(&client_ip);
                    return rate_limited_response();
                }
                // Channel-specific limiter passed; skip the group/default limiter
                return next.run(req).await;
            }
        }
    }

    // Pick the appropriate limiter
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
        assert_eq!(extract_channel_from_path("/api/v1/admin/rules"), None);
    }

    #[test]
    fn test_extract_channel_from_path_empty_channel() {
        assert_eq!(extract_channel_from_path("/api/v1/data/"), None);
    }

    #[test]
    fn test_classify_route_admin() {
        assert!(matches!(
            classify_route("/api/v1/admin/rules"),
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
        assert!(state.channel_limiters.is_empty());
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
    fn test_from_config_with_channel_limiters() {
        let mut channels = HashMap::new();
        channels.insert(
            "orders".to_string(),
            crate::config::ChannelRateLimit {
                rps: 50,
                burst: Some(25),
            },
        );
        channels.insert(
            "events".to_string(),
            crate::config::ChannelRateLimit {
                rps: 10,
                burst: None,
            },
        );
        let config = RateLimitConfig {
            enabled: true,
            default_rps: 100,
            default_burst: 50,
            channels,
            ..Default::default()
        };
        let state = RateLimitState::from_config(&config);
        assert_eq!(state.channel_limiters.len(), 2);
        assert!(state.channel_limiters.contains_key("orders"));
        assert!(state.channel_limiters.contains_key("events"));
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
