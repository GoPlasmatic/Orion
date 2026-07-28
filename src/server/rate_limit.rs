use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use ipnet::IpNet;
use serde_json::{Value, json};

use crate::channel::{KeyedLimiter, build_keyed_limiter};
use crate::config::RateLimitConfig;
use crate::metrics;
use crate::server::AppState;

/// Holds platform-level rate limiters (global defaults + endpoint-level).
/// Per-channel rate limiters live in `ChannelRegistry` and are built from DB config.
pub struct RateLimitState {
    default_limiter: Arc<KeyedLimiter>,
    admin_limiter: Option<Arc<KeyedLimiter>>,
    data_limiter: Option<Arc<KeyedLimiter>>,
    pub(crate) trusted_proxies: Vec<IpNet>,
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
            trusted_proxies: config.parsed_trusted_proxies(),
        }
    }
}

/// Resolve the client identity used as the rate-limit key (S8).
///
/// The direct peer IP (from `ConnectInfo`) is authoritative. Forwarded
/// headers are honored only when the peer is inside `trusted_proxies` —
/// otherwise any client could mint a fresh rate-limit bucket per request by
/// spoofing `X-Forwarded-For`. When no `ConnectInfo` is present (e.g.
/// `tower::oneshot` in tests), falls back to the header-only behavior.
pub(crate) fn extract_client_ip(req: &Request, trusted_proxies: &[IpNet]) -> String {
    // to_canonical: a server bound on `[::]` sees IPv4 peers as v4-mapped
    // IPv6 (`::ffff:1.2.3.4`), which would never match an IPv4 CIDR.
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().to_canonical());
    match peer {
        Some(ip) if peer_is_trusted(&ip, trusted_proxies) => {
            forwarded_client_ip(req).unwrap_or_else(|| ip.to_string())
        }
        Some(ip) => ip.to_string(),
        None => forwarded_client_ip(req).unwrap_or_else(|| "unknown".to_string()),
    }
}

fn peer_is_trusted(peer: &IpAddr, trusted_proxies: &[IpNet]) -> bool {
    trusted_proxies.iter().any(|net| net.contains(peer))
}

/// Client IP claimed by proxy headers: first `X-Forwarded-For` hop, else
/// `X-Real-IP` (shared policy in `engine::utils`). Only meaningful when the
/// direct peer is a trusted proxy.
fn forwarded_client_ip(req: &Request) -> Option<String> {
    crate::engine::utils::first_forwarded_value(|name| {
        req.headers().get(name).and_then(|v| v.to_str().ok())
    })
    .map(str::to_string)
}

/// Normalise a data-plane URI to the route path `dynamic_handler` resolves
/// against: the `/api/v1/data/` prefix and any `/async` suffix removed.
///
/// Returns `None` for the trace endpoints, which are real routes rather than
/// channels.
fn data_route_path(uri_path: &str) -> Option<&str> {
    let path = uri_path.strip_prefix("/api/v1/data/")?;
    let path = path.strip_suffix("/async").unwrap_or(path);
    let path = path.trim_matches('/').trim();
    if path.is_empty() || path == "traces" || path.starts_with("traces/") {
        return None;
    }
    Some(path)
}

/// Resolve which channel a data request targets, by the same two rules
/// `dynamic_handler` uses: the REST route table first, then a bare channel
/// name.
///
/// S9: this used to take the first path segment and look it up by name. For a
/// REST-routed channel that segment is the route prefix (`orders` in
/// `GET /orders/{id}`), not the channel name, so the lookup missed, the
/// per-channel limiter was never found, and the channel silently fell through
/// to the platform-wide limiter — the configured limit simply did not apply.
async fn resolve_data_channel(state: &AppState, method: &str, uri_path: &str) -> Option<String> {
    let route_path = data_route_path(uri_path)?;
    if let Some(matched) = state.channel_registry.match_route(method, route_path).await {
        return Some(matched.channel_name);
    }
    // Backward-compatible single-segment channel name, as in `dynamic_handler`.
    if !route_path.contains('/') {
        return Some(route_path.to_string());
    }
    None
}

/// Determine the route group from the matched path.
enum RouteGroup {
    Admin,
    Data,
    Operational,
}

impl RouteGroup {
    /// Bounded label for rejection metrics (O1) — never client-derived.
    fn label(&self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Data => "data",
            Self::Operational => "operational",
        }
    }
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

    let client_ip = extract_client_ip(&req, &rate_limit_state.trusted_proxies);
    let path = matched_path
        .as_ref()
        .map(|m: &MatchedPath| m.as_str())
        .unwrap_or(req.uri().path());
    let route_group = classify_route(path);

    // For data routes, check per-channel limiter from ChannelRegistry first
    if let RouteGroup::Data = &route_group {
        let uri_path = req.uri().path().to_string();
        let method = req.method().as_str().to_string();
        if let Some(channel) = resolve_data_channel(&state, &method, &uri_path).await
            && let Some(channel_config) = state.channel_registry.get_by_name(&channel).await
            && let Some(ref limiter) = channel_config.rate_limiter
        {
            let channel = channel.as_str();
            // Compute rate limit key from key_logic or default to client_ip
            let key = if let Some(ref compiled) = channel_config.rate_limit_key_logic {
                let context = build_rate_limit_context(&client_ip, channel, &req);
                match state
                    .datalogic
                    .session()
                    .eval_into::<serde_json::Value, _>(compiled, &context)
                {
                    Ok(val) => val.as_str().map(|s| s.to_string()).unwrap_or_else(|| {
                        serde_json::to_string(&val).unwrap_or_else(|_| client_ip.clone())
                    }),
                    // N5: falling back to client_ip here would silently
                    // re-dimension the limit mid-flight. The key is part of
                    // the control, so a request whose key cannot be computed
                    // is rejected rather than counted in the wrong bucket.
                    Err(e) => {
                        tracing::warn!(
                            channel = %channel,
                            error = %e,
                            "rate_limit.key_logic evaluation failed; rejecting request"
                        );
                        metrics::record_rate_limit_rejected(channel);
                        return rate_limited_response();
                    }
                }
            } else {
                client_ip.clone()
            };

            if !limiter.check(key).await {
                // Registry-confirmed channel name — bounded cardinality (O1)
                metrics::record_rate_limit_rejected(channel);
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
        metrics::record_rate_limit_rejected(route_group.label());
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
    // Route through OrionError so the 429 carries the same envelope
    // (request_id included) as every other error, then add retry-after.
    let mut resp =
        crate::errors::OrionError::RateLimited("Too many requests".to_string()).into_response();
    resp.headers_mut()
        .insert("retry-after", axum::http::HeaderValue::from_static("1"));
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};

    /// Attach a `ConnectInfo` peer address, as the serve layer does at runtime.
    fn with_peer(mut req: Request<Body>, peer: &str) -> Request<Body> {
        let addr: SocketAddr = peer.parse().expect("test");
        req.extensions_mut().insert(ConnectInfo(addr));
        req
    }

    fn nets(entries: &[&str]) -> Vec<IpNet> {
        entries
            .iter()
            .map(|e| e.parse::<IpNet>().expect("test"))
            .collect()
    }

    // -- No ConnectInfo (tower::oneshot tests): legacy header-only fallback --

    #[test]
    fn test_extract_client_ip_from_xff() {
        let req = Request::builder()
            .header("x-forwarded-for", "192.168.1.1, 10.0.0.1")
            .body(Body::empty())
            .expect("test");
        assert_eq!(extract_client_ip(&req, &[]), "192.168.1.1");
    }

    #[test]
    fn test_extract_client_ip_from_xff_single() {
        let req = Request::builder()
            .header("x-forwarded-for", "203.0.113.5")
            .body(Body::empty())
            .expect("test");
        assert_eq!(extract_client_ip(&req, &[]), "203.0.113.5");
    }

    #[test]
    fn test_extract_client_ip_from_x_real_ip() {
        let req = Request::builder()
            .header("x-real-ip", "10.0.0.5")
            .body(Body::empty())
            .expect("test");
        assert_eq!(extract_client_ip(&req, &[]), "10.0.0.5");
    }

    #[test]
    fn test_extract_client_ip_xff_takes_priority_over_x_real_ip() {
        let req = Request::builder()
            .header("x-forwarded-for", "1.2.3.4")
            .header("x-real-ip", "5.6.7.8")
            .body(Body::empty())
            .expect("test");
        assert_eq!(extract_client_ip(&req, &[]), "1.2.3.4");
    }

    #[test]
    fn test_extract_client_ip_no_headers() {
        let req = Request::builder().body(Body::empty()).expect("test");
        assert_eq!(extract_client_ip(&req, &[]), "unknown");
    }

    #[test]
    fn test_extract_client_ip_empty_xff() {
        let req = Request::builder()
            .header("x-forwarded-for", "")
            .body(Body::empty())
            .expect("test");
        assert_eq!(extract_client_ip(&req, &[]), "unknown");
    }

    #[test]
    fn test_extract_client_ip_empty_x_real_ip() {
        let req = Request::builder()
            .header("x-real-ip", "  ")
            .body(Body::empty())
            .expect("test");
        assert_eq!(extract_client_ip(&req, &[]), "unknown");
    }

    // -- With ConnectInfo: trusted-proxy gating (S8) --

    #[test]
    fn test_untrusted_peer_ignores_forwarded_headers() {
        let req = Request::builder()
            .header("x-forwarded-for", "1.2.3.4")
            .header("x-real-ip", "5.6.7.8")
            .body(Body::empty())
            .expect("test");
        let req = with_peer(req, "203.0.113.9:5000");
        // Default empty trust list: spoofed headers cannot mint an identity
        assert_eq!(extract_client_ip(&req, &[]), "203.0.113.9");
    }

    #[test]
    fn test_trusted_peer_uses_forwarded_header() {
        let req = Request::builder()
            .header("x-forwarded-for", "1.2.3.4, 10.0.0.1")
            .body(Body::empty())
            .expect("test");
        let req = with_peer(req, "10.0.0.7:5000");
        assert_eq!(extract_client_ip(&req, &nets(&["10.0.0.0/8"])), "1.2.3.4");
    }

    #[test]
    fn test_trusted_peer_without_headers_falls_back_to_peer() {
        let req = Request::builder().body(Body::empty()).expect("test");
        let req = with_peer(req, "10.0.0.7:5000");
        assert_eq!(extract_client_ip(&req, &nets(&["10.0.0.0/8"])), "10.0.0.7");
    }

    #[test]
    fn test_peer_outside_trusted_cidr_ignores_headers() {
        let req = Request::builder()
            .header("x-forwarded-for", "1.2.3.4")
            .body(Body::empty())
            .expect("test");
        let req = with_peer(req, "192.0.2.33:5000");
        assert_eq!(
            extract_client_ip(&req, &nets(&["10.0.0.0/8"])),
            "192.0.2.33"
        );
    }

    #[test]
    fn test_v4_mapped_v6_peer_matches_v4_cidr() {
        // A server bound on [::] sees IPv4 peers as ::ffff:a.b.c.d
        let req = Request::builder()
            .header("x-forwarded-for", "1.2.3.4")
            .body(Body::empty())
            .expect("test");
        let req = with_peer(req, "[::ffff:10.0.0.7]:5000");
        assert_eq!(extract_client_ip(&req, &nets(&["10.0.0.0/8"])), "1.2.3.4");
    }

    #[test]
    fn test_ipv6_trusted_proxy() {
        let req = Request::builder()
            .header("x-real-ip", "2001:db8::1")
            .body(Body::empty())
            .expect("test");
        let req = with_peer(req, "[fd00::1]:5000");
        assert_eq!(extract_client_ip(&req, &nets(&["fd00::/8"])), "2001:db8::1");
    }

    #[test]
    fn test_data_route_path_valid() {
        assert_eq!(data_route_path("/api/v1/data/orders"), Some("orders"));
    }

    /// The `/async` suffix is stripped, not treated as a second segment —
    /// otherwise `orders/async` would never match the `orders` route (S9).
    #[test]
    fn test_data_route_path_strips_async_suffix() {
        assert_eq!(data_route_path("/api/v1/data/orders/async"), Some("orders"));
        assert_eq!(
            data_route_path("/api/v1/data/orders/42/async"),
            Some("orders/42")
        );
    }

    /// S9: the multi-segment path is kept whole so the route table can match
    /// it. The old helper returned just `"orders"` here, which is the route
    /// prefix rather than the channel name.
    #[test]
    fn test_data_route_path_keeps_rest_segments() {
        assert_eq!(
            data_route_path("/api/v1/data/orders/ORD-1"),
            Some("orders/ORD-1")
        );
    }

    #[test]
    fn test_data_route_path_traces_excluded() {
        assert_eq!(data_route_path("/api/v1/data/traces"), None);
        assert_eq!(data_route_path("/api/v1/data/traces/abc123"), None);
    }

    #[test]
    fn test_data_route_path_wrong_prefix() {
        assert_eq!(data_route_path("/api/v1/admin/workflows"), None);
    }

    #[test]
    fn test_data_route_path_empty_channel() {
        assert_eq!(data_route_path("/api/v1/data/"), None);
    }

    /// `/api/v1/data/async` is a channel *named* `async`, not an async
    /// submission with no channel — only a `/async` suffix on a non-empty
    /// path is a submission. Mirrors `dynamic_handler`, whose `strip_suffix`
    /// does not match a bare `"async"` either.
    #[test]
    fn test_data_route_path_bare_async_is_a_channel_name() {
        assert_eq!(data_route_path("/api/v1/data/async"), Some("async"));
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
        // The admin plane gets its own, tighter limiter by default (S12) —
        // it used to share the anonymous data plane's 100/s budget.
        assert!(
            state.admin_limiter.is_some(),
            "admin_rps must default to a real limit"
        );
        assert!(state.data_limiter.is_none());
    }

    #[test]
    fn test_admin_rps_can_be_explicitly_unset() {
        // `admin_rps = null` still means "no separate limit".
        let config = RateLimitConfig {
            enabled: true,
            endpoints: crate::config::EndpointRateLimits {
                admin_rps: None,
                data_rps: None,
            },
            ..Default::default()
        };
        let state = RateLimitState::from_config(&config);
        assert!(state.admin_limiter.is_none());
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
    fn test_from_config_parses_trusted_proxies() {
        let config = RateLimitConfig {
            enabled: true,
            trusted_proxies: vec!["10.0.0.0/8".to_string(), "192.168.1.1".to_string()],
            ..Default::default()
        };
        let state = RateLimitState::from_config(&config);
        assert_eq!(state.trusted_proxies.len(), 2);
    }

    #[test]
    fn test_rate_limited_response_status() {
        let response = rate_limited_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get("retry-after")
                .expect("test")
                .to_str()
                .expect("test"),
            "1"
        );
    }
}
