//! Deterministic virtual clock for simulation timing.
//!
//! [`SimClock`] provides a shared, manually-advanced virtual clock that
//! replaces wall-clock time in simulation scenarios. All simulated network
//! operations reference this clock, enabling deterministic test scenarios
//! that don't depend on real elapsed time.

use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;

/// A shared virtual clock that can be manually advanced.
///
/// All [`SimNetwork`](crate::network::SimNetwork) timing operations reference
/// this clock instead of wall-clock time, enabling deterministic test
/// scenarios.
///
/// The clock is cheaply cloneable (all clones share the same inner state)
/// and safe to use from multiple threads.
#[derive(Clone)]
pub struct SimClock {
    inner: Arc<Mutex<ClockInner>>,
}

struct ClockInner {
    elapsed: Duration,
}

impl SimClock {
    /// Create a new virtual clock starting at time zero.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ClockInner {
                elapsed: Duration::ZERO,
            })),
        }
    }

    /// Current virtual time as [`Duration`] since clock creation.
    pub fn now(&self) -> Duration {
        self.inner.lock().elapsed
    }

    /// Advance the clock by the given duration.
    ///
    /// Multiple advances accumulate — calling `advance(1s)` twice results
    /// in `now()` returning `2s`.
    pub fn advance(&self, duration: Duration) {
        let mut inner = self.inner.lock();
        inner.elapsed += duration;
    }
}

impl Default for SimClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_starts_at_zero() {
        let clock = SimClock::new();
        assert_eq!(clock.now(), Duration::ZERO);
    }

    #[test]
    fn clock_advance() {
        let clock = SimClock::new();
        clock.advance(Duration::from_secs(1));
        assert_eq!(clock.now(), Duration::from_secs(1));
    }

    #[test]
    fn clock_multiple_advances() {
        let clock = SimClock::new();
        clock.advance(Duration::from_millis(500));
        clock.advance(Duration::from_millis(300));
        assert_eq!(clock.now(), Duration::from_millis(800));
    }

    #[test]
    fn clock_clones_share_state() {
        let clock_a = SimClock::new();
        let clock_b = clock_a.clone();
        clock_a.advance(Duration::from_secs(5));
        assert_eq!(clock_b.now(), Duration::from_secs(5));
    }

    #[test]
    fn clock_default() {
        let clock = SimClock::default();
        assert_eq!(clock.now(), Duration::ZERO);
    }
}
