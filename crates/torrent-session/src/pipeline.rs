//! Per-peer request pipeline with fixed permit-based queue depth.
//!
//! Modelled after rqbit's semaphore approach: each peer gets a fixed number of
//! request slots (default 128). Slots are consumed when requests are sent and
//! replenished when blocks arrive. No EWMA formula, no slow-start — the queue
//! depth is constant and immediately responsive.
//!
//! The pipeline tick is retained for throughput statistics and snub detection,
//! but no longer drives depth decisions.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Default per-peer request queue depth (matches rqbit's 128 semaphore permits).
///
/// 128 × 16 KiB = 2 MiB in flight — enough to saturate a 100 Mbps link at
/// 100 ms RTT. See <https://www.desmos.com/calculator/x3szur87ps>.
const DEFAULT_QUEUE_DEPTH: usize = 128;

/// Per-peer request pipeline state.
///
/// Tracks in-flight requests and measures throughput. Queue depth is fixed
/// at construction (like rqbit's semaphore permits) — no adaptive resizing.
pub(crate) struct PeerPipelineState {
    /// Bytes received in the current tick window (for throughput stats).
    last_second_bytes: u64,
    /// EWMA throughput estimate in bytes/sec (for reporting only).
    ewma_rate_bytes_sec: f64,
    /// When the last tick occurred.
    last_tick: Instant,
    /// Map of (piece, begin) -> request send time for RTT tracking.
    request_times: HashMap<(u32, u32), Instant>,
    /// Fixed queue depth (number of concurrent request slots).
    queue_depth: usize,
    /// Hard ceiling on queue depth.
    max_queue_depth: usize,
}

#[allow(dead_code)]
impl PeerPipelineState {
    /// Create a new pipeline state with fixed queue depth.
    ///
    /// `max_queue_depth` is the hard ceiling. `initial_queue_depth` sets the
    /// starting depth (clamped to `[1, max_queue_depth]`).
    pub fn new(
        max_queue_depth: usize,
        _request_queue_time: f64, // kept for API compat, unused
        initial_queue_depth: usize,
    ) -> Self {
        let depth = initial_queue_depth.clamp(1, max_queue_depth);
        Self {
            last_second_bytes: 0,
            ewma_rate_bytes_sec: 0.0,
            last_tick: Instant::now(),
            request_times: HashMap::new(),
            queue_depth: depth,
            max_queue_depth,
        }
    }

    /// Current number of concurrent request slots available.
    pub fn queue_depth(&self) -> usize {
        self.queue_depth
    }

    /// Whether we are in slow-start phase (always false — no slow-start).
    pub fn in_slow_start(&self) -> bool {
        false
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
    /// Unlike the old EWMA model, queue depth is NOT adjusted here — it
    /// stays fixed. Throughput bytes are accumulated for EWMA stats.
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

    /// Called once per second to update throughput stats.
    ///
    /// No longer drives queue depth decisions — kept for EWMA reporting
    /// and snub detection integration.
    pub fn tick(&mut self) {
        const ALPHA: f64 = 0.3;
        self.ewma_rate_bytes_sec =
            ALPHA * self.last_second_bytes as f64 + (1.0 - ALPHA) * self.ewma_rate_bytes_sec;
        self.last_second_bytes = 0;
        self.last_tick = Instant::now();
    }

    /// Override the queue depth directly (e.g. for snubbed peers).
    pub fn set_queue_depth_override(&mut self, depth: usize) {
        self.queue_depth = depth;
    }

    /// Reset to default queue depth (e.g. after un-snub or reconnect).
    pub fn reset_to_slow_start(&mut self) {
        self.queue_depth = DEFAULT_QUEUE_DEPTH.min(self.max_queue_depth);
        self.ewma_rate_bytes_sec = 0.0;
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

    #[test]
    fn fixed_queue_depth() {
        let state = PeerPipelineState::new(250, 3.0, 128);
        assert_eq!(state.queue_depth(), 128);
        assert!(!state.in_slow_start());
    }

    #[test]
    fn block_received_does_not_change_depth() {
        let mut state = PeerPipelineState::new(250, 3.0, 128);
        let now = Instant::now();

        for i in 0..20 {
            state.request_sent(0, i * 16384, now);
            state.block_received(0, i * 16384, 16384, now);
        }
        // Depth stays fixed at 128 — no slow-start growth
        assert_eq!(state.queue_depth(), 128);
    }

    #[test]
    fn tick_updates_ewma_only() {
        let mut state = PeerPipelineState::new(250, 3.0, 128);

        // Simulate 1 MB/s throughput for two ticks
        state.last_second_bytes = 1_048_576;
        state.tick();
        assert!(state.ewma_rate() > 0.0);
        // Depth unchanged
        assert_eq!(state.queue_depth(), 128);

        state.last_second_bytes = 1_048_576;
        state.tick();
        // Still 128 — no formula recomputation
        assert_eq!(state.queue_depth(), 128);
    }

    #[test]
    fn snub_override_and_reset() {
        let mut state = PeerPipelineState::new(250, 3.0, 128);

        // Snub: force depth to 1
        state.set_queue_depth_override(1);
        assert_eq!(state.queue_depth(), 1);

        // Un-snub: reset to default
        state.reset_to_slow_start();
        assert_eq!(state.queue_depth(), 128);
    }

    #[test]
    fn queue_depth_clamped_to_max() {
        let state = PeerPipelineState::new(5, 3.0, 128);
        // initial_queue_depth clamped to max
        assert_eq!(state.queue_depth(), 5);
    }

    #[test]
    fn block_timeout_and_most_recent_request() {
        let mut state = PeerPipelineState::new(250, 3.0, 128);

        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(100);
        let t2 = t0 + Duration::from_millis(200);

        state.request_sent(0, 0, t0);
        state.request_sent(1, 0, t1);
        state.request_sent(2, 0, t2);

        let (piece, begin, instant) = state.most_recent_request().unwrap();
        assert_eq!(piece, 2);
        assert_eq!(begin, 0);
        assert_eq!(instant, t2);

        let check_time = t0 + Duration::from_millis(150);
        let timed_out = state.timed_out_blocks(Duration::from_millis(100), check_time);
        assert_eq!(timed_out.len(), 1);
        assert!(timed_out.contains(&(0, 0)));

        let check_time2 = t0 + Duration::from_millis(300);
        let mut timed_out2 = state.timed_out_blocks(Duration::from_millis(100), check_time2);
        timed_out2.sort();
        assert_eq!(timed_out2.len(), 2);
        assert!(timed_out2.contains(&(0, 0)));
        assert!(timed_out2.contains(&(1, 0)));
    }
}
