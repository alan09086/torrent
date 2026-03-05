//! Token bucket rate limiter and local network detection.

use std::net::IpAddr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Token bucket rate limiter.
///
/// Tokens represent bytes. `rate` is bytes/second.
/// Tokens are added via `refill()` (called on a timer).
/// Burst capacity = 1 second of tokens.
#[allow(dead_code)] // consumed by torrent/session modules (wired in later tasks)
pub(crate) struct TokenBucket {
    rate: u64,     // bytes/sec, 0 = unlimited
    tokens: u64,   // current available tokens
    capacity: u64, // max tokens (= rate, i.e., 1 second burst)
}

#[allow(dead_code)]
impl TokenBucket {
    pub fn new(rate: u64) -> Self {
        Self {
            rate,
            tokens: 0,
            capacity: rate, // 1 second burst
        }
    }

    pub fn unlimited() -> Self {
        Self {
            rate: 0,
            tokens: 0,
            capacity: 0,
        }
    }

    pub fn is_unlimited(&self) -> bool {
        self.rate == 0
    }

    /// Current rate limit in bytes/sec (0 = unlimited).
    pub fn rate(&self) -> u64 {
        self.rate
    }

    /// Add tokens proportional to elapsed time.
    pub fn refill(&mut self, elapsed: Duration) {
        if self.rate == 0 {
            return;
        }
        let add = (self.rate as u128 * elapsed.as_millis() / 1000) as u64;
        self.tokens = (self.tokens + add).min(self.capacity);
    }

    /// Try to consume `amount` tokens. Returns true if allowed.
    pub fn try_consume(&mut self, amount: u64) -> bool {
        if self.rate == 0 {
            return true;
        }
        if self.tokens >= amount {
            self.tokens -= amount;
            true
        } else {
            false
        }
    }

    /// How many bytes can be consumed right now.
    pub fn available(&self) -> u64 {
        if self.rate == 0 {
            u64::MAX
        } else {
            self.tokens
        }
    }

    /// Update the rate limit. Resets capacity but preserves current tokens (clamped).
    pub fn set_rate(&mut self, rate: u64) {
        self.rate = rate;
        self.capacity = rate;
        if rate > 0 {
            self.tokens = self.tokens.min(self.capacity);
        }
    }
}

/// Transport type for per-class rate limiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)]
pub(crate) enum PeerTransport {
    Tcp,
    Utp,
}

/// Mixed-mode bandwidth allocation algorithm for TCP/uTP coexistence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MixedModeAlgorithm {
    /// Throttle uTP upload when any TCP peer is connected.
    /// uTP gets at most 10% of the global upload rate when TCP peers are present.
    PreferTcp,
    /// Allocate bandwidth proportional to the number of TCP vs uTP peers.
    PeerProportional,
}

/// Per-class rate limiter set (BEP 40 / libtorrent parity).
///
/// Maintains separate upload/download buckets for TCP and uTP, plus global
/// upload/download buckets. Uses check-before-consume pattern to avoid
/// partial consumption when one bucket has capacity but another doesn't.
#[allow(dead_code)]
pub(crate) struct RateLimiterSet {
    tcp_upload: TokenBucket,
    tcp_download: TokenBucket,
    utp_upload: TokenBucket,
    utp_download: TokenBucket,
    global_upload: TokenBucket,
    global_download: TokenBucket,
}

#[allow(dead_code)]
impl RateLimiterSet {
    /// Create a new rate limiter set. Rate of 0 = unlimited.
    pub fn new(
        tcp_upload_rate: u64,
        tcp_download_rate: u64,
        utp_upload_rate: u64,
        utp_download_rate: u64,
        global_upload_rate: u64,
        global_download_rate: u64,
    ) -> Self {
        Self {
            tcp_upload: TokenBucket::new(tcp_upload_rate),
            tcp_download: TokenBucket::new(tcp_download_rate),
            utp_upload: TokenBucket::new(utp_upload_rate),
            utp_download: TokenBucket::new(utp_download_rate),
            global_upload: TokenBucket::new(global_upload_rate),
            global_download: TokenBucket::new(global_download_rate),
        }
    }

    /// Refill all buckets proportional to elapsed time.
    pub fn refill(&mut self, elapsed: Duration) {
        self.tcp_upload.refill(elapsed);
        self.tcp_download.refill(elapsed);
        self.utp_upload.refill(elapsed);
        self.utp_download.refill(elapsed);
        self.global_upload.refill(elapsed);
        self.global_download.refill(elapsed);
    }

    /// Try to consume upload tokens for the given transport class.
    ///
    /// Checks both the class bucket and global bucket *before* consuming
    /// either, to avoid partial consumption without refund.
    pub fn try_consume_upload(&mut self, amount: u64, transport: PeerTransport) -> bool {
        let class = match transport {
            PeerTransport::Tcp => &self.tcp_upload,
            PeerTransport::Utp => &self.utp_upload,
        };
        // Check both before consuming either
        if !class.is_unlimited() && class.available() < amount {
            return false;
        }
        if !self.global_upload.is_unlimited() && self.global_upload.available() < amount {
            return false;
        }
        // Both have capacity — consume from both
        let class = match transport {
            PeerTransport::Tcp => &mut self.tcp_upload,
            PeerTransport::Utp => &mut self.utp_upload,
        };
        class.try_consume(amount);
        self.global_upload.try_consume(amount);
        true
    }

    /// Try to consume download tokens for the given transport class.
    pub fn try_consume_download(&mut self, amount: u64, transport: PeerTransport) -> bool {
        let class = match transport {
            PeerTransport::Tcp => &self.tcp_download,
            PeerTransport::Utp => &self.utp_download,
        };
        if !class.is_unlimited() && class.available() < amount {
            return false;
        }
        if !self.global_download.is_unlimited() && self.global_download.available() < amount {
            return false;
        }
        let class = match transport {
            PeerTransport::Tcp => &mut self.tcp_download,
            PeerTransport::Utp => &mut self.utp_download,
        };
        class.try_consume(amount);
        self.global_download.try_consume(amount);
        true
    }

    /// Update per-class rates at runtime (e.g., from apply_settings).
    pub fn set_rates(
        &mut self,
        tcp_upload: u64,
        tcp_download: u64,
        utp_upload: u64,
        utp_download: u64,
        global_upload: u64,
        global_download: u64,
    ) {
        self.tcp_upload.set_rate(tcp_upload);
        self.tcp_download.set_rate(tcp_download);
        self.utp_upload.set_rate(utp_upload);
        self.utp_download.set_rate(utp_download);
        self.global_upload.set_rate(global_upload);
        self.global_download.set_rate(global_download);
    }

    /// Apply mixed-mode bandwidth allocation based on peer transport composition.
    /// Only adjusts upload — download is not throttled by transport type.
    pub fn apply_mixed_mode(
        &mut self,
        algorithm: MixedModeAlgorithm,
        tcp_peers: usize,
        utp_peers: usize,
        global_upload_rate: u64,
    ) {
        if global_upload_rate == 0 {
            self.tcp_upload.set_rate(0);
            self.utp_upload.set_rate(0);
            return;
        }
        if tcp_peers == 0 && utp_peers == 0 {
            self.tcp_upload.set_rate(0);
            self.utp_upload.set_rate(0);
            return;
        }
        match algorithm {
            MixedModeAlgorithm::PreferTcp => {
                if tcp_peers > 0 && utp_peers > 0 {
                    let tcp_rate = global_upload_rate * 9 / 10;
                    let utp_rate = global_upload_rate / 10;
                    self.tcp_upload.set_rate(tcp_rate.max(1));
                    self.utp_upload.set_rate(utp_rate.max(1));
                } else {
                    self.tcp_upload.set_rate(0);
                    self.utp_upload.set_rate(0);
                }
            }
            MixedModeAlgorithm::PeerProportional => {
                let total = tcp_peers + utp_peers;
                let tcp_rate = global_upload_rate * tcp_peers as u64 / total as u64;
                let utp_rate = global_upload_rate * utp_peers as u64 / total as u64;
                self.tcp_upload.set_rate(if tcp_peers > 0 { tcp_rate.max(1) } else { 0 });
                self.utp_upload.set_rate(if utp_peers > 0 { utp_rate.max(1) } else { 0 });
            }
        }
    }
}

/// Check if an IP address is on a local/private network.
///
/// IPv4: loopback, private (RFC 1918), link-local (169.254.0.0/16).
/// IPv6: loopback (::1), link-local (fe80::/10), unique-local / ULA (fc00::/7).
#[allow(dead_code)] // consumed by torrent module (wired in later tasks)
pub(crate) fn is_local_network(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        IpAddr::V6(ip) => {
            if ip.is_loopback() {
                return true;
            }
            let octets = ip.octets();
            // fe80::/10 — link-local
            if octets[0] == 0xfe && (octets[1] & 0xc0) == 0x80 {
                return true;
            }
            // fc00::/7 — unique-local (ULA)
            if (octets[0] & 0xfe) == 0xfc {
                return true;
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_bucket_always_allows() {
        let mut tb = TokenBucket::unlimited();
        assert!(tb.try_consume(1_000_000));
        assert!(tb.is_unlimited());
        assert_eq!(tb.available(), u64::MAX);
    }

    #[test]
    fn limited_bucket_allows_up_to_capacity() {
        let mut tb = TokenBucket::new(1000); // 1000 bytes/sec
        tb.refill(Duration::from_millis(100)); // +100 tokens
        assert!(tb.try_consume(100));
        assert!(!tb.try_consume(1)); // exhausted
    }

    #[test]
    fn refill_adds_tokens_proportionally() {
        let mut tb = TokenBucket::new(10_000); // 10 KB/s
        tb.refill(Duration::from_millis(100)); // +1000 tokens
        assert!(tb.try_consume(1000));
        assert!(!tb.try_consume(1));
    }

    #[test]
    fn tokens_cap_at_one_second_burst() {
        let mut tb = TokenBucket::new(1000);
        tb.refill(Duration::from_secs(5)); // would be 5000, but capped at 1000
        assert!(tb.try_consume(1000));
        assert!(!tb.try_consume(1));
    }

    #[test]
    fn try_consume_partial() {
        let mut tb = TokenBucket::new(1000);
        tb.refill(Duration::from_millis(100)); // +100
        assert_eq!(tb.available(), 100);
        assert!(tb.try_consume(50));
        assert_eq!(tb.available(), 50);
    }

    #[test]
    fn set_rate_clamps_tokens() {
        let mut tb = TokenBucket::new(1000);
        tb.refill(Duration::from_secs(1)); // 1000 tokens
        assert_eq!(tb.available(), 1000);
        tb.set_rate(500); // capacity now 500, tokens clamped
        assert_eq!(tb.available(), 500);
    }

    #[test]
    fn local_network_detection() {
        assert!(is_local_network("127.0.0.1".parse().unwrap()));
        assert!(is_local_network("192.168.1.1".parse().unwrap()));
        assert!(is_local_network("10.0.0.1".parse().unwrap()));
        assert!(is_local_network("172.16.0.1".parse().unwrap()));
        assert!(is_local_network("169.254.1.1".parse().unwrap()));
        assert!(is_local_network("::1".parse().unwrap()));
        assert!(!is_local_network("8.8.8.8".parse().unwrap()));
        assert!(!is_local_network("1.2.3.4".parse().unwrap()));
    }

    #[test]
    fn ipv6_local_network_detection() {
        // Loopback
        assert!(is_local_network("::1".parse().unwrap()));
        // Link-local (fe80::/10)
        assert!(is_local_network("fe80::1".parse().unwrap()));
        assert!(is_local_network("fe80::abcd:1234".parse().unwrap()));
        // Unique-local / ULA (fc00::/7)
        assert!(is_local_network("fc00::1".parse().unwrap()));
        assert!(is_local_network("fd00::1".parse().unwrap()));
        assert!(is_local_network("fd12:3456:789a::1".parse().unwrap()));
        // Global unicast — not local
        assert!(!is_local_network("2001:db8::1".parse().unwrap()));
        assert!(!is_local_network("2607:f8b0:4004:800::200e".parse().unwrap()));
    }

    #[test]
    fn rate_limiter_set_all_unlimited() {
        let mut rls = RateLimiterSet::new(0, 0, 0, 0, 0, 0);
        rls.refill(Duration::from_secs(1));
        assert!(rls.try_consume_upload(1_000_000, PeerTransport::Tcp));
        assert!(rls.try_consume_upload(1_000_000, PeerTransport::Utp));
        assert!(rls.try_consume_download(1_000_000, PeerTransport::Tcp));
        assert!(rls.try_consume_download(1_000_000, PeerTransport::Utp));
    }

    #[test]
    fn rate_limiter_set_class_limited() {
        let mut rls = RateLimiterSet::new(1000, 1000, 500, 500, 0, 0);
        rls.refill(Duration::from_secs(1));
        // TCP: 1000 capacity
        assert!(rls.try_consume_upload(1000, PeerTransport::Tcp));
        assert!(!rls.try_consume_upload(1, PeerTransport::Tcp)); // exhausted
        // uTP: 500 capacity, independent
        assert!(rls.try_consume_upload(500, PeerTransport::Utp));
        assert!(!rls.try_consume_upload(1, PeerTransport::Utp));
    }

    #[test]
    fn rate_limiter_set_global_limits() {
        // Global upload limit = 500, class limit = 1000 each
        let mut rls = RateLimiterSet::new(1000, 0, 1000, 0, 500, 0);
        rls.refill(Duration::from_secs(1));
        // TCP class has 1000, but global only has 500
        assert!(rls.try_consume_upload(500, PeerTransport::Tcp));
        // Now global is exhausted — uTP should also be blocked
        assert!(!rls.try_consume_upload(1, PeerTransport::Utp));
    }

    #[test]
    fn rate_limiter_set_check_before_consume_no_partial() {
        // If global allows but class doesn't, no partial consumption
        let mut rls = RateLimiterSet::new(100, 0, 0, 0, 1000, 0);
        rls.refill(Duration::from_secs(1));
        assert!(rls.try_consume_upload(100, PeerTransport::Tcp));
        // Class exhausted, global still has 900 — should fail cleanly
        assert!(!rls.try_consume_upload(1, PeerTransport::Tcp));
        // uTP is unlimited, global has 900
        assert!(rls.try_consume_upload(900, PeerTransport::Utp));
    }

    #[test]
    fn rate_limiter_set_refill_all() {
        let mut rls = RateLimiterSet::new(1000, 2000, 500, 750, 5000, 10000);
        rls.refill(Duration::from_millis(100));
        // Each bucket should have 10% of its rate
        assert!(rls.try_consume_upload(100, PeerTransport::Tcp));
        assert!(rls.try_consume_download(200, PeerTransport::Tcp));
        assert!(rls.try_consume_upload(50, PeerTransport::Utp));
        assert!(rls.try_consume_download(75, PeerTransport::Utp));
    }

    #[test]
    fn rate_limiter_set_runtime_update() {
        let mut rls = RateLimiterSet::new(1000, 1000, 1000, 1000, 0, 0);
        rls.refill(Duration::from_secs(1));
        assert!(rls.try_consume_upload(1000, PeerTransport::Tcp));
        // Update TCP upload to 500
        rls.set_rates(500, 1000, 1000, 1000, 0, 0);
        rls.refill(Duration::from_secs(1));
        assert!(rls.try_consume_upload(500, PeerTransport::Tcp));
        assert!(!rls.try_consume_upload(1, PeerTransport::Tcp));
    }

    #[test]
    fn mixed_mode_prefer_tcp_both_present() {
        let mut rls = RateLimiterSet::new(0, 0, 0, 0, 10000, 0);
        rls.apply_mixed_mode(MixedModeAlgorithm::PreferTcp, 3, 5, 10000);
        rls.refill(Duration::from_secs(1));
        assert!(rls.try_consume_upload(9000, PeerTransport::Tcp));
        assert!(!rls.try_consume_upload(1, PeerTransport::Tcp));
        rls.refill(Duration::from_secs(1));
        assert!(rls.try_consume_upload(1000, PeerTransport::Utp));
        assert!(!rls.try_consume_upload(1, PeerTransport::Utp));
    }

    #[test]
    fn mixed_mode_prefer_tcp_only_utp() {
        // When only uTP peers exist, per-class rate is set to unlimited (0),
        // so uTP can consume up to the full global limit without per-class throttling.
        let mut rls = RateLimiterSet::new(0, 0, 0, 0, 10000, 0);
        rls.apply_mixed_mode(MixedModeAlgorithm::PreferTcp, 0, 5, 10000);
        rls.refill(Duration::from_secs(1));
        // uTP per-class bucket is unlimited, so full global capacity is available
        assert!(rls.try_consume_upload(10000, PeerTransport::Utp));
        assert!(!rls.try_consume_upload(1, PeerTransport::Utp));
    }

    #[test]
    fn mixed_mode_proportional() {
        let mut rls = RateLimiterSet::new(0, 0, 0, 0, 10000, 0);
        rls.apply_mixed_mode(MixedModeAlgorithm::PeerProportional, 3, 7, 10000);
        rls.refill(Duration::from_secs(1));
        assert!(rls.try_consume_upload(3000, PeerTransport::Tcp));
        assert!(!rls.try_consume_upload(1, PeerTransport::Tcp));
        rls.refill(Duration::from_secs(1));
        assert!(rls.try_consume_upload(7000, PeerTransport::Utp));
        assert!(!rls.try_consume_upload(1, PeerTransport::Utp));
    }

    #[test]
    fn mixed_mode_unlimited_global_noop() {
        let mut rls = RateLimiterSet::new(0, 0, 0, 0, 0, 0);
        rls.apply_mixed_mode(MixedModeAlgorithm::PeerProportional, 3, 7, 0);
        rls.refill(Duration::from_secs(1));
        assert!(rls.try_consume_upload(1_000_000, PeerTransport::Tcp));
        assert!(rls.try_consume_upload(1_000_000, PeerTransport::Utp));
    }
}
