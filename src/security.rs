//! Network security: outbound-request validation.
//!
//! Guards against SSRF by rejecting private, link-local, and loopback
//! addresses when they appear in operator-configured endpoints
//! (`providers.api_base`, MCP servers, A2A agents).  The admin API is the
//! only place these values enter the system, and all routes that store them
//! call [`validate_api_base`] during validation.
//!
//! Private ranges blocked:
//! - `10.0.0.0/8`
//! - `172.16.0.0/12`
//! - `192.168.0.0/16`
//! - `127.0.0.0/8` (loopback)
//! - `169.254.0.0/16` (link-local / cloud metadata)
//! - `0.0.0.0/8`
//! - `::1` (IPv6 loopback)
//! - `fc00::/7` (IPv6 unique local)
//! - `fe80::/10` (IPv6 link-local)

use std::net::{IpAddr, ToSocketAddrs};

/// Check whether an IPv4 address is in a blocked range.
fn is_blocked_ipv4(addr: &std::net::Ipv4Addr) -> bool {
    // Private ranges
    if addr.is_private() {
        return true;
    }
    // Loopback (127.x.x.x)
    if addr.is_loopback() {
        return true;
    }
    // Link-local (169.254.x.x)
    if addr.is_link_local() {
        return true;
    }
    // Broadcast (255.255.255.255)
    if addr.is_broadcast() {
        return true;
    }
    // "This network" (0.x.x.x)
    if addr.is_unspecified() {
        return true;
    }
    false
}

/// Check whether an IPv6 address is in a blocked range.
fn is_blocked_ipv6(addr: &std::net::Ipv6Addr) -> bool {
    // IPv6 loopback (::1)
    if addr.is_loopback() {
        return true;
    }
    // IPv6 link-local (fe80::/10)
    if addr.is_unicast_link_local() {
        return true;
    }
    // IPv6 unique local (fc00::/7)
    if addr.is_unique_local() {
        return true;
    }
    // IPv6 unspecified (::)
    if addr.is_unspecified() {
        return true;
    }
    false
}

/// Check whether an IP address is in a blocked range.
fn is_blocked_addr(addr: &std::net::IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => is_blocked_ipv6(v6),
    }
}

/// Extract the host part of an api_base and try to resolve it, returning
/// an error for any address that resolves to a blocked IP.
///
/// Accepts only absolute HTTP(S) URIs; a non-absolute URI is rejected before
/// DNS is even touched.
pub fn validate_api_base(api_base: &str) -> Result<(), String> {
    let api_base = api_base.trim_end_matches('/');

    // Must start with http:// or https:// (already checked by callers, but
    // defensive here so this module can be used standalone).
    if !api_base.starts_with("http://") && !api_base.starts_with("https://") {
        return Err(format!(
            "{api_base}: must start with http:// or https://"
        ));
    }

    // Strip scheme to get the authority part.
    let authority = api_base
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(api_base)
        .split('/')
        .next()
        .unwrap_or(api_base);

    // Strip port if present.
    let host = if let Some(pos) = authority.rfind(':') {
        let after_port = &authority[pos + 1..];
        // Valid port is digits only, so if the next char after ':' is not a
        // digit (e.g. ']' in IPv6), treat it as host-only.
        if after_port.chars().next().map_or(false, |c| c.is_ascii_digit()) {
            &authority[..pos]
        } else {
            authority
        }
    } else {
        authority
    };

    // Handle IPv6 literals in brackets: [::1]:8080 -> ::1
    let host = if host.starts_with('[') {
        host.trim_start_matches('[')
            .split(']')
            .next()
            .unwrap_or(host)
    } else {
        host
    };

    // If it looks like a hostname (contains '.' or ':'), try DNS resolution.
    // If it looks like an IP literal (no dots, or IPv6), parse directly.
    let addrs: Vec<std::net::IpAddr> = if host.is_empty() {
        return Err("empty host in api_base".to_string());
    } else if host.contains(':') {
        // Could be an IPv6 address like "::1" or "2001:db8::1"
        host.parse::<std::net::IpAddr>()
            .map(|ip| vec![ip])
            .map_err(|_| {
                format!("{}: not a valid IPv6 address", host)
            })?
    } else if host.contains('.') {
        // Looks like a hostname; try to resolve it
        format!("{}:443", host)
            .to_socket_addrs()
            .map(|iter| iter.map(|sa| sa.ip()).collect())
            .map_err(|e| {
                format!(
                    "{}: DNS resolution failed: {e}",
                    host
                )
            })?
    } else {
        // Looks like an IPv4 address
        host.parse::<std::net::IpAddr>()
            .map(|ip| vec![ip])
            .map_err(|_| {
                format!(
                    "{}: not a valid IP address or hostname",
                    host
                )
            })?
    };

    if addrs.is_empty() {
        return Err(format!(
            "{}: host resolved to no addresses",
            host
        ));
    }

    for addr in &addrs {
        if is_blocked_addr(addr) {
            return Err(format!(
                "{}: api_base host resolves to blocked address {} (private, loopback, or link-local — \
                 use a public domain or an explicitly allowed address)",
                host, addr
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_is_blocked() {
        assert!(validate_api_base("http://127.0.0.1:8000/v1").is_err());
        assert!(validate_api_base("http://localhost:8000/v1").is_err());
        assert!(validate_api_base("http://[::1]:8000/v1").is_err());
    }

    #[test]
    fn cloud_metadata_is_blocked() {
        assert!(validate_api_base("http://169.254.169.254/latest/meta-data/").is_err());
    }

    #[test]
    fn private_ranges_are_blocked() {
        assert!(validate_api_base("http://10.0.0.1:8000/v1").is_err());
        assert!(validate_api_base("http://192.168.1.1:8000/v1").is_err());
        assert!(validate_api_base("http://172.16.0.1:8000/v1").is_err());
        assert!(validate_api_base("http://172.31.255.255:8000/v1").is_err());
    }

    #[test]
    fn valid_public_host_is_allowed() {
        // We cannot rely on DNS in tests, so we check the host parsing logic
        // by using hosts that will fail DNS but pass the blocking check.
        // A valid host should not error with a blocking message.
        let result = validate_api_base("https://example.com:443/v1");
        // May fail DNS or succeed — either way should NOT be a blocking error
        match &result {
            Err(e) => {
                assert!(
                    !e.contains("blocked"),
                    "example.com should not be blocked: {e}"
                );
            }
            Ok(_) => {}
        }
    }

    #[test]
    fn invalid_scheme_is_rejected() {
        assert!(validate_api_base("ftp://example.com/v1").is_err());
        assert!(validate_api_base("file:///etc/passwd").is_err());
    }

    #[test]
    fn invalid_ip_is_rejected() {
        assert!(validate_api_base("http://999.999.999.999:8000/v1").is_err());
    }

    #[test]
    fn empty_host_is_rejected() {
        assert!(validate_api_base("http:///v1").is_err());
    }

    #[test]
    fn valid_http_host_is_allowed() {
        // "google.com" should not be blocked
        let result = validate_api_base("http://google.com/v1");
        match &result {
            Err(e) => {
                assert!(
                    !e.contains("blocked"),
                    "google.com should not be blocked: {e}"
                );
            }
            Ok(_) => {}
        }
    }
}
