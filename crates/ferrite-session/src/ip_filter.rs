//! IP and port filtering using sorted interval maps.
//!
//! Provides [`IpFilter`] for blocking peer connections by IP address range,
//! and [`PortFilter`] for blocking by port range. Supports eMule `.dat` and
//! P2P plaintext blocklist file formats.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::rate_limiter::is_local_network;

// ── Helper traits ────────────────────────────────────────────────────

/// Types that have a minimum and maximum value.
trait Bounded {
    fn min_value() -> Self;
    #[allow(dead_code)]
    fn max_value() -> Self;
}

/// Types that can produce a successor (saturating).
trait Successor {
    fn successor(self) -> Self;
}

impl Bounded for Ipv4Addr {
    fn min_value() -> Self { Ipv4Addr::new(0, 0, 0, 0) }
    fn max_value() -> Self { Ipv4Addr::new(255, 255, 255, 255) }
}

impl Successor for Ipv4Addr {
    fn successor(self) -> Self {
        let n: u32 = self.into();
        Ipv4Addr::from(n.saturating_add(1))
    }
}

impl Bounded for Ipv6Addr {
    fn min_value() -> Self { Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0) }
    fn max_value() -> Self { Ipv6Addr::new(0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff) }
}

impl Successor for Ipv6Addr {
    fn successor(self) -> Self {
        let n: u128 = self.into();
        Ipv6Addr::from(n.saturating_add(1))
    }
}

impl Bounded for u16 {
    fn min_value() -> Self { 0 }
    fn max_value() -> Self { u16::MAX }
}

impl Successor for u16 {
    fn successor(self) -> Self { self.saturating_add(1) }
}

// ── IntervalMap ──────────────────────────────────────────────────────

/// A sorted interval map where each entry means "from this key onward, flags
/// are this value". The entire key space defaults to flags=0 (allowed).
///
/// `add_rule` applies last-applied-wins semantics for overlapping ranges.
#[derive(Debug, Clone)]
struct IntervalMap<K: Ord + Clone + Bounded + Successor> {
    /// Sorted breakpoints: from this key onward, flags are the stored value.
    map: BTreeMap<K, u32>,
}

impl<K: Ord + Clone + Bounded + Successor> IntervalMap<K> {
    fn new() -> Self {
        let mut map = BTreeMap::new();
        // Entire space starts at 0 (allowed)
        map.insert(K::min_value(), 0);
        Self { map }
    }

    /// Set flags for the range `[first, last]`.
    fn add_rule(&mut self, first: K, last: K, flags: u32) {
        if first > last {
            return;
        }

        // Save the flags that were in effect at `last.successor()` before we modify anything,
        // so we can restore them after the range.
        let after_key = last.clone().successor();
        let flags_after = self.access(&after_key);

        // Save the flags that were in effect just before `first`.
        // We need this in case first == K::min_value().
        // Actually we just need to set the breakpoint at `first` to `flags`.

        // Remove all breakpoints strictly between first (exclusive) and after_key (exclusive)
        // We collect keys to remove to avoid borrowing issues
        let keys_to_remove: Vec<K> = self
            .map
            .range(first.clone()..after_key.clone())
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys_to_remove {
            self.map.remove(&k);
        }

        // Set the start of our range
        self.map.insert(first, flags);

        // Restore the flags after our range (only if after_key is still in bounds)
        if after_key > last {
            self.map.insert(after_key, flags_after);
        }

        self.minimize();
    }

    /// Look up flags for a key. O(log n).
    fn access(&self, key: &K) -> u32 {
        self.map
            .range(..=key.clone())
            .next_back()
            .map(|(_, &v)| v)
            .unwrap_or(0)
    }

    /// Remove consecutive entries with the same flags.
    fn minimize(&mut self) {
        let mut prev_flags: Option<u32> = None;
        let mut to_remove = Vec::new();

        for (k, &flags) in &self.map {
            if prev_flags == Some(flags) {
                to_remove.push(k.clone());
            }
            prev_flags = Some(flags);
        }

        for k in to_remove {
            self.map.remove(&k);
        }
    }

    /// Number of breakpoints in the map.
    fn num_ranges(&self) -> usize {
        // Count segments with non-zero flags
        let mut count = 0;
        for &flags in self.map.values() {
            if flags != 0 {
                count += 1;
            }
        }
        count
    }

    fn is_empty(&self) -> bool {
        self.num_ranges() == 0
    }
}

// ── IpFilter ─────────────────────────────────────────────────────────

/// IP address filter supporting both IPv4 and IPv6 ranges.
///
/// Flags: 0 = allowed, non-zero = blocked.
/// Local/private network addresses are always exempt from filtering.
#[derive(Debug, Clone)]
pub struct IpFilter {
    v4: IntervalMap<Ipv4Addr>,
    v6: IntervalMap<Ipv6Addr>,
}

impl IpFilter {
    /// Create a new filter that allows everything.
    pub fn new() -> Self {
        Self {
            v4: IntervalMap::new(),
            v6: IntervalMap::new(),
        }
    }

    /// Add a rule blocking (or allowing) a range of IP addresses.
    ///
    /// Both endpoints must be the same address family (both v4 or both v6).
    /// Mixed families are silently ignored.
    pub fn add_rule(&mut self, first: IpAddr, last: IpAddr, flags: u32) {
        match (first, last) {
            (IpAddr::V4(f), IpAddr::V4(l)) => self.v4.add_rule(f, l, flags),
            (IpAddr::V6(f), IpAddr::V6(l)) => self.v6.add_rule(f, l, flags),
            _ => {} // mixed families: ignore
        }
    }

    /// Return the flags for an address. 0 = allowed.
    pub fn access(&self, addr: IpAddr) -> u32 {
        match addr {
            IpAddr::V4(ip) => self.v4.access(&ip),
            IpAddr::V6(ip) => self.v6.access(&ip),
        }
    }

    /// Check if an address is blocked by the filter.
    ///
    /// Local/private network addresses (RFC 1918, loopback, link-local) are
    /// always exempt and return `false` even if they fall within a blocked range.
    pub fn is_blocked(&self, addr: IpAddr) -> bool {
        if is_local_network(addr) {
            return false;
        }
        self.access(addr) != 0
    }

    /// Total number of non-zero-flag ranges across both address families.
    pub fn num_ranges(&self) -> usize {
        self.v4.num_ranges() + self.v6.num_ranges()
    }

    /// True if no rules have been added.
    pub fn is_empty(&self) -> bool {
        self.v4.is_empty() && self.v6.is_empty()
    }
}

impl Default for IpFilter {
    fn default() -> Self {
        Self::new()
    }
}

// ── PortFilter ───────────────────────────────────────────────────────

/// Port range filter.
///
/// Flags: 0 = allowed, non-zero = blocked.
#[derive(Debug, Clone)]
pub struct PortFilter {
    ports: IntervalMap<u16>,
}

impl PortFilter {
    /// Create a new filter that allows all ports.
    pub fn new() -> Self {
        Self {
            ports: IntervalMap::new(),
        }
    }

    /// Add a rule for a port range.
    pub fn add_rule(&mut self, first: u16, last: u16, flags: u32) {
        self.ports.add_rule(first, last, flags);
    }

    /// Return the flags for a port. 0 = allowed.
    pub fn access(&self, port: u16) -> u32 {
        self.ports.access(&port)
    }

    /// Check if a port is blocked.
    pub fn is_blocked(&self, port: u16) -> bool {
        self.access(port) != 0
    }
}

impl Default for PortFilter {
    fn default() -> Self {
        Self::new()
    }
}

// ── File Parsers ─────────────────────────────────────────────────────

/// Errors from parsing IP filter files.
#[derive(Debug, thiserror::Error)]
pub enum IpFilterError {
    /// An IP address could not be parsed.
    #[error("invalid IP address on line {line}: {message}")]
    InvalidAddress {
        /// One-based line number in the filter file.
        line: usize,
        /// Parse error description.
        message: String,
    },

    /// A line could not be parsed (wrong number of fields, etc.).
    #[error("malformed line {line}: {message}")]
    MalformedLine {
        /// One-based line number in the filter file.
        line: usize,
        /// Description of the formatting problem.
        message: String,
    },
}

/// Parse an eMule `.dat` format blocklist.
///
/// Format: `first_ip - last_ip , level , description`
/// Lines starting with `#` are comments.
pub fn parse_dat(input: &str) -> Result<IpFilter, IpFilterError> {
    let mut filter = IpFilter::new();

    for (line_num, line) in input.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Split on comma to get: "first_ip - last_ip", "level", "description"
        let parts: Vec<&str> = line.splitn(3, ',').collect();
        if parts.len() < 2 {
            return Err(IpFilterError::MalformedLine {
                line: line_num + 1,
                message: "expected 'first_ip - last_ip , level , description'".into(),
            });
        }

        // Parse IP range
        let ip_range = parts[0].trim();
        let ips: Vec<&str> = ip_range.splitn(2, '-').collect();
        if ips.len() != 2 {
            return Err(IpFilterError::MalformedLine {
                line: line_num + 1,
                message: "expected 'first_ip - last_ip'".into(),
            });
        }

        let first: IpAddr = ips[0].trim().parse().map_err(|e: std::net::AddrParseError| {
            IpFilterError::InvalidAddress {
                line: line_num + 1,
                message: e.to_string(),
            }
        })?;

        let last: IpAddr = ips[1].trim().parse().map_err(|e: std::net::AddrParseError| {
            IpFilterError::InvalidAddress {
                line: line_num + 1,
                message: e.to_string(),
            }
        })?;

        // Parse level (flags)
        let level: u32 = parts[1].trim().parse().map_err(|_| {
            IpFilterError::MalformedLine {
                line: line_num + 1,
                message: "invalid level (expected integer)".into(),
            }
        })?;

        filter.add_rule(first, last, level);
    }

    Ok(filter)
}

/// Parse a P2P plaintext format blocklist.
///
/// Format: `description:first_ip-last_ip`
/// Lines starting with `#` are comments.
pub fn parse_p2p(input: &str) -> Result<IpFilter, IpFilterError> {
    let mut filter = IpFilter::new();

    for (line_num, line) in input.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Split on last ':' to separate description from IP range
        let colon_pos = line.rfind(':').ok_or_else(|| IpFilterError::MalformedLine {
            line: line_num + 1,
            message: "expected 'description:first_ip-last_ip'".into(),
        })?;

        let ip_range = &line[colon_pos + 1..];
        let ips: Vec<&str> = ip_range.splitn(2, '-').collect();
        if ips.len() != 2 {
            return Err(IpFilterError::MalformedLine {
                line: line_num + 1,
                message: "expected 'first_ip-last_ip' after ':'".into(),
            });
        }

        let first: IpAddr = ips[0].trim().parse().map_err(|e: std::net::AddrParseError| {
            IpFilterError::InvalidAddress {
                line: line_num + 1,
                message: e.to_string(),
            }
        })?;

        let last: IpAddr = ips[1].trim().parse().map_err(|e: std::net::AddrParseError| {
            IpFilterError::InvalidAddress {
                line: line_num + 1,
                message: e.to_string(),
            }
        })?;

        // P2P format always blocks (flags=1)
        filter.add_rule(first, last, 1);
    }

    Ok(filter)
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Test 1: IntervalMap: empty returns allowed for any key
    #[test]
    fn interval_map_empty_returns_zero() {
        let map: IntervalMap<Ipv4Addr> = IntervalMap::new();
        assert_eq!(map.access(&Ipv4Addr::new(0, 0, 0, 0)), 0);
        assert_eq!(map.access(&Ipv4Addr::new(192, 168, 1, 1)), 0);
        assert_eq!(map.access(&Ipv4Addr::new(255, 255, 255, 255)), 0);
    }

    // Test 2: IntervalMap: single range add + lookup inside/outside
    #[test]
    fn interval_map_single_range() {
        let mut map: IntervalMap<Ipv4Addr> = IntervalMap::new();
        map.add_rule(
            Ipv4Addr::new(10, 0, 0, 0),
            Ipv4Addr::new(10, 0, 0, 255),
            1,
        );

        // Inside range
        assert_eq!(map.access(&Ipv4Addr::new(10, 0, 0, 0)), 1);
        assert_eq!(map.access(&Ipv4Addr::new(10, 0, 0, 128)), 1);
        assert_eq!(map.access(&Ipv4Addr::new(10, 0, 0, 255)), 1);

        // Outside range
        assert_eq!(map.access(&Ipv4Addr::new(9, 255, 255, 255)), 0);
        assert_eq!(map.access(&Ipv4Addr::new(10, 0, 1, 0)), 0);
        assert_eq!(map.access(&Ipv4Addr::new(192, 168, 1, 1)), 0);
    }

    // Test 3: IntervalMap: overlapping ranges — last-applied-wins
    #[test]
    fn interval_map_overlapping_last_wins() {
        let mut map: IntervalMap<Ipv4Addr> = IntervalMap::new();
        // Block 10.0.0.0 - 10.0.0.255 with flags=1
        map.add_rule(
            Ipv4Addr::new(10, 0, 0, 0),
            Ipv4Addr::new(10, 0, 0, 255),
            1,
        );
        // Allow 10.0.0.100 - 10.0.0.200 with flags=0 (override)
        map.add_rule(
            Ipv4Addr::new(10, 0, 0, 100),
            Ipv4Addr::new(10, 0, 0, 200),
            0,
        );

        assert_eq!(map.access(&Ipv4Addr::new(10, 0, 0, 50)), 1); // still blocked
        assert_eq!(map.access(&Ipv4Addr::new(10, 0, 0, 100)), 0); // allowed (override)
        assert_eq!(map.access(&Ipv4Addr::new(10, 0, 0, 150)), 0); // allowed (override)
        assert_eq!(map.access(&Ipv4Addr::new(10, 0, 0, 200)), 0); // allowed (override)
        assert_eq!(map.access(&Ipv4Addr::new(10, 0, 0, 201)), 1); // blocked again
        assert_eq!(map.access(&Ipv4Addr::new(10, 0, 0, 255)), 1); // blocked
    }

    // Test 4: IpFilter IPv4: block /24, verify access inside/outside
    #[test]
    fn ip_filter_v4_block_range() {
        let mut filter = IpFilter::new();
        filter.add_rule(
            IpAddr::V4(Ipv4Addr::new(203, 0, 113, 0)),
            IpAddr::V4(Ipv4Addr::new(203, 0, 113, 255)),
            1,
        );

        // Inside blocked range (public IPs, not local)
        assert!(filter.is_blocked("203.0.113.0".parse().unwrap()));
        assert!(filter.is_blocked("203.0.113.128".parse().unwrap()));
        assert!(filter.is_blocked("203.0.113.255".parse().unwrap()));

        // Outside
        assert!(!filter.is_blocked("203.0.112.255".parse().unwrap()));
        assert!(!filter.is_blocked("203.0.114.0".parse().unwrap()));
        assert!(!filter.is_blocked("8.8.8.8".parse().unwrap()));
    }

    // Test 5: IpFilter IPv6: block range, verify access
    #[test]
    fn ip_filter_v6_block_range() {
        let mut filter = IpFilter::new();
        filter.add_rule(
            IpAddr::V6("2001:db8::0".parse().unwrap()),
            IpAddr::V6("2001:db8::ffff".parse().unwrap()),
            1,
        );

        assert!(filter.is_blocked("2001:db8::1".parse().unwrap()));
        assert!(filter.is_blocked("2001:db8::ff".parse().unwrap()));
        assert!(!filter.is_blocked("2001:db9::1".parse().unwrap()));
    }

    // Test 6: Local network exemption: blocked range doesn't affect RFC 1918/loopback
    #[test]
    fn ip_filter_local_network_exempt() {
        let mut filter = IpFilter::new();
        // Block everything
        filter.add_rule(
            IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
            IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255)),
            1,
        );
        filter.add_rule(
            IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0)),
            IpAddr::V6(Ipv6Addr::new(0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff)),
            1,
        );

        // Local IPs are exempt
        assert!(!filter.is_blocked("127.0.0.1".parse().unwrap()));
        assert!(!filter.is_blocked("192.168.1.1".parse().unwrap()));
        assert!(!filter.is_blocked("10.0.0.1".parse().unwrap()));
        assert!(!filter.is_blocked("172.16.0.1".parse().unwrap()));
        assert!(!filter.is_blocked("::1".parse().unwrap()));

        // But the raw access() still shows blocked
        assert_eq!(filter.access("127.0.0.1".parse().unwrap()), 1);

        // Public IPs are blocked
        assert!(filter.is_blocked("8.8.8.8".parse().unwrap()));
        assert!(filter.is_blocked("2001:db8::1".parse().unwrap()));
    }

    // Test 7: Export: minimized non-overlapping ranges
    #[test]
    fn ip_filter_num_ranges() {
        let mut filter = IpFilter::new();
        assert_eq!(filter.num_ranges(), 0);
        assert!(filter.is_empty());

        filter.add_rule(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 255)),
            1,
        );
        assert_eq!(filter.num_ranges(), 1);
        assert!(!filter.is_empty());

        filter.add_rule(
            IpAddr::V4(Ipv4Addr::new(172, 16, 0, 0)),
            IpAddr::V4(Ipv4Addr::new(172, 16, 255, 255)),
            1,
        );
        assert_eq!(filter.num_ranges(), 2);

        // Adding overlapping range that allows part of first range
        filter.add_rule(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 255)),
            0,
        );
        // First range is now allowed, so num_ranges drops
        assert_eq!(filter.num_ranges(), 1);
    }

    // Test 8: parse_dat: valid lines, comments, malformed line error
    #[test]
    fn parse_dat_valid() {
        let input = "\
# This is a comment
203.0.113.0 - 203.0.113.255 , 128 , Test range
198.51.100.0 - 198.51.100.255 , 1 , Another range
";
        let filter = parse_dat(input).unwrap();
        assert!(filter.is_blocked("203.0.113.50".parse().unwrap()));
        assert!(filter.is_blocked("198.51.100.1".parse().unwrap()));
        assert!(!filter.is_blocked("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn parse_dat_malformed() {
        let input = "this is not a valid line";
        let err = parse_dat(input).unwrap_err();
        assert!(matches!(err, IpFilterError::MalformedLine { line: 1, .. }));
    }

    // Test 9: parse_p2p: valid lines, comments, invalid IP error
    #[test]
    fn parse_p2p_valid() {
        let input = "\
# P2P blocklist
Some Bad Range:203.0.113.0-203.0.113.255
Another Range:198.51.100.0-198.51.100.255
";
        let filter = parse_p2p(input).unwrap();
        assert!(filter.is_blocked("203.0.113.50".parse().unwrap()));
        assert!(filter.is_blocked("198.51.100.1".parse().unwrap()));
        assert!(!filter.is_blocked("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn parse_p2p_invalid_ip() {
        let input = "Bad Range:999.999.999.999-203.0.113.255";
        let err = parse_p2p(input).unwrap_err();
        assert!(matches!(err, IpFilterError::InvalidAddress { line: 1, .. }));
    }

    // Test 10: PortFilter: block port range, verify access
    #[test]
    fn port_filter_block_range() {
        let mut filter = PortFilter::new();
        filter.add_rule(6881, 6889, 1);

        assert!(filter.is_blocked(6881));
        assert!(filter.is_blocked(6885));
        assert!(filter.is_blocked(6889));
        assert!(!filter.is_blocked(6880));
        assert!(!filter.is_blocked(6890));
        assert!(!filter.is_blocked(80));
    }
}
