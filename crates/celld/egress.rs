//! Public-only egress for a Worker's own `fetch` (`CELLD_EGRESS_PUBLIC_ONLY=1`).
//!
//! A fleet whose private network carries the internal listener (peer calls
//! and the unauthenticated operator API) must not let a Worker reach it. A
//! URL check in the Worker cannot see where a name resolves, so the check
//! lives here: a name resolves to its public addresses only (a name that has
//! none is refused), and a URL or a redirect that names a non-public IP
//! literal is refused before a connection is made. Service bindings, Durable
//! Object calls, and celld's own clients do not pass through here.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, OnceLock};

/// Whether this node limits a Worker's fetch to public addresses.
pub fn public_only() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        crate::env_vars::flag("CELLD_EGRESS_PUBLIC_ONLY", false).expect("validated CELLD_EGRESS_PUBLIC_ONLY")
    })
}

/// Why a request was refused. `fetch` rejects with this message; its
/// `egress refused:` prefix tells a caller the request can never pass.
#[derive(Debug)]
pub struct EgressRefused(pub String);

impl std::fmt::Display for EgressRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EgressRefused {}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0
        || (a == 100 && (64..128).contains(&b)) // shared address space (CGNAT)
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || a >= 240) // reserved
}

/// Whether an address is on the public internet.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_v4(v4);
            }
            let s = ip.segments();
            // NAT64 (64:ff9b::/96) reaches the IPv4 address it carries
            if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
                let [a, b] = s[6].to_be_bytes();
                let [c, d] = s[7].to_be_bytes();
                return public_v4(Ipv4Addr::new(a, b, c, d));
            }
            !(s[..6] == [0; 6] // unspecified, loopback, IPv4-compatible
                || ip.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local (Fly's 6PN is fdaa::/16)
                || (s[0] & 0xffc0) == 0xfe80 // link-local
                || (s[0] & 0xffc0) == 0xfec0 // site-local
                || (s[0] == 0x2001 && s[1] == 0x0db8)) // documentation
        }
    }
}

/// The refusal for a URL whose host is a non-public IP literal.
pub fn literal_refused(url: &str) -> Option<EgressRefused> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let ip: IpAddr = host.trim_start_matches('[').trim_end_matches(']').parse().ok()?;
    (!is_public(ip)).then(|| EgressRefused(format!("egress refused: {ip} is not a public address")))
}

/// The refusal for a redirect, when this node limits egress.
pub fn redirect_refused(url: &reqwest::Url) -> Option<EgressRefused> {
    if public_only() {
        literal_refused(url.as_str())
    } else {
        None
    }
}

/// A resolver that keeps a name's public addresses only.
struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let host = name.as_str().to_string();
            let all: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let public: Vec<SocketAddr> = all.into_iter().filter(|a| is_public(a.ip())).collect();
            if public.is_empty() {
                let refused = EgressRefused(format!("egress refused: {host} resolves to no public address"));
                return Err(Box::new(refused) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// A client for a Worker's fetch, with this node's egress policy.
pub fn client(builder: reqwest::ClientBuilder) -> reqwest::Client {
    let builder = if public_only() { builder.dns_resolver(Arc::new(PublicOnlyResolver)) } else { builder };
    builder.build().expect("build an outbound HTTP client")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public(s: &str) -> bool {
        is_public(s.parse().unwrap())
    }

    #[test]
    fn public_addresses() {
        for ip in ["93.184.216.34", "1.1.1.1", "66.241.125.20", "2606:4700::6810:84e5", "2a09:8280:1::199:8a1c:0"] {
            assert!(public(ip), "{ip} is public");
        }
    }

    #[test]
    fn non_public_addresses() {
        for ip in [
            "127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0",
            "255.255.255.255", "224.0.0.1", "192.0.0.8", "198.18.0.1", "240.0.0.1", "::", "::1",
            "::127.0.0.1", "::ffff:127.0.0.1", "::ffff:10.0.0.1", "64:ff9b::7f00:1", "fdaa:0:bfe:a7b:21a:59a5:632b:2",
            "fc00::1", "fe80::1", "fec0::1", "ff02::1", "2001:db8::1",
        ] {
            assert!(!public(ip), "{ip} is not public");
        }
    }

    #[test]
    fn literals_in_urls() {
        assert!(literal_refused("http://127.0.0.1:8081/state").is_some());
        assert!(literal_refused("http://[fdaa:0:bfe:a7b:21a:59a5:632b:2]:8081/shutdown").is_some());
        assert!(literal_refused("http://[::ffff:10.0.0.1]/").is_some());
        assert!(literal_refused("https://1.1.1.1/").is_none());
        assert!(literal_refused("https://example.com/").is_none(), "a name is the resolver's to judge");
        assert!(literal_refused("not a url").is_none());
    }
}
