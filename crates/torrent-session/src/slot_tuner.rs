//! Automatic upload slot tuning via hill-climbing.
//!
//! Observes aggregate upload throughput per unchoke interval and adjusts the
//! number of unchoke slots to maximize throughput.

/// Automatic upload slot tuner using hill-climbing optimization.
///
/// Every unchoke interval (10s), `observe(throughput)` is called with the total
/// upload bytes for that interval. The tuner adjusts the slot count:
/// - If throughput improved: continue in the same direction
/// - If throughput decreased: reverse direction
/// - Clamp to `[min_slots, max_slots]`
#[allow(dead_code)] // consumed by torrent module (wired in later tasks)
pub(crate) struct SlotTuner {
    slots: usize,
    min_slots: usize,
    max_slots: usize,
    prev_throughput: u64,
    direction: i8, // +1 = increasing, -1 = decreasing
    enabled: bool,
}

#[allow(dead_code)]
impl SlotTuner {
    /// Create a new tuner starting at `initial` slots, bounded by `[min, max]`.
    pub fn new(initial: usize, min: usize, max: usize) -> Self {
        Self {
            slots: initial.clamp(min, max),
            min_slots: min,
            max_slots: max,
            prev_throughput: 0,
            direction: 1, // start by trying to increase
            enabled: true,
        }
    }

    /// Create a disabled tuner that always returns a fixed slot count.
    pub fn disabled(slots: usize) -> Self {
        Self {
            slots,
            min_slots: slots,
            max_slots: slots,
            prev_throughput: 0,
            direction: 0,
            enabled: false,
        }
    }

    /// Current number of unchoke slots.
    pub fn current_slots(&self) -> usize {
        self.slots
    }

    /// Observe upload throughput for the latest interval and adjust slots.
    ///
    /// Call this once per unchoke interval with the total bytes uploaded.
    pub fn observe(&mut self, throughput: u64) {
        if !self.enabled {
            return;
        }

        // First observation: just record baseline, don't adjust
        if self.prev_throughput == 0 && throughput > 0 {
            self.prev_throughput = throughput;
            return;
        }

        // Compare to previous interval
        if throughput > self.prev_throughput {
            // Throughput improved: continue in same direction
            self.apply_direction();
        } else if throughput < self.prev_throughput {
            // Throughput decreased: reverse direction
            self.direction = -self.direction;
            self.apply_direction();
        }
        // If throughput == prev_throughput: hold steady (do nothing)

        self.prev_throughput = throughput;
    }

    fn apply_direction(&mut self) {
        let new_slots = self.slots as i64 + self.direction as i64;
        self.slots = (new_slots as usize).clamp(self.min_slots, self.max_slots);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_at_initial_slots() {
        let tuner = SlotTuner::new(4, 2, 20);
        assert_eq!(tuner.current_slots(), 4);
    }

    #[test]
    fn increases_slots_when_throughput_improves() {
        let mut tuner = SlotTuner::new(4, 2, 20);
        tuner.observe(100_000); // first observation (baseline)
        tuner.observe(120_000); // throughput improved → increase slots
        assert_eq!(tuner.current_slots(), 5);
    }

    #[test]
    fn decreases_slots_when_throughput_drops() {
        let mut tuner = SlotTuner::new(4, 2, 20);
        tuner.observe(100_000); // baseline
        tuner.observe(120_000); // improved → 5 slots, direction = +1
        assert_eq!(tuner.current_slots(), 5);
        tuner.observe(90_000); // dropped → reverse direction to -1, apply → 4
        assert_eq!(tuner.current_slots(), 4);
    }

    #[test]
    fn respects_min_max_bounds() {
        let mut tuner = SlotTuner::new(2, 2, 3);

        // Can't go below min even with decreasing throughput
        tuner.observe(100_000);
        tuner.observe(50_000); // would decrease, but direction reverses and applies
        assert!(tuner.current_slots() >= 2);

        // Fill up to max
        let mut tuner = SlotTuner::new(3, 2, 3);
        tuner.observe(100_000);
        tuner.observe(200_000); // increase
        assert!(tuner.current_slots() <= 3);
    }

    #[test]
    fn disabled_returns_fixed_slots() {
        let mut tuner = SlotTuner::disabled(4);
        assert_eq!(tuner.current_slots(), 4);
        tuner.observe(100_000);
        tuner.observe(200_000);
        assert_eq!(tuner.current_slots(), 4); // unchanged
    }

    #[test]
    fn stagnant_throughput_holds_steady() {
        let mut tuner = SlotTuner::new(4, 2, 20);
        tuner.observe(100_000);
        tuner.observe(100_000); // same → no change
        assert_eq!(tuner.current_slots(), 4);
    }
}
