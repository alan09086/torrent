//! Peer quality scoring for swarm management.
//!
//! Provides a composite scoring system that evaluates peers on bandwidth,
//! latency, reliability, and piece availability. Scores drive churn decisions
//! to replace underperforming peers with fresh candidates.

use std::time::{Duration, Instant};

use smallvec::SmallVec;

/// Current phase of swarm lifecycle.
///
/// The swarm transitions from aggressive discovery to conservative steady-state
/// after the discovery phase duration elapses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwarmPhase {
    /// First N seconds — aggressive churn to find good peers quickly.
    Discovery,
    /// After discovery — conservative churn to maintain stability.
    Steady,
}

/// Aggregated swarm-level statistics used for score normalisation.
#[derive(Debug, Clone)]
pub(crate) struct SwarmContext {
    /// Maximum EWMA download rate across all peers (bytes/sec).
    pub max_rate: f64,
    /// Minimum observed RTT across all peers (seconds).
    pub min_rtt: f64,
    /// Median composite score across all peers.
    pub median_score: f64,
}

/// Manages peer scoring parameters and churn timing.
///
/// Constructed with tuneable durations and thresholds. Tracks download start
/// time and last churn timestamp for phase-aware churn scheduling.
pub(crate) struct PeerScorer {
    /// How long a newly-connected peer is protected from eviction.
    probation_duration: Duration,
    /// Duration of the initial discovery phase.
    discovery_phase_duration: Duration,
    /// How often to churn peers during discovery.
    discovery_churn_interval: Duration,
    /// Fraction of worst peers to replace each churn cycle during discovery.
    discovery_churn_percent: f64,
    /// How often to churn peers during steady state.
    steady_churn_interval: Duration,
    /// Fraction of worst peers to replace each churn cycle during steady state.
    steady_churn_percent: f64,
    /// Peers scoring below this threshold are candidates for disconnection.
    min_score_threshold: f64,
    /// When the download started (None if not yet started).
    download_start: Option<Instant>,
    /// When we last performed a churn cycle.
    last_churn: Option<Instant>,
}

/// Score component weights. Must sum to 1.0.
const WEIGHT_BANDWIDTH: f64 = 0.40;
const WEIGHT_RTT: f64 = 0.20;
const WEIGHT_RELIABILITY: f64 = 0.25;
const WEIGHT_AVAILABILITY: f64 = 0.15;

/// Default RTT normalisation when no RTT data is available.
const DEFAULT_RTT_NORM: f64 = 0.5;

/// EWMA smoothing factor for RTT updates (same convention as pipeline.rs).
pub(crate) const RTT_EWMA_ALPHA: f64 = 0.3;

impl PeerScorer {
    /// Create a new scorer with explicit timing parameters.
    pub fn new(
        probation_duration: Duration,
        discovery_phase_duration: Duration,
        discovery_churn_interval: Duration,
        discovery_churn_percent: f64,
        steady_churn_interval: Duration,
        steady_churn_percent: f64,
        min_score_threshold: f64,
    ) -> Self {
        Self {
            probation_duration,
            discovery_phase_duration,
            discovery_churn_interval,
            discovery_churn_percent,
            steady_churn_interval,
            steady_churn_percent,
            min_score_threshold,
            download_start: None,
            last_churn: None,
        }
    }

    /// Record that the download has started. Sets the download start timestamp.
    pub fn start_download(&mut self) {
        self.download_start = Some(Instant::now());
    }

    /// Determine the current swarm phase based on elapsed download time.
    ///
    /// Returns `Discovery` if the download hasn't started or if we're still
    /// within the discovery phase duration.
    pub fn phase(&self) -> SwarmPhase {
        match self.download_start {
            Some(start) if start.elapsed() >= self.discovery_phase_duration => SwarmPhase::Steady,
            _ => SwarmPhase::Discovery,
        }
    }

    /// Returns the churn interval for the current phase.
    pub fn churn_interval(&self) -> Duration {
        match self.phase() {
            SwarmPhase::Discovery => self.discovery_churn_interval,
            SwarmPhase::Steady => self.steady_churn_interval,
        }
    }

    /// Returns the churn percentage for the current phase, with dampening.
    ///
    /// If the swarm median score exceeds 0.7 (indicating a healthy swarm),
    /// the churn percentage is halved to reduce unnecessary peer replacement.
    pub fn churn_percent(&self, median_score: f64) -> f64 {
        let base = match self.phase() {
            SwarmPhase::Discovery => self.discovery_churn_percent,
            SwarmPhase::Steady => self.steady_churn_percent,
        };
        if median_score > 0.7 { base * 0.5 } else { base }
    }

    /// Check whether it is time to perform a churn cycle.
    ///
    /// Returns `true` on the first call (no prior churn), or if enough time
    /// has elapsed since the last churn for the current phase's interval.
    pub fn should_churn(&self, now: Instant) -> bool {
        match self.last_churn {
            None => true,
            Some(last) => now.duration_since(last) >= self.churn_interval(),
        }
    }

    /// Record that a churn cycle was performed at the given instant.
    pub fn mark_churned(&mut self, now: Instant) {
        self.last_churn = Some(now);
    }

    /// Check whether a peer is still within its probation window.
    ///
    /// Peers in probation are protected from eviction to give them time to
    /// demonstrate their quality.
    pub fn is_in_probation(&self, connected_at: Instant) -> bool {
        connected_at.elapsed() < self.probation_duration
    }

    /// Returns the minimum score threshold below which peers are eviction candidates.
    pub fn min_score_threshold(&self) -> f64 {
        self.min_score_threshold
    }

    /// Compute a composite quality score for a peer.
    ///
    /// This is a **pure function** — no `&self` required. The score is a
    /// weighted combination of bandwidth, latency, reliability, and piece
    /// availability, normalised against swarm-wide context.
    ///
    /// # Score components
    ///
    /// - **Bandwidth** (0.40): peer's EWMA rate relative to the swarm's fastest peer.
    /// - **RTT** (0.20): ratio of swarm's best RTT to this peer's RTT (lower = better).
    /// - **Reliability** (0.25): fraction of blocks completed vs total (completed + timed out).
    /// - **Availability** (0.15): fraction of total pieces this peer has.
    ///
    /// Returns 0.0 for snubbed peers. Result is clamped to `[0.0, 1.0]`.
    #[allow(clippy::too_many_arguments)]
    pub fn compute_score(
        ewma_rate: f64,
        avg_rtt: Option<f64>,
        blocks_completed: u64,
        blocks_timed_out: u64,
        piece_count: u32,
        total_pieces: u32,
        snubbed: bool,
        ctx: &SwarmContext,
    ) -> f64 {
        if snubbed {
            return 0.0;
        }

        let bandwidth_norm = if ctx.max_rate > 0.0 {
            (ewma_rate / ctx.max_rate).min(1.0)
        } else {
            0.0
        };

        let rtt_norm = match (avg_rtt, ctx.min_rtt > 0.0) {
            (Some(rtt), true) if rtt > 0.0 => (ctx.min_rtt / rtt).min(1.0),
            _ => DEFAULT_RTT_NORM,
        };

        let total_blocks = blocks_completed.saturating_add(blocks_timed_out);
        let reliability = if total_blocks > 0 {
            blocks_completed as f64 / total_blocks as f64
        } else {
            1.0
        };

        let availability = if total_pieces > 0 {
            f64::from(piece_count) / f64::from(total_pieces)
        } else {
            0.0
        };

        let raw = WEIGHT_BANDWIDTH * bandwidth_norm
            + WEIGHT_RTT * rtt_norm
            + WEIGHT_RELIABILITY * reliability
            + WEIGHT_AVAILABILITY * availability;

        raw.clamp(0.0, 1.0)
    }

    /// Build swarm-level context from per-peer data in a single pass.
    ///
    /// Input iterator yields `(ewma_rate, avg_rtt, current_score)` per peer.
    /// Computes `max_rate`, `min_rtt`, and `median_score` for normalisation.
    ///
    /// Returns safe defaults for an empty iterator.
    pub fn build_swarm_context(
        data: impl Iterator<Item = (f64, Option<f64>, f64)>,
    ) -> SwarmContext {
        let mut max_rate: f64 = 0.0;
        let mut min_rtt: f64 = f64::MAX;
        let mut has_rtt = false;
        let mut scores: SmallVec<[f64; 128]> = SmallVec::new();

        for (rate, rtt, score) in data {
            if rate > max_rate {
                max_rate = rate;
            }
            if let Some(r) = rtt
                && r < min_rtt
            {
                min_rtt = r;
                has_rtt = true;
            }
            scores.push(score);
        }

        if scores.is_empty() {
            return SwarmContext {
                max_rate: 0.0,
                min_rtt: 0.0,
                median_score: 0.0,
            };
        }

        let len = scores.len();
        let mid = len / 2;
        // select_nth_unstable_by for O(n) median without full sort.
        scores.select_nth_unstable_by(mid, |a, b| {
            a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
        });
        let median_score = if len % 2 == 1 {
            scores[mid]
        } else {
            // For even counts, average the two middle values.
            let lower = *scores
                .get(..mid)
                .and_then(|s| {
                    s.iter()
                        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                })
                .unwrap_or(&scores[mid]);
            (lower + scores[mid]) * 0.5
        };

        SwarmContext {
            max_rate,
            min_rtt: if has_rtt { min_rtt } else { 0.0 },
            median_score,
        }
    }
}

/// Compute one step of an EWMA update.
///
/// `current` is the existing average (or 0.0 for the first sample),
/// `sample` is the new observation, and `alpha` controls responsiveness.
/// Returns the updated average.
pub(crate) fn ewma_update(current: f64, sample: f64, alpha: f64) -> f64 {
    alpha * sample + (1.0 - alpha) * current
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Helper: create a PeerScorer with typical test defaults.
    fn test_scorer() -> PeerScorer {
        PeerScorer::new(
            Duration::from_secs(30), // probation
            Duration::from_secs(60), // discovery phase
            Duration::from_secs(10), // discovery churn interval
            0.20,                    // discovery churn percent
            Duration::from_secs(30), // steady churn interval
            0.10,                    // steady churn percent
            0.2,                     // min score threshold
        )
    }

    fn test_ctx() -> SwarmContext {
        SwarmContext {
            max_rate: 1_000_000.0,
            min_rtt: 0.020, // 20ms
            median_score: 0.5,
        }
    }

    // ---- Test 1: Score computation with known inputs ----
    #[test]
    fn score_computation_known_inputs() {
        let ctx = test_ctx();
        // Peer downloading at half max rate, 40ms RTT, perfect reliability, has half the pieces
        let score = PeerScorer::compute_score(
            500_000.0,   // ewma_rate
            Some(0.040), // avg_rtt = 40ms
            100,         // blocks_completed
            0,           // blocks_timed_out
            500,         // piece_count
            1000,        // total_pieces
            false,       // not snubbed
            &ctx,
        );

        // bandwidth_norm = 500_000 / 1_000_000 = 0.5
        // rtt_norm = 0.020 / 0.040 = 0.5
        // reliability = 100 / 100 = 1.0
        // availability = 500 / 1000 = 0.5
        // score = 0.4*0.5 + 0.2*0.5 + 0.25*1.0 + 0.15*0.5
        //       = 0.20 + 0.10 + 0.25 + 0.075 = 0.625
        let expected = 0.625;
        assert!(
            (score - expected).abs() < 1e-10,
            "expected {expected}, got {score}"
        );

        // Verify weights sum to 1.0
        assert!(
            (WEIGHT_BANDWIDTH + WEIGHT_RTT + WEIGHT_RELIABILITY + WEIGHT_AVAILABILITY - 1.0).abs()
                < 1e-10,
            "weights must sum to 1.0"
        );
    }

    // ---- Test 2: Snubbed peers score exactly 0.0 ----
    #[test]
    fn snubbed_peer_scores_zero() {
        let ctx = test_ctx();
        let score = PeerScorer::compute_score(
            1_000_000.0, // max rate — would be 1.0 bandwidth
            Some(0.020), // best RTT — would be 1.0 rtt
            1000,        // perfect reliability
            0,
            1000, // full availability
            1000,
            true, // SNUBBED
            &ctx,
        );
        assert_eq!(score, 0.0, "snubbed peer must score exactly 0.0");
    }

    // ---- Test 3: Probation check ----
    #[test]
    fn probation_check() {
        let scorer = PeerScorer::new(
            Duration::from_millis(100), // 100ms probation
            Duration::from_secs(60),
            Duration::from_secs(10),
            0.20,
            Duration::from_secs(30),
            0.10,
            0.2,
        );

        let connected_at = Instant::now();
        // Immediately after connecting, should be in probation.
        assert!(
            scorer.is_in_probation(connected_at),
            "peer should be in probation immediately after connecting"
        );

        // After probation expires, should not be in probation.
        // We can't wait real-time in tests, so we test with a timestamp far in the past.
        let old_connection = Instant::now() - Duration::from_secs(1);
        assert!(
            !scorer.is_in_probation(old_connection),
            "peer connected 1s ago should be past 100ms probation"
        );
    }

    // ---- Test 4: Phase transitions ----
    #[test]
    fn phase_transitions() {
        let mut scorer = PeerScorer::new(
            Duration::from_secs(30),
            Duration::from_millis(50), // 50ms discovery phase for fast testing
            Duration::from_secs(10),
            0.20,
            Duration::from_secs(30),
            0.10,
            0.2,
        );

        // Before download starts, should be Discovery.
        assert_eq!(scorer.phase(), SwarmPhase::Discovery);

        // Start download — still Discovery (elapsed ~0).
        scorer.start_download();
        assert_eq!(scorer.phase(), SwarmPhase::Discovery);

        // Override download_start to be in the past to simulate elapsed time.
        scorer.download_start = Some(Instant::now() - Duration::from_millis(100));
        assert_eq!(
            scorer.phase(),
            SwarmPhase::Steady,
            "should transition to Steady after discovery_phase_duration"
        );
    }

    // ---- Test 5: Churn dampening when median > 0.7 ----
    #[test]
    fn churn_dampening_high_median() {
        let scorer = test_scorer();

        // Normal median — full percent.
        let normal = scorer.churn_percent(0.5);
        // Discovery phase (download not started), so base = 0.20.
        assert!((normal - 0.20).abs() < 1e-10, "expected 0.20, got {normal}");

        // High median — halved.
        let dampened = scorer.churn_percent(0.8);
        assert!(
            (dampened - 0.10).abs() < 1e-10,
            "expected 0.10 (halved), got {dampened}"
        );

        // Boundary: exactly 0.7 should NOT dampen.
        let boundary = scorer.churn_percent(0.7);
        assert!(
            (boundary - 0.20).abs() < 1e-10,
            "median=0.7 should not dampen, got {boundary}"
        );
    }

    // ---- Test 6: Normalisation edge cases ----
    #[test]
    fn normalisation_edge_cases() {
        // All same rate: bandwidth_norm = 1.0 (rate == max_rate)
        let ctx = SwarmContext {
            max_rate: 100.0,
            min_rtt: 0.050,
            median_score: 0.5,
        };
        let score = PeerScorer::compute_score(100.0, Some(0.050), 10, 0, 50, 100, false, &ctx);
        // bw=1.0, rtt=1.0, rel=1.0, avail=0.5
        // 0.4*1.0 + 0.2*1.0 + 0.25*1.0 + 0.15*0.5 = 0.925
        assert!(
            (score - 0.925).abs() < 1e-10,
            "single best peer score: {score}"
        );

        // Zero rates: max_rate = 0 → bandwidth_norm = 0.0
        let zero_ctx = SwarmContext {
            max_rate: 0.0,
            min_rtt: 0.0,
            median_score: 0.0,
        };
        let score = PeerScorer::compute_score(0.0, None, 0, 0, 0, 100, false, &zero_ctx);
        // bw=0.0, rtt=0.5 (default, min_rtt=0 → fallback), rel=1.0, avail=0.0
        // 0.4*0.0 + 0.2*0.5 + 0.25*1.0 + 0.15*0.0 = 0.35
        assert!((score - 0.35).abs() < 1e-10, "zero-rate peer: {score}");

        // No RTT data: uses default 0.5, bandwidth capped at 1.0
        let score_no_rtt = PeerScorer::compute_score(500.0, None, 10, 0, 50, 100, false, &ctx);
        // bw = (500/100).min(1.0) = 1.0
        // rtt = 0.5 (default), rel = 1.0, avail = 0.5
        // raw = 0.4*1.0 + 0.2*0.5 + 0.25*1.0 + 0.15*0.5 = 0.4+0.1+0.25+0.075 = 0.825
        assert!(
            (score_no_rtt - 0.825).abs() < 1e-10,
            "capped bandwidth score: {score_no_rtt}"
        );

        // Zero total_pieces: availability = 0.0
        let score = PeerScorer::compute_score(50.0, Some(0.050), 10, 0, 0, 0, false, &ctx);
        // bw = 50/100 = 0.5, rtt = 0.050/0.050 = 1.0, rel = 1.0, avail = 0.0
        // 0.4*0.5 + 0.2*1.0 + 0.25*1.0 + 0.15*0.0 = 0.2+0.2+0.25+0.0 = 0.65
        assert!((score - 0.65).abs() < 1e-10, "zero total_pieces: {score}");
    }

    // ---- Test 7: SwarmContext computation ----
    #[test]
    fn swarm_context_computation() {
        let peers = vec![
            (100_000.0, Some(0.030), 0.6),
            (200_000.0, Some(0.020), 0.8),
            (50_000.0, Some(0.050), 0.3),
            (150_000.0, None, 0.5),
            (80_000.0, Some(0.040), 0.7),
        ];

        let ctx = PeerScorer::build_swarm_context(peers.into_iter());

        assert!(
            (ctx.max_rate - 200_000.0).abs() < 1e-10,
            "max_rate: {}",
            ctx.max_rate
        );
        assert!(
            (ctx.min_rtt - 0.020).abs() < 1e-10,
            "min_rtt: {}",
            ctx.min_rtt
        );

        // Scores sorted: [0.3, 0.5, 0.6, 0.7, 0.8] — median (index 2) = 0.6
        assert!(
            (ctx.median_score - 0.6).abs() < 1e-10,
            "median_score: {}",
            ctx.median_score
        );
    }

    // ---- Test 7b: SwarmContext with even peer count ----
    #[test]
    fn swarm_context_even_count() {
        // 4 peers — even count should average two middle values
        let peers = vec![
            (1000.0, Some(0.020), 0.3),
            (5000.0, Some(0.050), 0.5),
            (3000.0, Some(0.030), 0.7),
            (8000.0, Some(0.010), 0.9),
        ];
        let ctx = PeerScorer::build_swarm_context(peers.into_iter());
        assert!((ctx.max_rate - 8000.0).abs() < 1e-10);
        assert!((ctx.min_rtt - 0.010).abs() < 1e-10);
        // Scores sorted: [0.3, 0.5, 0.7, 0.9] — median = (0.5 + 0.7) / 2 = 0.6
        assert!(
            (ctx.median_score - 0.6).abs() < 1e-10,
            "expected 0.6, got {}",
            ctx.median_score
        );
    }

    // ---- Test 8: SwarmContext with empty iterator ----
    #[test]
    fn swarm_context_empty() {
        let ctx = PeerScorer::build_swarm_context(std::iter::empty());
        assert_eq!(ctx.max_rate, 0.0);
        assert_eq!(ctx.min_rtt, 0.0);
        assert_eq!(ctx.median_score, 0.0);
    }

    // ---- Test 9: RTT EWMA convergence ----
    #[test]
    fn rtt_ewma_convergence() {
        let target = 0.050; // 50ms
        let mut current = 0.0;

        // Feed the same value repeatedly — should converge toward target.
        for _ in 0..100 {
            current = ewma_update(current, target, RTT_EWMA_ALPHA);
        }

        assert!(
            (current - target).abs() < 1e-6,
            "EWMA should converge to {target}, got {current}"
        );

        // After just a few iterations, should be reasonably close.
        let mut quick = 0.0;
        for _ in 0..10 {
            quick = ewma_update(quick, target, RTT_EWMA_ALPHA);
        }
        assert!(
            (quick - target).abs() < 0.005,
            "EWMA should approach {target} after 10 iterations, got {quick}"
        );
    }

    // ---- Test 10: should_churn timing ----
    #[test]
    fn should_churn_timing() {
        let mut scorer = PeerScorer::new(
            Duration::from_secs(30),
            Duration::from_secs(60),
            Duration::from_millis(100), // 100ms churn interval (discovery)
            0.20,
            Duration::from_secs(30),
            0.10,
            0.2,
        );

        let now = Instant::now();

        // First call with no prior churn: should be true.
        assert!(
            scorer.should_churn(now),
            "first churn check should return true"
        );

        // Mark churned, then immediately check: should be false.
        scorer.mark_churned(now);
        assert!(
            !scorer.should_churn(now),
            "should not churn immediately after churning"
        );

        // Check slightly before interval: still false.
        let before_interval = now + Duration::from_millis(50);
        assert!(
            !scorer.should_churn(before_interval),
            "should not churn before interval elapses"
        );

        // Check after interval: should be true.
        let after_interval = now + Duration::from_millis(150);
        assert!(
            scorer.should_churn(after_interval),
            "should churn after interval elapses"
        );
    }

    // ---- Test 11: churn_percent phase values ----
    #[test]
    fn churn_percent_phase_values() {
        let mut scorer = PeerScorer::new(
            Duration::from_secs(30),
            Duration::from_millis(50), // 50ms discovery phase
            Duration::from_secs(10),
            0.25, // discovery churn percent
            Duration::from_secs(30),
            0.08, // steady churn percent
            0.2,
        );

        // Discovery phase (download not started).
        assert!(
            (scorer.churn_percent(0.5) - 0.25).abs() < 1e-10,
            "discovery phase should use discovery_churn_percent"
        );

        // Transition to Steady by backdating download_start.
        scorer.download_start = Some(Instant::now() - Duration::from_millis(100));
        assert_eq!(scorer.phase(), SwarmPhase::Steady);
        assert!(
            (scorer.churn_percent(0.5) - 0.08).abs() < 1e-10,
            "steady phase should use steady_churn_percent"
        );

        // Dampened in steady.
        assert!(
            (scorer.churn_percent(0.9) - 0.04).abs() < 1e-10,
            "high median in steady should halve: 0.08 / 2 = 0.04"
        );
    }
}
