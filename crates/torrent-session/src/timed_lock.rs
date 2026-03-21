//! Lock timing diagnostics for hot-path synchronization primitives.
//!
//! `TimedGuard<G>` wraps any lock guard (Mutex, RwLock read/write) and logs
//! a warning if the guard is held longer than a configurable threshold.
//! When the threshold is `Duration::MAX` (disabled), `Instant::now()` is
//! never called — zero overhead on the hot path.

use std::ops::{Deref, DerefMut};
use std::time::{Duration, Instant};

use tracing::warn;

/// Settings for lock timing diagnostics.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LockTimingSettings {
    /// How long a lock can be held before a warning is emitted.
    /// `Duration::MAX` means disabled (no timing overhead).
    pub warn_threshold: Duration,
}

impl LockTimingSettings {
    /// Create settings from the user-facing millisecond value.
    ///
    /// A value of `0` disables timing entirely (`Duration::MAX`).
    pub fn from_ms(ms: u64) -> Self {
        Self {
            warn_threshold: if ms == 0 {
                Duration::MAX
            } else {
                Duration::from_millis(ms)
            },
        }
    }

    /// Whether timing is enabled (threshold is not MAX).
    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.warn_threshold != Duration::MAX
    }
}

impl Default for LockTimingSettings {
    fn default() -> Self {
        Self::from_ms(50)
    }
}

/// A lock guard wrapper that warns when held too long.
///
/// Transparent via `Deref`/`DerefMut` — callers interact with the inner
/// guard as usual. On `Drop`, if the guard was held longer than the
/// threshold, a warning is emitted with the caller's label.
pub(crate) struct TimedGuard<G> {
    guard: G,
    /// `None` when timing is disabled (threshold == Duration::MAX).
    acquired_at: Option<Instant>,
    threshold: Duration,
    label: &'static str,
}

impl<G> TimedGuard<G> {
    /// Wrap a lock guard with timing diagnostics.
    ///
    /// When `settings.warn_threshold == Duration::MAX`, the `Instant::now()`
    /// call is skipped entirely — zero overhead.
    #[inline]
    pub fn new(guard: G, settings: &LockTimingSettings, label: &'static str) -> Self {
        let acquired_at = if settings.is_enabled() {
            Some(Instant::now())
        } else {
            None
        };
        Self {
            guard,
            acquired_at,
            threshold: settings.warn_threshold,
            label,
        }
    }
}

impl<G: Deref> Deref for TimedGuard<G> {
    type Target = G::Target;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<G: DerefMut> DerefMut for TimedGuard<G> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

impl<G> Drop for TimedGuard<G> {
    fn drop(&mut self) {
        if let Some(acquired_at) = self.acquired_at {
            let held = acquired_at.elapsed();
            if held > self.threshold {
                warn!(
                    lock = self.label,
                    held_ms = held.as_millis() as u64,
                    threshold_ms = self.threshold.as_millis() as u64,
                    "lock held longer than threshold"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    #[test]
    fn timed_guard_deref_deref_mut() {
        let m = Mutex::new(42u32);
        let settings = LockTimingSettings::from_ms(100);

        // Read via Deref
        {
            let guard = TimedGuard::new(m.lock(), &settings, "test_mutex");
            assert_eq!(*guard, 42);
        }

        // Write via DerefMut
        {
            let mut guard = TimedGuard::new(m.lock(), &settings, "test_mutex");
            *guard = 99;
        }

        assert_eq!(*m.lock(), 99);
    }

    #[test]
    fn timed_guard_warns_on_slow_lock() {
        // We can't easily capture tracing output in a unit test, but we can
        // verify the timing logic works without panicking.
        let m = Mutex::new(());
        let settings = LockTimingSettings::from_ms(1); // 1ms threshold

        let guard = TimedGuard::new(m.lock(), &settings, "slow_test");
        assert!(guard.acquired_at.is_some());
        std::thread::sleep(Duration::from_millis(5));
        drop(guard); // Should emit warning (1ms threshold, 5ms held)
    }

    #[test]
    fn timed_guard_no_warn_under_threshold() {
        let m = Mutex::new(());
        let settings = LockTimingSettings::from_ms(1000); // 1s threshold

        let guard = TimedGuard::new(m.lock(), &settings, "fast_test");
        assert!(guard.acquired_at.is_some());
        drop(guard); // Should NOT warn (held < 1s)
    }

    #[test]
    fn timed_guard_disabled_when_zero() {
        let m = Mutex::new(());
        let settings = LockTimingSettings::from_ms(0); // Disabled

        assert!(!settings.is_enabled());
        assert_eq!(settings.warn_threshold, Duration::MAX);

        let guard = TimedGuard::new(m.lock(), &settings, "disabled_test");
        assert!(guard.acquired_at.is_none()); // No Instant::now() called
        drop(guard);
    }
}
