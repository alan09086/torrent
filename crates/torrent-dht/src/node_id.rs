//! BEP 42: DHT Security Extension — Node ID generation and verification.
//!
//! Ties DHT node IDs to IP addresses using CRC32C, making Sybil attacks
//! computationally expensive. The first 21 bits of a valid node ID must
//! match `CRC32C(masked_ip | (r << 29))`, and the last byte must equal `r`.
//!
//! Reference: <https://www.bittorrent.org/beps/bep_0042.html>

use std::net::IpAddr;

use torrent_core::Id20;

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

// ── BEP 42 masks ────────────────────────────────────────────────────────

/// IPv4 mask: only certain bits of the IP affect the node ID.
const IPV4_MASK: [u8; 4] = [0x03, 0x0f, 0x3f, 0xff];

/// IPv6 mask: applied to the high 64 bits of the address.
const IPV6_MASK: [u8; 8] = [0x01, 0x03, 0x07, 0x0f, 0x1f, 0x3f, 0x7f, 0xff];

// ── Node ID generation ──────────────────────────────────────────────────

/// Generate a BEP 42-compliant node ID for the given IP address.
///
/// The `r` value (0..8) controls which of the 8 valid prefixes is used.
/// The remaining bytes (3..19) are filled randomly.
pub fn generate_node_id(ip: IpAddr, r: u8) -> Id20 {
    debug_assert!(r < 8, "r must be in range [0, 8)");
    let r = r & 0x07; // clamp to 3 bits

    let crc = compute_ip_crc(ip, r);

    // Build the node ID:
    // - Bytes 0..3: first 21 bits from CRC, rest random
    // - Bytes 3..19: random
    // - Byte 19: r
    let mut id = [0u8; 20];

    // Fill with random bytes first
    fill_random(&mut id);

    // Set first 21 bits from CRC32C result (big-endian)
    // Bits 0..7 of CRC → id[0]
    // Bits 8..15 of CRC → id[1]
    // Bits 16..20 of CRC → top 5 bits of id[2]
    id[0] = (crc >> 24) as u8;
    id[1] = (crc >> 16) as u8;
    // Top 5 bits from CRC, bottom 3 bits random (preserve existing random)
    id[2] = ((crc >> 8) as u8 & 0xF8) | (id[2] & 0x07);

    // Last byte must be r
    id[19] = r;

    Id20(id)
}

/// Compute the CRC32C hash for BEP 42 node ID derivation.
///
/// For IPv4: `CRC32C((ip & 0x030f3fff) | (r << 29))`
/// For IPv6: `CRC32C((ip[0..8] & 0x0103070f1f3f7fff) | (r << 61))`
fn compute_ip_crc(ip: IpAddr, r: u8) -> u32 {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            // Apply mask and pack into big-endian u32
            let masked: u32 = ((octets[0] & IPV4_MASK[0]) as u32) << 24
                | ((octets[1] & IPV4_MASK[1]) as u32) << 16
                | ((octets[2] & IPV4_MASK[2]) as u32) << 8
                | (octets[3] & IPV4_MASK[3]) as u32;
            // OR in r at bits 29-31
            let value = masked | ((r as u32) << 29);
            crc32c(&value.to_be_bytes())
        }
        IpAddr::V6(v6) => {
            let octets = v6.octets();
            // Apply mask to first 8 bytes, pack into big-endian u64
            let mut masked: u64 = 0;
            for i in 0..8 {
                masked |= ((octets[i] & IPV6_MASK[i]) as u64) << (56 - i * 8);
            }
            // OR in r at bits 61-63
            let value = masked | ((r as u64) << 61);
            crc32c(&value.to_be_bytes())
        }
    }
}

// ── Node ID verification ────────────────────────────────────────────────

/// Check whether a node ID is valid for the given IP address (BEP 42).
///
/// Returns `true` if the first 21 bits of the node ID match the CRC32C
/// prefix for any `r` value in `[0, 8)`, and the last byte equals that `r`.
///
/// Local/private IPs are always considered valid (exempt from enforcement).
pub fn is_valid_node_id(id: &Id20, ip: IpAddr) -> bool {
    if is_bep42_exempt(ip) {
        return true;
    }

    let r = id.0[19] & 0x07;
    let crc = compute_ip_crc(ip, r);

    // Compare first 21 bits
    let id_prefix = ((id.0[0] as u32) << 24) | ((id.0[1] as u32) << 16) | ((id.0[2] as u32) << 8);
    let crc_prefix = crc & 0xFFFF_F800; // top 21 bits of CRC (in top 21 bits of u32)

    (id_prefix & 0xFFFF_F800) == crc_prefix
}

/// Returns `true` if the IP is exempt from BEP 42 enforcement.
///
/// Exempt ranges (BEP 42 section on "local networks"):
/// - 10.0.0.0/8
/// - 172.16.0.0/12
/// - 192.168.0.0/16
/// - 169.254.0.0/16
/// - 127.0.0.0/8
/// - IPv6 loopback (::1), link-local (fe80::/10), unique local (fc00::/7)
pub fn is_bep42_exempt(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 10                                          // 10.0.0.0/8
                || (o[0] == 172 && (o[1] & 0xF0) == 16)        // 172.16.0.0/12
                || (o[0] == 192 && o[1] == 168)                 // 192.168.0.0/16
                || (o[0] == 169 && o[1] == 254)                 // 169.254.0.0/16
                || o[0] == 127                                   // 127.0.0.0/8
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            v6.is_loopback()                                    // ::1
                || (seg[0] & 0xFFC0) == 0xFE80                 // fe80::/10
                || (seg[0] & 0xFE00) == 0xFC00                 // fc00::/7
        }
    }
}

// ── Random fill helper ──────────────────────────────────────────────────

/// Fill a byte slice with pseudo-random bytes (xorshift64, no external dep).
fn fill_random(buf: &mut [u8]) {
    use std::cell::Cell;
    use std::time::SystemTime;

    thread_local! {
        static STATE: Cell<u64> = Cell::new(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
        );
    }

    for byte in buf.iter_mut() {
        STATE.with(|s| {
            let mut x = s.get();
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            s.set(x);
            *byte = x as u8;
        });
    }
}

// ── External IP Voter ───────────────────────────────────────────────────

/// Source of an external IP observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IpVoteSource {
    /// KRPC response `ip` field from a DHT node.
    Dht(u64),
    /// NAT traversal (UPnP, NAT-PMP, PCP).
    Nat,
    /// Tracker announce response.
    Tracker,
}

impl IpVoteSource {
    /// Derive a unique source_id for deduplication in the voter.
    ///
    /// DHT sources use the hash of the remote node's address.
    /// NAT and tracker sources use distinct well-known IDs so they
    /// don't overwrite each other's votes.
    pub fn source_id(&self) -> u64 {
        match self {
            IpVoteSource::Dht(addr_hash) => *addr_hash,
            IpVoteSource::Nat => 0xFFFF_0001,
            IpVoteSource::Tracker => 0xFFFF_0002,
        }
    }
}

/// Consensus-based external IP detection.
///
/// Aggregates IP observations from multiple independent sources and
/// determines our external IP when a majority agrees.
#[derive(Debug, Clone)]
pub struct ExternalIpVoter {
    /// (source_identifier, voted_ip) — source_identifier is an opaque hash
    /// to deduplicate votes from the same source (e.g., same DHT node).
    votes: Vec<(u64, IpAddr)>,
    /// Current consensus IP, if any.
    consensus: Option<IpAddr>,
    /// Minimum number of votes required before establishing consensus.
    min_votes: usize,
}

impl ExternalIpVoter {
    /// Create a new voter. `min_votes` is the minimum number of agreeing
    /// observations before consensus is established (default recommendation: 10).
    pub fn new(min_votes: usize) -> Self {
        ExternalIpVoter {
            votes: Vec::new(),
            consensus: None,
            min_votes: min_votes.max(1),
        }
    }

    /// Record a vote for an external IP address.
    ///
    /// `source_id` should be unique per source (e.g., hash of the DHT node's
    /// socket address) to prevent a single source from stuffing votes.
    ///
    /// Returns `Some(ip)` if consensus changed (new IP or first consensus).
    pub fn add_vote(&mut self, source_id: u64, ip: IpAddr) -> Option<IpAddr> {
        // Ignore local/private IPs — they're never our real external address
        if is_bep42_exempt(ip) {
            return None;
        }

        // Deduplicate: replace existing vote from this source
        if let Some(existing) = self.votes.iter_mut().find(|(id, _)| *id == source_id) {
            existing.1 = ip;
        } else {
            self.votes.push((source_id, ip));
        }

        // Evict oldest votes if the list grows too large (cap at 100)
        if self.votes.len() > 100 {
            self.votes.drain(0..self.votes.len() - 100);
        }

        self.evaluate_consensus()
    }

    /// Current consensus IP, if established.
    pub fn consensus(&self) -> Option<IpAddr> {
        self.consensus
    }

    /// Number of recorded votes.
    pub fn vote_count(&self) -> usize {
        self.votes.len()
    }

    /// Evaluate votes and return `Some(ip)` if consensus changed.
    fn evaluate_consensus(&mut self) -> Option<IpAddr> {
        if self.votes.len() < self.min_votes {
            return None;
        }

        // Count votes per IP
        let mut counts: Vec<(IpAddr, usize)> = Vec::new();
        for (_, ip) in &self.votes {
            if let Some(entry) = counts.iter_mut().find(|(addr, _)| addr == ip) {
                entry.1 += 1;
            } else {
                counts.push((*ip, 1));
            }
        }

        // Find the IP with the most votes
        let (best_ip, best_count) = counts.iter().max_by_key(|(_, c)| *c)?;

        // Require strict majority (>50% of all votes)
        if *best_count * 2 > self.votes.len() {
            let new_consensus = *best_ip;
            if self.consensus != Some(new_consensus) {
                self.consensus = Some(new_consensus);
                return Some(new_consensus);
            }
        }

        None
    }
}

impl Default for ExternalIpVoter {
    fn default() -> Self {
        Self::new(10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    // ── CRC32C baseline ─────────────────────────────────────────────

    #[test]
    fn crc32c_empty() {
        assert_eq!(crc32c(&[]), 0x0000_0000);
    }

    #[test]
    fn crc32c_known_value() {
        // CRC32C of "123456789" = 0xE3069283 (RFC 3720)
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    // ── BEP 42 test vectors ─────────────────────────────────────────
    //
    // From BEP 42 specification:
    // | IP             | rand | Node ID prefix (first 3 bytes)  | last byte |
    // |----------------|------|---------------------------------|-----------|
    // | 124.31.75.21   |    1 | 5fbfbf                          | 01        |
    // | 21.75.31.124   |   86 | 5a3ce9                          | 56        |
    // | 65.23.51.170   |   22 | a5d432                          | 16        |
    // | 84.124.73.14   |   65 | 1b0321                          | 41        |
    // | 43.213.53.83   |   90 | e56f6c                          | 5a        |

    #[test]
    fn bep42_test_vector_1() {
        let ip = IpAddr::V4(Ipv4Addr::new(124, 31, 75, 21));
        let r = 1u8;
        let crc = compute_ip_crc(ip, r & 0x07);
        // Expected first 3 bytes of node ID: 0x5f, 0xbf, 0xbf
        assert_eq!((crc >> 24) as u8, 0x5f);
        assert_eq!((crc >> 16) as u8, 0xbf);
        assert_eq!((crc >> 8) as u8 & 0xF8, 0xbf & 0xF8);
    }

    #[test]
    fn bep42_test_vector_2() {
        let ip = IpAddr::V4(Ipv4Addr::new(21, 75, 31, 124));
        let r = 86u8;
        let crc = compute_ip_crc(ip, r & 0x07);
        assert_eq!((crc >> 24) as u8, 0x5a);
        assert_eq!((crc >> 16) as u8, 0x3c);
        assert_eq!((crc >> 8) as u8 & 0xF8, 0xe9 & 0xF8);
    }

    #[test]
    fn bep42_test_vector_3() {
        let ip = IpAddr::V4(Ipv4Addr::new(65, 23, 51, 170));
        let r = 22u8;
        let crc = compute_ip_crc(ip, r & 0x07);
        assert_eq!((crc >> 24) as u8, 0xa5);
        assert_eq!((crc >> 16) as u8, 0xd4);
        assert_eq!((crc >> 8) as u8 & 0xF8, 0x32 & 0xF8);
    }

    #[test]
    fn bep42_test_vector_4() {
        let ip = IpAddr::V4(Ipv4Addr::new(84, 124, 73, 14));
        let r = 65u8;
        let crc = compute_ip_crc(ip, r & 0x07);
        assert_eq!((crc >> 24) as u8, 0x1b);
        assert_eq!((crc >> 16) as u8, 0x03);
        assert_eq!((crc >> 8) as u8 & 0xF8, 0x21 & 0xF8);
    }

    #[test]
    fn bep42_test_vector_5() {
        let ip = IpAddr::V4(Ipv4Addr::new(43, 213, 53, 83));
        let r = 90u8;
        let crc = compute_ip_crc(ip, r & 0x07);
        assert_eq!((crc >> 24) as u8, 0xe5);
        assert_eq!((crc >> 16) as u8, 0x6f);
        assert_eq!((crc >> 8) as u8 & 0xF8, 0x6c & 0xF8);
    }

    // ── generate + verify round-trip ────────────────────────────────

    #[test]
    fn generated_id_verifies() {
        let ip = IpAddr::V4(Ipv4Addr::new(124, 31, 75, 21));
        for r in 0..8u8 {
            let id = generate_node_id(ip, r);
            assert!(
                is_valid_node_id(&id, ip),
                "generated ID should verify for r={r}"
            );
        }
    }

    #[test]
    fn generated_id_fails_for_wrong_ip() {
        let ip = IpAddr::V4(Ipv4Addr::new(124, 31, 75, 21));
        let wrong_ip = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));
        let id = generate_node_id(ip, 3);
        assert!(
            !is_valid_node_id(&id, wrong_ip),
            "ID generated for one IP should not verify for a different IP"
        );
    }

    #[test]
    fn random_id_almost_certainly_fails_verification() {
        let ip = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
        // A truly random 20-byte ID has a 1/2^21 chance of passing.
        // Generate several and expect all to fail.
        let mut all_fail = true;
        for _ in 0..100 {
            let mut buf = [0u8; 20];
            fill_random(&mut buf);
            let id = Id20(buf);
            if is_valid_node_id(&id, ip) {
                all_fail = false;
            }
        }
        // With 100 trials and 1/2M chance each, probability of any passing is ~0.005%
        assert!(all_fail, "random IDs should almost never pass BEP 42 verification");
    }

    // ── Exempt IPs ──────────────────────────────────────────────────

    #[test]
    fn local_ips_always_valid() {
        let random_id = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        assert!(is_valid_node_id(&random_id, "127.0.0.1".parse().unwrap()));
        assert!(is_valid_node_id(&random_id, "10.0.0.1".parse().unwrap()));
        assert!(is_valid_node_id(&random_id, "192.168.1.1".parse().unwrap()));
        assert!(is_valid_node_id(&random_id, "172.16.5.1".parse().unwrap()));
        assert!(is_valid_node_id(&random_id, "169.254.1.1".parse().unwrap()));
        assert!(is_valid_node_id(&random_id, "::1".parse().unwrap()));
        assert!(is_valid_node_id(&random_id, "fe80::1".parse().unwrap()));
        assert!(is_valid_node_id(&random_id, "fc00::1".parse().unwrap()));
    }

    #[test]
    fn public_ips_not_exempt() {
        assert!(!is_bep42_exempt("8.8.8.8".parse().unwrap()));
        assert!(!is_bep42_exempt("1.2.3.4".parse().unwrap()));
        assert!(!is_bep42_exempt("2001:db8::1".parse().unwrap()));
    }

    // ── IPv6 generation ─────────────────────────────────────────────

    #[test]
    fn ipv6_generated_id_verifies() {
        let ip = IpAddr::V6(Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1));
        for r in 0..8u8 {
            let id = generate_node_id(ip, r);
            assert!(
                is_valid_node_id(&id, ip),
                "IPv6 generated ID should verify for r={r}"
            );
        }
    }

    // ── Last byte = r ───────────────────────────────────────────────

    #[test]
    fn last_byte_is_r() {
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5));
        for r in 0..8u8 {
            let id = generate_node_id(ip, r);
            assert_eq!(id.0[19] & 0x07, r, "last byte low 3 bits must be r");
        }
    }

    // ── ExternalIpVoter ─────────────────────────────────────────────

    #[test]
    fn voter_no_consensus_below_threshold() {
        let mut voter = ExternalIpVoter::new(3);
        let ip: IpAddr = "203.0.113.5".parse().unwrap();
        assert!(voter.add_vote(1, ip).is_none());
        assert!(voter.add_vote(2, ip).is_none());
        assert!(voter.consensus().is_none());
    }

    #[test]
    fn voter_reaches_consensus() {
        let mut voter = ExternalIpVoter::new(3);
        let ip: IpAddr = "203.0.113.5".parse().unwrap();
        voter.add_vote(1, ip);
        voter.add_vote(2, ip);
        let result = voter.add_vote(3, ip);
        assert_eq!(result, Some(ip));
        assert_eq!(voter.consensus(), Some(ip));
    }

    #[test]
    fn voter_requires_majority() {
        let mut voter = ExternalIpVoter::new(3);
        let ip_a: IpAddr = "203.0.113.5".parse().unwrap();
        let ip_b: IpAddr = "198.51.100.1".parse().unwrap();
        voter.add_vote(1, ip_a);
        voter.add_vote(2, ip_b);
        // 1 vs 1 — no majority
        voter.add_vote(3, ip_a);
        // Now 2 vs 1 — ip_a has majority (2/3 > 50%)
        assert_eq!(voter.consensus(), Some(ip_a));
    }

    #[test]
    fn voter_ignores_private_ips() {
        let mut voter = ExternalIpVoter::new(1);
        assert!(voter.add_vote(1, "192.168.1.1".parse().unwrap()).is_none());
        assert!(voter.add_vote(2, "10.0.0.1".parse().unwrap()).is_none());
        assert_eq!(voter.vote_count(), 0);
    }

    #[test]
    fn voter_deduplicates_same_source() {
        let mut voter = ExternalIpVoter::new(2);
        let ip: IpAddr = "203.0.113.5".parse().unwrap();
        voter.add_vote(1, ip);
        voter.add_vote(1, ip); // same source_id
        assert_eq!(voter.vote_count(), 1);
        // Still below threshold (only 1 unique source)
        assert!(voter.consensus().is_none());
    }

    #[test]
    fn voter_consensus_changes_on_new_majority() {
        let mut voter = ExternalIpVoter::new(2);
        let ip_a: IpAddr = "203.0.113.5".parse().unwrap();
        let ip_b: IpAddr = "198.51.100.1".parse().unwrap();
        voter.add_vote(1, ip_a);
        voter.add_vote(2, ip_a);
        assert_eq!(voter.consensus(), Some(ip_a));

        // Now 3 sources vote for ip_b
        voter.add_vote(3, ip_b);
        voter.add_vote(4, ip_b);
        voter.add_vote(5, ip_b);
        // 2 for a, 3 for b — b has majority (3/5 > 50%)
        assert_eq!(voter.consensus(), Some(ip_b));
    }

    // ── IpVoteSource ────────────────────────────────────────────────

    #[test]
    fn vote_source_ids_are_distinct() {
        let nat = IpVoteSource::Nat;
        let tracker = IpVoteSource::Tracker;
        let dht = IpVoteSource::Dht(12345);
        assert_ne!(nat.source_id(), tracker.source_id());
        assert_ne!(nat.source_id(), dht.source_id());
        assert_ne!(tracker.source_id(), dht.source_id());
    }
}
