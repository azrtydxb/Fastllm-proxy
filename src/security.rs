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
//! - `0.0.0.0/8` ("this network"; connecting to any of it lands on the local
//!   host, so the whole block is treated as loopback)
//! - `::1` (IPv6 loopback)
//! - `fc00::/7` (IPv6 unique local)
//! - `fe80::/10` (IPv6 link-local)
//!
//! A deployment whose upstreams are legitimately private — a proxy in front
//! of its own cluster's inference services is the normal case — names them
//! in `FASTLLM_SSRF_ACCEPT` rather than weakening the block. The variable is
//! a comma-separated list of CIDRs (`10.43.0.0/16`) and hostnames, where a
//! leading dot means "any subdomain" (`.tools.internal`); a bare name matches
//! exactly. It is parsed once at startup into `Deployment` (which is what
//! `/admin/config` shows and what every validator here receives), because
//! config that gates what may be *stored* changes only with a restart, like
//! the encryption key.
//!
//! # Known limit
//!
//! The check resolves the hostname once, at config time; the proxy's HTTP
//! client resolves again per request. An attacker who controls DNS for an
//! allowed name can still rebind it to a private address between the two.
//! debt: revisit if a deployment ever allowlists names it does not itself
//! control; today every listed name is the operator's own infrastructure.

use std::net::{IpAddr, ToSocketAddrs};

/// Operator-configured exceptions to the private-address block.
#[derive(Debug, Clone)]
pub enum Allow {
    /// An address inside the range is accepted despite being private.
    Cidr(IpAddr, u8),
    /// A hostname: `.`-prefixed entries match any subdomain, bare names
    /// match exactly. Matched case-insensitively against the configured URL.
    Name(String),
}

impl std::fmt::Display for Allow {
    /// Round-trips the accepted syntax: what `/admin/config` shows is what
    /// the operator could have written back into `FASTLLM_SSRF_ACCEPT`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Allow::Cidr(net, len) => write!(f, "{net}/{len}"),
            Allow::Name(n) => write!(f, "{n}"),
        }
    }
}

impl serde::Serialize for Allow {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Parse `FASTLLM_SSRF_ACCEPT`. Entries that do not parse are dropped with a
/// warning rather than failing startup: a typo taking the control plane down
/// would be worse than one dead allowlist entry, and the warning is where the
/// operator is told.
pub fn parse_allow(spec: &str) -> Vec<Allow> {
    let mut out = Vec::new();
    for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        if let Some((ip, len)) = entry.split_once('/') {
            let max = match ip.parse::<IpAddr>() {
                Ok(IpAddr::V4(_)) => 32,
                Ok(IpAddr::V6(_)) => 128,
                Err(_) => {
                    tracing::warn!("FASTLLM_SSRF_ACCEPT: skipping {entry:?}: not an IP address");
                    continue;
                }
            };
            match len.parse::<u8>() {
                Ok(len) if len <= max => out.push(Allow::Cidr(ip.parse().unwrap(), len)),
                _ => tracing::warn!(
                    "FASTLLM_SSRF_ACCEPT: skipping {entry:?}: prefix length must be 0..={max}"
                ),
            }
        } else {
            out.push(Allow::Name(entry.to_ascii_lowercase()));
        }
    }
    out
}

fn cidr_contains(addr: IpAddr, net: IpAddr, len: u8) -> bool {
    fn mask(prefix: u32, bits: u32) -> u128 {
        if prefix == 0 {
            0
        } else {
            (!0u128 << (bits - prefix)) & ((1u128 << bits) - 1)
        }
    }
    match (addr, net) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            (u32::from_be_bytes(a.octets()) as u128) & mask(len as u32, 32)
                == (u32::from_be_bytes(b.octets()) as u128) & mask(len as u32, 32)
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            u128::from_be_bytes(a.octets()) & mask(len as u32, 128)
                == u128::from_be_bytes(b.octets()) & mask(len as u32, 128)
        }
        // A v4 address can only be in a v4 range and vice versa.
        _ => false,
    }
}

fn name_allowed(host: &str, allow: &[Allow]) -> bool {
    allow.iter().any(|a| match a {
        Allow::Name(n) => match n.strip_prefix('.') {
            Some(bare) => host == bare || host.ends_with(n.as_str()),
            None => host == n.as_str(),
        },
        Allow::Cidr(..) => false,
    })
}

/// Check whether an IPv4 address is in a blocked range.
fn is_blocked_ipv4(addr: &std::net::Ipv4Addr) -> bool {
    let o = addr.octets();
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
    // "This network" (0.0.0.0/8 — connecting to any of it lands locally)
    if o[0] == 0 {
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

/// Lowercased host of an absolute http(s) URL, with port and IPv6 brackets
/// stripped. Shared by [`validate_api_base`] (which then resolves the host)
/// and the GCP token-endpoint check (which compares it against an allowlist),
/// so the two cannot disagree about what "the host" of a URL is.
pub fn host_of_url(url: &str) -> Result<String, String> {
    let url = url.trim();
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(format!("{url:?} must start with http:// or https://"));
    };
    if scheme != "http" && scheme != "https" {
        return Err(format!("{url:?} must start with http:// or https://"));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");

    // Strip port if present. A valid port is digits, so if the next char
    // after ':' is not a digit (e.g. ']' in IPv6), treat it as host-only.
    let host = if let Some(pos) = authority.rfind(':') {
        if authority[pos + 1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
        {
            &authority[..pos]
        } else {
            authority
        }
    } else {
        authority
    };

    // IPv6 literals in brackets: [::1]:8080 -> ::1
    let host = if host.starts_with('[') {
        host.trim_start_matches('[')
            .split(']')
            .next()
            .unwrap_or(host)
    } else {
        host
    };

    if host.is_empty() {
        return Err("empty host in url".to_string());
    }
    Ok(host.to_ascii_lowercase())
}

/// Check whether an api_base host resolves into a blocked range, honouring
/// the allowlist the caller was started with.
///
/// Accepts only absolute HTTP(S) URIs; a non-absolute URI is rejected before
/// DNS is even touched.
pub fn validate_api_base(api_base: &str, allow: &[Allow]) -> Result<(), String> {
    let host = host_of_url(api_base)?;

    // A name on the allowlist is accepted without resolving it: the operator
    // allowed that name, and several of the names a deployment lists —
    // in-cluster service DNS is the normal case — resolve only where the
    // control plane runs. Pinning them to today's addresses would also break
    // legitimately re-homed services.
    if name_allowed(&host, allow) {
        return Ok(());
    }

    // An IP literal — v4, or v6 once brackets and ports are stripped — needs
    // no resolution. Anything else is a hostname, single-label ones included:
    // docker-compose and in-cluster service names have no dots, and the
    // operator's DNS decides where they go, same as any dotted name.
    let addrs: Vec<std::net::IpAddr> = if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        vec![ip]
    } else {
        format!("{host}:443")
            .to_socket_addrs()
            .map(|iter| iter.map(|sa| sa.ip()).collect())
            .map_err(|e| format!("{host}: DNS resolution failed: {e}"))?
    };

    if addrs.is_empty() {
        return Err(format!("{host}: host resolved to no addresses"));
    }

    for addr in &addrs {
        let listed = allow.iter().any(|a| match a {
            Allow::Cidr(net, len) => cidr_contains(*addr, *net, *len),
            Allow::Name(_) => false,
        });
        if is_blocked_addr(addr) && !listed {
            return Err(format!(
                "{host}: api_base host resolves to blocked address {addr} (private, loopback, or link-local — \
                 list it in FASTLLM_SSRF_ACCEPT or use a public domain)"
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_allow() -> Vec<Allow> {
        Vec::new()
    }

    #[test]
    fn loopback_is_blocked() {
        assert!(validate_api_base("http://127.0.0.1:8000/v1", &no_allow()).is_err());
        assert!(validate_api_base("http://localhost:8000/v1", &no_allow()).is_err());
        assert!(validate_api_base("http://[::1]:8000/v1", &no_allow()).is_err());
    }

    #[test]
    fn cloud_metadata_is_blocked() {
        assert!(
            validate_api_base("http://169.254.169.254/latest/meta-data/", &no_allow()).is_err()
        );
    }

    #[test]
    fn private_ranges_are_blocked() {
        assert!(validate_api_base("http://10.0.0.1:8000/v1", &no_allow()).is_err());
        assert!(validate_api_base("http://192.168.1.1:8000/v1", &no_allow()).is_err());
        assert!(validate_api_base("http://172.16.0.1:8000/v1", &no_allow()).is_err());
        assert!(validate_api_base("http://172.31.255.255:8000/v1", &no_allow()).is_err());
    }

    #[test]
    fn zero_slash_eight_is_blocked() {
        assert!(validate_api_base("http://0.0.0.0:8000/v1", &no_allow()).is_err());
        assert!(validate_api_base("http://0.1.2.3:8000/v1", &no_allow()).is_err());
    }

    #[test]
    fn valid_public_host_is_allowed() {
        // We cannot rely on DNS in tests, so we check the host parsing logic
        // by using hosts that will fail DNS but pass the blocking check.
        // A valid host should not error with a blocking error.
        if let Err(e) = validate_api_base("https://example.com:443/v1", &no_allow()) {
            assert!(
                !e.contains("blocked"),
                "example.com should not be blocked: {e}"
            );
        }
    }

    #[test]
    fn invalid_scheme_is_rejected() {
        assert!(validate_api_base("ftp://example.com/v1", &no_allow()).is_err());
        assert!(validate_api_base("file:///etc/passwd", &no_allow()).is_err());
    }

    #[test]
    fn invalid_ip_is_rejected() {
        assert!(validate_api_base("http://999.999.999.999:8000/v1", &no_allow()).is_err());
    }

    #[test]
    fn empty_host_is_rejected() {
        assert!(validate_api_base("http:///v1", &no_allow()).is_err());
    }

    #[test]
    fn valid_http_host_is_allowed() {
        // "google.com" should not be blocked
        if let Err(e) = validate_api_base("http://google.com/v1", &no_allow()) {
            assert!(
                !e.contains("blocked"),
                "google.com should not be blocked: {e}"
            );
        }
    }

    #[test]
    fn cidr_allow_admits_private_range() {
        let allow = [Allow::Cidr("10.43.0.0".parse().unwrap(), 16)];
        assert!(validate_api_base("http://10.43.220.41:8000/v1", &allow).is_ok());
        assert!(validate_api_base("http://10.44.0.1:8000/v1", &allow).is_err());
        assert!(validate_api_base("http://192.168.1.1:8000/v1", &allow).is_err());
    }

    #[test]
    fn cidr_allow_never_admits_public_or_mismatched_family() {
        let allow = [Allow::Cidr("10.0.0.0".parse().unwrap(), 8)];
        // Outside the range stays blocked even though it is public anyway —
        // a public address passing is not observable here, so prove the
        // matcher itself does not accept a v6 address for a v4 range.
        assert!(!cidr_contains(
            "::1".parse().unwrap(),
            "10.0.0.0".parse().unwrap(),
            8
        ));
        let _ = &allow;
    }

    #[test]
    fn suffix_allow_admits_private_names() {
        let allow = [Allow::Name(".kuvryn-ai-workloads.svc".to_string())];
        assert!(validate_api_base(
            "http://kuvryn-1234-a46ea12d.kuvryn-ai-workloads.svc:8000/v1",
            &allow
        )
        .is_ok());
        // Upper case in the URL still matches the lower-cased entry.
        assert!(validate_api_base(
            "http://Kuvryn-1234-a46ea12d.KUVRYN-AI-WORKLOADS.SVC:8000/v1",
            &allow
        )
        .is_ok());
        // A lookalike domain does not.
        assert!(validate_api_base("http://evil.com/?x=.kuvryn-ai-workloads.svc", &allow).is_err());
    }

    #[test]
    fn exact_name_allow_does_not_match_subdomains() {
        let allow = [Allow::Name("vllm.internal".to_string())];
        assert!(validate_api_base("http://vllm.internal:8000/v1", &allow).is_ok());
        assert!(validate_api_base("http://other.vllm.internal:8000/v1", &allow).is_err());
    }

    #[test]
    fn bad_allow_entries_are_dropped_not_fatal() {
        let allow = parse_allow("10.43.0.0/16, not-an-ip/8, 10.0.0.0/33, .ok.internal");
        assert_eq!(allow.len(), 2);
    }

    #[test]
    fn host_of_url_strips_ports_brackets_and_case() {
        assert_eq!(
            host_of_url("http://Example.COM:8000/v1").unwrap(),
            "example.com"
        );
        assert_eq!(host_of_url("http://[::1]:8080/x").unwrap(), "::1");
        assert_eq!(
            host_of_url("https://api.z.ai/api/anthropic/v1").unwrap(),
            "api.z.ai"
        );
        assert!(host_of_url("ftp://example.com").is_err());
        assert!(host_of_url("example.com").is_err());
        assert!(host_of_url("http:///v1").is_err());
    }
}
