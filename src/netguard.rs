//! Blocks requests to loopback, private and other non-public addresses so the
//! server can't be used to reach its own host or LAN (SSRF). DNS results are
//! filtered inside the HTTP client's resolver, so the address that was checked
//! is the address that gets connected to (no DNS rebinding window).

use std::error::Error;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const fn v4(a: u8, b: u8, c: u8, d: u8) -> u32 {
    u32::from_be_bytes([a, b, c, d])
}

/// IANA special-purpose IPv4 ranges that are not globally reachable.
const BLOCKED_V4: [(u32, u32); 15] = [
    (v4(0, 0, 0, 0), 8),       // "this network"
    (v4(10, 0, 0, 0), 8),      // private
    (v4(100, 64, 0, 0), 10),   // carrier-grade NAT (also used by Tailscale)
    (v4(127, 0, 0, 0), 8),     // loopback
    (v4(169, 254, 0, 0), 16),  // link-local, including cloud metadata endpoints
    (v4(172, 16, 0, 0), 12),   // private
    (v4(192, 0, 0, 0), 24),    // IETF protocol assignments
    (v4(192, 0, 2, 0), 24),    // documentation
    (v4(192, 88, 99, 0), 24),  // 6to4 relay anycast
    (v4(192, 168, 0, 0), 16),  // private
    (v4(198, 18, 0, 0), 15),   // benchmarking
    (v4(198, 51, 100, 0), 24), // documentation
    (v4(203, 0, 113, 0), 24),  // documentation
    (v4(224, 0, 0, 0), 4),     // multicast
    (v4(240, 0, 0, 0), 4),     // reserved and broadcast
];

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let n = u32::from(ip);
    !BLOCKED_V4.iter().any(|&(base, bits)| n >> (32 - bits) == base >> (32 - bits))
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let g = ip.segments();
    let embedded = |hi: usize, lo: usize| Ipv4Addr::from((u32::from(g[hi]) << 16) | u32::from(g[lo]));

    if g[..5] == [0; 5] && g[5] == 0xffff {
        return is_public_v4(embedded(6, 7)); // ::ffff:0:0/96 IPv4-mapped
    }
    if g[0] == 0x64 && g[1] == 0xff9b && g[2..6] == [0; 4] {
        return is_public_v4(embedded(6, 7)); // 64:ff9b::/96 NAT64
    }
    if g[0] == 0x2002 {
        return is_public_v4(embedded(1, 2)); // 2002::/16 6to4
    }
    // Only 2000::/3 is global unicast. This excludes ::, ::1, IPv4-compatible,
    // ULA (fc00::/7), link-local (fe80::/10), multicast (ff00::/8), discard, etc.
    if g[0] & 0xe000 != 0x2000 {
        return false;
    }
    let blocked = (g[0] == 0x2001 && g[1] == 0) // Teredo
        || (g[0] == 0x2001 && g[1] == 0x0db8) // documentation
        || (g[0] == 0x2001 && g[1] == 0x0002 && g[2] == 0) // benchmarking
        || (g[0] == 0x2001 && matches!(g[1] & 0xfff0, 0x0010 | 0x0020)) // ORCHID
        || (g[0] == 0x3fff && g[1] < 0x1000); // documentation 3fff::/20
    !blocked
}

pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => is_public_v6(ip),
    }
}

/// Error returned by the resolver when a host has no public address.
#[derive(Debug)]
pub struct BlockedAddress {
    pub host: String,
    pub addresses: Vec<IpAddr>,
}

impl fmt::Display for BlockedAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let list: Vec<String> = self.addresses.iter().map(IpAddr::to_string).collect();
        write!(
            f,
            "{} resolves only to non-public addresses ({}); requests to private networks are disabled on this server",
            self.host,
            list.join(", ")
        )
    }
}

impl Error for BlockedAddress {}

/// DNS resolver for reqwest that only returns public addresses.
pub struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let allowed: Vec<SocketAddr> = resolved.iter().copied().filter(|a| is_public(a.ip())).collect();
            if allowed.is_empty() {
                let addresses = resolved.iter().map(SocketAddr::ip).collect();
                return Err(Box::new(BlockedAddress { host, addresses }) as Box<dyn Error + Send + Sync>);
            }
            Ok(Box::new(allowed.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::dns::Resolve;
    use std::str::FromStr;

    #[test]
    fn non_public_addresses_are_blocked() {
        for ip in [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "127.255.255.254",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.2.1",
            "192.168.1.1",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:7f00:1",
            "::ffff:10.0.0.1",
            "::127.0.0.1",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b:1::1",
            "2002:7f00:1::1",
            "2001::1",
            "2001:db8::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "ff02::1",
            "100::1",
            "3fff::1",
        ] {
            assert!(!is_public(IpAddr::from_str(ip).unwrap()), "{ip}");
        }
    }

    #[test]
    fn public_addresses_are_allowed() {
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.215.14",
            "172.32.0.1",
            "100.128.0.1",
            "192.169.0.1",
            "2606:4700:4700::1111",
            "2a00:1450:4001::200e",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::1",
        ] {
            assert!(is_public(IpAddr::from_str(ip).unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn resolver_refuses_localhost() {
        let name = reqwest::dns::Name::from_str("localhost").unwrap();
        let err = PublicOnlyResolver.resolve(name).await.err().expect("localhost must be refused");
        assert!(err.downcast_ref::<BlockedAddress>().is_some(), "{err}");
    }
}
