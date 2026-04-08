use std::net::IpAddr;

/// Check if an IP address is private, loopback, link-local, or otherwise internal.
pub fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()               // 127.0.0.0/8
            || v4.is_private()              // 10/8, 172.16/12, 192.168/16
            || v4.is_link_local()           // 169.254.0.0/16
            || v4.is_unspecified()          // 0.0.0.0
            || v4.is_broadcast()            // 255.255.255.255
            || v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64 // 100.64.0.0/10 (CGNAT)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()                // ::1
            || v6.is_unspecified()          // ::
            // IPv4-mapped ::ffff:x.x.x.x — check inner v4
            || v6.to_ipv4_mapped().is_some_and(|v4| is_private_ip(&IpAddr::V4(v4)))
        }
    }
}

/// Validate that a URL does not target private/internal IP addresses (SSRF protection).
/// Resolves the hostname and checks all resolved addresses.
pub async fn validate_url_not_private(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("Invalid URL '{}': {}", url, e))?;

    let host = match parsed.host_str() {
        Some(h) => h,
        None => return Err(format!("URL '{}' has no host", url)),
    };

    // Direct IP address check
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_private_ip(&ip) {
            return Err(format!(
                "URL '{}' targets private/internal IP address {}",
                url, ip
            ));
        }
        return Ok(());
    }

    // DNS resolution check
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addr = format!("{}:{}", host, port);
    match tokio::net::lookup_host(&addr).await {
        Ok(addrs) => {
            for socket_addr in addrs {
                if is_private_ip(&socket_addr.ip()) {
                    return Err(format!(
                        "URL '{}' resolves to private/internal IP address {}",
                        url,
                        socket_addr.ip()
                    ));
                }
            }
        }
        Err(_) => {
            // DNS resolution failure is not an SSRF issue — let the HTTP client handle it
        }
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_is_private_ip_loopback() {
        assert!(is_private_ip(&"127.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"127.0.0.2".parse().unwrap()));
        assert!(is_private_ip(&"::1".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_rfc1918() {
        assert!(is_private_ip(&"10.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"10.255.255.255".parse().unwrap()));
        assert!(is_private_ip(&"172.16.0.1".parse().unwrap()));
        assert!(is_private_ip(&"172.31.255.255".parse().unwrap()));
        assert!(is_private_ip(&"192.168.0.1".parse().unwrap()));
        assert!(is_private_ip(&"192.168.255.255".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_link_local() {
        assert!(is_private_ip(&"169.254.0.1".parse().unwrap()));
        assert!(is_private_ip(&"169.254.169.254".parse().unwrap())); // Cloud metadata
    }

    #[test]
    fn test_is_private_ip_cgnat() {
        assert!(is_private_ip(&"100.64.0.1".parse().unwrap()));
        assert!(is_private_ip(&"100.127.255.255".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_public() {
        assert!(!is_private_ip(&"8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip(&"1.1.1.1".parse().unwrap()));
        assert!(!is_private_ip(&"203.0.113.1".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_v4_mapped_v6() {
        // ::ffff:127.0.0.1
        assert!(is_private_ip(&"::ffff:127.0.0.1".parse().unwrap()));
        // ::ffff:10.0.0.1
        assert!(is_private_ip(&"::ffff:10.0.0.1".parse().unwrap()));
        // ::ffff:8.8.8.8 (public)
        assert!(!is_private_ip(&"::ffff:8.8.8.8".parse().unwrap()));
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
    async fn test_validate_url_not_private_no_host() {
        assert!(
            validate_url_not_private("data:text/plain,hello")
                .await
                .is_err()
        );
    }
}
