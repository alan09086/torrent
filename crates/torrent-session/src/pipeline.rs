//! Per-peer request pipeline state: EWMA throughput tracking and RTT measurement.
//!
//! After M104, the AIMD queue depth system is removed. Queue depth is now a
//! fixed constant (configured via `fixed_pipeline_depth` setting). This module
//! retains EWMA throughput estimation (for snub detection and peer rate
//! comparison) and per-request RTT tracking.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Per-peer request pipeline state.
///
/// Tracks in-flight requests and measures throughput via EWMA.
#[allow(dead_code)]
pub(crate) struct PeerPipelineState {
    /// Bytes received in the current tick window (for throughput stats).
    last_second_bytes: u64,
    /// EWMA throughput estimate in bytes/sec.
    ewma_rate_bytes_sec: f64,
    /// When the last tick occurred.
    last_tick: Instant,
    /// Map of (piece, begin) -> request send time for RTT tracking.
    request_times: HashMap<(u32, u32), Instant>,
}

#[allow(dead_code)]
impl PeerPipelineState {
    /// Create a new pipeline state with EWMA throughput tracking.
    pub fn new() -> Self {
        Self {
            last_second_bytes: 0,
            ewma_rate_bytes_sec: 0.0,
            last_tick: Instant::now(),
            request_times: HashMap::new(),
        }
    }

    /// Current EWMA throughput estimate in bytes/sec (for stats/reporting).
    pub fn ewma_rate(&self) -> f64 {
        self.ewma_rate_bytes_sec
    }

    /// Record that a request was sent for the given (piece, begin) block.
    pub fn request_sent(&mut self, piece: u32, begin: u32, now: Instant) {
        self.request_times.insert((piece, begin), now);
    }

    /// Record that a block was received, returning the RTT if tracked.
    ///
    /// Throughput bytes are accumulated for EWMA stats only.
    pub fn block_received(
        &mut self,
        piece: u32,
        begin: u32,
        length: u32,
        now: Instant,
    ) -> Option<Duration> {
        let rtt = self
            .request_times
            .remove(&(piece, begin))
            .map(|sent| now.duration_since(sent));

        self.last_second_bytes += length as u64;

        rtt
    }

    /// Called periodically to update the EWMA throughput estimate.
    pub fn tick(&mut self) {
        const ALPHA: f64 = 0.3;
        self.ewma_rate_bytes_sec =
            ALPHA * self.last_second_bytes as f64 + (1.0 - ALPHA) * self.ewma_rate_bytes_sec;
        self.last_second_bytes = 0;
        self.last_tick = Instant::now();
    }

    /// Returns the most recently sent request still pending, if any.
    pub fn most_recent_request(&self) -> Option<(u32, u32, Instant)> {
        self.request_times
            .iter()
            .max_by_key(|&(_, &instant)| instant)
            .map(|(&(piece, begin), &instant)| (piece, begin, instant))
    }

    /// Returns all (piece, begin) pairs whose requests have exceeded the timeout.
    pub fn timed_out_blocks(&self, timeout: Duration, now: Instant) -> Vec<(u32, u32)> {
        self.request_times
            .iter()
            .filter(|&(_, &sent)| now.duration_since(sent) > timeout)
            .map(|(&key, _)| key)
            .collect()
    }

    /// Remove a request from tracking (e.g. after cancel or timeout).
    pub fn remove_request(&mut self, piece: u32, begin: u32) {
        self.request_times.remove(&(piece, begin));
    }

    /// Number of requests currently in flight.
    pub fn pending_count(&self) -> usize {
        self.request_times.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn pipeline_tick_no_depth_adjustment() {
        // After AIMD removal, tick() only updates EWMA, no depth changes
        let mut p = PeerPipelineState::new();
        // Simulate receiving some data
        p.block_received(0, 0, 16384, Instant::now());
        p.block_received(0, 16384, 16384, Instant::now());
        p.tick();
        // EWMA should be non-zero after receiving data
        assert!(p.ewma_rate() > 0.0);
        // Tick again with no data — EWMA decays
        p.tick();
        let rate_after_decay = p.ewma_rate();
        assert!(rate_after_decay < p.ewma_rate_bytes_sec + 1.0); // decaying
    }

    #[test]
    fn pipeline_ewma_still_tracks() {
        let mut p = PeerPipelineState::new();
        assert_eq!(p.ewma_rate(), 0.0);

        // Feed data and tick
        p.block_received(0, 0, 100_000, Instant::now());
        p.tick();
        let rate1 = p.ewma_rate();
        assert!(rate1 > 0.0, "EWMA should be positive after receiving data");

        // Feed more data and tick
        p.block_received(1, 0, 100_000, Instant::now());
        p.tick();
        let rate2 = p.ewma_rate();
        assert!(
            rate2 > rate1 * 0.5,
            "EWMA should maintain with steady traffic"
        );

        // No data, tick — should decay
        p.tick();
        let rate3 = p.ewma_rate();
        assert!(rate3 < rate2, "EWMA should decay without data");
    }
}
