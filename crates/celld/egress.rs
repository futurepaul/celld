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
//!
//! An operator opens named internal hosts beside the public internet with
//! `CELLD_EGRESS_ALLOW` ([`Allow`]): a name it lists keeps its private
//! addresses too, and an address in a network it lists passes. It never
//! opens loopback, link-local, unspecified, multicast or broadcast
//! addresses, `localhost`, a name it marks `!`, or an address of this
//! machine's own: those stay refused whatever it lists.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, OnceLock};

/// Whether this node limits a Worker's egress to public addresses.
pub fn public_only() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        crate::env_vars::flag("CELLD_EGRESS_PUBLIC_ONLY", false).expect("validated CELLD_EGRESS_PUBLIC_ONLY")
    })
}

/// The most entries `CELLD_EGRESS_ALLOW` holds.
pub const ALLOW_MAX: usize = 128;
/// The longest entry: a DNS name's most (253 bytes), a `*.` or `!` and a
/// `!*.` spare.
pub const ALLOW_ENTRY_MAX_BYTES: usize = 256;

/// The internal hosts a node's operator opens (`CELLD_EGRESS_ALLOW`), a
/// comma-separated list of:
///
/// - `name`: that host name, exactly;
/// - `*.name`: one label under the name, not the name itself;
/// - `a.b.c.d`, `a.b.c.d/n`, an IPv6 address or network: those addresses;
/// - `!name`: that name and every name under it are never opened, whatever
///   else matches them (the platform's own hosts, which a glob would cover).
///
/// An entry naming loopback, link-local, unspecified, multicast, broadcast
/// or `localhost`, or a network holding one, is refused at start.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Allow {
    names: Vec<String>,
    under: Vec<String>,
    nets: Vec<(IpAddr, u8)>,
    never: Vec<String>,
}

/// A host name as it is compared: lower case, no trailing dot.
fn name_of(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// Whether `name` is `under` or a name below it.
fn at_or_under(name: &str, under: &str) -> bool {
    name == under || name.strip_suffix(under).is_some_and(|rest| rest.ends_with('.'))
}

/// A host name a URL keeps as a name: labels of letters, digits, `-` and
/// `_`, the last not all digits (`10.1` is an address).
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && !name.rsplit('.').next().is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit()))
        && name.split('.').all(|l| {
            (1..=63).contains(&l.len())
                && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
                && !l.starts_with('-')
                && !l.ends_with('-')
        })
}

/// An IPv6 address that reaches an IPv4 one (mapped, or NAT64's
/// `64:ff9b::/96`), as that address.
fn as_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    ip.to_ipv4_mapped().or_else(|| {
        (s[..6] == [0x64, 0xff9b, 0, 0, 0, 0]).then(|| Ipv4Addr::from((u32::from(s[6]) << 16) | u32::from(s[7])))
    })
}

/// An address, IPv4 behind IPv6 as the IPv4 it reaches.
fn plain(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => as_v4(v6).map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

fn bits(ip: IpAddr) -> (u128, u8) {
    match ip {
        IpAddr::V4(v4) => (u128::from(u32::from(v4)), 32),
        IpAddr::V6(v6) => (u128::from(v6), 128),
    }
}

/// The network `ip/prefix`, its host bits cleared.
fn masked(ip: IpAddr, prefix: u8) -> IpAddr {
    let (b, width) = bits(ip);
    let b = if prefix == 0 { 0 } else { b & (u128::MAX << (width - prefix)) };
    match ip {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::from(b as u32)),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::from(b)),
    }
}

/// Whether two networks of one family share an address.
fn overlap((a, a_len): (IpAddr, u8), (b, b_len): (IpAddr, u8)) -> bool {
    let shorter = a_len.min(b_len);
    a.is_ipv4() == b.is_ipv4() && masked(a, shorter) == masked(b, shorter)
}

/// The networks the allowlist never opens, and what each is.
const NEVER_NETS: [(IpAddr, u8, &str); 11] = [
    (IpAddr::V4(Ipv4Addr::new(127, 0, 0, 0)), 8, "loopback"),
    (IpAddr::V4(Ipv4Addr::new(169, 254, 0, 0)), 16, "link-local"),
    (IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 8, "unspecified"),
    (IpAddr::V4(Ipv4Addr::new(224, 0, 0, 0)), 4, "multicast"),
    (IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255)), 32, "broadcast"),
    // `::`, `::1` and the IPv4-compatible `::a.b.c.d`
    (IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0)), 96, "loopback"),
    (IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0)), 10, "link-local"),
    (IpAddr::V6(Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0)), 8, "multicast"),
    // IPv4 behind IPv6: listed as IPv4, a wider network holds IPv4's loopback
    (IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0)), 96, "loopback"),
    (IpAddr::V6(Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0)), 96, "loopback"),
    (IpAddr::V6(Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0)), 48, "loopback"),
];

/// What a network holds that is never opened, if it does.
fn never_net(ip: IpAddr, prefix: u8) -> Option<&'static str> {
    NEVER_NETS.iter().find(|(n, len, _)| overlap((ip, prefix), (*n, *len))).map(|(_, _, what)| *what)
}

/// Whether `ip` is one of this machine's own addresses: one a socket can
/// bind to. A node set to bind any address (`ip_nonlocal_bind`) calls
/// every address its own, and the allowlist opens none.
fn own_address(ip: IpAddr) -> bool {
    std::net::UdpSocket::bind((ip, 0)).is_ok()
}

impl Allow {
    /// The list `raw` names (`CELLD_EGRESS_ALLOW`), or why it is refused.
    pub fn parse(raw: &str) -> Result<Allow, String> {
        let mut allow = Allow::default();
        let entries: Vec<&str> = raw.split(',').map(str::trim).filter(|e| !e.is_empty()).collect();
        if entries.len() > ALLOW_MAX {
            return Err(format!("{} entries, more than {ALLOW_MAX}", entries.len()));
        }
        for entry in entries {
            if entry.len() > ALLOW_ENTRY_MAX_BYTES {
                return Err(format!("an entry is longer than {ALLOW_ENTRY_MAX_BYTES} bytes"));
            }
            let not = || format!("{entry:?} is no host name, `*.` glob, `!` name, IP address or network");
            let never = |what: &str| format!("{entry:?} is or holds a {what} address, which egress never reaches");
            if let Some(name) = entry.strip_prefix('!') {
                let name = name_of(name);
                if !valid_name(&name) {
                    return Err(not());
                }
                allow.never.push(name);
                continue;
            }
            let net = match entry.split_once('/') {
                Some((ip, prefix)) => {
                    let ip: IpAddr = ip.parse().map_err(|_| not())?;
                    let len = prefix.parse::<u8>().ok().filter(|p| *p <= bits(ip).1 && !prefix.starts_with('+')).ok_or_else(not)?;
                    Some((ip, len))
                }
                None => entry.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().ok().map(|ip| (ip, bits(ip).1)),
            };
            if let Some((ip, len)) = net {
                let (ip, len) = match ip {
                    IpAddr::V6(v6) if len >= 96 && as_v4(v6).is_some() => (IpAddr::V4(as_v4(v6).expect("checked")), len - 96),
                    _ => (ip, len),
                };
                if let Some(what) = never_net(ip, len) {
                    return Err(never(what));
                }
                allow.nets.push((masked(ip, len), len));
                continue;
            }
            let (list, name) = match entry.strip_prefix("*.") {
                Some(under) => (&mut allow.under, name_of(under)),
                None => (&mut allow.names, name_of(entry)),
            };
            if !valid_name(&name) {
                return Err(not());
            }
            if at_or_under(&name, "localhost") {
                return Err(never("localhost"));
            }
            list.push(name);
        }
        Ok(allow)
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.under.is_empty() && self.nets.is_empty()
    }

    /// Whether the list opens `ip`, which `host` resolved to (or which it
    /// is, as a literal): a name it lists, or an address in a network it
    /// lists, and never one of what it never opens.
    pub fn admits(&self, host: &str, ip: IpAddr) -> bool {
        let ip = plain(ip);
        if never_net(ip, bits(ip).1).is_some() {
            return false;
        }
        let name = name_of(bare(host));
        let literal = name.parse::<IpAddr>().is_ok();
        if !literal && (at_or_under(&name, "localhost") || self.never.iter().any(|n| at_or_under(&name, n))) {
            return false;
        }
        let named = !literal
            && (self.names.contains(&name)
                || self.under.iter().any(|n| {
                    name.strip_suffix(n.as_str()).and_then(|l| l.strip_suffix('.')).is_some_and(|l| !l.is_empty() && !l.contains('.'))
                }));
        (named || self.nets.iter().any(|(n, len)| overlap((*n, *len), (ip, bits(ip).1)))) && !own_address(ip)
    }
}

/// This node's allowlist (`CELLD_EGRESS_ALLOW`), when it lists anything.
pub fn allowed() -> Option<&'static Allow> {
    static ALLOW: OnceLock<Allow> = OnceLock::new();
    let allow = ALLOW.get_or_init(|| {
        let raw = crate::env_vars::value("CELLD_EGRESS_ALLOW").expect("validated CELLD_EGRESS_ALLOW").unwrap_or_default();
        Allow::parse(&raw).expect("validated CELLD_EGRESS_ALLOW")
    });
    (!allow.is_empty()).then_some(allow)
}

/// Checks `CELLD_EGRESS_ALLOW` at start: it parses, and it is set only
/// beside `CELLD_EGRESS_PUBLIC_ONLY=1`, which it widens.
pub fn validate() -> anyhow::Result<()> {
    let Some(raw) = crate::env_vars::value("CELLD_EGRESS_ALLOW")? else {
        return Ok(());
    };
    let allow = Allow::parse(&raw).map_err(|e| anyhow::anyhow!("CELLD_EGRESS_ALLOW: {e}"))?;
    if !allow.is_empty() && !crate::env_vars::flag("CELLD_EGRESS_PUBLIC_ONLY", false)? {
        anyhow::bail!("CELLD_EGRESS_ALLOW widens CELLD_EGRESS_PUBLIC_ONLY=1, which is not set: without it a Worker's egress is open already");
    }
    Ok(())
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

/// The refusal for a host that is an IP literal `public` keeps out and
/// `allow` does not open.
fn host_refused(host: &str, public: fn(IpAddr) -> bool, allow: Option<&Allow>) -> Option<EgressRefused> {
    let ip: IpAddr = bare(host).parse().ok()?;
    (!public(ip) && !allow.is_some_and(|a| a.admits(host, ip)))
        .then(|| EgressRefused(format!("egress refused: {ip} is not a public address")))
}

/// The refusal for a URL whose host is a non-public IP literal this node's
/// allowlist does not open.
pub fn literal_refused(url: &str) -> Option<EgressRefused> {
    let parsed = reqwest::Url::parse(url).ok()?;
    host_refused(parsed.host_str()?, is_public, allowed())
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

/// The addresses of `host:port` that `public` (or `allow`) lets a
/// connection reach: an IP literal as it is, a name's public addresses and
/// those `allow` opens, refused when it has none. `fetch`'s resolver and
/// every socket dial take their addresses from here.
async fn public_addrs(
    host: &str,
    port: u16,
    public: fn(IpAddr) -> bool,
    allow: Option<&Allow>,
) -> Result<Vec<SocketAddr>, DialError> {
    if let Some(refused) = host_refused(host, public, allow) {
        return Err(DialError::Refused(refused));
    }
    // A name the system resolver reads as a number (`127.1`, `2130706433`)
    // is a name here: it resolves, and the filter judges what it became.
    let all = tokio::net::lookup_host((bare(host), port)).await.map_err(DialError::Io)?;
    let kept: Vec<SocketAddr> = all.filter(|a| public(a.ip()) || allow.is_some_and(|al| al.admits(host, a.ip()))).collect();
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
    /// What the operator opens beside it (`CELLD_EGRESS_ALLOW`).
    allow: Option<&'static Allow>,
}

impl std::fmt::Debug for Policy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.public.is_some() { "Policy::PUBLIC" } else { "Policy::OPEN" })
    }
}

impl Policy {
    /// No limit: a node without the setting, celld's own connections, and an
    /// object's own container port, which the node handed it (`getTcpPort`).
    pub const OPEN: Policy = Policy { public: None, allow: None };

    /// Public addresses only.
    pub const PUBLIC: Policy = Policy { public: Some(is_public), allow: None };

    /// This node's rule for a Worker's socket.
    pub fn node() -> Policy {
        if public_only() {
            Policy { allow: allowed(), ..Policy::PUBLIC }
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
        let addrs = public_addrs(host, port, public, self.allow).await?;
        tokio::net::TcpStream::connect(&addrs[..]).await.map_err(DialError::Io)
    }
}

/// A resolver that keeps a name's public addresses only.
struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            match public_addrs(name.as_str(), 0, is_public, allowed()).await {
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
    pub(crate) const LOOPBACK_IS_PUBLIC: Policy = Policy { public: Some(|ip| ip.is_loopback()), allow: None };

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
        let addrs = public_addrs("1.1.1.1", 443, is_public, None).await.unwrap();
        assert_eq!(addrs, ["1.1.1.1:443".parse::<SocketAddr>().unwrap()]);
        let addrs = public_addrs("[2606:4700::1111]", 443, is_public, None).await.unwrap();
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

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// One of this machine's own non-loopback addresses, if it has a route
    /// out (a connected UDP socket sends nothing).
    fn own_v4() -> Option<IpAddr> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect("10.255.255.254:9").ok()?;
        Some(socket.local_addr().ok()?.ip()).filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
    }

    // Valid: names, one-level globs, addresses and networks, in any case,
    // with trailing dots; an empty or unset list is no list.
    #[test]
    fn an_allowlist_reads_its_entries() {
        let a = Allow::parse(" Wiki.Corp.Example. , *.corp.internal, 10.20.0.0/16, 100.101.102.103, ::ffff:10.30.0.0/112, [fd7a:115c:a1e0::5], !fragment.corp.internal ").unwrap();
        assert_eq!(a.names, ["wiki.corp.example"]);
        assert_eq!(a.under, ["corp.internal"]);
        assert_eq!(a.nets, [(ip("10.20.0.0"), 16), (ip("100.101.102.103"), 32), (ip("10.30.0.0"), 16), (ip("fd7a:115c:a1e0::5"), 128)]);
        assert_eq!(a.never, ["fragment.corp.internal"]);
        assert!(Allow::parse("").unwrap().is_empty() && Allow::parse(" , ").unwrap().is_empty());
        assert!(Allow::parse("!fragment.corp.internal").unwrap().is_empty(), "a `!` alone opens nothing");
    }

    // Invalid: out of shape, past the bounds, and the never set, alone or
    // held by a wider network.
    #[test]
    fn an_allowlist_refuses_what_it_cannot_open() {
        for bad in ["*", "*.", "*.*.corp", "wiki:80", "http://wiki/", "10.1", "10.0.0.0/33", "10.0.0.0/+8", "bücher.corp", "!", "!*.corp"] {
            let said = Allow::parse(bad).unwrap_err();
            assert!(said.contains("is no host name"), "{bad}: {said}");
        }
        for (bad, what) in [
            ("127.0.0.1", "loopback"), ("0.0.0.0/0", "loopback"), ("96.0.0.0/3", "loopback"), ("::1", "loopback"), ("::/0", "loopback"),
            ("::ffff:127.0.0.1", "loopback"), ("64:ff9b::/96", "loopback"), ("169.254.169.254", "link-local"), ("169.0.0.0/8", "link-local"),
            ("fe80::1", "link-local"), ("0.0.0.0", "unspecified"), ("224.0.0.1", "multicast"), ("ff02::1", "multicast"),
            ("255.255.255.255", "broadcast"), ("localhost", "localhost"), ("a.fragment.localhost", "localhost"), ("*.localhost", "localhost"),
        ] {
            let said = Allow::parse(&format!("wiki.corp,{bad}")).unwrap_err();
            assert!(said.contains(&format!("is or holds a {what} address")), "{bad}: {said}");
        }
        assert!(Allow::parse(&vec!["wiki.corp"; ALLOW_MAX].join(",")).is_ok());
        assert!(Allow::parse(&vec!["wiki.corp"; ALLOW_MAX + 1].join(",")).unwrap_err().contains("more than"));
        assert!(Allow::parse(&format!("{}.corp", "a".repeat(ALLOW_ENTRY_MAX_BYTES))).unwrap_err().contains("longer than"));
    }

    // What it opens: a listed name at any private address, a glob's one
    // label, a listed network's addresses (mapped too), and never a `!`
    // name, localhost, loopback, link-local or the machine's own address.
    #[test]
    fn an_allowlist_admits_what_it_lists() {
        let a = Allow::parse("wiki.corp.example,*.corp.internal,10.20.0.0/16,!fragment.corp.internal").unwrap();
        for (host, to) in [
            ("wiki.corp.example", "192.168.9.9"), ("WIKI.corp.example.", "100.70.0.1"), ("git.corp.internal", "172.16.0.5"),
            ("10.20.3.4", "10.20.3.4"), ("[::ffff:10.20.3.4]", "::ffff:10.20.3.4"), ("other.example", "10.20.0.9"), ("other.example", "64:ff9b::a14:9"),
        ] {
            assert!(a.admits(host, ip(to)), "{host} at {to}");
        }
        for (host, to) in [
            ("wiki2.corp.example", "192.168.9.9"), ("corp.internal", "172.16.0.5"), ("a.git.corp.internal", "172.16.0.5"),
            ("fragment.corp.internal", "172.16.0.5"), ("x.fragment.corp.internal", "10.20.0.1"), ("wiki.corp.example", "127.0.0.1"),
            ("wiki.corp.example", "::ffff:127.0.0.1"), ("wiki.corp.example", "169.254.169.254"), ("wiki.corp.example", "::1"),
            ("localhost", "10.20.0.1"), ("10.21.0.1", "10.21.0.1"),
        ] {
            assert!(!a.admits(host, ip(to)), "{host} at {to}");
        }
        if let Some(own) = own_v4() {
            let mine = Allow::parse(&format!("box.corp,{own}/32")).unwrap();
            assert!(!mine.admits("box.corp", own) && !mine.admits(&own.to_string(), own), "the machine's own {own}");
        }
    }

    // A literal the list opens is kept for the dial with no lookup; one it
    // does not is refused, as before.
    #[tokio::test]
    async fn a_listed_literal_is_kept() {
        let a = Allow::parse("10.255.0.0/16").unwrap();
        let addrs = public_addrs("10.255.0.1", 80, is_public, Some(&a)).await.unwrap();
        assert_eq!(addrs, ["10.255.0.1:80".parse::<SocketAddr>().unwrap()]);
        let message = match public_addrs("10.254.0.1", 80, is_public, Some(&a)).await {
            Err(DialError::Refused(refused)) => refused.0,
            other => panic!("expected a refusal, got {other:?}"),
        };
        assert_eq!(message, "egress refused: 10.254.0.1 is not a public address");
    }

    // The machine's own address stays shut even when listed: a listener
    // there never sees the refused dial.
    #[tokio::test]
    async fn an_own_address_stays_shut() {
        let Some(own) = own_v4() else { return };
        let listener = TcpListener::bind((own, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let allow: &'static Allow = Box::leak(Box::new(Allow::parse(&format!("{own}/32")).unwrap()));
        let message = refusal(Policy { allow: Some(allow), ..Policy::PUBLIC }.connect(&own.to_string(), port).await);
        assert_eq!(message, format!("egress refused: {own} is not a public address"));
        let open = Policy::OPEN.connect(&own.to_string(), port).await.unwrap();
        first_accepted_is(&listener, &open).await;
    }
}
