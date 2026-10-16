//! The SSRF guard: decide whether a host may be contacted.
//!
//! Because URLs are model-chosen, this is the security-critical file. It
//! classifies an address as private when it can reach infrastructure — loopback,
//! the 0/8 "this network" block, RFC1918, link-local (including cloud metadata),
//! CGNAT, multicast, unique local, documentation, and every IPv6 form that
//! embeds such an IPv4 (NAT64 well-known, 6to4, IPv4-mapped, and
//! IPv4-compatible). Local-use NAT64 is refused entirely: its translation
//! layout is network-specific and cannot be inferred from the address.
//! Name-based hosts are checked by name, and a custom
//! reqwest resolver re-checks every address at connect time, so a name that
//! resolves inside the network — or a DNS answer that changes between the check
//! and the dial (rebinding) — is refused before a socket is opened.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Why a host was refused.
#[derive(Debug, Clone)]
pub struct GuardError(String);

impl GuardError {
    pub fn message(&self) -> String {
        self.0.clone()
    }
}

impl std::fmt::Display for GuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GuardError {}

/// Hostnames that are local by name and must never be dialed.
const LOCAL_SUFFIXES: &[&str] = &[".localhost", ".local", ".internal", ".home.arpa"];

/// Refuse a host that is local by name or by address. `allow_private_networks` disables
/// the check only for tests and air-gapped mirrors; it is never the default.
///
/// `url::Url::host_str()` returns a bracketed IPv6 literal (`[::1]`), so the
/// brackets are stripped before the address is parsed — otherwise a bracketed
/// loopback or NAT64 literal would fall through the name checks and be dialed.
///
/// This checks the literal the URL names. A hostname is only fully vetted when
/// [`GuardedResolver`] resolves it, which the fetcher installs on its client.
pub fn check_host(host: &str, allow_private_networks: bool) -> Result<(), GuardError> {
    if allow_private_networks {
        return Ok(());
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return Err(GuardError("empty host".into()));
    }
    if host == "localhost" || host == "metadata.google.internal" {
        return Err(GuardError(format!("refusing local host {host:?}")));
    }
    if LOCAL_SUFFIXES.iter().any(|suffix| host.ends_with(suffix)) {
        return Err(GuardError(format!("refusing local host {host:?}")));
    }
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = literal.parse::<IpAddr>() {
        if !is_public_ip(ip) {
            return Err(GuardError(format!("refusing private address {ip}")));
        }
    }
    Ok(())
}

/// The addresses a name may dial: those that are public. Returns an error
/// naming the offending address when any answer is private, so a split answer
/// cannot hide an internal address behind a public one.
pub fn allowed_addresses(
    name: &str,
    addresses: impl IntoIterator<Item = IpAddr>,
    allow_private_networks: bool,
) -> Result<Vec<SocketAddr>, GuardError> {
    let mut allowed = Vec::new();
    let mut any = false;
    for ip in addresses {
        any = true;
        if !allow_private_networks && !is_public_ip(ip) {
            return Err(GuardError(format!(
                "refusing {name}: resolves to private {ip}"
            )));
        }
        allowed.push(SocketAddr::new(ip, 0));
    }
    if !any {
        return Err(GuardError(format!("resolve {name}: no addresses")));
    }
    Ok(allowed)
}

/// A reqwest resolver that enforces the guard at connect time. This is what
/// closes DNS rebinding: the address the socket is opened to is the one that
/// passed [`allowed_addresses`], never a fresh, unchecked resolution.
pub struct GuardedResolver {
    allow_private_networks: bool,
}

impl GuardedResolver {
    pub fn new(allow_private_networks: bool) -> Self {
        GuardedResolver {
            allow_private_networks,
        }
    }
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let allow_private_networks = self.allow_private_networks;
        let host = name.as_str().to_string();
        Box::pin(async move {
            // The system resolver is the trust root; we only filter its answers.
            let resolved = tokio::net::lookup_host((host.as_str(), 0)).await;
            let addresses: Vec<IpAddr> = match resolved {
                Ok(addresses) => addresses.map(|address| address.ip()).collect(),
                Err(error) => {
                    return Err(Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
                }
            };
            match allowed_addresses(&host, addresses, allow_private_networks) {
                Ok(allowed) => {
                    let iter: reqwest::dns::Addrs = Box::new(allowed.into_iter());
                    Ok(iter)
                }
                Err(error) => Err(Box::new(error) as Box<dyn std::error::Error + Send + Sync>),
            }
        })
    }
}

/// Whether an address is routable on the public internet. `false` means the
/// fetcher must refuse it.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

/// IPv4 classification. Standard: loopback, private, link-local, unspecified,
/// multicast; plus the 0/8 this-network block, CGNAT, the TEST-NET blocks,
/// benchmarking, and reserved.
fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    if ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
    {
        return false;
    }
    !matches!(
        (a, b, c),
        // 0/8 "this network": only .0 is the unspecified address, the rest is
        // reserved and must not be dialed (RFC 1122 §3.2.1.3).
        (0, _, _)
        // 100.64/10 carrier-grade NAT
        | (100, 64..=127, _)
        // 192.0.0/24
        | (192, 0, 0)
        // 198.18/15 benchmarking, 198.51.100/24 TEST-NET-2, 203.0.113/24 TEST-NET-3
        | (198, 18..=19, _)
        | (198, 51, 100)
        | (203, 0, 113)
        // 240/4 reserved
        | (240..=255, _, _)
    )
}

/// IPv6 classification, including the transition forms that embed an IPv4.
fn is_public_v6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || is_unique_local(&ip)
        || is_unicast_link_local(&ip)
    {
        return false;
    }
    // IPv4-mapped (::ffff:a.b.c.d) and IPv4-compatible (::a.b.c.d): classify the
    // embedded IPv4 directly. `to_ipv4` handles the mapped form; the compatible
    // form is caught by inspecting the low 32 bits when the high 96 are zero.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let segments = ip.segments();
    if segments[..6].iter().all(|&s| s == 0) {
        let embedded = Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        );
        if embedded != Ipv4Addr::UNSPECIFIED {
            return is_public_v4(embedded);
        }
    }
    // NAT64 well-known prefix 64:ff9b::/96 embeds the IPv4 in the last 32 bits.
    if segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2..6].iter().all(|&s| s == 0) {
        let embedded = Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        );
        return is_public_v4(embedded);
    }
    // RFC 8215 reserves this range for network-specific translation mechanisms,
    // including /64 and /96 prefixes. No fixed offset identifies the destination
    // IPv4, so refuse the whole local-use range rather than guess its layout.
    if segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2] == 0x0001 {
        return false;
    }
    // 6to4 2002::/16 embeds the IPv4 in the next 32 bits.
    if segments[0] == 0x2002 {
        let embedded = Ipv4Addr::new(
            (segments[1] >> 8) as u8,
            segments[1] as u8,
            (segments[2] >> 8) as u8,
            segments[2] as u8,
        );
        return is_public_v4(embedded);
    }
    match segments[0] {
        // 2001::/32 Teredo and 2001:db8::/32 documentation
        0x2001 if segments[1] == 0x0000 || segments[1] == 0x0db8 => false,
        // 2001:2::/48 benchmarking
        0x2001 if segments[1] == 0x0002 => false,
        // 3fff::/20 documentation (RFC 9637)
        0x3fff if segments[1] <= 0x0fff => false,
        _ => true,
    }
}

/// fc00::/7 unique local addresses.
fn is_unique_local(ip: &Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

/// fe80::/10 link-local unicast addresses.
fn is_unicast_link_local(ip: &Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test address")
    }

    /// Every way an address can reach infrastructure must be classified private.
    #[test]
    fn private_addresses_are_refused() {
        let private = [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.5.4",
            "192.168.1.1",
            "169.254.169.254", // cloud metadata
            "100.64.0.1",      // CGNAT
            "0.0.0.0",
            "0.0.0.1", // 0/8 "this network"
            "0.1.2.3",
            "0.255.255.255",
            "::1",
            "fc00::1",
            "fe80::1",
            // IPv6 forms that embed a blocked IPv4.
            "64:ff9b::7f00:1",          // NAT64 -> 127.0.0.1
            "64:ff9b::a9fe:a9fe",       // NAT64 -> 169.254.169.254
            "64:ff9b::6440:1",          // NAT64 -> 100.64.0.1 (CGNAT)
            "64:ff9b::cb00:7105",       // NAT64 -> 203.0.113.5 (TEST-NET-3)
            "64:ff9b:1:7f00:0:100::",   // local-use /48 NAT64 -> 127.0.0.1
            "64:ff9b:1:a9fe:a9:fe00::", // local-use /48 NAT64 -> 169.254.169.254
            "64:ff9b:1:6440:0:100::",   // local-use /48 NAT64 -> 100.64.0.1 (CGNAT)
            "64:ff9b:1:cb00:71:500::",  // local-use /48 NAT64 -> 203.0.113.5 (TEST-NET-3)
            "2002:7f00:1::",            // 6to4 -> 127.0.0.1
            "2002:a9fe:a9fe::",         // 6to4 -> 169.254.169.254
            "2002:6440:1::",            // 6to4 -> 100.64.0.1 (CGNAT)
            "2002:cb00:7105::",         // 6to4 -> 203.0.113.5 (TEST-NET-3)
            "::ffff:127.0.0.1",         // IPv4-mapped loopback
            "::127.0.0.1",              // IPv4-compatible
            "2001::1",                  // Teredo
            "2001:db8::1",              // documentation
            "3fff::1",                  // documentation (RFC 9637), bottom of /20
            "3fff:0fff:ffff::1",        // top of the 3fff::/20 documentation range
        ];
        for address in private {
            assert!(!is_public_ip(ip(address)), "{address} should be private");
        }
    }

    #[test]
    fn public_addresses_are_allowed() {
        let public = [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "142.250.72.14",
            "2606:4700:4700::1111",
            "2606:4700::6810:85e5",
            "::ffff:8.8.8.8",
            // The well-known translation layout can safely classify its IPv4.
            "64:ff9b::101:101", // well-known NAT64 -> 1.1.1.1
            // Only 3fff::/20 is documentation; neighbours stay publicly routable.
            "3ffe::1",      // outside the 3fff::/20 range
            "3fff:1000::1", // first address past the 3fff::/20 range
        ];
        for address in public {
            assert!(is_public_ip(ip(address)), "{address} should be public");
        }
    }

    /// Local-use layouts cannot be guessed, even when prefix bits resemble public IPv4.
    #[test]
    fn local_use_nat64_is_refused_for_every_translation_layout() {
        for address in [
            "64:ff9b:1:cb00:71:500::", // /48 -> documentation IPv4, with the reserved u octet
            "64:ff9b:1:8a9:fe:a9fe::", // /56 -> metadata
            "64:ff9b:1:808:a9:fea9:fe00:0", // /64 -> metadata
            "64:ff9b:1:808:8:0:a9fe:a9fe", // /96 -> metadata
            "64:ff9b:1:808:8:0:7f00:1", // /96 -> loopback
            "64:ff9b:1:808:8:0:101:101", // also refuse an apparently public destination
        ] {
            assert!(!is_public_ip(ip(address)), "{address} should be refused");
            assert!(check_host(&format!("[{address}]"), false).is_err());
            assert!(allowed_addresses("translation.example", [ip(address)], false).is_err());
        }
    }

    #[test]
    fn local_names_are_refused() {
        for host in [
            "localhost",
            "foo.local",
            "x.internal",
            "metadata.google.internal",
            "127.0.0.1",
            "10.0.0.1",
            "localhost.",
            // Bracketed literals as `url::Url::host_str` returns them.
            "[::1]",
            "[::ffff:127.0.0.1]",
            "[64:ff9b::a9fe:a9fe]",
            "[64:ff9b:1:7f00:0:100::]",
            "[2002:7f00:1::]",
            "[3fff::1]",
        ] {
            assert!(check_host(host, false).is_err(), "{host} should be refused");
        }
        assert!(check_host("example.com", false).is_ok());
        assert!(check_host("[2606:4700:4700::1111]", false).is_ok());
    }

    #[test]
    fn allow_private_networks_disables_the_guard_only_when_asked() {
        assert!(check_host("127.0.0.1", true).is_ok());
        assert!(check_host("localhost", true).is_ok());
        assert!(check_host("[64:ff9b:1:808:8:0:a9fe:a9fe]", true).is_ok());
    }

    /// The resolver-level check is what closes rebinding: a name that resolves
    /// to a private address must be refused even though its literal is public.
    #[test]
    fn a_name_resolving_private_is_refused() {
        use std::net::IpAddr;
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        let public: IpAddr = "93.184.216.34".parse().unwrap();
        // A split answer is refused, not partially allowed.
        assert!(allowed_addresses("evil.example", [public, loopback], false).is_err());
        assert_eq!(
            allowed_addresses("ok.example", [public], false)
                .unwrap()
                .len(),
            1
        );
        assert!(allowed_addresses("anything", [], false).is_err());
        // The escape hatch used only by tests and mirrors.
        assert!(allowed_addresses("evil.example", [loopback], true).is_ok());
    }
}
