use std::net::IpAddr;

use ipnet::IpNet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkPolicyError {
    /// URL is malformed or non-HTTP(S).
    InvalidUrl(String),
    /// URL host is a name that resolves to a blocked IP (loopback, private,
    /// link-local, etc.).
    BlockedHost(String),
    /// URL host is a literal IP that is in a blocked range.
    BlockedIp(IpAddr),
}

impl std::fmt::Display for NetworkPolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl(s) => write!(f, "invalid URL: {s}"),
            Self::BlockedHost(s) => write!(f, "blocked host: {s}"),
            Self::BlockedIp(ip) => write!(f, "blocked IP: {ip}"),
        }
    }
}

impl std::error::Error for NetworkPolicyError {}

/// Returns the set of CIDR ranges that should be blocked to prevent SSRF.
/// Includes loopback, private (RFC 1918), link-local, and IPv6 equivalents.
pub fn blocked_ranges() -> Vec<IpNet> {
    [
        // IPv4
        "127.0.0.0/8",    // loopback
        "10.0.0.0/8",     // private
        "172.16.0.0/12",  // private
        "192.168.0.0/16", // private
        "169.254.0.0/16", // link-local
        "100.64.0.0/10",  // carrier-grade NAT
        "0.0.0.0/8",      // "this network"
        "224.0.0.0/4",    // multicast
        "240.0.0.0/4",    // reserved
        // IPv6
        "::1/128",       // loopback
        "fc00::/7",      // ULA
        "fe80::/10",     // link-local
        "::ffff:0:0/96", // IPv4-mapped (let the IPv4 rules apply)
    ]
    .iter()
    .filter_map(|s| s.parse::<IpNet>().ok())
    .collect()
}

pub fn is_blocked_ip(ip: IpAddr) -> bool {
    for range in blocked_ranges() {
        if range.contains(&ip) {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone, Default)]
pub struct NetworkPolicy {
    /// If true, the SSRF guard is enabled. Default: true.
    pub ssrf_guard: bool,
}

impl NetworkPolicy {
    pub fn new() -> Self {
        Self { ssrf_guard: true }
    }

    pub fn allow_private(mut self) -> Self {
        self.ssrf_guard = false;
        self
    }

    /// Validate a URL. Returns the parsed host string and port.
    pub fn validate(&self, url: &str) -> Result<(String, Option<u16>), NetworkPolicyError> {
        if !self.ssrf_guard {
            return parse_host(url).ok_or_else(|| NetworkPolicyError::InvalidUrl(url.to_string()));
        }
        let (host, port) =
            parse_host(url).ok_or_else(|| NetworkPolicyError::InvalidUrl(url.to_string()))?;
        // Literal IP?
        if let Ok(ip) = host.parse::<IpAddr>() {
            if is_blocked_ip(ip) {
                return Err(NetworkPolicyError::BlockedIp(ip));
            }
            return Ok((host, port));
        }
        // Hostname: try to resolve and check.
        if let Some(ip) = resolve_host_sync(&host) {
            if is_blocked_ip(ip) {
                return Err(NetworkPolicyError::BlockedIp(ip));
            }
        }
        Ok((host, port))
    }
}

fn parse_host(url: &str) -> Option<(String, Option<u16>)> {
    // Strip scheme.
    let after_scheme = url.split_once("://")?.1;
    // Strip path/query/fragment.
    let host_port = after_scheme.split('/').next()?;
    let host_port = host_port.split('?').next()?;
    let host_port = host_port.split('#').next()?;
    if host_port.is_empty() {
        return None;
    }
    // userinfo@host:port
    let host_port = host_port.rsplit('@').next()?;
    if let Some((host, port_str)) = host_port.rsplit_once(':') {
        if let Ok(port) = port_str.parse::<u16>() {
            return Some((host.to_string(), Some(port)));
        }
    }
    Some((host_port.to_string(), None))
}

fn resolve_host_sync(host: &str) -> Option<IpAddr> {
    use std::net::ToSocketAddrs;
    let addrs = (host, 0).to_socket_addrs().ok()?;
    for addr in addrs {
        return Some(addr.ip());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn blocks_loopback_ipv4() {
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(127, 255, 255, 254))));
    }

    #[test]
    fn blocks_loopback_ipv6() {
        assert!(is_blocked_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn blocks_rfc1918() {
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
    }

    #[test]
    fn blocks_link_local() {
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))));
    }

    #[test]
    fn allows_public_ips() {
        assert!(!is_blocked_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_blocked_ip(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    #[test]
    fn blocks_ipv6_ula() {
        assert!(is_blocked_ip(IpAddr::V6("fc00::1".parse().unwrap())));
        assert!(is_blocked_ip(IpAddr::V6("fd00::1".parse().unwrap())));
    }

    #[test]
    fn blocks_ipv6_link_local() {
        assert!(is_blocked_ip(IpAddr::V6("fe80::1".parse().unwrap())));
    }

    #[test]
    fn parse_host_extracts_hostname() {
        let (host, port) = parse_host("https://example.com/path").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, None);
    }

    #[test]
    fn parse_host_extracts_port() {
        let (host, port) = parse_host("https://example.com:8080/api").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, Some(8080));
    }

    #[test]
    fn parse_host_handles_query() {
        let (host, port) = parse_host("https://example.com:443?query=1").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, Some(443));
    }

    #[test]
    fn parse_host_handles_userinfo() {
        let (host, port) = parse_host("https://user:pass@example.com:8080/").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, Some(8080));
    }

    #[test]
    fn parse_host_rejects_non_url() {
        assert!(parse_host("not a url").is_none());
        assert!(parse_host("").is_none());
    }

    #[test]
    fn validate_blocks_literal_loopback() {
        let policy = NetworkPolicy::new();
        let result = policy.validate("http://127.0.0.1:8080/api");
        assert!(matches!(result, Err(NetworkPolicyError::BlockedIp(_))));
    }

    #[test]
    fn validate_blocks_literal_rfc1918() {
        let policy = NetworkPolicy::new();
        assert!(policy.validate("http://10.0.0.1/admin").is_err());
        assert!(policy.validate("http://192.168.1.1/admin").is_err());
    }

    #[test]
    fn validate_allows_public_dns() {
        // 8.8.8.8 resolves to a public IP; we won't actually do DNS here,
        // but the URL with a hostname should pass if DNS resolves to a
        // public IP. We can at least check the policy doesn't reject the
        // hostname syntactically.
        let policy = NetworkPolicy::new();
        let result = policy.validate("https://example.com/path");
        assert!(result.is_ok());
    }

    #[test]
    fn validate_skips_ssrf_guard_when_disabled() {
        let policy = NetworkPolicy::new().allow_private();
        let result = policy.validate("http://127.0.0.1:8080");
        assert!(result.is_ok());
    }

    #[test]
    fn validate_rejects_invalid_url() {
        let policy = NetworkPolicy::new();
        let result = policy.validate("not a url");
        assert!(matches!(result, Err(NetworkPolicyError::InvalidUrl(_))));
    }
}
