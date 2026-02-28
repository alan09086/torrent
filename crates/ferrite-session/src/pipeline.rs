//! Per-peer dynamic request queue sizing with slow-start.
//!
//! Each peer connection maintains a `PeerPipelineState` that adaptively sizes
//! its request queue. New connections start in slow-start mode (growing by one
//! per received block), then switch to a steady-state formula based on EWMA
//! throughput once the transfer rate plateaus.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ferrite_core::DEFAULT_CHUNK_SIZE;

/// Per-peer dynamic request queue sizing with slow-start.
///
/// Tracks in-flight requests, measures throughput via EWMA, and adjusts the
/// number of concurrent requests to keep the peer's connection saturated
/// without over-requesting.
pub(crate) struct PeerPipelineState {
    /// Exponentially weighted moving average of bytes/sec throughput.
    ewma_rate_bytes_sec: f64,
    /// Current number of concurrent requests to maintain.
    queue_depth: usize,
    /// Whether we are still in slow-start phase.
    in_slow_start: bool,
    /// Bytes received in the current second.
    last_second_bytes: u64,
    /// Bytes received in the previous second (for plateau detection).
    prev_second_bytes: u64,
    /// When the last tick occurred.
    last_tick: Instant,
    /// Map of (piece, begin) -> request send time for RTT tracking.
    request_times: HashMap<(u32, u32), Instant>,
    /// Hard ceiling on queue depth.
    max_queue_depth: usize,
    /// Target request queue time in seconds (how far ahead to request).
    request_queue_time: f64,
    /// Whether slow-start has ever been exited (prevents re-entry detection issues).
    slow_start_exited: bool,
}

#[allow(dead_code)] // consumed by TorrentActor integration (Task 4)
impl PeerPipelineState {
    /// Create a new pipeline state starting in slow-start with queue depth 2.
    pub fn new(max_queue_depth: usize, request_queue_time: f64) -> Self {
        Self {
            ewma_rate_bytes_sec: 0.0,
            queue_depth: 2,
            in_slow_start: true,
            last_second_bytes: 0,
            prev_second_bytes: 0,
            last_tick: Instant::now(),
            request_times: HashMap::new(),
            max_queue_depth,
            request_queue_time,
            slow_start_exited: false,
        }
    }

    /// Current number of concurrent requests to maintain.
    pub fn queue_depth(&self) -> usize {
        self.queue_depth
    }

    /// Whether we are still in slow-start phase.
    pub fn in_slow_start(&self) -> bool {
        self.in_slow_start
    }

    /// Current EWMA throughput estimate in bytes/sec.
    pub fn ewma_rate(&self) -> f64 {
        self.ewma_rate_bytes_sec
    }

    /// Record that a request was sent for the given (piece, begin) block.
    pub fn request_sent(&mut self, piece: u32, begin: u32, now: Instant) {
        self.request_times.insert((piece, begin), now);
    }

    /// Record that a block was received, returning the RTT if the request was tracked.
    ///
    /// In slow-start, each received block grows the queue depth by one (up to max).
    pub fn block_received(
        &mut self,
        piece: u32,
        begin: u32,
        length: u32,
        now: Instant,
    ) -> Option<Duration> {
        let rtt = self.request_times.remove(&(piece, begin)).map(|sent| now.duration_since(sent));

        if self.in_slow_start {
            self.queue_depth = (self.queue_depth + 1).min(self.max_queue_depth);
        }

        self.last_second_bytes += length as u64;

        rtt
    }

    /// Called once per second to update EWMA and check for slow-start exit.
    pub fn tick(&mut self) {
        const ALPHA: f64 = 0.3;

        // EWMA update
        self.ewma_rate_bytes_sec =
            ALPHA * self.last_second_bytes as f64 + (1.0 - ALPHA) * self.ewma_rate_bytes_sec;

        // Slow-start exit check: throughput plateau (delta < 10 KB/s between seconds)
        if self.in_slow_start {
            let delta =
                (self.last_second_bytes as i64 - self.prev_second_bytes as i64).unsigned_abs();
            if delta < 10_240 && self.prev_second_bytes > 0 {
                self.in_slow_start = false;
                self.slow_start_exited = true;
            }
        }

        // In steady state, recompute queue depth from EWMA
        if !self.in_slow_start && self.slow_start_exited {
            self.recompute_queue_depth();
        }

        // Shift the per-second counters
        self.prev_second_bytes = self.last_second_bytes;
        self.last_second_bytes = 0;
        self.last_tick = Instant::now();
    }

    /// Recompute queue depth from EWMA rate and target queue time.
    ///
    /// `depth = ewma_rate * request_queue_time / chunk_size`, clamped to [2, max].
    fn recompute_queue_depth(&mut self) {
        let depth = (self.ewma_rate_bytes_sec * self.request_queue_time
            / DEFAULT_CHUNK_SIZE as f64) as usize;
        self.queue_depth = depth.clamp(2, self.max_queue_depth);
    }

    /// Override the queue depth directly (e.g. for snubbed peers).
    ///
    /// Exits slow-start and marks it as having been exited.
    pub fn set_queue_depth_override(&mut self, depth: usize) {
        self.queue_depth = depth;
        self.in_slow_start = false;
        self.slow_start_exited = true;
    }

    /// Reset to slow-start state (e.g. after unchoke or reconnect).
    pub fn reset_to_slow_start(&mut self) {
        self.queue_depth = 2;
        self.in_slow_start = true;
        self.slow_start_exited = false;
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
    fn slow_start_grows_and_exits_on_plateau() {
        let mut state = PeerPipelineState::new(250, 3.0);
        assert!(state.in_slow_start());
        assert_eq!(state.queue_depth(), 2);

        let now = Instant::now();

        // Simulate receiving several blocks — each should increment queue_depth by 1
        for i in 0..10 {
            state.request_sent(0, i * DEFAULT_CHUNK_SIZE, now);
            state.block_received(0, i * DEFAULT_CHUNK_SIZE, DEFAULT_CHUNK_SIZE, now);
        }
        // Started at 2, received 10 blocks -> 2 + 10 = 12
        assert_eq!(state.queue_depth(), 12);
        assert!(state.in_slow_start());

        // Simulate two ticks with plateaued throughput (same bytes, delta < 10KB)
        // First tick: set a baseline
        state.last_second_bytes = 100_000;
        state.prev_second_bytes = 0; // first tick, prev is 0 so won't trigger exit yet
        state.tick();
        // After first tick, prev_second_bytes was 0, so the plateau check requires prev > 0.
        // Now prev_second_bytes = 100_000, last_second_bytes = 0 (reset by tick).

        // Second tick: same throughput — delta = 0, should exit slow-start
        state.last_second_bytes = 100_000;
        state.tick();

        assert!(!state.in_slow_start());
    }

    #[test]
    fn steady_state_formula() {
        let mut state = PeerPipelineState::new(250, 3.0);

        // Force out of slow-start
        state.in_slow_start = false;
        state.slow_start_exited = true;

        // Set EWMA to 100 KB/s
        state.ewma_rate_bytes_sec = 100.0 * 1024.0;

        state.recompute_queue_depth();

        // Expected: 100 * 1024 * 3.0 / 16384 = 18.75 -> truncated to 18
        let expected = (100.0 * 1024.0 * 3.0 / DEFAULT_CHUNK_SIZE as f64) as usize;
        assert_eq!(expected, 18);
        assert_eq!(state.queue_depth(), expected);
    }

    #[test]
    fn queue_depth_clamped() {
        // Very low EWMA -> floor of 2
        let mut state = PeerPipelineState::new(250, 3.0);
        state.in_slow_start = false;
        state.slow_start_exited = true;
        state.ewma_rate_bytes_sec = 100.0; // ~100 B/s -> depth ≈ 0 -> clamped to 2
        state.recompute_queue_depth();
        assert_eq!(state.queue_depth(), 2);

        // Very high EWMA -> ceiling of max_queue_depth
        state.ewma_rate_bytes_sec = 10.0 * 1024.0 * 1024.0; // 10 MB/s
        state.recompute_queue_depth();
        assert_eq!(state.queue_depth(), 250);

        // Small max_queue_depth is respected
        let mut state2 = PeerPipelineState::new(5, 3.0);
        state2.in_slow_start = false;
        state2.slow_start_exited = true;
        state2.ewma_rate_bytes_sec = 10.0 * 1024.0 * 1024.0; // 10 MB/s
        state2.recompute_queue_depth();
        assert_eq!(state2.queue_depth(), 5);
    }

    #[test]
    fn block_timeout_and_most_recent_request() {
        let mut state = PeerPipelineState::new(250, 3.0);

        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(100);
        let t2 = t0 + Duration::from_millis(200);

        // Send 3 requests at different times
        state.request_sent(0, 0, t0);
        state.request_sent(1, 0, t1);
        state.request_sent(2, 0, t2);

        // most_recent_request should return the t2 one
        let (piece, begin, instant) = state.most_recent_request().unwrap();
        assert_eq!(piece, 2);
        assert_eq!(begin, 0);
        assert_eq!(instant, t2);

        // At t0+150ms with timeout=100ms -> only request at t0 has timed out
        let check_time = t0 + Duration::from_millis(150);
        let timed_out = state.timed_out_blocks(Duration::from_millis(100), check_time);
        assert_eq!(timed_out.len(), 1);
        assert!(timed_out.contains(&(0, 0)));

        // At t0+300ms with timeout=100ms -> requests at t0 and t1 have timed out
        let check_time2 = t0 + Duration::from_millis(300);
        let mut timed_out2 = state.timed_out_blocks(Duration::from_millis(100), check_time2);
        timed_out2.sort();
        assert_eq!(timed_out2.len(), 2);
        assert!(timed_out2.contains(&(0, 0)));
        assert!(timed_out2.contains(&(1, 0)));
    }
}
