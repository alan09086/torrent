//! BEP 40: Canonical Peer Priority.
//!
//! Computes a deterministic priority value for a peer connection so that both
//! sides agree on which connections to prefer.  Uses CRC32C (Castagnoli) over
//! the sorted, masked IP pair.
//!
//! Reference: <https://www.bittorrent.org/beps/bep_0040.html>

use std::net::IpAddr;

// ── CRC32C (Castagnoli polynomial 0x1EDC6F41) ──────────────────────────

/// Precomputed CRC32C lookup table (Castagnoli polynomial, bit-reflected).
const CRC32C_TABLE: [u32; 256] = {
    const POLY: u32 = 0x82F6_3B78; // bit-reflected 0x1EDC6F41
    let mut table = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut crc = i;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ POLY;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i as usize] = crc;
        i += 1;
    }
    table
};

/// Compute CRC32C checksum of `data`.
fn crc32c(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        let idx = ((crc ^ byte as u32) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32C_TABLE[idx];
    }
    crc ^ 0xFFFF_FFFF
}

// ── IPv4 masking (BEP 40 §4) ───────────────────────────────────────────

/// BEP 40 IPv4 masks based on network proximity.
const IPV4_MASK_DEFAULT: [u8; 4] = [0xFF, 0xFF, 0x55, 0x55];
const IPV4_MASK_SAME_16: [u8; 4] = [0xFF, 0xFF, 0xFF, 0x55];
const IPV4_MASK_SAME_24: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];

fn ipv4_mask(a: &[u8; 4], b: &[u8; 4]) -> [u8; 4] {
    if a[0] == b[0] && a[1] == b[1] {
        if a[2] == b[2] {
            IPV4_MASK_SAME_24
        } else {
            IPV4_MASK_SAME_16
        }
    } else {
        IPV4_MASK_DEFAULT
    }
}

fn apply_mask_v4(ip: &[u8; 4], mask: &[u8; 4]) -> [u8; 4] {
    [
        ip[0] & mask[0],
        ip[1] & mask[1],
        ip[2] & mask[2],
        ip[3] & mask[3],
    ]
}

// ── IPv6 masking (BEP 40 §4) ───────────────────────────────────────────

/// BEP 40 IPv6 masks (16 bytes) based on network proximity.
const IPV6_MASK_DEFAULT: [u8; 16] = [
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55,
];
const IPV6_MASK_SAME_48: [u8; 16] = [
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55,
];
const IPV6_MASK_SAME_56: [u8; 16] = [
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55,
];

fn ipv6_mask(a: &[u8; 16], b: &[u8; 16]) -> [u8; 16] {
    // Check if same /48 (first 6 bytes)
    if a[..6] == b[..6] {
        // Check if same /56 (first 7 bytes)
        if a[6] == b[6] {
            IPV6_MASK_SAME_56
        } else {
            IPV6_MASK_SAME_48
        }
    } else {
        IPV6_MASK_DEFAULT
    }
}

fn apply_mask_v6(ip: &[u8; 16], mask: &[u8; 16]) -> [u8; 16] {
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = ip[i] & mask[i];
    }
    out
}

// ── Public API ──────────────────────────────────────────────────────────

/// Compute BEP 40 canonical peer priority for a connection.
///
/// Both peers compute the same value (assuming `my_ip` is what the peer sees
/// us as — typically from the extension handshake `yourip` field or NAT
/// discovery).
///
/// Lower priority = more preferred connection.
#[allow(dead_code)]
pub(crate) fn canonical_peer_priority(my_ip: IpAddr, peer_ip: IpAddr) -> u32 {
    match (my_ip, peer_ip) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let a_bytes = a.octets();
            let b_bytes = b.octets();
            let mask = ipv4_mask(&a_bytes, &b_bytes);
            let ma = apply_mask_v4(&a_bytes, &mask);
            let mb = apply_mask_v4(&b_bytes, &mask);
            // Sort: lower masked IP first
            let (first, second) = if ma <= mb { (ma, mb) } else { (mb, ma) };
            let mut buf = [0u8; 8];
            buf[..4].copy_from_slice(&first);
            buf[4..].copy_from_slice(&second);
            crc32c(&buf)
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let a_bytes = a.octets();
            let b_bytes = b.octets();
            let mask = ipv6_mask(&a_bytes, &b_bytes);
            let ma = apply_mask_v6(&a_bytes, &mask);
            let mb = apply_mask_v6(&b_bytes, &mask);
            let (first, second) = if ma <= mb { (ma, mb) } else { (mb, ma) };
            let mut buf = [0u8; 32];
            buf[..16].copy_from_slice(&first);
            buf[16..].copy_from_slice(&second);
            crc32c(&buf)
        }
        // Mixed v4/v6: map v4 to v4-mapped-v6 and use v6 path
        (IpAddr::V4(a), IpAddr::V6(b)) => {
            canonical_peer_priority(IpAddr::V6(a.to_ipv6_mapped()), IpAddr::V6(b))
        }
        (IpAddr::V6(a), IpAddr::V4(b)) => {
            canonical_peer_priority(IpAddr::V6(a), IpAddr::V6(b.to_ipv6_mapped()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn crc32c_empty() {
        assert_eq!(crc32c(&[]), 0x0000_0000);
    }

    #[test]
    fn crc32c_known_value() {
        // CRC32C of "123456789" = 0xE3069283
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn bep40_reference_vector_different_networks() {
        // BEP 40 test vector 1: 123.213.32.10 vs 98.76.54.32 → 0xec2d7224
        let a = IpAddr::V4(Ipv4Addr::new(123, 213, 32, 10));
        let b = IpAddr::V4(Ipv4Addr::new(98, 76, 54, 32));
        assert_eq!(canonical_peer_priority(a, b), 0xec2d7224);
    }

    #[test]
    fn bep40_reference_vector_same_slash16() {
        // BEP 40 test vector 2: 123.213.32.10 vs 123.213.32.234 → 0x99568189
        let a = IpAddr::V4(Ipv4Addr::new(123, 213, 32, 10));
        let b = IpAddr::V4(Ipv4Addr::new(123, 213, 32, 234));
        assert_eq!(canonical_peer_priority(a, b), 0x99568189);
    }

    #[test]
    fn priority_is_symmetric() {
        let a = IpAddr::V4(Ipv4Addr::new(10, 0, 1, 5));
        let b = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100));
        assert_eq!(canonical_peer_priority(a, b), canonical_peer_priority(b, a));
    }

    #[test]
    fn priority_symmetric_ipv6() {
        let a = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 1));
        let b = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 2, 0, 0, 0, 0, 2));
        assert_eq!(canonical_peer_priority(a, b), canonical_peer_priority(b, a));
    }

    #[test]
    fn same_slash24_uses_full_mask() {
        let a = IpAddr::V4(Ipv4Addr::new(10, 20, 30, 1));
        let b = IpAddr::V4(Ipv4Addr::new(10, 20, 30, 2));
        let p1 = canonical_peer_priority(a, b);
        // Changing last octet should change priority (full mask keeps it)
        let c = IpAddr::V4(Ipv4Addr::new(10, 20, 30, 3));
        let p2 = canonical_peer_priority(a, c);
        assert_ne!(p1, p2);
    }

    #[test]
    fn different_networks_masked_to_slash16() {
        // Two IPs in different /16s: last two octets get masked with 0x55
        let a = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let b = IpAddr::V4(Ipv4Addr::new(20, 0, 0, 1));
        let p1 = canonical_peer_priority(a, b);
        // Changing only the masked bits should NOT change priority
        let c = IpAddr::V4(Ipv4Addr::new(10, 0, 0xAA, 1));
        let p2 = canonical_peer_priority(c, b);
        // 0 & 0x55 == 0, 0xAA & 0x55 == 0 — same masked value, same priority
        assert_eq!(p1, p2);
    }

    #[test]
    fn ipv4_mask_selection() {
        // Different /16
        let a = [10, 0, 1, 1];
        let b = [20, 0, 1, 1];
        assert_eq!(ipv4_mask(&a, &b), IPV4_MASK_DEFAULT);

        // Same /16, different /24
        let a = [10, 0, 1, 1];
        let b = [10, 0, 2, 1];
        assert_eq!(ipv4_mask(&a, &b), IPV4_MASK_SAME_16);

        // Same /24
        let a = [10, 0, 1, 1];
        let b = [10, 0, 1, 2];
        assert_eq!(ipv4_mask(&a, &b), IPV4_MASK_SAME_24);
    }

    #[test]
    fn ipv6_mask_selection() {
        // Different /48
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        a[0] = 0x20;
        a[1] = 0x01;
        b[0] = 0x20;
        b[1] = 0x02;
        assert_eq!(ipv6_mask(&a, &b), IPV6_MASK_DEFAULT);

        // Same /48, different /56 (first 6 bytes match, 7th differs)
        let mut d = a;
        d[6] = 1;
        assert_eq!(ipv6_mask(&a, &d), IPV6_MASK_SAME_48);

        // Same /56
        let mut e = a;
        e[7] = 1;
        assert_eq!(ipv6_mask(&a, &e), IPV6_MASK_SAME_56);
    }

    #[test]
    fn mixed_v4_v6_uses_mapped() {
        let v4 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let v6 = IpAddr::V6(Ipv4Addr::new(10, 0, 0, 1).to_ipv6_mapped());
        // v4 vs v6-mapped-v4 should produce same as v6-mapped vs v6-mapped
        assert_eq!(
            canonical_peer_priority(v4, v6),
            canonical_peer_priority(v6, v6),
        );
    }

    #[test]
    fn sort_peers_by_priority_descending() {
        // Simulate the sort_available_peers logic: sort descending so pop() gives lowest
        use std::net::SocketAddr;
        let my_ip = IpAddr::V4(Ipv4Addr::new(123, 213, 32, 10));
        let peers = vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(98, 76, 54, 32)), 6881),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(123, 213, 32, 234)), 6881),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 6881),
        ];
        let mut scored: Vec<(SocketAddr, u32)> = peers
            .iter()
            .map(|a| (*a, canonical_peer_priority(my_ip, a.ip())))
            .collect();
        // Sort descending by priority
        scored.sort_by(|a, b| b.1.cmp(&a.1));
        // Last element (pop target) should have lowest priority = most preferred
        let last = scored.last().unwrap();
        let min_priority = scored.iter().map(|s| s.1).min().unwrap();
        assert_eq!(last.1, min_priority);
    }
}
