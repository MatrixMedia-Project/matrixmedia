//! The address policy for outbound requests whose target an outsider chooses.
//!
//! One function for every such client -- mm-fleet's provider endpoints and
//! mm-matrix's federation discovery -- so a gap closed for one is closed for
//! both.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

fn v4_is_forbidden(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_private()
        // 169.254/16, which includes the 169.254.169.254 metadata address.
        || ip.is_link_local()
        || ip.is_broadcast()
        // CGNAT 100.64.0.0/10.
        || (o[0] == 100 && (o[1] & 0b1100_0000) == 64)
}

/// The IPv4 address an IPv6 address merely carries, if it is one of the forms that route
/// to it: NAT64 `64:ff9b::/96` (RFC 6052, the last 32 bits) and 6to4 `2002::/16` (RFC 3056,
/// the 32 bits after the prefix). IPv4-mapped and compatible forms are handled by the caller.
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    let pair =
        |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6].iter().all(|&x| x == 0) {
        return Some(pair(s[6], s[7]));
    }
    if s[0] == 0x2002 {
        return Some(pair(s[1], s[2]));
    }
    None
}

fn v6_is_forbidden(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    let s0 = ip.segments()[0];
    // ULA fc00::/7 and link-local fe80::/10.
    if (s0 & 0xfe00) == 0xfc00 || (s0 & 0xffc0) == 0xfe80 {
        return true;
    }
    // `::ffff:a.b.c.d` (mapped) and `::a.b.c.d` (compatible) are the IPv4 address in
    // disguise, as are NAT64 and 6to4 forms: judge them as that address.
    ip.to_ipv4()
        .or_else(|| embedded_v4(ip))
        .is_some_and(v4_is_forbidden)
}

/// True for an address an outbound request must never connect to: loopback,
/// private, link-local (cloud metadata), CGNAT, broadcast, unspecified, IPv6
/// unique-local/link-local, and any IPv6 form that carries one of those IPv4
/// addresses (mapped, compatible, NAT64, 6to4).
pub fn ip_is_forbidden(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_is_forbidden(v4),
        IpAddr::V6(v6) => v6_is_forbidden(v6),
    }
}
