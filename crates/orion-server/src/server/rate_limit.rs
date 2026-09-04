//! Platform-level rate limiting middleware.
//!
//! S15/N16: the **per-channel** limiter used to live here too, which is why
//! it applied to HTTP ingress and nothing else — this middleware pulls
//! `AppState` and resolves the target channel from the URI, neither of which
//! a Kafka record or an in-process `channel_call` has. It moved to
//! `channel::guards`, where [`crate::channel::guards::apply_guards`] runs it
//! on every transport. What is left here is the platform budget
//! (`[rate_limit]`), keyed by client IP and applied per route group.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use ipnet::IpNet;

use crate::channel::{KeyedLimiter, build_keyed_limiter};
use crate::config::RateLimitConfig;
use crate::metrics;

/// Holds platform-level rate limiters (global defaults + endpoint-level).
/// Per-channel rate limiters live in `ChannelSnapshot` and are applied by the
/// ingress guards, not here.
///
/// `trusted_proxies` deliberately does **not** live here even though the key
/// is spelled `rate_limit.trusted_proxies`: it decides whether a forwarded
/// header may name the client, which the per-channel limit asks with the
/// platform limiter off. It is on `AppState`
/// ([`crate::server::state::AppStateInner::trusted_proxies`]).
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

/// Resolve the client identity used as the rate-limit key (S8).
///
/// The direct peer IP (from `ConnectInfo`) is authoritative. Forwarded
/// headers are honored only when the peer is inside `trusted_proxies` —
/// otherwise any client could mint a fresh rate-limit bucket per request by
/// spoofing `X-Forwarded-For`. When no `ConnectInfo` is present (e.g.
/// `tower::oneshot` in tests), falls back to the header-only behaviour.
pub(crate) fn extract_client_ip(req: &Request, trusted_proxies: &[IpNet]) -> String {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0);
    client_ip_from_parts(peer.as_ref(), req.headers(), trusted_proxies)
}

/// [`extract_client_ip`] against a peer address and header map rather than a
/// whole `Request`, so the data-plane handler — which has already consumed
/// the body — can identify the caller for its channel's rate limit the same
/// way the middleware does for the platform's (S15).
pub(crate) fn client_ip_from_parts(
    peer: Option<&SocketAddr>,
    headers: &axum::http::HeaderMap,
    trusted_proxies: &[IpNet],
) -> String {
    // to_canonical: a server bound on `[::]` sees IPv4 peers as v4-mapped
    // IPv6 (`::ffff:1.2.3.4`), which would never match an IPv4 CIDR.
    match peer.map(|p| p.ip().to_canonical()) {
        Some(ip) if peer_is_trusted(&ip, trusted_proxies) => {
            forwarded_client_ip(headers, trusted_proxies).unwrap_or_else(|| ip.to_string())
        }
        Some(ip) => ip.to_string(),
        None => {
            forwarded_client_ip(headers, trusted_proxies).unwrap_or_else(|| "unknown".to_string())
        }
    }
}

fn peer_is_trusted(peer: &IpAddr, trusted_proxies: &[IpNet]) -> bool {
    trusted_proxies.iter().any(|net| net.contains(peer))
}

/// Client IP claimed by `X-Forwarded-For`, resolved right to left (S8).
///
/// Each proxy appends the peer it accepted from, so the rightmost hop is the
/// only one written by our own trusted proxy — everything further left is
/// whatever the client chose to send. Walk from the right, skip hops that are
/// themselves trusted proxies (chained LB/CDN), and take the first hop
/// outside the trust list; if every hop is trusted, the leftmost one is the
/// origin. Taking the leftmost hop unconditionally would let any client
/// behind a real proxy mint a fresh rate-limit identity per request and
/// plant a chosen IP in audit logs. Falls back to `X-Real-IP` — proxy-set,
/// not appended-to — when no usable XFF hop exists.
fn forwarded_client_ip(
    headers: &axum::http::HeaderMap,
    trusted_proxies: &[IpNet],
) -> Option<String> {
    let xff_client = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|xff| {
            let mut candidate = None;
            for hop in xff
                .split(',')
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .rev()
            {
                candidate = Some(hop);
                let trusted = hop
                    .parse::<IpAddr>()
                    .is_ok_and(|ip| peer_is_trusted(&ip.to_canonical(), trusted_proxies));
                if !trusted {
                    break;
                }
            }
            candidate
        });
    if let Some(hop) = xff_client {
        return Some(hop.to_string());
    }
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
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

/// Which platform budget a request is metered against.
///
/// `data_mounts` participates because the data plane may answer outside
/// `/api/v1/data`: without it, root-mounted data traffic falls through to
/// `Operational` and is metered against the default limiter rather than
/// `rate_limit.endpoints.data_rps` — the channel's own limit still applies,
/// but the platform budget the operator configured does not.
fn classify_route(path: &str, data_mounts: &[String]) -> RouteGroup {
    if path.starts_with("/api/v1/admin") {
        RouteGroup::Admin
    } else if path.starts_with("/api/v1/data") || under_a_data_mount(path, data_mounts) {
        RouteGroup::Data
    } else {
        RouteGroup::Operational
    }
}

/// Whether `path` is served by one of the configured data mounts.
///
/// `"/"` matches everything except the platform routes, which are checked
/// first by the caller — a static route always wins over the catch-all, so a
/// request reaching here under a root mount really is data-plane traffic.
fn under_a_data_mount(path: &str, data_mounts: &[String]) -> bool {
    data_mounts
        .iter()
        .any(|m| m == "/" || crate::server::routes::path_claims(m, path))
}

/// Platform rate limiting middleware.
///
/// Applies the config-file budget (`[rate_limit]`) keyed by client IP, per
/// route group. Per-channel limits are enforced by the ingress guards, on
/// every transport rather than only this one (S15) — a data request is
/// therefore metered twice: once here against the platform budget, once in
/// `apply_guards` against its channel's own limit.
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

    let client_ip = extract_client_ip(&req, state.trusted_proxies());
    // Same rule as admin auth: under `server.data_mounts` the matched path is
    // a catch-all, which classifies as `Operational` and would meter
    // root-mounted data traffic against the default limiter instead of
    // `rate_limit.endpoints.data_rps`.
    let path = match matched_path.as_ref().map(|m: &MatchedPath| m.as_str()) {
        Some(p) if !crate::server::admin_auth::is_data_catch_all(p) => p,
        _ => req.uri().path(),
    };
    let route_group = classify_route(path, &state.config.server.data_mounts);

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

/// The 429 every limiter answers with: the standard error envelope
/// (`request_id` included) plus `retry-after`, both supplied by
/// `OrionError::into_response` so the guard-side 429 and this one are the
/// same response.
fn rate_limited_response() -> Response {
    crate::errors::OrionError::RateLimited("Too many requests".to_string()).into_response()
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
        // Rightmost hop: the only one a proxy (not the client) appended.
        let req = Request::builder()
            .header("x-forwarded-for", "192.168.1.1, 10.0.0.1")
            .body(Body::empty())
            .expect("test");
        assert_eq!(extract_client_ip(&req, &[]), "10.0.0.1");
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
        // Chained trusted proxies: 10.0.0.1 is itself trusted, so the walk
        // continues left to the real client.
        let req = Request::builder()
            .header("x-forwarded-for", "1.2.3.4, 10.0.0.1")
            .body(Body::empty())
            .expect("test");
        let req = with_peer(req, "10.0.0.7:5000");
        assert_eq!(extract_client_ip(&req, &nets(&["10.0.0.0/8"])), "1.2.3.4");
    }

    /// S8: the leftmost XFF hop is client-supplied — a trusted proxy only
    /// *appends*. A spoofed prefix must not become the identity.
    #[test]
    fn test_trusted_peer_ignores_client_supplied_xff_prefix() {
        let req = Request::builder()
            .header("x-forwarded-for", "6.6.6.6, 198.51.100.7")
            .body(Body::empty())
            .expect("test");
        let req = with_peer(req, "10.0.0.7:5000");
        assert_eq!(
            extract_client_ip(&req, &nets(&["10.0.0.0/8"])),
            "198.51.100.7"
        );
    }

    #[test]
    fn test_all_trusted_xff_resolves_to_leftmost() {
        // Every hop trusted: the leftmost is the origin.
        let req = Request::builder()
            .header("x-forwarded-for", "10.0.0.2, 10.0.0.3")
            .body(Body::empty())
            .expect("test");
        let req = with_peer(req, "10.0.0.7:5000");
        assert_eq!(extract_client_ip(&req, &nets(&["10.0.0.0/8"])), "10.0.0.2");
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
    fn test_classify_route_admin() {
        assert!(matches!(
            classify_route("/api/v1/admin/workflows", &[]),
            RouteGroup::Admin
        ));
    }

    #[test]
    fn test_classify_route_data() {
        assert!(matches!(
            classify_route("/api/v1/data/orders", &[]),
            RouteGroup::Data
        ));
    }

    #[test]
    fn test_classify_route_operational() {
        assert!(matches!(
            classify_route("/health", &[]),
            RouteGroup::Operational
        ));
        assert!(matches!(
            classify_route("/metrics", &[]),
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

    /// S15: the trust list is parsed from `[rate_limit]` but is *not* gated on
    /// `rate_limit.enabled`, because the per-channel limit keys on the same
    /// client identity and applies with the platform limiter off. Parsing it
    /// off a disabled config must still yield the CIDRs.
    #[test]
    fn trusted_proxies_parse_with_the_platform_limiter_disabled() {
        let config = RateLimitConfig {
            enabled: false,
            trusted_proxies: vec!["10.0.0.0/8".to_string(), "192.168.1.1".to_string()],
            ..Default::default()
        };
        assert_eq!(config.parsed_trusted_proxies().len(), 2);
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
