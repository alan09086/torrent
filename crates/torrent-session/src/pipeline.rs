//! Per-peer request pipeline with BDP-based adaptive queue depth.
//!
//! Modelled after libtorrent's bandwidth-delay product approach: each peer's
//! queue depth is computed from its EWMA throughput and a configurable request
//! queue time. Fast peers get deeper pipelines; slow peers are clamped to the
//! floor. The formula is:
//!
//! ```text
//! queue_depth = (ewma_rate_bytes_sec × request_queue_time / BLOCK_SIZE)
//!               .clamp(QUEUE_DEPTH_FLOOR, max_queue_depth)
//! ```
//!
//! Depth is recomputed on each tick (once per second). Snub overrides set a
//! `depth_overridden` flag that prevents tick from changing the depth until
//! `reset_to_slow_start()` clears it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Minimum pipeline depth for any peer (prevents starvation).
const QUEUE_DEPTH_FLOOR: usize = 16;

/// Standard BitTorrent block size (16 KiB).
const BLOCK_SIZE: f64 = 16384.0;

/// Per-peer request pipeline state.
///
/// Tracks in-flight requests and measures throughput. Queue depth is
/// adaptively computed from the bandwidth-delay product on each tick.
pub(crate) struct PeerPipelineState {
    /// Bytes received in the current tick window (for throughput stats).
    last_second_bytes: u64,
    /// EWMA throughput estimate in bytes/sec.
    ewma_rate_bytes_sec: f64,
    /// When the last tick occurred.
    last_tick: Instant,
    /// Map of (piece, begin) -> request send time for RTT tracking.
    request_times: HashMap<(u32, u32), Instant>,
    /// Current queue depth (number of concurrent request slots).
    queue_depth: usize,
    /// Hard ceiling on queue depth.
    max_queue_depth: usize,
    /// Request queue time in seconds (BDP formula parameter).
    request_queue_time: f64,
    /// Initial queue depth for reset_to_slow_start().
    initial_queue_depth: usize,
    /// Whether depth was explicitly overridden (e.g. snub). Prevents BDP tick
    /// from overwriting the override until `reset_to_slow_start()` clears it.
    depth_overridden: bool,
}

#[allow(dead_code)]
impl PeerPipelineState {
    /// Create a new pipeline state with adaptive queue depth.
    ///
    /// `max_queue_depth` is the hard ceiling. `request_queue_time` is the BDP
    /// time parameter in seconds. `initial_queue_depth` sets the starting
    /// depth (clamped to `[1, max_queue_depth]`).
    pub fn new(
        max_queue_depth: usize,
        request_queue_time: f64,
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
            request_queue_time,
            initial_queue_depth: depth,
            depth_overridden: false,
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
    /// Queue depth is NOT adjusted here — it is recomputed on tick via the
    /// BDP formula. Throughput bytes are accumulated for EWMA stats.
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

    /// Called once per second to update throughput stats and recompute queue depth.
    ///
    /// Uses the BDP formula to adaptively size the queue. If `depth_overridden`
    /// is set (e.g. by snub), the depth is left unchanged.
    pub fn tick(&mut self) {
        const ALPHA: f64 = 0.3;
        self.ewma_rate_bytes_sec =
            ALPHA * self.last_second_bytes as f64 + (1.0 - ALPHA) * self.ewma_rate_bytes_sec;
        self.last_second_bytes = 0;
        self.last_tick = Instant::now();

        // BDP-based adaptive queue depth (libtorrent model)
        if !self.depth_overridden {
            let bdp = (self.ewma_rate_bytes_sec * self.request_queue_time / BLOCK_SIZE) as usize;
            self.queue_depth = bdp.clamp(QUEUE_DEPTH_FLOOR, self.max_queue_depth);
        }
    }

    /// Override the queue depth directly (e.g. for snubbed peers).
    ///
    /// Sets `depth_overridden` to prevent `tick()` from overwriting the value.
    pub fn set_queue_depth_override(&mut self, depth: usize) {
        self.queue_depth = depth;
        self.depth_overridden = true;
    }

    /// Reset to initial queue depth (e.g. after un-snub or reconnect).
    ///
    /// Clears the `depth_overridden` flag so tick() resumes BDP computation.
    pub fn reset_to_slow_start(&mut self) {
        self.queue_depth = self.initial_queue_depth;
        self.ewma_rate_bytes_sec = 0.0;
        self.depth_overridden = false;
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
    fn initial_queue_depth() {
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
        // Depth only changes on tick, not on block_received
        assert_eq!(state.queue_depth(), 128);
    }

    #[test]
    fn tick_updates_ewma_and_depth() {
        let mut state = PeerPipelineState::new(250, 3.0, 128);

        // Simulate 1 MB/s throughput
        state.last_second_bytes = 1_048_576;
        state.tick();
        assert!(state.ewma_rate() > 0.0);
        // After first tick: EWMA = 0.3 * 1_048_576 = 314_572.8
        // BDP = 314_572.8 * 3.0 / 16384 = 57.6 → 57, clamped to [16, 250] = 57
        assert_eq!(state.queue_depth(), 57);

        state.last_second_bytes = 1_048_576;
        state.tick();
        // EWMA = 0.3 * 1_048_576 + 0.7 * 314_572.8 = 534_773.76
        // BDP = 534_773.76 * 3.0 / 16384 = 97.9 → 97
        assert_eq!(state.queue_depth(), 97);
    }

    #[test]
    fn snub_override_and_reset() {
        let mut state = PeerPipelineState::new(250, 3.0, 128);

        // Snub: force depth to 1
        state.set_queue_depth_override(1);
        assert_eq!(state.queue_depth(), 1);
        assert!(state.depth_overridden);

        // Un-snub: reset to initial depth
        state.reset_to_slow_start();
        assert_eq!(state.queue_depth(), 128);
        assert!(!state.depth_overridden);
    }

    #[test]
    fn queue_depth_clamped_to_max() {
        let state = PeerPipelineState::new(5, 3.0, 128);
        // initial_queue_depth clamped to max
        assert_eq!(state.queue_depth(), 5);
    }

    #[test]
    fn adaptive_depth_from_bdp() {
        // Steady-state 1 MB/s: set both EWMA and last_second_bytes so EWMA
        // stays at 1_000_000 after tick (0.3 * 1M + 0.7 * 1M = 1M).
        // BDP = 1_000_000 * 3.0 / 16384 = 183.10 → 183
        let mut state = PeerPipelineState::new(250, 3.0, 128);
        state.ewma_rate_bytes_sec = 1_000_000.0;
        state.last_second_bytes = 1_000_000;
        state.tick();
        assert_eq!(state.queue_depth(), 183);
    }

    #[test]
    fn adaptive_depth_floor() {
        // Very slow peer (100 bytes/s) → depth clamped to QUEUE_DEPTH_FLOOR (16)
        let mut state = PeerPipelineState::new(250, 3.0, 128);
        state.last_second_bytes = 100;
        state.tick();
        // EWMA = 0.3 * 100 = 30
        // BDP = 30 * 3.0 / 16384 = 0.0054 → 0, clamped to 16
        assert_eq!(state.queue_depth(), QUEUE_DEPTH_FLOOR);
    }

    #[test]
    fn adaptive_depth_ceiling() {
        // Very fast peer (100 MB/s) → depth clamped to max_queue_depth (250)
        let mut state = PeerPipelineState::new(250, 3.0, 128);
        state.ewma_rate_bytes_sec = 100_000_000.0;
        state.last_second_bytes = 100_000_000;
        state.tick();
        // EWMA = 0.3 * 100M + 0.7 * 100M = 100M
        // BDP = 100_000_000 * 3.0 / 16384 = 18310 → clamped to 250
        assert_eq!(state.queue_depth(), 250);
    }

    #[test]
    fn snub_prevents_bdp_override() {
        // After snub, tick() does not change depth from 1
        let mut state = PeerPipelineState::new(250, 3.0, 128);
        state.set_queue_depth_override(1);
        assert_eq!(state.queue_depth(), 1);

        // Even with high throughput, tick should not change depth
        state.last_second_bytes = 10_000_000;
        state.tick();
        assert_eq!(state.queue_depth(), 1);

        // Another tick still locked
        state.last_second_bytes = 10_000_000;
        state.tick();
        assert_eq!(state.queue_depth(), 1);
    }

    #[test]
    fn zero_rate_clamps_to_floor() {
        // EWMA = 0 → depth = QUEUE_DEPTH_FLOOR (16)
        let mut state = PeerPipelineState::new(250, 3.0, 128);
        state.ewma_rate_bytes_sec = 0.0;
        state.last_second_bytes = 0;
        state.tick();
        assert_eq!(state.queue_depth(), QUEUE_DEPTH_FLOOR);
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
