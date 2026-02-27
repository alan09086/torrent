use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// LEDBAT target delay in microseconds (100ms).
const CCONTROL_TARGET: u32 = 100_000;

/// Maximum congestion window increase per RTT (bytes).
const MAX_CWND_INCREASE: u32 = 3000;

/// Minimum congestion window (bytes). Must be at least one packet.
const MIN_WINDOW: u32 = 1500;

/// Base delay history window (2 minutes).
const BASE_DELAY_WINDOW: Duration = Duration::from_secs(120);

/// Initial retransmission timeout (ms).
const INITIAL_RTO_MS: u64 = 1000;

/// Minimum retransmission timeout (ms).
const MIN_RTO_MS: u64 = 500;

/// LEDBAT congestion controller for uTP.
///
/// Implements the Low Extra Delay Background Transport algorithm:
/// yields to competing flows by targeting a fixed queuing delay.
pub struct LedbatController {
    /// Rolling minimum base delay samples (timestamp, delay_us).
    base_delay_history: VecDeque<(Instant, u32)>,
    /// Current congestion window (bytes).
    cwnd: u32,
    /// Maximum allowed window (bytes).
    max_window: u32,
    /// Bytes currently in flight (sent but not yet ACKed).
    bytes_in_flight: u32,
    /// Smoothed round-trip time (microseconds).
    srtt: u32,
    /// RTT variance (microseconds).
    rtt_var: u32,
    /// Retransmission timeout (milliseconds).
    rto: u64,
    /// Whether we have at least one RTT sample.
    has_rtt_sample: bool,
}

impl Default for LedbatController {
    fn default() -> Self {
        Self::new()
    }
}

impl LedbatController {
    /// Create a new controller with default initial values.
    pub fn new() -> Self {
        Self {
            base_delay_history: VecDeque::new(),
            cwnd: MIN_WINDOW,
            max_window: MIN_WINDOW,
            bytes_in_flight: 0,
            srtt: 0,
            rtt_var: 0,
            rto: INITIAL_RTO_MS,
            has_rtt_sample: false,
        }
    }

    /// Returns the current congestion window.
    pub fn cwnd(&self) -> u32 {
        self.cwnd
    }

    /// Returns the current RTO in milliseconds.
    pub fn rto_ms(&self) -> u64 {
        self.rto
    }

    /// Returns the smoothed RTT in microseconds.
    pub fn srtt_us(&self) -> u32 {
        self.srtt
    }

    /// How many bytes we can still send.
    pub fn available_window(&self) -> u32 {
        self.cwnd.saturating_sub(self.bytes_in_flight)
    }

    /// Record that `n` bytes were sent (added to in-flight).
    pub fn on_send(&mut self, n: u32) {
        self.bytes_in_flight += n;
    }

    /// Record that `n` bytes were ACKed (removed from in-flight).
    pub fn on_bytes_acked(&mut self, n: u32) {
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(n);
    }

    /// Process an ACK with a measured one-way delay.
    ///
    /// `delay_us` is the measured queuing delay from timestamp_difference.
    /// `bytes_acked` is the number of bytes acknowledged.
    pub fn on_ack(&mut self, delay_us: u32, bytes_acked: u32, now: Instant) {
        // Update base delay history
        self.add_base_delay_sample(delay_us, now);
        self.on_bytes_acked(bytes_acked);

        let base_delay = self.base_delay();
        if base_delay == 0 {
            return;
        }

        // Our delay = measured - base (the queuing component)
        let our_delay = delay_us.saturating_sub(base_delay);
        let off_target = CCONTROL_TARGET as i64 - our_delay as i64;

        // LEDBAT window adjustment
        // cwnd += MAX_CWND_INCREASE * (off_target / target) * (bytes_acked / cwnd)
        if self.cwnd > 0 {
            let window_factor = bytes_acked as i64 * 1_000 / self.cwnd as i64;
            let delay_factor = off_target * 1_000 / CCONTROL_TARGET as i64;
            let scaled_gain = MAX_CWND_INCREASE as i64 * delay_factor * window_factor / 1_000_000;

            let new_cwnd = (self.cwnd as i64 + scaled_gain).max(MIN_WINDOW as i64) as u32;
            self.cwnd = new_cwnd;
            self.max_window = new_cwnd;
        }
    }

    /// Handle a packet loss event — halve the window.
    pub fn on_loss(&mut self) {
        self.max_window = (self.max_window / 2).max(MIN_WINDOW);
        self.cwnd = self.max_window;
    }

    /// Handle a timeout — exponential backoff on RTO.
    pub fn on_timeout(&mut self) {
        self.rto = self.rto.saturating_mul(2);
        self.on_loss();
    }

    /// Update RTT estimate using Jacobson's algorithm.
    ///
    /// `rtt_us` is the measured round-trip time in microseconds.
    pub fn update_rtt(&mut self, rtt_us: u32) {
        if !self.has_rtt_sample {
            // First sample
            self.srtt = rtt_us;
            self.rtt_var = rtt_us / 2;
            self.has_rtt_sample = true;
        } else {
            // Jacobson EWMA
            let delta = rtt_us.abs_diff(self.srtt);
            self.rtt_var = (3 * self.rtt_var + delta) / 4;
            self.srtt = (7 * self.srtt + rtt_us) / 8;
        }

        // RTO = srtt + 4 * rtt_var (in microseconds, convert to ms)
        let rto_us = self.srtt as u64 + 4 * self.rtt_var as u64;
        let rto_ms = rto_us / 1000;
        self.rto = rto_ms.max(MIN_RTO_MS);
    }

    /// Add a delay sample to base delay history.
    fn add_base_delay_sample(&mut self, delay_us: u32, now: Instant) {
        // Remove old samples outside the window
        while let Some(&(t, _)) = self.base_delay_history.front() {
            if now.duration_since(t) > BASE_DELAY_WINDOW {
                self.base_delay_history.pop_front();
            } else {
                break;
            }
        }
        self.base_delay_history.push_back((now, delay_us));
    }

    /// Get the minimum base delay from history.
    fn base_delay(&self) -> u32 {
        self.base_delay_history
            .iter()
            .map(|&(_, d)| d)
            .min()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledbat_window_increases_below_target() {
        let mut cc = LedbatController::new();
        cc.cwnd = 5000;
        cc.max_window = 5000;
        let now = Instant::now();

        // Base delay = 10ms, measured delay = 20ms → queuing = 10ms
        // off_target = 100ms - 10ms = 90ms → positive → window should grow
        cc.on_send(1000);
        cc.on_ack(10_000, 1000, now); // base delay sample
        let cwnd_after_base = cc.cwnd;

        cc.on_send(1000);
        cc.on_ack(20_000, 1000, now); // queuing delay = 10ms, well below 100ms target

        assert!(
            cc.cwnd > cwnd_after_base,
            "cwnd should increase when delay < target: {} <= {}",
            cc.cwnd,
            cwnd_after_base
        );
    }

    #[test]
    fn ledbat_window_decreases_above_target() {
        let mut cc = LedbatController::new();
        cc.cwnd = 10_000;
        cc.max_window = 10_000;
        let now = Instant::now();

        // Establish base delay
        cc.on_send(1000);
        cc.on_ack(10_000, 1000, now);
        let cwnd_after_base = cc.cwnd;

        // Measured delay = 210ms, base = 10ms → queuing = 200ms > 100ms target
        // off_target = 100ms - 200ms = -100ms → negative → window should shrink
        cc.cwnd = 10_000;
        cc.max_window = 10_000;
        cc.on_send(1000);
        cc.on_ack(210_000, 1000, now);

        assert!(
            cc.cwnd < 10_000,
            "cwnd should decrease when delay > target: {} >= 10000",
            cc.cwnd,
        );
        let _ = cwnd_after_base;
    }

    #[test]
    fn ledbat_halves_on_loss() {
        let mut cc = LedbatController::new();
        cc.cwnd = 10_000;
        cc.max_window = 10_000;

        cc.on_loss();
        assert_eq!(cc.cwnd, 5000);
        assert_eq!(cc.max_window, 5000);

        cc.on_loss();
        assert_eq!(cc.cwnd, 2500);

        // Respect minimum
        cc.cwnd = MIN_WINDOW;
        cc.max_window = MIN_WINDOW;
        cc.on_loss();
        assert_eq!(cc.cwnd, MIN_WINDOW);
    }

    #[test]
    fn rtt_estimation() {
        let mut cc = LedbatController::new();

        // First sample: 50ms
        cc.update_rtt(50_000);
        assert_eq!(cc.srtt, 50_000);
        assert_eq!(cc.rtt_var, 25_000);

        // Consistent samples should converge
        for _ in 0..20 {
            cc.update_rtt(50_000);
        }

        // After many consistent samples, srtt should be very close to 50ms
        let diff = if cc.srtt > 50_000 {
            cc.srtt - 50_000
        } else {
            50_000 - cc.srtt
        };
        assert!(
            diff < 1000,
            "srtt should converge to 50ms, got {}us",
            cc.srtt
        );

        // Variance should be very low
        assert!(
            cc.rtt_var < 5000,
            "rtt_var should be low with consistent samples, got {}us",
            cc.rtt_var
        );
    }
}
