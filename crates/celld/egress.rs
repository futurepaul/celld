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
//! addresses, `localhost`, or a name it marks `!`: those stay refused
//! whatever it lists, public or not. Under `CELLD_EGRESS_PUBLIC_ONLY`, an
//! address of this machine's own is refused too, list or no list, however
//! public it is: a Worker never reaches its own node's listeners.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, OnceLock};

/// Whether this node limits a Worker's egress to public addresses.
pub fn public_only() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        crate::env_vars::flag("CELLD_EGRESS_PUBLIC_ONLY", false).expect("validated CELLD_EGRESS_PUBLIC_ONLY")
    })
}

/// The most entries `CELLD_EGRESS_ALLOW` holds. Fragment's deployment
/// config, the list's usual source, bounds its own entries at 64
/// (`fragment_core::egress::ALLOW_MAX`) and adds its `!` markers (the
/// platform's host and suffix) when it renders this variable, so a node
/// takes more than it: this bound is a node's own guard on what it reads,
/// not the product's limit.
pub const ALLOW_MAX: usize = 128;
/// The longest entry: a DNS name's most (253 bytes) with a `*.` or `!`
/// before it. Fragment's own entries are at most 255 bytes; its `!`
/// markers are a name and one byte.
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

/// The addresses this machine's interfaces hold, from the kernel's own
/// tables in this network namespace: IPv4's local routes
/// (`/proc/net/fib_trie`'s `/32 host LOCAL` entries, secondary addresses
/// among them) and IPv6's addresses (`/proc/net/if_inet6`, absent when
/// IPv6 is off). Reading them opens no socket, so a sandbox that denies
/// `bind` (seccomp, systemd's `SocketBindDeny=`) still tells them; glibc's
/// `getifaddrs` binds a netlink socket, and such a sandbox refuses it.
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)] // the host's network tables, not node storage
fn interface_addrs() -> std::io::Result<Vec<IpAddr>> {
    let mut addrs = local_v4(&std::fs::read_to_string("/proc/net/fib_trie")?);
    match std::fs::read_to_string("/proc/net/if_inet6") {
        Ok(table) => addrs.extend(table.lines().filter_map(|line| {
            let hex = line.split_whitespace().next()?;
            u128::from_str_radix(hex, 16).ok().filter(|_| hex.len() == 32).map(|bits| plain(IpAddr::V6(Ipv6Addr::from(bits))))
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(addrs)
}

/// The addresses `/proc/net/fib_trie` routes to this host: each
/// `|-- a.b.c.d` followed by `/32 host LOCAL`.
#[cfg(target_os = "linux")]
fn local_v4(trie: &str) -> Vec<IpAddr> {
    let mut addrs = Vec::new();
    let mut last: Option<Ipv4Addr> = None;
    for line in trie.lines().map(str::trim) {
        if let Some(addr) = line.strip_prefix("|-- ").or_else(|| line.strip_prefix("+-- ")) {
            last = addr.split('/').next().and_then(|a| a.parse().ok());
        } else if line == "/32 host LOCAL" {
            addrs.extend(last.map(IpAddr::V4));
        }
    }
    addrs
}

/// The addresses this machine's interfaces hold (`getifaddrs`, which reads
/// them by sysctl on these systems, no `bind`), IPv4 behind IPv6 as IPv4.
#[cfg(all(unix, not(target_os = "linux")))]
fn interface_addrs() -> std::io::Result<Vec<IpAddr>> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs writes a list it owns into `head`, freed below.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut addrs = Vec::new();
    let mut at = head;
    while !at.is_null() {
        // SAFETY: `at` is a node of the list getifaddrs made, not yet freed;
        // its address, when set, is a sockaddr of the family it names.
        unsafe {
            let ifa = &*at;
            if !ifa.ifa_addr.is_null() {
                match i32::from((*ifa.ifa_addr).sa_family) {
                    libc::AF_INET => {
                        let sin = &*ifa.ifa_addr.cast::<libc::sockaddr_in>();
                        addrs.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr))));
                    }
                    libc::AF_INET6 => {
                        let sin6 = &*ifa.ifa_addr.cast::<libc::sockaddr_in6>();
                        addrs.push(plain(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr))));
                    }
                    _ => {}
                }
            }
            at = ifa.ifa_next;
        }
    }
    // SAFETY: `head` came from getifaddrs and is freed once.
    unsafe { libc::freeifaddrs(head) };
    Ok(addrs)
}

#[cfg(not(unix))]
fn interface_addrs() -> std::io::Result<Vec<IpAddr>> {
    Err(std::io::Error::other("no interface discovery on this platform"))
}

/// Whether `ip` is this machine's own: loopback or unspecified (both reach
/// it), or an address one of its interfaces holds, compared as IPv4 when
/// it is IPv4 behind IPv6. The interfaces are read at every ask, so an
/// address the machine gains or loses (DHCP, a tailnet coming up) counts
/// at once; on Linux that is two in-memory tables, read for each address
/// a new connection may use. A read that fails is an `Err`, which refuses
/// the destination (the node cannot tell, so it fails closed).
/// An address family the node cannot open a socket in (IPv6 disabled)
/// is simply on no interface: a public address there is not its own, and
/// its dial fails as unreachable.
pub(crate) fn own_address(ip: IpAddr) -> Result<bool, String> {
    let ip = plain(ip);
    if ip.is_loopback() || ip.is_unspecified() {
        return Ok(true);
    }
    let addrs = interface_addrs().map_err(|e| format!("reading this node's interfaces failed: {e}"))?;
    Ok(addrs.contains(&ip))
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

    /// Whether the list names nothing at all, neither to open nor to keep
    /// shut.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.under.is_empty() && self.nets.is_empty() && self.never.is_empty()
    }

    /// Whether `host` is a name the list never opens (`!`): refused
    /// whatever it resolves to, a public address included.
    pub fn never_names(&self, host: &str) -> bool {
        let name = name_of(bare(host));
        name.parse::<IpAddr>().is_err() && self.never.iter().any(|n| at_or_under(&name, n))
    }

    /// Whether the list opens `ip`, which `host` resolved to (or which it
    /// is, as a literal): a name it lists, or an address in a network it
    /// lists, and never one of what it never opens. The machine's own
    /// addresses are the policy's to refuse (`Policy`), list or no list.
    pub fn admits(&self, host: &str, ip: IpAddr) -> bool {
        let ip = plain(ip);
        if never_net(ip, bits(ip).1).is_some() || self.never_names(host) {
            return false;
        }
        let name = name_of(bare(host));
        let literal = name.parse::<IpAddr>().is_ok();
        if !literal && at_or_under(&name, "localhost") {
            return false;
        }
        let named = !literal
            && (self.names.contains(&name)
                || self.under.iter().any(|n| {
                    name.strip_suffix(n.as_str()).and_then(|l| l.strip_suffix('.')).is_some_and(|l| !l.is_empty() && !l.contains('.'))
                }));
        named || self.nets.iter().any(|(n, len)| overlap((*n, *len), (ip, bits(ip).1)))
    }
}

/// This node's allowlist (`CELLD_EGRESS_ALLOW`), when it names anything.
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
/// `allow` does not open, or one `own` claims.
fn host_refused(host: &str, public: fn(IpAddr) -> bool, allow: Option<&Allow>, own: Own) -> Option<EgressRefused> {
    let ip: IpAddr = bare(host).parse().ok()?;
    if !public(ip) && !allow.is_some_and(|a| a.admits(host, ip)) {
        return Some(EgressRefused(format!("egress refused: {ip} is not a public address")));
    }
    own_refused(ip, own)
}

/// How a rule tells this machine's own addresses (`own_address`).
type Own = fn(IpAddr) -> Result<bool, String>;

/// The refusal for an address `own` claims, or cannot tell about.
fn own_refused(ip: IpAddr, own: Own) -> Option<EgressRefused> {
    match own(ip) {
        Ok(false) => None,
        Ok(true) => Some(EgressRefused(format!("egress refused: {ip} is this node's own address"))),
        Err(why) => Some(EgressRefused(format!("egress refused: cannot tell whether {ip} is this node's own address ({why})"))),
    }
}

/// The refusal for a URL whose host is an IP literal this node keeps out:
/// its own, or a non-public one its allowlist does not open.
pub fn literal_refused(url: &str) -> Option<EgressRefused> {
    let parsed = reqwest::Url::parse(url).ok()?;
    host_refused(parsed.host_str()?, is_public, allowed(), own_address)
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
/// those `allow` opens, refused when it has none. Whatever lets an address
/// in, a name `allow` marks `!` and an address `own` claims are refused.
/// `fetch`'s resolver and every socket dial take their addresses from here.
async fn public_addrs(
    host: &str,
    port: u16,
    public: fn(IpAddr) -> bool,
    allow: Option<&Allow>,
    own: Own,
) -> Result<Vec<SocketAddr>, DialError> {
    if let Some(refused) = host_refused(host, public, allow, own) {
        return Err(DialError::Refused(refused));
    }
    if allow.is_some_and(|a| a.never_names(host)) {
        return Err(DialError::Refused(EgressRefused(format!("egress refused: {host} is never reached"))));
    }
    // A name the system resolver reads as a number (`127.1`, `2130706433`)
    // is a name here: it resolves, and the filter judges what it became.
    let all = tokio::net::lookup_host((bare(host), port)).await.map_err(DialError::Io)?;
    let mut kept = Vec::new();
    for a in all.filter(|a| public(a.ip()) || allow.is_some_and(|al| al.admits(host, a.ip()))) {
        match own(a.ip()) {
            Ok(false) => kept.push(a),
            Ok(true) => {}
            Err(_) => return Err(DialError::Refused(own_refused(a.ip(), own).expect("an Err refuses"))),
        }
    }
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
    /// Whether an address is this machine's own, refused whenever `public`
    /// limits anything (`own_address`).
    own: Own,
}

impl std::fmt::Debug for Policy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.public.is_some() { "Policy::PUBLIC" } else { "Policy::OPEN" })
    }
}

impl Policy {
    /// No limit: a node without the setting, celld's own connections, and an
    /// object's own container port, which the node handed it (`getTcpPort`).
    pub const OPEN: Policy = Policy { public: None, allow: None, own: own_address };

    /// Public addresses only.
    pub const PUBLIC: Policy = Policy { public: Some(is_public), allow: None, own: own_address };

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
        let addrs = public_addrs(host, port, public, self.allow, self.own).await?;
        tokio::net::TcpStream::connect(&addrs[..]).await.map_err(DialError::Io)
    }
}

/// A resolver that keeps a name's public addresses only.
struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            match public_addrs(name.as_str(), 0, is_public, allowed(), own_address).await {
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

    /// A rule under which loopback stands in for the public internet (and
    /// so is no address of this machine's), so a test can watch an allowed
    /// dial land without leaving the machine.
    pub(crate) const LOOPBACK_IS_PUBLIC: Policy = Policy { public: Some(|ip| ip.is_loopback()), allow: None, own: |_| Ok(false) };

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
        let addrs = public_addrs("1.1.1.1", 443, is_public, None, own_address).await.unwrap();
        assert_eq!(addrs, ["1.1.1.1:443".parse::<SocketAddr>().unwrap()]);
        let addrs = public_addrs("[2606:4700::1111]", 443, is_public, None, own_address).await.unwrap();
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
        let shut = Allow::parse("!fragment.corp.internal").unwrap();
        assert!(!shut.is_empty() && !shut.admits("fragment.corp.internal", ip("10.0.0.1")), "a `!` alone opens nothing, and is kept");
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
    }

    // A literal the list opens is kept for the dial with no lookup; one it
    // does not is refused, as before.
    #[tokio::test]
    async fn a_listed_literal_is_kept() {
        let a = Allow::parse("10.255.0.0/16").unwrap();
        let addrs = public_addrs("10.255.0.1", 80, is_public, Some(&a), own_address).await.unwrap();
        assert_eq!(addrs, ["10.255.0.1:80".parse::<SocketAddr>().unwrap()]);
        let message = match public_addrs("10.254.0.1", 80, is_public, Some(&a), own_address).await {
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
        assert_eq!(message, format!("egress refused: {own} is this node's own address"));
        let open = Policy::OPEN.connect(&own.to_string(), port).await.unwrap();
        first_accepted_is(&listener, &open).await;
    }

    // Review round 1, reproduction 1: a `!` name is refused even when DNS
    // answers it with public addresses (the platform's public host).
    #[tokio::test]
    async fn a_never_name_with_a_public_answer_is_refused() {
        let allow = Allow::parse("wiki.internal,!one.one.one.one").unwrap();
        match public_addrs("one.one.one.one", 80, is_public, Some(&allow), own_address).await {
            Err(DialError::Refused(refused)) => assert!(refused.0.starts_with("egress refused: "), "{}", refused.0),
            Err(DialError::Io(error)) => eprintln!("skipped: one.one.one.one does not resolve here ({error})"),
            Ok(addrs) => panic!("a `!` name reached {addrs:?}"),
        }
        // the same, offline: loopback stands in for the public internet
        let allow = Allow::parse("wiki.internal,!localhost").unwrap();
        let kept = public_addrs("localhost", 80, |ip| ip.is_loopback(), Some(&allow), |_| Ok(false)).await;
        assert!(matches!(kept, Err(DialError::Refused(_))), "{kept:?}");
    }

    // Review round 1, reproduction 2: an address the rule calls public but
    // that is this machine's own stays shut (a node's global address).
    #[tokio::test]
    async fn an_own_address_the_rule_calls_public_is_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let allow: &'static Allow = Box::leak(Box::new(Allow::parse("wiki.internal").unwrap()));
        // the stand-in, with this machine's real probe in place of its "none"
        let rule = Policy { allow: Some(allow), own: own_address, ..LOOPBACK_IS_PUBLIC };
        let message = refusal(rule.connect("127.0.0.1", port).await);
        assert!(message.starts_with("egress refused: "), "{message}");
        let open = Policy::OPEN.connect("127.0.0.1", port).await.unwrap();
        first_accepted_is(&listener, &open).await;
    }

    // With no list at all, public-only still refuses this machine's own
    // address, however public the rule calls it: by literal and by name.
    #[tokio::test]
    async fn with_no_list_an_own_address_is_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let everything_public = Policy { public: Some(|_| true), allow: None, own: own_address };
        for host in ["127.0.0.1", "localhost"] {
            let message = refusal(everything_public.connect(host, port).await);
            assert!(message.starts_with("egress refused: "), "{host}: {message}");
        }
        assert_eq!(
            host_refused("127.0.0.1", |_| true, None, own_address).map(|r| r.0).as_deref(),
            Some("egress refused: 127.0.0.1 is this node's own address")
        );
        let open = Policy::OPEN.connect("127.0.0.1", port).await.unwrap();
        first_accepted_is(&listener, &open).await;
    }

    // This machine's addresses, by its interfaces: loopback and
    // unspecified in each spelling, an interface's address and its mapped
    // form are its own; a public address or one no interface has is not.
    #[test]
    #[allow(clippy::disallowed_methods)] // the test host's own tables
    fn own_addresses_by_interface() {
        for own in ["127.0.0.1", "127.0.0.2", "::1", "::ffff:127.0.0.1", "0.0.0.0", "::"] {
            assert_eq!(own_address(ip(own)), Ok(true), "{own}");
        }
        for other in ["1.1.1.1", "::ffff:1.1.1.1", "2606:4700::1111", "64:ff9b::101:101"] {
            assert_eq!(own_address(ip(other)), Ok(false), "{other}");
        }
        let addrs = interface_addrs().unwrap();
        assert!(addrs.contains(&ip("127.0.0.1")), "{addrs:?}");
        if let Some(own) = own_v4() {
            assert!(addrs.contains(&own), "{own} in {addrs:?}");
            assert_eq!(own_address(own), Ok(true));
            let IpAddr::V4(v4) = own else { unreachable!("own_v4") };
            assert_eq!(own_address(IpAddr::V6(v4.to_ipv6_mapped())), Ok(true));
        }
        if !addrs.contains(&ip("10.255.255.254")) {
            assert_eq!(own_address(ip("10.255.255.254")), Ok(false));
        }
        assert!(addrs.contains(&ip("::1")) || !std::path::Path::new("/proc/net/if_inet6").exists(), "{addrs:?}");
        // a read that fails is refused, saying why
        let refused = own_refused(ip("1.1.1.1"), |_| Err("reading this node's interfaces failed: test".into()));
        assert_eq!(
            refused.map(|r| r.0).as_deref(),
            Some("egress refused: cannot tell whether 1.1.1.1 is this node's own address (reading this node's interfaces failed: test)")
        );
    }

    /// The child half of `a_bind_denied_sandbox_keeps_public_egress`: it
    /// runs only in the process that test starts, under a seccomp filter
    /// that answers every `bind` with `CELLD_TEST_BIND_DENIED`'s errno,
    /// while TCP connect still binds its own port implicitly.
    #[cfg(target_os = "linux")]
    #[test]
    #[allow(clippy::disallowed_methods)] // a child process of the test's own, on the host's clock
    fn bind_denied_child() {
        let Ok(errno) = std::env::var("CELLD_TEST_BIND_DENIED") else { return };
        let errno: u32 = errno.parse().unwrap();
        let no_inet6 = std::env::var("CELLD_TEST_NO_INET6").is_ok();
        deny_bind(errno, no_inet6);
        let own: IpAddr = std::env::var("CELLD_TEST_OWN").unwrap().parse().unwrap();
        let port: u16 = std::env::var("CELLD_TEST_PORT").unwrap().parse().unwrap();
        let error = std::net::UdpSocket::bind("0.0.0.0:0").expect_err("the filter denies bind");
        assert_eq!(error.raw_os_error(), Some(errno as i32), "{error}");
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            // TCP still leaves: connect binds implicitly
            let open = Policy::OPEN.connect(&own.to_string(), port).await;
            assert!(open.is_ok(), "a plain TCP connect under the filter: {:?}", open.err());
            // public literals are no address of this node's
            for host in ["1.1.1.1", "[2606:4700::1111]"] {
                assert!(host_refused(host, is_public, None, own_address).is_none(), "{host}");
                let kept = public_addrs(host, 443, is_public, None, own_address).await;
                assert!(kept.is_ok(), "{host}: {:?}", kept.err().map(|e| e.to_string()));
            }
            // with IPv6 sockets refused (IPv6 off), a public IPv6 address is
            // still no address of this node's: its dial fails as unreachable
            if no_inet6 {
                let off = std::net::TcpListener::bind("[::1]:0").expect_err("the filter refuses IPv6 sockets");
                assert_eq!(off.raw_os_error(), Some(libc::EAFNOSUPPORT), "{off}");
                match Policy::PUBLIC.connect("2606:4700::1111", 443).await {
                    Err(DialError::Io(_)) => {}
                    Err(DialError::Refused(refused)) => panic!("refused, not unreachable: {}", refused.0),
                    Ok(_) => panic!("an IPv6 socket under a filter that refuses them"),
                }
            }
            // the node's own address stays shut, however public the rule calls it
            let everything_public = Policy { public: Some(|_| true), allow: None, own: own_address };
            let message = refusal(everything_public.connect(&own.to_string(), port).await);
            assert_eq!(message, format!("egress refused: {own} is this node's own address"));
            // the review's reproduction, where the internet is reachable
            let direct = tokio::time::timeout(std::time::Duration::from_secs(3), tokio::net::TcpStream::connect("1.1.1.1:443")).await;
            if matches!(direct, Ok(Ok(_))) {
                let public = Policy::PUBLIC.connect("1.1.1.1", 443).await;
                assert!(public.is_ok(), "Policy::PUBLIC after a direct connect worked: {:?}", public.err().map(|e| e.to_string()));
            } else {
                eprintln!("1.1.1.1:443 is unreachable here: the review's direct reproduction is skipped");
            }
        });
    }

    /// Installs a seccomp filter on every thread of this process: `bind`
    /// fails with `errno`, and with `no_inet6` an IPv6 socket fails with
    /// EAFNOSUPPORT (IPv6 off); every other call is allowed.
    #[cfg(target_os = "linux")]
    fn deny_bind(errno: u32, no_inet6: bool) {
        let ld = |k| libc::sock_filter { code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, jt: 0, jf: 0, k };
        let jeq = |k, jf| libc::sock_filter { code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16, jt: 0, jf, k };
        let ret = |k| libc::sock_filter { code: (libc::BPF_RET | libc::BPF_K) as u16, jt: 0, jf: 0, k };
        // a family no socket call names stands in when IPv6 stays on
        let family = if no_inet6 { libc::AF_INET6 as u32 } else { u32::MAX };
        let filter = [
            ld(0), // the syscall number, at seccomp_data's offset 0
            jeq(libc::SYS_bind as u32, 1),
            ret(libc::SECCOMP_RET_ERRNO | errno),
            jeq(libc::SYS_socket as u32, 3),
            ld(16), // socket's first argument (its family), low half
            jeq(family, 1),
            ret(libc::SECCOMP_RET_ERRNO | libc::EAFNOSUPPORT as u32),
            ret(libc::SECCOMP_RET_ALLOW),
        ];
        let program = libc::sock_fprog { len: filter.len() as u16, filter: filter.as_ptr().cast_mut() };
        // SAFETY: plain syscalls on a filter that outlives them (the kernel copies it)
        unsafe {
            assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0, "no_new_privs");
            let set = libc::syscall(libc::SYS_seccomp, libc::SECCOMP_SET_MODE_FILTER, libc::SECCOMP_FILTER_FLAG_TSYNC, &program as *const libc::sock_fprog);
            assert_eq!(set, 0, "seccomp: {}", std::io::Error::last_os_error());
        }
    }

    // Review round 2's reproduction: a sandbox that denies explicit `bind`
    // (seccomp EACCES or EPERM, systemd's SocketBindDeny=) still lets TCP
    // out, and public egress must still pass there; the node's own
    // address must still be refused. Run in a child process, so the filter
    // reaches no other test.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_bind_denied_sandbox_keeps_public_egress() {
        if std::env::var("CELLD_TEST_BIND_DENIED").is_ok() {
            return;
        }
        let Some(own) = own_v4() else { return };
        let listener = std::net::TcpListener::bind((own, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        for (errno, no_inet6) in [(libc::EACCES, false), (libc::EPERM, false), (libc::EACCES, true)] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            if no_inet6 {
                child.env("CELLD_TEST_NO_INET6", "1");
            }
            let out = child
                .args(["--exact", "egress::tests::bind_denied_child", "--test-threads=1", "--nocapture"])
                .env("CELLD_TEST_BIND_DENIED", errno.to_string())
                .env("CELLD_TEST_OWN", own.to_string())
                .env("CELLD_TEST_PORT", port.to_string())
                .output()
                .unwrap();
            let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(out.status.success() && said.contains("1 passed"), "errno {errno}, IPv6 off {no_inet6}:\n{said}");
        }
    }

    // The kernel's IPv4 table, as it prints it: the local routes' addresses
    // are this host's (a secondary one too), a network's or a broadcast's
    // are not.
    #[cfg(target_os = "linux")]
    #[test]
    fn local_routes_name_the_hosts_addresses() {
        let trie = "Main:\n  +-- 0.0.0.0/0 3 0 5\n     |-- 0.0.0.0\n        /0 universe UNICAST\n     +-- 192.168.8.0/24 2 0 2\n        |-- 192.168.8.0\n           /24 link UNICAST\n        |-- 192.168.8.197\n           /32 host LOCAL\n        |-- 192.168.8.200\n           /32 host LOCAL\n        |-- 192.168.8.255\n           /32 link BROADCAST\nLocal:\n  +-- 127.0.0.0/8 2 0 2\n     +-- 127.0.0.0/31 1 0 0\n        |-- 127.0.0.0\n           /8 host LOCAL\n        |-- 127.0.0.1\n           /32 host LOCAL\n     |-- 100.101.102.103\n        /32 host LOCAL\n";
        assert_eq!(local_v4(trie), [ip("192.168.8.197"), ip("192.168.8.200"), ip("127.0.0.1"), ip("100.101.102.103")]);
    }
}
