use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Check if an IP address is private, loopback, link-local, or otherwise internal.
pub fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => is_private_v6(v6),
    }
}

fn is_private_v4(v4: &Ipv4Addr) -> bool {
    let o = v4.octets();
    v4.is_loopback()               // 127.0.0.0/8
        || v4.is_private()          // 10/8, 172.16/12, 192.168/16
        || v4.is_link_local()       // 169.254.0.0/16
        || v4.is_broadcast()        // 255.255.255.255
        || v4.is_multicast()        // 224.0.0.0/4
        || o[0] == 0                // 0.0.0.0/8 "this network" — 0.x.y.z reaches localhost on Linux
        || (o[0] == 100 && (o[1] & 0xC0) == 64)       // 100.64.0.0/10 (CGNAT)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)    // 192.0.0.0/24 (IETF protocol assignments)
        || (o[0] == 198 && (o[1] & 0xFE) == 18)       // 198.18.0.0/15 (benchmarking)
        || (o[0] == 192 && o[1] == 0 && o[2] == 2)    // 192.0.2.0/24 (TEST-NET-1)
        || (o[0] == 198 && o[1] == 51 && o[2] == 100) // 198.51.100.0/24 (TEST-NET-2)
        || (o[0] == 203 && o[1] == 0 && o[2] == 113) // 203.0.113.0/24 (TEST-NET-3)
}

fn is_private_v6(v6: &Ipv6Addr) -> bool {
    let s = v6.segments();
    v6.is_loopback()                     // ::1
        || v6.is_unspecified()           // ::
        || v6.is_multicast()             // ff00::/8
        || (s[0] & 0xFE00) == 0xFC00     // fc00::/7 (unique local)
        || (s[0] & 0xFFC0) == 0xFE80     // fe80::/10 (link-local)
        || s[0] == 0x2002                // 2002::/16 (6to4, deprecated — relays to an opaque v4 endpoint)
        || (s[0] == 0x2001 && s[1] == 0) // 2001::/32 (Teredo — the embedded v4 is obfuscated)
        // 64:ff9b::/96 (NAT64) is judged by the embedded IPv4 so DNS64 egress to
        // public hosts keeps working while translated internal targets stay blocked
        || nat64_embedded_v4(v6).is_some_and(|v4| is_private_v4(&v4))
        // IPv4-mapped ::ffff:a.b.c.d and IPv4-compatible ::a.b.c.d — check the inner v4
        || v6.to_ipv4().is_some_and(|v4| is_private_v4(&v4))
}

/// The IPv4 address embedded in a NAT64 well-known-prefix (64:ff9b::/96) address.
fn nat64_embedded_v4(v6: &Ipv6Addr) -> Option<Ipv4Addr> {
    let s = v6.segments();
    (s[0] == 0x0064 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0]).then(|| {
        let o = v6.octets();
        Ipv4Addr::new(o[12], o[13], o[14], o[15])
    })
}

/// How long a validated lookup stays pinned for [`PinnedDnsResolver`]. The gap
/// between validation and connection inside a single request is microseconds;
/// 30s comfortably covers it (including retries within one call) while still
/// letting legitimate DNS changes propagate.
const PIN_TTL: Duration = Duration::from_secs(30);
/// Cache bound: past this, expired entries are purged and, if the cache is
/// still full, the soonest-expiring entry is evicted.
const PIN_MAX_ENTRIES: usize = 1024;

struct PinnedEntry {
    addrs: Vec<SocketAddr>,
    expires: Instant,
}

static PINNED_LOOKUPS: LazyLock<Mutex<HashMap<String, PinnedEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn pin_lock() -> MutexGuard<'static, HashMap<String, PinnedEntry>> {
    // Entries are plain data — a panic mid-update cannot corrupt them, so a
    // poisoned lock is safe to keep using.
    PINNED_LOOKUPS.lock().unwrap_or_else(|p| p.into_inner())
}

fn pin_validated(host: &str, addrs: &[SocketAddr]) {
    let mut map = pin_lock();
    if map.len() >= PIN_MAX_ENTRIES {
        let now = Instant::now();
        map.retain(|_, e| e.expires > now);
    }
    if map.len() >= PIN_MAX_ENTRIES
        && let Some(oldest) = map
            .iter()
            .min_by_key(|(_, e)| e.expires)
            .map(|(k, _)| k.clone())
    {
        map.remove(&oldest);
    }
    map.insert(
        host.to_ascii_lowercase(),
        PinnedEntry {
            addrs: addrs.to_vec(),
            expires: Instant::now() + PIN_TTL,
        },
    );
}

fn pinned_addrs(host: &str) -> Option<Vec<SocketAddr>> {
    let map = pin_lock();
    map.get(&host.to_ascii_lowercase())
        .filter(|e| e.expires > Instant::now())
        .map(|e| e.addrs.clone())
}

/// DNS resolver for the shared reqwest client: hosts that just passed
/// [`validate_url_not_private`] resolve to the exact addresses that were vetted
/// (resolve once, connect to the same addresses — closing the DNS-rebinding
/// TOCTOU between validation and connection). Hosts with no pinned entry
/// (connectors with `allow_private_urls: true`, or an expired pin) fall back
/// to system DNS.
pub struct PinnedDnsResolver;

impl reqwest::dns::Resolve for PinnedDnsResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            if let Some(addrs) = pinned_addrs(name.as_str()) {
                return Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs);
            }
            // reqwest replaces port 0 with the URL's actual port.
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Validate that a URL does not target private/internal IP addresses (SSRF
/// protection), returning the addresses that were vetted.
///
/// The scheme is checked first (S7): every caller is an HTTP egress path, so
/// anything outside `http`/`https` — `gopher://`, `file://`, `ftp://` — is
/// rejected outright rather than address-checked and waved through. This also
/// keeps `port_or_known_default` honest: for http/https it always knows the
/// port, so the pinned `SocketAddr` set can never be built against a bogus
/// default for some other scheme.
///
/// Resolves the hostname and checks all resolved addresses; a host that fails
/// to resolve is an error (never silently allowed through). Validated lookups
/// are pinned so that a client built with [`PinnedDnsResolver`] connects to
/// these exact addresses rather than re-resolving (DNS-rebinding protection).
pub async fn validate_url_not_private(url: &str) -> Result<Vec<SocketAddr>, String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("Invalid URL '{url}': {e}"))?;

    let scheme = parsed.scheme();
    if !matches!(scheme, "http" | "https") {
        return Err(format!(
            "URL '{url}' uses scheme '{scheme}' — only http and https are allowed"
        ));
    }

    let host = match parsed.host() {
        Some(h) => h,
        None => return Err(format!("URL '{url}' has no host")),
    };
    // Always `Some` for http/https (checked above); 80 is unreachable.
    let port = parsed.port_or_known_default().unwrap_or(80);

    // IP-literal URLs involve no DNS at connect time, so no pinning is needed.
    let check_ip = |ip: IpAddr| {
        if is_private_ip(&ip) {
            Err(format!(
                "URL '{url}' targets private/internal IP address {ip}"
            ))
        } else {
            Ok(vec![SocketAddr::new(ip, port)])
        }
    };

    match host {
        url::Host::Ipv4(ip) => check_ip(IpAddr::V4(ip)),
        url::Host::Ipv6(ip) => check_ip(IpAddr::V6(ip)),
        url::Host::Domain(domain) => {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((domain, port))
                .await
                .map_err(|e| format!("Failed to resolve host '{domain}' for URL '{url}': {e}"))?
                .collect();

            if addrs.is_empty() {
                return Err(format!(
                    "Host '{domain}' for URL '{url}' resolved to no addresses"
                ));
            }
            for socket_addr in &addrs {
                if is_private_ip(&socket_addr.ip()) {
                    return Err(format!(
                        "URL '{}' resolves to private/internal IP address {}",
                        url,
                        socket_addr.ip()
                    ));
                }
            }
            pin_validated(domain, &addrs);
            Ok(addrs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(addr: &str) {
        assert!(
            is_private_ip(&addr.parse().expect("test")),
            "{addr} should be blocked"
        );
    }

    fn allowed(addr: &str) {
        assert!(
            !is_private_ip(&addr.parse().expect("test")),
            "{addr} should be allowed"
        );
    }

    #[test]
    fn test_is_private_ip_loopback() {
        blocked("127.0.0.1");
        blocked("127.0.0.2");
        blocked("127.255.255.255");
        blocked("::1");
    }

    #[test]
    fn test_is_private_ip_rfc1918() {
        blocked("10.0.0.1");
        blocked("10.255.255.255");
        blocked("172.16.0.1");
        blocked("172.31.255.255");
        blocked("192.168.0.1");
        blocked("192.168.255.255");
        allowed("172.15.255.255");
        allowed("172.32.0.1");
    }

    #[test]
    fn test_is_private_ip_link_local() {
        blocked("169.254.0.1");
        blocked("169.254.169.254"); // Cloud metadata
        allowed("169.253.255.255");
        allowed("169.255.0.1");
    }

    #[test]
    fn test_is_private_ip_cgnat() {
        blocked("100.64.0.1");
        blocked("100.127.255.255");
        allowed("100.63.255.255");
        allowed("100.128.0.1");
    }

    #[test]
    fn test_is_private_ip_this_network() {
        blocked("0.0.0.0");
        blocked("0.0.0.1");
        blocked("0.255.255.255");
        allowed("1.0.0.1");
    }

    #[test]
    fn test_is_private_ip_ietf_protocol_assignments() {
        blocked("192.0.0.0");
        blocked("192.0.0.255");
        allowed("192.0.1.1");
    }

    #[test]
    fn test_is_private_ip_benchmarking() {
        blocked("198.18.0.1");
        blocked("198.19.255.255");
        allowed("198.17.255.255");
        allowed("198.20.0.1");
    }

    #[test]
    fn test_is_private_ip_test_net_ranges() {
        blocked("192.0.2.1"); // TEST-NET-1
        blocked("198.51.100.7"); // TEST-NET-2
        blocked("203.0.113.9"); // TEST-NET-3
        allowed("192.0.3.1");
        allowed("198.51.101.1");
        allowed("203.0.112.1");
        allowed("203.0.114.1");
    }

    #[test]
    fn test_is_private_ip_v4_multicast_broadcast() {
        blocked("224.0.0.251");
        blocked("239.255.255.255");
        blocked("255.255.255.255");
        allowed("223.255.255.255");
    }

    #[test]
    fn test_is_private_ip_public() {
        allowed("8.8.8.8");
        allowed("1.1.1.1");
        allowed("93.184.216.34");
    }

    #[test]
    fn test_is_private_ip_v6_unspecified() {
        blocked("::");
    }

    #[test]
    fn test_is_private_ip_v6_unique_local() {
        blocked("fc00::1");
        blocked("fd12:3456:789a::1");
        blocked("fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff");
        allowed("fbff::1");
    }

    #[test]
    fn test_is_private_ip_v6_link_local() {
        blocked("fe80::1");
        blocked("febf:ffff::1"); // still inside fe80::/10
        allowed("fe00::1");
        allowed("fec0::1"); // deprecated site-local is outside fe80::/10
    }

    #[test]
    fn test_is_private_ip_v6_6to4() {
        blocked("2002::1");
        blocked("2002:c0a8:101::1");
        blocked("2002:808:808::1"); // whole /16 blocked, even public embedded v4
        allowed("2003::1");
    }

    #[test]
    fn test_is_private_ip_v6_teredo() {
        blocked("2001::1");
        blocked("2001:0:5ef5:79fd::1");
        allowed("2001:4860:4860::8888"); // Google DNS — 2001::/32 only, not 2001::/16
    }

    #[test]
    fn test_is_private_ip_v6_nat64() {
        blocked("64:ff9b::a00:1"); // embeds 10.0.0.1
        blocked("64:ff9b::7f00:1"); // embeds 127.0.0.1
        blocked("64:ff9b::a9fe:a9fe"); // embeds 169.254.169.254
        allowed("64:ff9b::808:808"); // embeds 8.8.8.8 — legit DNS64 egress
        allowed("64:ff9c::a00:1"); // outside the well-known prefix
    }

    #[test]
    fn test_is_private_ip_v6_multicast() {
        blocked("ff02::1");
        blocked("ff05::2");
        allowed("fe00::2");
    }

    #[test]
    fn test_is_private_ip_v4_mapped_v6() {
        blocked("::ffff:127.0.0.1");
        blocked("::ffff:10.0.0.1");
        blocked("::ffff:192.168.1.1");
        blocked("::ffff:169.254.169.254");
        allowed("::ffff:8.8.8.8");
    }

    #[test]
    fn test_is_private_ip_v4_compatible_v6() {
        blocked("::127.0.0.1");
        blocked("::10.0.0.1");
        blocked("::0.0.0.1");
        allowed("::8.8.8.8");
    }

    #[test]
    fn test_is_private_ip_v6_public() {
        allowed("2600::1");
        allowed("2606:4700:4700::1111");
    }

    #[tokio::test]
    async fn test_validate_url_not_private_direct_ip() {
        assert!(
            validate_url_not_private("http://127.0.0.1/api")
                .await
                .is_err()
        );
        assert!(
            validate_url_not_private("http://10.0.0.1:8080/api")
                .await
                .is_err()
        );
        assert!(
            validate_url_not_private("http://192.168.1.1/api")
                .await
                .is_err()
        );
        assert!(
            validate_url_not_private("http://169.254.169.254/latest/meta-data")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_validate_url_not_private_ipv6_literal() {
        assert!(validate_url_not_private("http://[::1]/api").await.is_err());
        assert!(
            validate_url_not_private("http://[fd00::1]/api")
                .await
                .is_err()
        );
        assert!(
            validate_url_not_private("http://[2600::1]/api")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_validate_url_not_private_public_ip() {
        assert!(validate_url_not_private("http://8.8.8.8/api").await.is_ok());
    }

    #[tokio::test]
    async fn test_validate_url_not_private_dns_failure_is_error() {
        // RFC 2606 reserves .invalid — it never resolves, and an unresolvable
        // host must be rejected rather than silently passed to the HTTP client.
        assert!(
            validate_url_not_private("http://orion-ssrf-test.invalid/api")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_validate_url_not_private_no_host() {
        assert!(
            validate_url_not_private("data:text/plain,hello")
                .await
                .is_err()
        );
    }

    /// S7: schemes outside http/https are rejected before any host or DNS
    /// check — a public host must not rescue a gopher/ftp/file URL.
    #[tokio::test]
    async fn test_validate_url_rejects_non_http_schemes() {
        for url in [
            "gopher://public.example:70/",
            "ftp://8.8.8.8/pub",
            "file:///etc/passwd",
            "redis://8.8.8.8:6379",
            "ws://8.8.8.8/socket",
        ] {
            let err = validate_url_not_private(url)
                .await
                .expect_err("non-http scheme must be rejected");
            assert!(
                err.contains("only http and https are allowed"),
                "unexpected error for {url}: {err}"
            );
        }
    }

    /// The scheme check must not touch DNS: an unresolvable host behind a
    /// forbidden scheme still fails on the scheme, not on resolution.
    #[tokio::test]
    async fn test_scheme_rejection_precedes_dns() {
        let err = validate_url_not_private("gopher://orion-ssrf-test.invalid:70/")
            .await
            .expect_err("test");
        assert!(err.contains("only http and https are allowed"), "{err}");
    }

    #[tokio::test]
    async fn test_validate_url_returns_vetted_addrs_for_ip_literal() {
        let addrs = validate_url_not_private("https://8.8.8.8/api")
            .await
            .expect("test");
        assert_eq!(addrs, vec!["8.8.8.8:443".parse().expect("test")]);
    }

    #[test]
    fn test_pin_roundtrip_is_case_insensitive() {
        let addr: SocketAddr = "93.184.216.34:80".parse().expect("test");
        pin_validated("Pin-Roundtrip.Example", &[addr]);
        assert_eq!(pinned_addrs("pin-roundtrip.example"), Some(vec![addr]));
        assert_eq!(pinned_addrs("never-pinned.example"), None);
    }

    #[tokio::test]
    async fn test_resolver_returns_pinned_addrs() {
        use reqwest::dns::Resolve;
        let addr: SocketAddr = "93.184.216.34:443".parse().expect("test");
        pin_validated("resolver-pin.example", &[addr]);
        let resolved: Vec<SocketAddr> = PinnedDnsResolver
            .resolve("resolver-pin.example".parse().expect("test"))
            .await
            .expect("test")
            .collect();
        assert_eq!(resolved, vec![addr]);
    }

    #[tokio::test]
    async fn test_resolver_falls_back_to_system_dns_when_unpinned() {
        use reqwest::dns::Resolve;
        let resolved: Vec<SocketAddr> = PinnedDnsResolver
            .resolve("localhost".parse().expect("test"))
            .await
            .expect("test")
            .collect();
        assert!(!resolved.is_empty());
        assert!(resolved.iter().all(|a| a.ip().is_loopback()));
    }
}
