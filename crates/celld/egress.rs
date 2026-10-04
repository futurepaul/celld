//! Public-only egress for a Worker's own network (`CELLD_EGRESS_PUBLIC_ONLY=1`):
//! `fetch`, outbound WebSockets, and `connect()`.
//!
//! A fleet whose private network carries the internal listener (peer calls
//! and the unauthenticated operator API) must not let a Worker reach it. A
//! URL check in the Worker cannot see where a name resolves, so the check
//! lives here: a name resolves to its public addresses only (a name that has
//! none is refused), and a URL, a redirect, or a socket address that names a
//! non-public IP literal is refused before a connection is made. `fetch`
//! takes the rule through its clients' resolver ([`client`]); an outbound
//! WebSocket and `connect()` dial through [`Policy::connect`], which resolves
//! the same way and connects to the addresses it kept, so a name cannot
//! resolve a second time to somewhere else. Service bindings, Durable Object
//! calls, and celld's own clients do not pass through here.
//!
//! The same clients carry the roots a Worker trusts (`client`).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, OnceLock};

/// Whether this node limits a Worker's egress to public addresses.
pub fn public_only() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        crate::env_vars::flag("CELLD_EGRESS_PUBLIC_ONLY", false).expect("validated CELLD_EGRESS_PUBLIC_ONLY")
    })
}

/// Why a request was refused. `fetch` rejects with this message, as do
/// `connect()` and an outbound WebSocket; its `egress refused:` prefix tells
/// a caller the request can never pass.
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

/// A host with the brackets of a URL's IPv6 literal taken off.
fn bare(host: &str) -> &str {
    host.trim_start_matches('[').trim_end_matches(']')
}

/// The refusal for a host that is an IP literal `public` keeps out.
fn host_refused(host: &str, public: fn(IpAddr) -> bool) -> Option<EgressRefused> {
    let ip: IpAddr = bare(host).parse().ok()?;
    (!public(ip)).then(|| EgressRefused(format!("egress refused: {ip} is not a public address")))
}

/// The refusal for a URL whose host is a non-public IP literal.
pub fn literal_refused(url: &str) -> Option<EgressRefused> {
    let parsed = reqwest::Url::parse(url).ok()?;
    host_refused(parsed.host_str()?, is_public)
}

/// The refusal for a redirect, when this node limits egress.
pub fn redirect_refused(url: &reqwest::Url) -> Option<EgressRefused> {
    if public_only() {
        literal_refused(url.as_str())
    } else {
        None
    }
}

/// Why a dial did not connect.
#[derive(Debug)]
pub enum DialError {
    /// The policy kept the destination out.
    Refused(EgressRefused),
    /// The name did not resolve, or no address answered.
    Io(std::io::Error),
}

impl std::fmt::Display for DialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DialError::Refused(refused) => refused.fmt(f),
            DialError::Io(error) => error.fmt(f),
        }
    }
}

/// The addresses of `host:port` that `public` lets a connection reach: an IP
/// literal as it is, a name's public addresses, refused when it has none.
/// `fetch`'s resolver and every socket dial take their addresses from here.
async fn public_addrs(
    host: &str,
    port: u16,
    public: fn(IpAddr) -> bool,
) -> Result<Vec<SocketAddr>, DialError> {
    if let Some(refused) = host_refused(host, public) {
        return Err(DialError::Refused(refused));
    }
    // A name the system resolver reads as a number (`127.1`, `2130706433`)
    // is a name here: it resolves, and the filter judges what it became.
    let all = tokio::net::lookup_host((bare(host), port)).await.map_err(DialError::Io)?;
    let kept: Vec<SocketAddr> = all.filter(|a| public(a.ip())).collect();
    if kept.is_empty() {
        return Err(DialError::Refused(EgressRefused(format!(
            "egress refused: {host} resolves to no public address"
        ))));
    }
    Ok(kept)
}

/// The rule a Worker's socket (`connect()`, an outbound WebSocket) dials
/// under. The op that opens the socket chooses it on the JavaScript thread,
/// where the calling object is known.
#[derive(Clone, Copy)]
pub struct Policy {
    /// What counts as public; `None` when nothing is limited.
    public: Option<fn(IpAddr) -> bool>,
}

impl std::fmt::Debug for Policy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.public.is_some() { "Policy::PUBLIC" } else { "Policy::OPEN" })
    }
}

impl Policy {
    /// No limit: a node without the setting, celld's own connections, and an
    /// object's own container port, which the node handed it (`getTcpPort`).
    pub const OPEN: Policy = Policy { public: None };

    /// Public addresses only.
    pub const PUBLIC: Policy = Policy { public: Some(is_public) };

    /// This node's rule for a Worker's socket.
    pub fn node() -> Policy {
        if public_only() {
            Policy::PUBLIC
        } else {
            Policy::OPEN
        }
    }

    /// Open a TCP connection to `host:port` under this rule. Unlimited, it is
    /// the plain connect it always was.
    pub async fn connect(self, host: &str, port: u16) -> Result<tokio::net::TcpStream, DialError> {
        let Some(public) = self.public else {
            return tokio::net::TcpStream::connect((host, port)).await.map_err(DialError::Io);
        };
        let addrs = public_addrs(host, port, public).await?;
        tokio::net::TcpStream::connect(&addrs[..]).await.map_err(DialError::Io)
    }
}

/// A resolver that keeps a name's public addresses only.
struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            match public_addrs(name.as_str(), 0, is_public).await {
                Ok(public) => Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs),
                Err(DialError::Refused(refused)) => Err(Box::new(refused) as Box<dyn std::error::Error + Send + Sync>),
                Err(DialError::Io(error)) => Err(Box::new(error) as Box<dyn std::error::Error + Send + Sync>),
            }
        })
    }
}

/// A client for a Worker's fetch, with this node's egress policy and roots.
///
/// reqwest is built with the platform's roots as well as Mozilla's
/// (object_store asks for them, and Cargo unifies a crate's features across
/// the build), so every client would read the host's store, or the
/// `SSL_CERT_FILE` and `SSL_CERT_DIR` that replace it. A Worker trusts what
/// its WebSockets and `connect()` trust instead: Mozilla's roots and the
/// operator's `CELLD_EXTRA_CA_FILE` (tls_roots.rs).
pub fn client(builder: reqwest::ClientBuilder) -> reqwest::Client {
    let mut builder = builder.tls_built_in_native_certs(false);
    for certificate in crate::tls_roots::extra_certificates() {
        let certificate = reqwest::Certificate::from_der(certificate).expect("a checked root");
        builder = builder.add_root_certificate(certificate);
    }
    let builder = if public_only() { builder.dns_resolver(Arc::new(PublicOnlyResolver)) } else { builder };
    builder.build().expect("build an outbound HTTP client")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn public(s: &str) -> bool {
        is_public(s.parse().unwrap())
    }

    /// A rule under which loopback stands in for the public internet, so a
    /// test can watch an allowed dial land without leaving the machine.
    pub(crate) const LOOPBACK_IS_PUBLIC: Policy = Policy { public: Some(|ip| ip.is_loopback()) };

    /// Hosts a public-only rule refuses, as `connect()` and a URL name them:
    /// loopback, private, link-local (the metadata service), shared,
    /// unique-local (Fly's 6PN), and a mapped private address.
    const NON_PUBLIC_HOSTS: &[&str] = &[
        "127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0",
        "::1", "[::1]", "fe80::1", "fdaa:0:bfe:a7b:21a:59a5:632b:2", "[::ffff:10.0.0.1]",
    ];

    fn refusal(result: Result<tokio::net::TcpStream, DialError>) -> String {
        match result {
            Err(DialError::Refused(refused)) => refused.0,
            Err(DialError::Io(error)) => panic!("expected a refusal, got an I/O error: {error}"),
            Ok(stream) => panic!("expected a refusal, connected to {:?}", stream.peer_addr()),
        }
    }

    /// The next connection `listener` accepts is `stream`'s, so nothing that
    /// was refused before it reached the listener.
    async fn first_accepted_is(listener: &TcpListener, stream: &tokio::net::TcpStream) {
        let (_, peer) = listener.accept().await.unwrap();
        assert_eq!(peer, stream.local_addr().unwrap(), "a refused dial reached the listener");
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

    // Valid: a public literal is kept as it is, with no lookup.
    #[tokio::test]
    async fn a_public_literal_is_kept() {
        let addrs = public_addrs("1.1.1.1", 443, is_public).await.unwrap();
        assert_eq!(addrs, ["1.1.1.1:443".parse::<SocketAddr>().unwrap()]);
        let addrs = public_addrs("[2606:4700::1111]", 443, is_public).await.unwrap();
        assert_eq!(addrs, ["[2606:4700::1111]:443".parse::<SocketAddr>().unwrap()]);
    }

    // Invalid: a socket naming a non-public literal is refused before it
    // dials, so a listener there never sees it.
    #[tokio::test]
    async fn sockets_refuse_non_public_literals() {
        for host in NON_PUBLIC_HOSTS {
            let message = refusal(Policy::PUBLIC.connect(host, 80).await);
            assert!(message.starts_with("egress refused: "), "{host}: {message}");
            assert!(message.ends_with(" is not a public address"), "{host}: {message}");
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let message = refusal(Policy::PUBLIC.connect("127.0.0.1", port).await);
        assert_eq!(message, "egress refused: 127.0.0.1 is not a public address");
        let open = Policy::OPEN.connect("127.0.0.1", port).await.unwrap();
        first_accepted_is(&listener, &open).await;
    }

    // Invalid: a name is judged by what it resolves to, including the
    // numeric spellings the system resolver accepts and that `connect()`'s
    // address parser leaves as names (`connect("127.1:80")`).
    #[tokio::test]
    async fn sockets_refuse_names_that_resolve_to_no_public_address() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        for host in ["localhost", "127.1", "2130706433", "0x7f000001"] {
            let message = refusal(Policy::PUBLIC.connect(host, port).await);
            assert_eq!(message, format!("egress refused: {host} resolves to no public address"));
        }
        let open = Policy::OPEN.connect("127.0.0.1", port).await.unwrap();
        first_accepted_is(&listener, &open).await;
    }

    // Valid: with loopback standing in for public, the same dial connects,
    // by literal and by name, and other private addresses stay refused.
    #[tokio::test]
    async fn sockets_reach_what_the_rule_calls_public() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let by_literal = LOOPBACK_IS_PUBLIC.connect("127.0.0.1", port).await.unwrap();
        first_accepted_is(&listener, &by_literal).await;
        let by_name = LOOPBACK_IS_PUBLIC.connect("localhost", port).await.unwrap();
        first_accepted_is(&listener, &by_name).await;
        let message = refusal(LOOPBACK_IS_PUBLIC.connect("10.1.2.3", port).await);
        assert_eq!(message, "egress refused: 10.1.2.3 is not a public address");
    }

    // Off: a dial is the plain connect it always was, loopback included.
    #[tokio::test]
    async fn sockets_without_the_setting_are_unchanged() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        for host in ["127.0.0.1", "localhost"] {
            let stream = Policy::OPEN.connect(host, port).await.unwrap();
            first_accepted_is(&listener, &stream).await;
        }
        if !public_only() {
            assert!(Policy::node().public.is_none(), "unset, a node does not limit its sockets");
        }
    }
}
