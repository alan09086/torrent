//! Per-peer request pipeline with AIMD (Additive Increase, Multiplicative
//! Decrease) queue depth control.
//!
//! Each peer starts in **slow-start** mode where queue depth doubles each tick
//! when throughput is increasing. Once throughput first decreases, the peer
//! exits slow-start and enters steady-state AIMD:
//!
//! - **Additive increase**: depth grows by +4 blocks per tick while throughput
//!   increases (>1% EWMA growth).
//! - **Multiplicative decrease**: after 3 consecutive ticks of throughput
//!   decline (>5% EWMA drop), depth is halved (floor 8).
//!
//! A 4-tick cooldown follows every depth change to let the new depth stabilise
//! before the next adjustment.
//!
//! Snub overrides set a `depth_overridden` flag that reduces depth until
//! `reset_to_slow_start()` restores it to `initial_queue_depth` and
//! re-enters slow-start.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Per-peer request pipeline state.
///
/// Tracks in-flight requests and measures throughput. Queue depth is adjusted
/// each tick via AIMD (slow-start doubling, then +4 additive increase / halve
/// on 3 consecutive decreases).
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
    /// Current queue depth (number of concurrent request slots).
    queue_depth: usize,
    /// Hard ceiling on queue depth.
    max_queue_depth: usize,
    /// Request queue time in seconds (kept for API compatibility, unused in fixed-depth model).
    request_queue_time: f64,
    /// Initial queue depth for reset_to_slow_start().
    initial_queue_depth: usize,
    /// Whether depth was explicitly overridden (e.g. snub). Prevents tick
    /// from overwriting the override until `reset_to_slow_start()` clears it.
    depth_overridden: bool,
    /// Whether we are in slow-start phase (multiplicative increase: double).
    in_slow_start: bool,
    /// Count of consecutive ticks where throughput decreased.
    consecutive_decrease: u8,
    /// Ticks remaining before next AIMD adjustment is allowed.
    cooldown_ticks: u8,
    /// Previous tick's EWMA rate, for detecting increase/decrease.
    prev_ewma: f64,
}

#[allow(dead_code)]
impl PeerPipelineState {
    /// Create a new pipeline state with AIMD queue depth control.
    ///
    /// `max_queue_depth` is the hard ceiling. `request_queue_time` is retained
    /// for API compatibility but unused. `initial_queue_depth` sets the
    /// starting depth (clamped to `[1, max_queue_depth]`). The peer starts in
    /// slow-start mode.
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
            in_slow_start: true,
            consecutive_decrease: 0,
            cooldown_ticks: 0,
            prev_ewma: 0.0,
        }
    }

    /// Current number of concurrent request slots available.
    pub fn queue_depth(&self) -> usize {
        self.queue_depth
    }

    /// Whether we are in slow-start phase (multiplicative increase).
    pub fn in_slow_start(&self) -> bool {
        self.in_slow_start
    }

    /// Current target depth (alias for `queue_depth`).
    pub fn target_depth(&self) -> usize {
        self.queue_depth
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
    /// Queue depth is NOT adjusted here. Throughput bytes are accumulated for
    /// EWMA stats only.
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

    /// Called periodically to update throughput stats and adjust queue depth
    /// via AIMD.
    ///
    /// Updates the EWMA rate, then adjusts depth:
    /// - Slow-start: doubles depth on throughput increase.
    /// - Steady-state: +4 blocks on increase, halve after 3 consecutive
    ///   decreases.
    /// - 4-tick cooldown after every depth change.
    /// - Skipped entirely when `depth_overridden` (snub).
    pub fn tick(&mut self) {
        const ALPHA: f64 = 0.3;
        self.ewma_rate_bytes_sec =
            ALPHA * self.last_second_bytes as f64 + (1.0 - ALPHA) * self.ewma_rate_bytes_sec;
        self.last_second_bytes = 0;
        self.last_tick = Instant::now();

        if self.depth_overridden {
            self.prev_ewma = self.ewma_rate_bytes_sec;
            return; // Snub override — skip AIMD
        }
        if self.cooldown_ticks > 0 {
            self.cooldown_ticks -= 1;
            self.prev_ewma = self.ewma_rate_bytes_sec;
            return; // Cooling down — skip adjustment
        }

        let increased = self.ewma_rate_bytes_sec > self.prev_ewma * 1.01; // 1% threshold
        let decreased = self.ewma_rate_bytes_sec < self.prev_ewma * 0.95; // 5% threshold

        if increased {
            self.consecutive_decrease = 0;
            if self.in_slow_start {
                // Multiplicative increase: double
                self.queue_depth = (self.queue_depth * 2).min(self.max_queue_depth);
            } else {
                // Additive increase: +4 blocks
                self.queue_depth = (self.queue_depth + 4).min(self.max_queue_depth);
            }
            self.cooldown_ticks = 4;
        } else if decreased {
            self.consecutive_decrease += 1;
            self.in_slow_start = false; // Exit slow-start on first decrease
            if self.consecutive_decrease >= 3 {
                // Multiplicative decrease: halve
                self.queue_depth = (self.queue_depth / 2).max(8);
                self.consecutive_decrease = 0;
                self.cooldown_ticks = 4;
            }
        }

        self.prev_ewma = self.ewma_rate_bytes_sec;
    }

    /// Override the queue depth directly (e.g. for snubbed peers).
    ///
    /// Sets `depth_overridden` to prevent `tick()` from overwriting the value.
    pub fn set_queue_depth_override(&mut self, depth: usize) {
        self.queue_depth = depth;
        self.depth_overridden = true;
    }

    /// Reset to initial queue depth and re-enter slow-start.
    ///
    /// Clears the `depth_overridden` flag and resets all AIMD state so the
    /// peer ramps up from `initial_queue_depth` again.
    pub fn reset_to_slow_start(&mut self) {
        self.queue_depth = self.initial_queue_depth;
        self.ewma_rate_bytes_sec = 0.0;
        self.depth_overridden = false;
        self.in_slow_start = true;
        self.consecutive_decrease = 0;
        self.cooldown_ticks = 0;
        self.prev_ewma = 0.0;
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

    /// Helper: drain N cooldown ticks (zero throughput, no AIMD adjustment).
    fn drain_cooldown(state: &mut PeerPipelineState, n: usize) {
        for _ in 0..n {
            state.last_second_bytes = 0;
            state.tick();
        }
    }

    #[test]
    fn initial_queue_depth() {
        let state = PeerPipelineState::new(250, 3.0, 128);
        assert_eq!(state.queue_depth(), 128);
        assert_eq!(state.target_depth(), 128);
        // Starts in slow-start
        assert!(state.in_slow_start());
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
        // With AIMD, first throughput tick from 0 causes slow-start doubling
        let mut state = PeerPipelineState::new(500, 3.0, 128);

        // Simulate 1 MB/s throughput — first tick from prev_ewma=0 → increase
        state.last_second_bytes = 1_048_576;
        state.tick();
        assert!(state.ewma_rate() > 0.0);
        // Slow-start doubles: 128 → 256
        assert_eq!(state.queue_depth(), 256);

        // Drain 4-tick cooldown
        drain_cooldown(&mut state, 4);

        // Another increase tick — doubles again: 256 → 500 (clamped to max)
        state.last_second_bytes = 2_000_000;
        state.tick();
        assert_eq!(state.queue_depth(), 500);
    }

    #[test]
    fn snub_override_and_reset() {
        let mut state = PeerPipelineState::new(250, 3.0, 128);

        // Snub: force depth to 1
        state.set_queue_depth_override(1);
        assert_eq!(state.queue_depth(), 1);
        assert!(state.depth_overridden);

        // Un-snub: reset to initial depth and re-enter slow-start
        state.reset_to_slow_start();
        assert_eq!(state.queue_depth(), 128);
        assert!(!state.depth_overridden);
        assert!(state.in_slow_start());
    }

    #[test]
    fn queue_depth_clamped_to_max() {
        let state = PeerPipelineState::new(5, 3.0, 128);
        // initial_queue_depth clamped to max
        assert_eq!(state.queue_depth(), 5);
    }

    #[test]
    fn snub_override_survives_tick() {
        // After snub, tick() skips AIMD — depth stays at override value
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
    fn zero_rate_keeps_depth() {
        // AIMD with zero rate: prev_ewma=0, new ewma=0 → neither increased
        // nor decreased (0 is not > 0*1.01 and 0 is not < 0*0.95), so depth
        // stays unchanged.
        let mut state = PeerPipelineState::new(250, 3.0, 128);
        state.ewma_rate_bytes_sec = 0.0;
        state.last_second_bytes = 0;
        state.tick();
        assert_eq!(state.queue_depth(), 128);
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

    // --- AIMD-specific tests ---

    #[test]
    fn aimd_slow_start_doubles() {
        // In slow-start, each tick with increasing throughput doubles depth
        let mut state = PeerPipelineState::new(2048, 3.0, 16);
        assert!(state.in_slow_start());
        assert_eq!(state.queue_depth(), 16);

        // Tick 1: throughput increase → double 16 → 32
        state.last_second_bytes = 100_000;
        state.tick();
        assert_eq!(state.queue_depth(), 32);
        assert!(state.in_slow_start());

        // Drain cooldown
        drain_cooldown(&mut state, 4);

        // Tick 2: throughput increase → double 32 → 64
        state.last_second_bytes = 500_000;
        state.tick();
        assert_eq!(state.queue_depth(), 64);
        assert!(state.in_slow_start());

        // Drain cooldown
        drain_cooldown(&mut state, 4);

        // Tick 3: throughput increase → double 64 → 128
        state.last_second_bytes = 2_000_000;
        state.tick();
        assert_eq!(state.queue_depth(), 128);
        assert!(state.in_slow_start());
    }

    #[test]
    fn aimd_exits_slow_start_on_decrease() {
        let mut state = PeerPipelineState::new(2048, 3.0, 16);
        assert!(state.in_slow_start());

        // First tick: increase → double to 32
        state.last_second_bytes = 1_000_000;
        state.tick();
        assert_eq!(state.queue_depth(), 32);
        assert!(state.in_slow_start());

        // Drain cooldown
        drain_cooldown(&mut state, 4);

        // Now simulate a throughput decrease (need prev_ewma to be high enough
        // that the new EWMA is < prev * 0.95)
        // After cooldown, prev_ewma is very low (decayed). We need to set up
        // a clear decrease: first establish a high prev_ewma, then drop.
        state.prev_ewma = 1_000_000.0;
        state.ewma_rate_bytes_sec = 1_000_000.0;
        state.last_second_bytes = 0; // This will drop EWMA: 0.3*0 + 0.7*1M = 700k < 950k
        state.tick();
        assert!(!state.in_slow_start(), "should exit slow-start on decrease");
    }

    #[test]
    fn aimd_additive_increase() {
        // In steady-state (not slow-start), increase is +4 blocks per tick
        let mut state = PeerPipelineState::new(2048, 3.0, 100);
        state.in_slow_start = false; // Force out of slow-start

        // Set up an increasing throughput scenario
        state.prev_ewma = 100_000.0;
        state.ewma_rate_bytes_sec = 100_000.0;
        state.last_second_bytes = 500_000; // Will increase EWMA
        state.tick();
        // Additive: 100 + 4 = 104
        assert_eq!(state.queue_depth(), 104);

        // Drain cooldown
        drain_cooldown(&mut state, 4);

        // Another additive increase
        state.last_second_bytes = 1_000_000;
        state.tick();
        // 104 + 4 = 108
        assert_eq!(state.queue_depth(), 108);
    }

    #[test]
    fn aimd_multiplicative_decrease_after_3() {
        // After 3 consecutive decreases, depth is halved
        let mut state = PeerPipelineState::new(2048, 3.0, 128);
        state.in_slow_start = false;

        // Set up high baseline
        state.ewma_rate_bytes_sec = 1_000_000.0;
        state.prev_ewma = 1_000_000.0;

        // Decrease 1: EWMA drops > 5%
        state.last_second_bytes = 0; // EWMA: 0.3*0 + 0.7*1M = 700k < 950k
        state.tick();
        assert_eq!(state.queue_depth(), 128); // Not halved yet
        assert_eq!(state.consecutive_decrease, 1);

        // Decrease 2
        state.last_second_bytes = 0; // EWMA drops further
        state.tick();
        assert_eq!(state.queue_depth(), 128);
        assert_eq!(state.consecutive_decrease, 2);

        // Decrease 3: halve
        state.last_second_bytes = 0;
        state.tick();
        assert_eq!(state.queue_depth(), 64); // 128 / 2
        assert_eq!(state.consecutive_decrease, 0); // Reset after halving
    }

    #[test]
    fn aimd_cooldown_skips_adjustment() {
        let mut state = PeerPipelineState::new(2048, 3.0, 16);

        // Trigger slow-start increase → sets cooldown_ticks = 4
        state.last_second_bytes = 1_000_000;
        state.tick();
        assert_eq!(state.queue_depth(), 32); // Doubled
        assert_eq!(state.cooldown_ticks, 4);

        // Next 4 ticks with high throughput should NOT change depth (cooldown)
        for i in 0..4 {
            state.last_second_bytes = 2_000_000;
            state.tick();
            assert_eq!(
                state.queue_depth(),
                32,
                "depth should not change during cooldown tick {}",
                i
            );
        }
        assert_eq!(state.cooldown_ticks, 0);

        // Now a tick with increase should adjust depth again
        state.last_second_bytes = 5_000_000;
        state.tick();
        assert_eq!(state.queue_depth(), 64); // Still in slow-start, doubles
    }

    #[test]
    fn aimd_respects_floor() {
        // Depth should never go below 8 on multiplicative decrease
        let mut state = PeerPipelineState::new(2048, 3.0, 12);
        state.in_slow_start = false;

        // Set up for 3 consecutive decreases
        state.ewma_rate_bytes_sec = 1_000_000.0;
        state.prev_ewma = 1_000_000.0;

        // 3 decreases → halve: 12/2 = 6, but floor is 8
        for _ in 0..3 {
            state.last_second_bytes = 0;
            state.tick();
        }
        assert_eq!(state.queue_depth(), 8); // Clamped to floor
    }

    #[test]
    fn aimd_snub_override_skips_aimd() {
        // When depth_overridden, tick() skips all AIMD logic
        let mut state = PeerPipelineState::new(250, 3.0, 128);
        state.set_queue_depth_override(2);

        // Tick with increasing throughput — AIMD skipped
        state.last_second_bytes = 10_000_000;
        state.tick();
        assert_eq!(state.queue_depth(), 2);

        // Multiple ticks — still skipped
        for _ in 0..10 {
            state.last_second_bytes = 10_000_000;
            state.tick();
            assert_eq!(state.queue_depth(), 2);
        }
    }

    #[test]
    fn reset_to_slow_start_re_enters() {
        let mut state = PeerPipelineState::new(2048, 3.0, 64);

        // Get out of slow-start
        state.in_slow_start = false;
        state.consecutive_decrease = 2;
        state.cooldown_ticks = 3;
        state.queue_depth = 200;
        state.prev_ewma = 500_000.0;
        state.ewma_rate_bytes_sec = 400_000.0;
        state.depth_overridden = true;

        state.reset_to_slow_start();

        assert_eq!(state.queue_depth(), 64); // Back to initial
        assert!(state.in_slow_start()); // Re-entered slow-start
        assert_eq!(state.consecutive_decrease, 0);
        assert_eq!(state.cooldown_ticks, 0);
        assert_eq!(state.prev_ewma, 0.0);
        assert_eq!(state.ewma_rate_bytes_sec, 0.0);
        assert!(!state.depth_overridden);

        // And it works: increasing throughput doubles
        state.last_second_bytes = 1_000_000;
        state.tick();
        assert_eq!(state.queue_depth(), 128); // 64 * 2
        assert!(state.in_slow_start());
    }
}
