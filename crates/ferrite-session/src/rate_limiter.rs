//! Token bucket rate limiter and local network detection.

use std::net::IpAddr;
use std::time::Duration;

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

/// Check if an IP address is on a local/private network (RFC 1918, loopback, link-local).
#[allow(dead_code)] // consumed by torrent module (wired in later tasks)
pub(crate) fn is_local_network(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        IpAddr::V6(ip) => ip.is_loopback(),
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
}
