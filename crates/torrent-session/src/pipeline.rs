//! Per-peer request pipeline with semaphore-based permit scheduling.
//!
//! Each peer gets a `tokio::sync::Semaphore` with a fixed number of request
//! permits. A request driver acquires permits to dispatch requests reactively,
//! and permits are released when blocks arrive. A `tokio::sync::Notify` allows
//! waking the driver when new pieces become available for requesting.
//!
//! The pipeline tick is retained for throughput statistics and snub detection,
//! but no longer drives depth decisions.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Notify, Semaphore};

/// Per-peer request pipeline state.
///
/// Tracks in-flight requests and measures throughput. Request scheduling is
/// driven by a `Semaphore` — the request driver acquires a permit before
/// each request, and permits are released when blocks arrive.
pub(crate) struct PeerPipelineState {
    /// Bytes received in the current tick window (for throughput stats).
    last_second_bytes: u64,
    /// EWMA throughput estimate in bytes/sec (for reporting only).
    ewma_rate_bytes_sec: f64,
    /// When the last tick occurred.
    last_tick: Instant,
    /// Map of (piece, begin) -> request send time for RTT tracking.
    request_times: HashMap<(u32, u32), Instant>,
    /// Semaphore controlling concurrent request slots.
    semaphore: Arc<Semaphore>,
    /// Notify handle for waking the request driver when new pieces are available.
    notify: Arc<Notify>,
    /// Maximum number of permits (fixed at construction).
    max_permits: usize,
}

#[allow(dead_code)]
impl PeerPipelineState {
    /// Create a new pipeline state with the given number of request permits.
    ///
    /// The semaphore is initialized with `initial_queue_depth.max(1)` permits.
    pub fn new(initial_queue_depth: usize) -> Self {
        let permits = initial_queue_depth.max(1);
        Self {
            last_second_bytes: 0,
            ewma_rate_bytes_sec: 0.0,
            last_tick: Instant::now(),
            request_times: HashMap::new(),
            semaphore: Arc::new(Semaphore::new(permits)),
            notify: Arc::new(Notify::new()),
            max_permits: permits,
        }
    }

    // ---- Semaphore-based API ----

    /// Get a clone of the semaphore `Arc` (for use by the request driver).
    pub fn semaphore(&self) -> Arc<Semaphore> {
        Arc::clone(&self.semaphore)
    }

    /// Get a clone of the notify `Arc` (for use by the request driver).
    pub fn notify(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }

    /// Maximum number of permits this pipeline was configured with.
    pub fn max_permits(&self) -> usize {
        self.max_permits
    }

    /// Number of permits currently available (not acquired).
    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// Wake the request driver (calls `notify.notify_one()`).
    pub fn wake_driver(&self) {
        self.notify.notify_one();
    }

    /// Release one permit back to the semaphore (called when a block arrives).
    pub fn release_permit(&self) {
        self.semaphore.add_permits(1);
    }

    /// Restore permits so that `available + currently_in_flight == max_permits`.
    ///
    /// If `currently_in_flight >= max_permits`, no permits are added (the
    /// semaphore is already at capacity or over-committed).
    pub fn restore_full_permits(&self, currently_in_flight: usize) {
        let to_add = self.max_permits.saturating_sub(currently_in_flight);
        if to_add > 0 {
            self.semaphore.add_permits(to_add);
        }
    }

    /// Reset permits to full capacity given the current in-flight count.
    ///
    /// Closes the old semaphore and creates a new one with
    /// `max_permits - currently_in_flight` available permits.
    pub fn reset_permits(&mut self, currently_in_flight: usize) {
        self.semaphore.close();
        let available = self.max_permits.saturating_sub(currently_in_flight);
        self.semaphore = Arc::new(Semaphore::new(available));
    }

    // ---- Unchanged methods ----

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
    /// Queue depth is NOT adjusted here — it stays fixed. Throughput bytes
    /// are accumulated for EWMA stats.
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
        let state = PeerPipelineState::new(128);
        assert_eq!(state.max_permits(), 128);
        assert_eq!(state.available_permits(), 128);
    }

    #[test]
    fn block_received_does_not_change_depth() {
        let mut state = PeerPipelineState::new(128);
        let now = Instant::now();

        for i in 0..20 {
            state.request_sent(0, i * 16384, now);
            state.block_received(0, i * 16384, 16384, now);
        }
        // Permits stay at 128 — no slow-start growth
        assert_eq!(state.max_permits(), 128);
    }

    #[test]
    fn tick_updates_ewma_only() {
        let mut state = PeerPipelineState::new(128);

        // Simulate 1 MB/s throughput for two ticks
        state.last_second_bytes = 1_048_576;
        state.tick();
        assert!(state.ewma_rate() > 0.0);
        // Permits unchanged
        assert_eq!(state.max_permits(), 128);

        state.last_second_bytes = 1_048_576;
        state.tick();
        // Still 128 — no formula recomputation
        assert_eq!(state.max_permits(), 128);
    }

    #[test]
    fn reset_permits_restores_availability() {
        let mut state = PeerPipelineState::new(128);

        // Acquire some permits to reduce availability
        let _p1 = state.semaphore().try_acquire_owned().unwrap();
        let _p2 = state.semaphore().try_acquire_owned().unwrap();
        assert_eq!(state.available_permits(), 126);

        // reset_permits(0) restores to max
        state.reset_permits(0);
        assert_eq!(state.available_permits(), 128);
    }

    #[test]
    fn min_permits_clamped_to_one() {
        let state = PeerPipelineState::new(0);
        // initial_queue_depth of 0 gets clamped to 1
        assert_eq!(state.max_permits(), 1);
        assert_eq!(state.available_permits(), 1);
    }

    #[test]
    fn block_timeout_and_most_recent_request() {
        let mut state = PeerPipelineState::new(128);

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

    // ---- Semaphore-specific tests ----

    #[tokio::test]
    async fn test_semaphore_acquire_and_release() {
        let state = PeerPipelineState::new(2);
        let sem = state.semaphore();

        assert_eq!(state.available_permits(), 2);

        // Acquire 2 permits
        let _p1 = sem.acquire().await.unwrap();
        let _p2 = sem.acquire().await.unwrap();
        assert_eq!(state.available_permits(), 0);

        // Release one via the pipeline API
        state.release_permit();
        assert_eq!(state.available_permits(), 1);
    }

    #[tokio::test]
    async fn test_restore_full_permits() {
        let state = PeerPipelineState::new(4);
        let sem = state.semaphore();

        // Acquire 3 permits (simulating 3 in-flight requests)
        let _p1 = sem.acquire().await.unwrap();
        let _p2 = sem.acquire().await.unwrap();
        let _p3 = sem.acquire().await.unwrap();
        assert_eq!(state.available_permits(), 1);

        // Forget the acquired permits (they won't be returned automatically)
        _p1.forget();
        _p2.forget();
        _p3.forget();

        // Now available is still 1, and we have 3 "in flight"
        assert_eq!(state.available_permits(), 1);

        // Restore with 3 currently in flight: should add max_permits - 3 = 1
        state.restore_full_permits(3);
        assert_eq!(state.available_permits(), 2);

        // Restore with 0 in flight: should add max_permits - 0 = 4
        // But we already have 2, so total will be 6 — restore_full_permits
        // trusts the caller to provide the correct in-flight count
        state.restore_full_permits(0);
        assert_eq!(state.available_permits(), 6);
    }

    #[tokio::test]
    async fn test_restore_full_permits_saturating() {
        let state = PeerPipelineState::new(4);

        // If in-flight exceeds max_permits, no permits are added
        state.restore_full_permits(10);
        // Should still have the original 4 permits — nothing added
        assert_eq!(state.available_permits(), 4);
    }

    #[tokio::test]
    async fn test_notify_wakes_driver() {
        let state = PeerPipelineState::new(1);
        let notify = state.notify();

        let woke = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let woke_clone = Arc::clone(&woke);

        let handle = tokio::spawn(async move {
            notify.notified().await;
            woke_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        // Give the spawned task time to start waiting
        tokio::task::yield_now().await;

        // Wake the driver
        state.wake_driver();

        // Wait for the task to complete
        handle.await.unwrap();
        assert!(woke.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn test_reset_permits() {
        let mut state = PeerPipelineState::new(8);

        // Reset with 3 in flight — should have 5 available
        state.reset_permits(3);
        assert_eq!(state.available_permits(), 5);
        assert_eq!(state.max_permits(), 8);

        // Reset with 0 in flight — should have 8 available
        state.reset_permits(0);
        assert_eq!(state.available_permits(), 8);

        // Reset with more than max — should have 0 available
        state.reset_permits(100);
        assert_eq!(state.available_permits(), 0);
    }
}
