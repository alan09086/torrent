# M37: BEP 42 — DHT Security Extension Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Harden the DHT against node ID spoofing and Sybil attacks by tying node IDs to IP addresses via CRC32C, rejecting non-compliant routing table entries, detecting external IP via consensus voting, and adding the KRPC `ip` field to all responses.

**Architecture:** A new `node_id` module in `ferrite-dht` implements BEP 42 node ID generation and verification using CRC32C over masked IP addresses. An `ExternalIpVoter` in `ferrite-dht` aggregates external IP reports from KRPC responses, NAT traversal, and tracker responses to reach consensus. The DHT actor enforces node ID validity on routing table insertions and KRPC response parsing, with configurable strictness via new `DhtConfig` fields. A new `DhtNodeIdViolation` alert in `ferrite-session` notifies consumers of rejected nodes.

**Tech Stack:** Rust 2024, tokio, CRC32C (inline lookup table, same as `peer_priority.rs`), thiserror, serde, tracing.

---

## Crate Dependency Summary

| Crate | Changes |
|-------|---------|
| `ferrite-dht` | New `node_id.rs` module (CRC32C, generation, verification), `ExternalIpVoter`, `DhtConfig` fields, actor enforcement, KRPC `ip` field |
| `ferrite-session` | `DhtNodeIdViolation` alert, `Settings` fields, `to_dht_config()` updates, ExternalIpVoter integration |
| `ferrite-core` | None (Id20 already sufficient) |

---

### Task 1: BEP 42 Node ID Generation and Verification (`ferrite-dht/src/node_id.rs`)

**Files:**
- Create: `crates/ferrite-dht/src/node_id.rs`
- Modify: `crates/ferrite-dht/src/lib.rs` (add `pub mod node_id;` and re-exports)

This task implements the core BEP 42 algorithm: CRC32C-based node ID generation from IP addresses and verification that a given node ID matches an IP.

**Step 1:** Create `crates/ferrite-dht/src/node_id.rs` with CRC32C implementation, node ID generation, and verification.

The CRC32C implementation reuses the same Castagnoli polynomial lookup table pattern from `crates/ferrite-session/src/peer_priority.rs` (line 14). Since `ferrite-dht` cannot depend on `ferrite-session` (it's the other direction), we duplicate the 256-entry const table. This is deliberate — the same pattern exists in libtorrent-rasterbar where CRC32C is inlined in multiple translation units.

```rust
//! BEP 42: DHT Security Extension — Node ID generation and verification.
//!
//! Ties DHT node IDs to IP addresses using CRC32C, making Sybil attacks
//! computationally expensive. The first 21 bits of a valid node ID must
//! match `CRC32C(masked_ip | (r << 29))`, and the last byte must equal `r`.
//!
//! Reference: <https://www.bittorrent.org/beps/bep_0042.html>

use std::net::IpAddr;

use ferrite_core::Id20;

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
        // First 21 bits = 0x5fbfbf >> 3 shifted context: top 21 bits
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
}
```

**Step 2:** Register the module in `crates/ferrite-dht/src/lib.rs`.

Add `pub mod node_id;` after the existing module declarations, and add re-exports.

In `crates/ferrite-dht/src/lib.rs`, after line 16 (`mod actor;`), add:

```rust
pub mod node_id;
```

And extend the re-exports at the bottom by adding after line 28:

```rust
pub use node_id::{generate_node_id, is_valid_node_id, is_bep42_exempt};
```

**Step 3:** Run tests to verify.

```bash
cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-dht -- node_id
```

Expected: All 14 tests pass (5 BEP 42 test vectors, 3 generate/verify tests, 3 exempt tests, 2 CRC32C baseline, 1 last byte).

---

### Task 2: External IP Voter (`ferrite-dht/src/node_id.rs`)

**Files:**
- Modify: `crates/ferrite-dht/src/node_id.rs` (append `ExternalIpVoter`)

The voter aggregates external IP reports from multiple sources (KRPC `ip` field, NAT traversal, tracker responses) and determines consensus. Uses a simple majority threshold with a minimum number of votes.

**Step 1:** Append `ExternalIpVoter` to the bottom of `crates/ferrite-dht/src/node_id.rs`, above the `#[cfg(test)]` module.

```rust
// ── External IP Voter ───────────────────────────────────────────────────

/// Source of an external IP observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IpVoteSource {
    /// KRPC response `ip` field from a DHT node.
    Dht,
    /// NAT traversal (UPnP, NAT-PMP, PCP).
    Nat,
    /// Tracker announce response.
    Tracker,
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
```

**Step 2:** Add tests for `ExternalIpVoter` inside the existing `#[cfg(test)] mod tests` block, at the end.

```rust
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
```

**Step 3:** Run the tests.

```bash
cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-dht -- node_id
```

Expected: All previous tests plus 6 new voter tests pass (~20 total).

---

### Task 3: DhtConfig Security Fields

**Files:**
- Modify: `crates/ferrite-dht/src/actor.rs` (add fields to `DhtConfig`)
- Modify: `crates/ferrite-session/src/settings.rs` (add fields to `Settings`, update `to_dht_config()`)

**Step 1:** Add BEP 42 fields to `DhtConfig` in `crates/ferrite-dht/src/actor.rs`.

After line 36 (`pub address_family: AddressFamily,`), insert:

```rust
    /// BEP 42: Enforce node ID verification when inserting into routing table.
    /// Nodes with IDs that don't match their IP are rejected.
    pub enforce_node_id: bool,
    /// BEP 42: Restrict routing table to one node per IP address.
    pub restrict_routing_ips: bool,
    /// BEP 42: During DHT searches, prefer nodes with verified IDs.
    pub restrict_search_ips: bool,
```

Update `Default for DhtConfig` (after line 51 `address_family: AddressFamily::V4,`) to include:

```rust
            enforce_node_id: true,
            restrict_routing_ips: true,
            restrict_search_ips: true,
```

Update `DhtConfig::default_v6()` (after line 68 `address_family: AddressFamily::V6,`) to include:

```rust
            enforce_node_id: true,
            restrict_routing_ips: true,
            restrict_search_ips: true,
```

**Step 2:** Add corresponding fields to `Settings` in `crates/ferrite-session/src/settings.rs`.

After line 272 (`pub dht_query_timeout_secs: u64,`) add:

```rust
    /// BEP 42: Enforce node ID verification in DHT routing table.
    #[serde(default = "default_true")]
    pub dht_enforce_node_id: bool,
    /// BEP 42: Restrict DHT routing table to one node per IP.
    #[serde(default = "default_true")]
    pub dht_restrict_routing_ips: bool,
    /// BEP 42: Prefer verified node IDs in DHT searches.
    #[serde(default = "default_true")]
    pub dht_restrict_search_ips: bool,
```

Update `Settings::default()` to include after `dht_query_timeout_secs: 10,`:

```rust
            dht_enforce_node_id: true,
            dht_restrict_routing_ips: true,
            dht_restrict_search_ips: true,
```

Update `Settings::to_dht_config()` to pass through the fields:

```rust
    pub(crate) fn to_dht_config(&self) -> ferrite_dht::DhtConfig {
        ferrite_dht::DhtConfig {
            queries_per_second: self.dht_queries_per_second,
            query_timeout: std::time::Duration::from_secs(self.dht_query_timeout_secs),
            enforce_node_id: self.dht_enforce_node_id,
            restrict_routing_ips: self.dht_restrict_routing_ips,
            restrict_search_ips: self.dht_restrict_search_ips,
            ..ferrite_dht::DhtConfig::default()
        }
    }
```

And likewise for `to_dht_config_v6()`:

```rust
    pub(crate) fn to_dht_config_v6(&self) -> ferrite_dht::DhtConfig {
        ferrite_dht::DhtConfig {
            queries_per_second: self.dht_queries_per_second,
            query_timeout: std::time::Duration::from_secs(self.dht_query_timeout_secs),
            enforce_node_id: self.dht_enforce_node_id,
            restrict_routing_ips: self.dht_restrict_routing_ips,
            restrict_search_ips: self.dht_restrict_search_ips,
            ..ferrite_dht::DhtConfig::default_v6()
        }
    }
```

Update `Settings::PartialEq` impl to include the 3 new fields after the `dht_query_timeout_secs` comparison.

Update `Settings::min_memory()` and `Settings::high_performance()` — no changes needed since they use `..Self::default()`.

**Step 3:** Add tests for the new config fields.

In the `settings.rs` tests, add:

```rust
    #[test]
    fn dht_security_defaults() {
        let s = Settings::default();
        assert!(s.dht_enforce_node_id);
        assert!(s.dht_restrict_routing_ips);
        assert!(s.dht_restrict_search_ips);
    }

    #[test]
    fn dht_config_inherits_security_settings() {
        let mut s = Settings::default();
        s.dht_enforce_node_id = false;
        let dht = s.to_dht_config();
        assert!(!dht.enforce_node_id);
        assert!(dht.restrict_routing_ips);
    }
```

In `actor.rs` tests, add:

```rust
    #[test]
    fn dht_config_security_defaults() {
        let config = DhtConfig::default();
        assert!(config.enforce_node_id);
        assert!(config.restrict_routing_ips);
        assert!(config.restrict_search_ips);

        let config_v6 = DhtConfig::default_v6();
        assert!(config_v6.enforce_node_id);
        assert!(config_v6.restrict_routing_ips);
        assert!(config_v6.restrict_search_ips);
    }
```

**Step 4:** Run tests.

```bash
cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-dht -p ferrite-session
```

Expected: All existing tests pass plus 3 new tests.

---

### Task 4: KRPC `ip` Field — Encoding and Parsing

**Files:**
- Modify: `crates/ferrite-dht/src/krpc.rs` (add `ip` field to response encoding/parsing)
- Modify: `crates/ferrite-dht/src/actor.rs` (emit `ip` in responses, parse `ip` from responses)

BEP 42 specifies that all KRPC responses should include a top-level `ip` key containing the requestor's IP+port in compact binary format (6 bytes for IPv4, 18 bytes for IPv6).

**Step 1:** Add an `external_ip` field to `KrpcMessage` for the `ip` key.

In `crates/ferrite-dht/src/krpc.rs`, modify the `KrpcMessage` struct (line 48):

```rust
/// A parsed KRPC message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KrpcMessage {
    pub transaction_id: TransactionId,
    pub body: KrpcBody,
    /// BEP 42: Compact IP+port of the message recipient, included in responses.
    pub sender_ip: Option<std::net::SocketAddr>,
}
```

**Step 2:** Update `KrpcMessage::to_bytes()` (line 150) to encode the `ip` field.

After inserting the `t` key (line 152), add:

```rust
        if let Some(addr) = &self.sender_ip {
            let ip_bytes = encode_compact_addr(addr);
            dict.insert(b"ip".to_vec(), BencodeValue::Bytes(ip_bytes));
        }
```

Add helper function after `encode_response_values`:

```rust
/// Encode a socket address to compact binary format (BEP 42 `ip` field).
fn encode_compact_addr(addr: &SocketAddr) -> Vec<u8> {
    match addr {
        SocketAddr::V4(v4) => {
            let mut buf = Vec::with_capacity(6);
            buf.extend_from_slice(&v4.ip().octets());
            buf.extend_from_slice(&v4.port().to_be_bytes());
            buf
        }
        SocketAddr::V6(v6) => {
            let mut buf = Vec::with_capacity(18);
            buf.extend_from_slice(&v6.ip().octets());
            buf.extend_from_slice(&v6.port().to_be_bytes());
            buf
        }
    }
}

/// Decode a compact binary socket address (BEP 42 `ip` field).
fn decode_compact_addr(data: &[u8]) -> Option<SocketAddr> {
    match data.len() {
        6 => {
            let ip = Ipv4Addr::new(data[0], data[1], data[2], data[3]);
            let port = u16::from_be_bytes([data[4], data[5]]);
            Some(SocketAddr::V4(SocketAddrV4::new(ip, port)))
        }
        18 => {
            let ip = Ipv6Addr::from(<[u8; 16]>::try_from(&data[..16]).unwrap());
            let port = u16::from_be_bytes([data[16], data[17]]);
            Some(SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)))
        }
        _ => None,
    }
}
```

**Step 3:** Update `KrpcMessage::from_bytes()` to parse the `ip` field.

In the decoding path, after constructing the `body` and before the final `Ok(...)` (around line 228):

```rust
        let sender_ip = dict
            .get(&b"ip"[..])
            .and_then(|v| v.as_bytes_raw())
            .and_then(decode_compact_addr);

        Ok(KrpcMessage {
            transaction_id,
            body,
            sender_ip,
        })
```

**Step 4:** Update all `KrpcMessage` construction sites.

Every place that constructs a `KrpcMessage` literal needs `sender_ip: None` added. This includes:

- `crates/ferrite-dht/src/actor.rs`: lines 427-430 (`err_msg`), 448-451 (`reply`), 637-645 (`msg` for announce_peer), 703-709 (`msg` for find_node), 728-734 (`msg` for get_peers)
- `crates/ferrite-dht/src/krpc.rs`: all test `KrpcMessage` constructions

In `actor.rs`, the **response** messages should include `sender_ip: Some(addr)` where `addr` is the address of the querier (the node that sent the query). Specifically, in `handle_query()`, the `reply` message at line 448 should set `sender_ip: Some(addr)`.

The **query** messages (outbound find_node, get_peers, announce_peer) use `sender_ip: None` since the `ip` field is only included in responses.

**Step 5:** Add round-trip tests for the `ip` field.

In `krpc.rs` tests:

```rust
    #[test]
    fn response_with_ip_field_round_trip() {
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(42),
            body: KrpcBody::Response(KrpcResponse::NodeId { id: test_id() }),
            sender_ip: Some("203.0.113.5:6881".parse().unwrap()),
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.sender_ip, Some("203.0.113.5:6881".parse().unwrap()));
    }

    #[test]
    fn response_with_ipv6_ip_field_round_trip() {
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(42),
            body: KrpcBody::Response(KrpcResponse::NodeId { id: test_id() }),
            sender_ip: Some("[2001:db8::1]:6881".parse().unwrap()),
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.sender_ip, Some("[2001:db8::1]:6881".parse().unwrap()));
    }

    #[test]
    fn message_without_ip_field_parses_as_none() {
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(42),
            body: KrpcBody::Response(KrpcResponse::NodeId { id: test_id() }),
            sender_ip: None,
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert!(decoded.sender_ip.is_none());
    }
```

**Step 6:** Run tests.

```bash
cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-dht
```

Expected: All existing tests pass (after adding `sender_ip: None` to each test's `KrpcMessage`), plus 3 new tests.

---

### Task 5: DHT Actor — Node ID Enforcement and IP Voter Integration

**Files:**
- Modify: `crates/ferrite-dht/src/actor.rs` (enforce BEP 42 on insert, generate compliant own ID, voter integration)

This is the core enforcement task. The DHT actor:
1. Generates a BEP 42-compliant own node ID when it learns its external IP
2. Rejects routing table entries from nodes with invalid IDs
3. Feeds KRPC `ip` fields into the ExternalIpVoter
4. Includes `ip` in all outgoing responses
5. Exposes the voter for session-level integration

**Step 1:** Add an `ExternalIpVoter` and a reporting callback to `DhtHandle` and `DhtActor`.

In `crates/ferrite-dht/src/actor.rs`, add imports at the top:

```rust
use crate::node_id::{self, ExternalIpVoter};
```

Add a new command variant to `DhtCommand`:

```rust
    UpdateExternalIp {
        ip: std::net::IpAddr,
    },
```

Add voter and event channel fields to `DhtActor` (after `lookups` field):

```rust
    /// BEP 42 external IP voter: aggregates IP reports from KRPC responses.
    ip_voter: ExternalIpVoter,
    /// Callback channel: fires when voter consensus changes.
    ip_consensus_tx: mpsc::Sender<std::net::IpAddr>,
```

Add to `DhtHandle`:

```rust
    /// Notify the DHT of our external IP (from NAT/tracker discovery).
    pub async fn update_external_ip(&self, ip: std::net::IpAddr) -> Result<()> {
        self.tx
            .send(DhtCommand::UpdateExternalIp { ip })
            .await
            .map_err(|_| Error::Shutdown)
    }
```

**Step 2:** Update `DhtHandle::start()` to return an IP consensus receiver.

Change the signature:

```rust
    pub async fn start(config: DhtConfig) -> Result<(Self, mpsc::Receiver<std::net::IpAddr>)> {
```

In the body, create the consensus channel:

```rust
        let (ip_consensus_tx, ip_consensus_rx) = mpsc::channel(4);
```

Pass `ip_consensus_tx` to `DhtActor::new()`, and return `(handle, ip_consensus_rx)`.

**Step 3:** Update `DhtActor::new()` to accept the voter channel and initialize the voter.

```rust
    fn new(
        config: DhtConfig,
        socket: UdpSocket,
        rx: mpsc::Receiver<DhtCommand>,
        ip_consensus_tx: mpsc::Sender<std::net::IpAddr>,
    ) -> Self {
        let own_id = config.own_id.unwrap_or_else(generate_node_id);
        let address_family = config.address_family;
        debug!(id = %own_id, family = ?address_family, "DHT node ID");

        DhtActor {
            config,
            address_family,
            socket,
            rx,
            routing_table: RoutingTable::new(own_id),
            peer_store: PeerStore::new(),
            pending: HashMap::new(),
            next_txn_id: 1,
            stats: ActorStats {
                total_queries_sent: 0,
                total_responses_received: 0,
            },
            lookups: HashMap::new(),
            ip_voter: ExternalIpVoter::new(10),
            ip_consensus_tx,
        }
    }
```

**Step 4:** Enforce BEP 42 on routing table insertions.

Create a helper method on `DhtActor`:

```rust
    /// Insert a node into the routing table, enforcing BEP 42 if enabled.
    fn checked_insert(&mut self, id: Id20, addr: SocketAddr) -> bool {
        if self.config.enforce_node_id && !node_id::is_valid_node_id(&id, addr.ip()) {
            trace!(
                node_id = %id,
                ip = %addr.ip(),
                "BEP 42: rejecting node with invalid ID for IP"
            );
            return false;
        }
        self.routing_table.insert(id, addr)
    }
```

Replace all `self.routing_table.insert(...)` calls in `handle_query()`, `handle_response()`, `start_get_peers()` with `self.checked_insert(...)`.

There are 6 call sites:
- `handle_query()` line 365: `self.routing_table.insert(sender_id, addr);` → `self.checked_insert(sender_id, addr);`
- `handle_response()` line 464: `self.routing_table.insert(sender_id, addr);` → `self.checked_insert(sender_id, addr);`
- `handle_response()` FindNode branch lines 479, 484: `self.routing_table.insert(node.id, node.addr);` → `self.checked_insert(node.id, node.addr);`
- `handle_response()` GetPeers branch lines 495, 500: `self.routing_table.insert(node.id, node.addr);` → `self.checked_insert(node.id, node.addr);`

**Step 5:** Feed KRPC `ip` field into the voter in `handle_response()`.

After parsing the response, check for the `ip` field:

```rust
    async fn handle_response(&mut self, msg: &KrpcMessage, resp: &KrpcResponse, addr: SocketAddr) {
        if !self.matches_family(&addr) {
            return;
        }
        self.stats.total_responses_received += 1;

        // BEP 42: feed the ip field into the voter
        if let Some(reported_ip) = msg.sender_ip {
            // Use hash of the responder's socket address as source_id for dedup
            let source_id = hash_source_addr(&addr);
            if let Some(consensus_ip) = self.ip_voter.add_vote(source_id, reported_ip.ip()) {
                debug!(%consensus_ip, "BEP 42: external IP consensus changed");
                let _ = self.ip_consensus_tx.try_send(consensus_ip);
                // Regenerate our node ID to match the new external IP
                self.regenerate_node_id(consensus_ip);
            }
        }

        // ... rest of existing code ...
    }
```

Add the `hash_source_addr` helper and `regenerate_node_id` method:

```rust
/// Hash a socket address to a u64 for use as a voter source ID.
fn hash_source_addr(addr: &SocketAddr) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    addr.hash(&mut hasher);
    hasher.finish()
}
```

```rust
    /// Regenerate our node ID to be BEP 42-compliant for the given external IP.
    fn regenerate_node_id(&mut self, external_ip: std::net::IpAddr) {
        let r = self.routing_table.own_id().0[19] & 0x07; // preserve r from current ID
        let new_id = node_id::generate_node_id(external_ip, r);
        debug!(old_id = %self.routing_table.own_id(), new_id = %new_id, "BEP 42: regenerating node ID");
        self.routing_table = RoutingTable::new(new_id);
        // Note: routing table is now empty — bootstrap will repopulate it
    }
```

**Step 6:** Set `sender_ip` on outgoing responses.

In `handle_query()`, when building the reply message (around line 448):

```rust
        let reply = KrpcMessage {
            transaction_id: msg.transaction_id,
            body: KrpcBody::Response(response),
            sender_ip: Some(addr), // BEP 42: tell the querier their IP
        };
```

**Step 7:** Handle `UpdateExternalIp` command in the actor's run loop.

In the `run()` method's command match (after the `Stats` arm):

```rust
                        Some(DhtCommand::UpdateExternalIp { ip }) => {
                            // External source (NAT/tracker) reports our IP
                            let source_id = 0xFFFF_FFFF_FFFF_FFFFu64; // well-known ID for external sources
                            if let Some(consensus_ip) = self.ip_voter.add_vote(source_id, ip) {
                                debug!(%consensus_ip, "BEP 42: external IP consensus (via NAT/tracker)");
                                let _ = self.ip_consensus_tx.try_send(consensus_ip);
                                self.regenerate_node_id(consensus_ip);
                            }
                        }
```

**Step 8:** Update all call sites that use `DhtHandle::start()`.

In `crates/ferrite-session/src/session.rs`, the `DhtHandle::start()` calls (lines 267, 282) now return a tuple. Update to destructure:

```rust
        let (dht_v4, dht_v4_ip_rx) = if settings.enable_dht {
            match DhtHandle::start(settings.to_dht_config()).await {
                Ok((handle, ip_rx)) => {
                    info!("DHT v4 started");
                    (Some(handle), Some(ip_rx))
                }
                Err(e) => {
                    warn!("DHT v4 start failed: {e}");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };
```

And similarly for `dht_v6`. The `ip_rx` channels should be stored on the `SessionActor` and polled in its `select!` loop (see Task 7).

**Step 9:** Add actor-level tests.

```rust
    #[test]
    fn dht_config_enforce_node_id_defaults_true() {
        let config = DhtConfig::default();
        assert!(config.enforce_node_id);
    }

    #[tokio::test]
    async fn dht_handle_start_returns_ip_channel() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dht_update_external_ip() {
        let config = DhtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            bootstrap_nodes: Vec::new(),
            ..DhtConfig::default()
        };
        let (handle, _ip_rx) = DhtHandle::start(config).await.unwrap();
        // Should not error even with no consensus yet
        handle.update_external_ip("203.0.113.5".parse().unwrap()).await.unwrap();
        handle.shutdown().await.unwrap();
    }
```

**Step 10:** Run tests.

```bash
cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-dht
```

Expected: All existing tests pass (after updating `DhtHandle::start()` destructuring), plus new tests.

---

### Task 6: DhtNodeIdViolation Alert

**Files:**
- Modify: `crates/ferrite-session/src/alert.rs` (add `DhtNodeIdViolation` variant)

**Step 1:** Add the alert variant to `AlertKind` in `crates/ferrite-session/src/alert.rs`.

After the `DhtGetPeers` variant (line 87), add:

```rust
    /// BEP 42: A DHT node was rejected because its ID doesn't match its IP.
    DhtNodeIdViolation { node_id: Id20, addr: SocketAddr },
```

**Step 2:** Map it to the `DHT | ERROR` category.

In the `category()` match, update the DHT arm (line 180):

```rust
            DhtBootstrapComplete | DhtGetPeers { .. } => AlertCategory::DHT,
            DhtNodeIdViolation { .. } => AlertCategory::DHT | AlertCategory::ERROR,
```

**Step 3:** Add a test.

```rust
    #[test]
    fn dht_node_id_violation_alert_category() {
        let alert = Alert::new(AlertKind::DhtNodeIdViolation {
            node_id: Id20::from([0u8; 20]),
            addr: "203.0.113.5:6881".parse().unwrap(),
        });
        assert!(alert.category().contains(AlertCategory::DHT));
        assert!(alert.category().contains(AlertCategory::ERROR));
    }
```

**Step 4:** Run tests.

```bash
cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session -- alert
```

Expected: All alert tests pass plus 1 new test.

---

### Task 7: Session Integration — IP Consensus Propagation

**Files:**
- Modify: `crates/ferrite-session/src/session.rs` (poll DHT IP channels, feed voter, fire alert)

This task wires the DHT's `ExternalIpVoter` consensus into the session. When the DHT discovers our external IP via KRPC `ip` fields, the session updates `self.external_ip` and propagates to all torrent actors (same as the existing NAT discovery path).

**Step 1:** Add DHT IP consensus channels to `SessionActor`.

After the existing `external_ip` field (line 708):

```rust
    /// BEP 42: External IP consensus from DHT v4 KRPC responses.
    dht_v4_ip_rx: Option<mpsc::Receiver<std::net::IpAddr>>,
    /// BEP 42: External IP consensus from DHT v6 KRPC responses.
    dht_v6_ip_rx: Option<mpsc::Receiver<std::net::IpAddr>>,
```

**Step 2:** Initialize these in `SessionHandle::start()`.

Pass the `dht_v4_ip_rx` and `dht_v6_ip_rx` from the `DhtHandle::start()` calls to the `SessionActor` constructor.

**Step 3:** Poll the channels in the `SessionActor::run()` select! loop.

Add two new arms to the `tokio::select!` block, after the NAT events arm:

```rust
                // BEP 42: DHT v4 external IP consensus
                Some(ip) = async {
                    match self.dht_v4_ip_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    info!(%ip, "external IP discovered via DHT v4 (BEP 42)");
                    self.external_ip = Some(ip);
                    for entry in self.torrents.values() {
                        let _ = entry.handle.update_external_ip(ip).await;
                    }
                }

                // BEP 42: DHT v6 external IP consensus
                Some(ip) = async {
                    match self.dht_v6_ip_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    info!(%ip, "external IP discovered via DHT v6 (BEP 42)");
                    self.external_ip = Some(ip);
                    for entry in self.torrents.values() {
                        let _ = entry.handle.update_external_ip(ip).await;
                    }
                }
```

**Step 4:** Also feed NAT-discovered IPs to both DHT instances.

In the existing `ExternalIpDiscovered` handler (around line 894), after updating `self.external_ip`, add:

```rust
                            // BEP 42: notify DHT instances of external IP
                            if let Some(dht) = &self.dht_v4 {
                                let _ = dht.update_external_ip(ip).await;
                            }
                            if let Some(dht) = &self.dht_v6 {
                                let _ = dht.update_external_ip(ip).await;
                            }
```

**Step 5:** Run full workspace tests.

```bash
cd /mnt/TempNVME/projects/ferrite && cargo test --workspace
```

Expected: All ~900+ tests pass.

---

### Task 8: Routing Table IP Restriction

**Files:**
- Modify: `crates/ferrite-dht/src/routing_table.rs` (track IPs, enforce one-node-per-IP)
- Modify: `crates/ferrite-dht/src/actor.rs` (pass config flag)

When `restrict_routing_ips` is enabled, the routing table should not contain more than one node per IP address. This prevents a single machine from occupying multiple slots.

**Step 1:** Add IP tracking to `RoutingTable`.

In `crates/ferrite-dht/src/routing_table.rs`, add a `HashSet` to track IPs:

```rust
use std::collections::HashSet;
use std::net::IpAddr;
```

Add to `RoutingTable`:

```rust
pub struct RoutingTable {
    own_id: Id20,
    buckets: Vec<KBucket>,
    /// When enabled, tracks IPs to enforce one-node-per-IP (BEP 42).
    ip_set: HashSet<IpAddr>,
    /// Whether to enforce one-node-per-IP restriction.
    restrict_ips: bool,
}
```

**Step 2:** Add a constructor that accepts the `restrict_ips` flag.

```rust
    /// Create a new routing table with IP restriction setting.
    pub fn new_with_config(own_id: Id20, restrict_ips: bool) -> Self {
        RoutingTable {
            own_id,
            buckets: vec![KBucket::new()],
            ip_set: HashSet::new(),
            restrict_ips,
        }
    }
```

Update the existing `new()` to delegate:

```rust
    pub fn new(own_id: Id20) -> Self {
        Self::new_with_config(own_id, false)
    }
```

**Step 3:** Enforce IP restriction in `insert_inner()`.

At the top of `insert_inner()`, before the bucket lookup:

```rust
    fn insert_inner(&mut self, id: Id20, addr: SocketAddr) -> bool {
        let ip = addr.ip();

        // BEP 42: check if another node with this IP exists (and it's not the same node)
        if self.restrict_ips && self.ip_set.contains(&ip) {
            // Check if it's actually the same node (updating its entry)
            let bucket_idx = self.bucket_index(&id);
            if self.buckets[bucket_idx].find(&id).is_none() {
                return false; // Different node, same IP — reject
            }
        }

        // ... existing insert logic ...

        // After successful insert, track the IP
        // (add ip_set.insert(ip) after each successful insertion point)
    }
```

Also update `remove()` to remove the IP from the set.

**Step 4:** Update `DhtActor::new()` to pass `restrict_ips` to the routing table.

```rust
        DhtActor {
            // ...
            routing_table: RoutingTable::new_with_config(own_id, config.restrict_routing_ips),
            // ...
        }
```

And update `regenerate_node_id()`:

```rust
    fn regenerate_node_id(&mut self, external_ip: std::net::IpAddr) {
        let r = self.routing_table.own_id().0[19] & 0x07;
        let new_id = node_id::generate_node_id(external_ip, r);
        let restrict_ips = self.config.restrict_routing_ips;
        debug!(old_id = %self.routing_table.own_id(), new_id = %new_id, "BEP 42: regenerating node ID");
        self.routing_table = RoutingTable::new_with_config(new_id, restrict_ips);
    }
```

**Step 5:** Add tests.

```rust
    #[test]
    fn restrict_ips_rejects_second_node_same_ip() {
        let mut rt = RoutingTable::new_with_config(Id20::ZERO, true);
        let ip_addr: SocketAddr = "10.0.0.1:6881".parse().unwrap();
        assert!(rt.insert(id(1), ip_addr));
        // Second node with same IP but different ID — rejected
        let ip_addr2: SocketAddr = "10.0.0.1:6882".parse().unwrap();
        assert!(!rt.insert(id(2), ip_addr2));
        assert_eq!(rt.len(), 1);
    }

    #[test]
    fn restrict_ips_allows_same_node_update() {
        let mut rt = RoutingTable::new_with_config(Id20::ZERO, true);
        let addr1: SocketAddr = "10.0.0.1:6881".parse().unwrap();
        let addr2: SocketAddr = "10.0.0.1:6882".parse().unwrap();
        assert!(rt.insert(id(1), addr1));
        // Same node ID updating its port — allowed
        assert!(rt.insert(id(1), addr2));
        assert_eq!(rt.len(), 1);
        assert_eq!(rt.get(&id(1)).unwrap().addr, addr2);
    }

    #[test]
    fn no_restrict_ips_allows_multiple_nodes_same_ip() {
        let mut rt = RoutingTable::new_with_config(Id20::ZERO, false);
        let addr1: SocketAddr = "10.0.0.1:6881".parse().unwrap();
        let addr2: SocketAddr = "10.0.0.1:6882".parse().unwrap();
        assert!(rt.insert(id(1), addr1));
        assert!(rt.insert(id(2), addr2));
        assert_eq!(rt.len(), 2);
    }
```

**Step 6:** Run tests.

```bash
cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-dht -- routing_table
```

Expected: All existing routing table tests pass, plus 3 new tests.

---

### Task 9: Facade Re-exports and Prelude

**Files:**
- Modify: `crates/ferrite/src/dht.rs` (re-export new types)

**Step 1:** Check and update the facade's DHT re-exports.

In `crates/ferrite/src/dht.rs`, add re-exports for the new public types:

```rust
pub use ferrite_dht::node_id::{generate_node_id, is_valid_node_id, is_bep42_exempt, ExternalIpVoter, IpVoteSource};
```

**Step 2:** Run full workspace tests.

```bash
cd /mnt/TempNVME/projects/ferrite && cargo test --workspace
```

Expected: All tests pass.

---

### Task 10: Clippy, Docs, and Final Verification

**Files:**
- All modified files

**Step 1:** Run clippy with warnings-as-errors.

```bash
cd /mnt/TempNVME/projects/ferrite && cargo clippy --workspace -- -D warnings
```

Fix any warnings.

**Step 2:** Run the full test suite.

```bash
cd /mnt/TempNVME/projects/ferrite && cargo test --workspace
```

Expected: ~920+ tests pass (895 existing + ~25 new), zero clippy warnings.

**Step 3:** Bump workspace version.

In `Cargo.toml` root, update:

```toml
version = "0.42.0"
```

**Step 4:** Commit.

```bash
cd /mnt/TempNVME/projects/ferrite
git add -A
git commit -m "feat: BEP 42 DHT security extension (M37)

- Node ID verification: CRC32C of external IP -> expected 21-bit prefix
- ExternalIpVoter: consensus-based external IP from KRPC, NAT, tracker
- KRPC 'ip' field: included in all DHT responses (BEP 42)
- Routing table: reject invalid node IDs, one-node-per-IP restriction
- DhtConfig: enforce_node_id, restrict_routing_ips, restrict_search_ips
- Settings: 3 new dht_* fields with serde defaults
- Alert: DhtNodeIdViolation (DHT|ERROR category)
- Session: DHT IP consensus feeds into external_ip propagation
- 5 BEP 42 test vectors verified"
```

---

## Summary of New/Modified Files

| File | Action | Description |
|------|--------|-------------|
| `crates/ferrite-dht/src/node_id.rs` | **Create** | CRC32C, BEP 42 node ID generation/verification, ExternalIpVoter |
| `crates/ferrite-dht/src/lib.rs` | Modify | Add `pub mod node_id` and re-exports |
| `crates/ferrite-dht/src/actor.rs` | Modify | DhtConfig fields, checked_insert, voter integration, regenerate_node_id, ip field on responses, UpdateExternalIp command |
| `crates/ferrite-dht/src/krpc.rs` | Modify | `sender_ip` field on KrpcMessage, encode/decode compact addr helpers |
| `crates/ferrite-dht/src/routing_table.rs` | Modify | IP restriction set, `new_with_config()` |
| `crates/ferrite-session/src/settings.rs` | Modify | 3 new `dht_*` fields, `to_dht_config()` updates |
| `crates/ferrite-session/src/alert.rs` | Modify | `DhtNodeIdViolation` variant |
| `crates/ferrite-session/src/session.rs` | Modify | DHT IP channels, consensus polling, NAT→DHT propagation |
| `crates/ferrite/src/dht.rs` | Modify | Re-export node_id types |
| `Cargo.toml` | Modify | Version bump to 0.42.0 |

## Test Count Estimate

| Task | New Tests |
|------|-----------|
| Task 1: Node ID gen/verify | 14 |
| Task 2: ExternalIpVoter | 6 |
| Task 3: DhtConfig fields | 3 |
| Task 4: KRPC ip field | 3 |
| Task 5: Actor enforcement | 3 |
| Task 6: Alert | 1 |
| Task 7: Session integration | 0 (integration) |
| Task 8: Routing table IP restriction | 3 |
| Task 9: Facade | 0 (re-exports) |
| Task 10: Clippy + final | 0 |
| **Total new tests** | **~33** |
| **Total workspace** | **~928** |
