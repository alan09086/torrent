use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Algorithm enums
// ---------------------------------------------------------------------------

/// Choking algorithm used when we are seeding.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedChokingAlgorithm {
    /// Unchoke peers we upload to fastest.
    #[default]
    FastestUpload,
    /// Round-robin through all interested peers.
    RoundRobin,
    /// Prefer leechers over seeds (anti-leech).
    AntiLeech,
}

/// Top-level choking algorithm variant.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChokingAlgorithm {
    /// Fixed number of unchoke slots (libtorrent default).
    #[default]
    FixedSlots,
    /// Rate-based unchoking (auto-adjusts slots).
    RateBased,
}

// ---------------------------------------------------------------------------
// PeerInfo
// ---------------------------------------------------------------------------

/// Information about a peer used by the choking algorithm.
#[derive(Debug, Clone)]
pub(crate) struct PeerInfo {
    pub addr: SocketAddr,
    /// Bytes/sec they are uploading TO us.
    pub download_rate: u64,
    /// Bytes/sec we are uploading TO them.
    pub upload_rate: u64,
    /// Peer is interested in our data.
    pub interested: bool,
    /// BEP 21: peer declared upload-only status.
    pub upload_only: bool,
    /// Whether this peer is a seed (has all pieces).
    pub is_seed: bool,
}

// ---------------------------------------------------------------------------
// ChokeDecision
// ---------------------------------------------------------------------------

/// Result of a choking decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChokeDecision {
    /// Peers that should be unchoked.
    pub to_unchoke: Vec<SocketAddr>,
    /// Peers that should be choked.
    pub to_choke: Vec<SocketAddr>,
}

// ---------------------------------------------------------------------------
// ChokerStrategy trait
// ---------------------------------------------------------------------------

/// Trait for pluggable choking strategies.
pub(crate) trait ChokerStrategy: Send + Sync {
    /// Given the current peer list, decide who to unchoke/choke.
    fn decide(
        &mut self,
        peers: &[PeerInfo],
        unchoke_slots: usize,
        seed_mode: bool,
    ) -> ChokeDecision;

    /// Rotate the optimistic unchoke peer.
    fn rotate_optimistic(&mut self, peers: &[PeerInfo]);

    /// Observe current throughput for rate-based slot adjustment.
    /// No-op by default (fixed-slots strategies ignore this).
    fn observe_throughput(&mut self, _throughput: u64) {}

    /// Return the dynamically computed slot count, if this strategy manages its own.
    /// Returns `None` by default (fixed-slots strategies defer to the `Choker`'s `unchoke_slots`).
    #[allow(dead_code)] // Part of the trait API; called by RateBasedStrategy but not dispatched externally yet.
    fn dynamic_slots(&self) -> Option<usize> {
        None
    }
}

// ---------------------------------------------------------------------------
// FixedSlotsStrategy
// ---------------------------------------------------------------------------

/// Fixed-slots choking strategy.
///
/// Leech mode: tit-for-tat (sort by download rate descending).
/// Seed mode: configurable via [`SeedChokingAlgorithm`].
pub(crate) struct FixedSlotsStrategy {
    optimistic_peer: Option<SocketAddr>,
    seed_algorithm: SeedChokingAlgorithm,
    round_robin_offset: usize,
}

impl FixedSlotsStrategy {
    pub fn new(seed_algorithm: SeedChokingAlgorithm) -> Self {
        Self {
            optimistic_peer: None,
            seed_algorithm,
            round_robin_offset: 0,
        }
    }

    /// Select an optimistic unchoke peer.
    ///
    /// If the current optimistic peer is interested and not in the regular set,
    /// keep it. Otherwise pick the first interested peer not in the regular set.
    fn select_optimistic(
        &self,
        interested: &[&PeerInfo],
        regular_unchokes: &[SocketAddr],
    ) -> Option<SocketAddr> {
        // Keep existing optimistic peer if it qualifies.
        if let Some(opt) = self.optimistic_peer {
            let still_interested = interested.iter().any(|p| p.addr == opt && !p.upload_only);
            let already_regular = regular_unchokes.contains(&opt);
            if still_interested && !already_regular {
                return Some(opt);
            }
        }

        // Pick first interested peer not already in regular unchokes (exclude upload-only).
        interested
            .iter()
            .find(|p| !regular_unchokes.contains(&p.addr) && !p.upload_only)
            .map(|p| p.addr)
    }

    /// Sort interested peers according to the seed-mode algorithm.
    fn sort_seed_mode(&mut self, interested: &mut [&PeerInfo], unchoke_slots: usize) {
        match self.seed_algorithm {
            SeedChokingAlgorithm::FastestUpload => {
                interested.sort_by(|a, b| b.upload_rate.cmp(&a.upload_rate));
            }
            SeedChokingAlgorithm::RoundRobin => {
                // Sort by addr for a deterministic order, then rotate.
                interested.sort_by(|a, b| a.addr.cmp(&b.addr));
                if !interested.is_empty() {
                    let offset = self.round_robin_offset % interested.len();
                    interested.rotate_left(offset);
                    self.round_robin_offset =
                        self.round_robin_offset.wrapping_add(unchoke_slots) % interested.len();
                }
            }
            SeedChokingAlgorithm::AntiLeech => {
                // Non-seed peers first (sorted by upload rate desc), then seeds.
                interested.sort_by(|a, b| match (a.is_seed, b.is_seed) {
                    (false, true) => std::cmp::Ordering::Less,
                    (true, false) => std::cmp::Ordering::Greater,
                    _ => b.upload_rate.cmp(&a.upload_rate),
                });
            }
        }
    }
}

impl ChokerStrategy for FixedSlotsStrategy {
    fn decide(
        &mut self,
        peers: &[PeerInfo],
        unchoke_slots: usize,
        seed_mode: bool,
    ) -> ChokeDecision {
        let all_addrs: Vec<SocketAddr> = peers.iter().map(|p| p.addr).collect();

        // Interested peers sorted by rate descending.
        let mut interested: Vec<&PeerInfo> = peers.iter().filter(|p| p.interested).collect();
        if seed_mode {
            self.sort_seed_mode(&mut interested, unchoke_slots);
        } else {
            // Leech mode: unchoke peers that upload to us fastest (tit-for-tat)
            interested.sort_by(|a, b| b.download_rate.cmp(&a.download_rate));
        }

        // Regular unchokes: top N.
        let regular_count = unchoke_slots.min(interested.len());
        let regular_unchokes: Vec<SocketAddr> =
            interested[..regular_count].iter().map(|p| p.addr).collect();

        // Optimistic unchoke selection.
        let optimistic = self.select_optimistic(&interested, &regular_unchokes);
        self.optimistic_peer = optimistic;

        // Build the final unchoke set.
        let mut to_unchoke = regular_unchokes;
        if let Some(opt) = optimistic
            && !to_unchoke.contains(&opt)
        {
            to_unchoke.push(opt);
        }

        // Everyone not in to_unchoke gets choked.
        let to_choke: Vec<SocketAddr> = all_addrs
            .into_iter()
            .filter(|a| !to_unchoke.contains(a))
            .collect();

        ChokeDecision {
            to_unchoke,
            to_choke,
        }
    }

    fn rotate_optimistic(&mut self, peers: &[PeerInfo]) {
        let mut interested: Vec<&PeerInfo> = peers
            .iter()
            .filter(|p| p.interested && !p.upload_only)
            .collect();
        // Sort ascending by download rate so the first non-optimistic is picked.
        interested.sort_by(|a, b| a.download_rate.cmp(&b.download_rate));

        self.optimistic_peer = interested
            .iter()
            .find(|p| Some(p.addr) != self.optimistic_peer)
            .map(|p| p.addr);
    }
}

// ---------------------------------------------------------------------------
// RateBasedStrategy
// ---------------------------------------------------------------------------

/// Rate-based choking strategy that dynamically adjusts unchoke slots
/// based on observed throughput.
///
/// When throughput increases, adds slots to utilize available bandwidth.
/// When throughput drops significantly (>10%), removes slots to reduce
/// overhead. Respects configured min/max bounds and upload rate limits.
pub(crate) struct RateBasedStrategy {
    /// Underlying fixed-slots strategy for peer ranking and optimistic unchoke.
    inner: FixedSlotsStrategy,
    /// Current number of dynamically-adjusted unchoke slots.
    dynamic_slots: usize,
    /// Previous throughput observation (bytes/sec).
    prev_throughput: u64,
    /// Upload rate limit (bytes/sec). 0 means unlimited.
    upload_rate_limit: u64,
    /// Minimum number of unchoke slots.
    min_slots: usize,
    /// Maximum number of unchoke slots.
    max_slots: usize,
}

impl RateBasedStrategy {
    pub fn new(
        seed_algorithm: SeedChokingAlgorithm,
        upload_rate_limit: u64,
        min_slots: usize,
        max_slots: usize,
    ) -> Self {
        Self {
            inner: FixedSlotsStrategy::new(seed_algorithm),
            dynamic_slots: min_slots.max(2),
            prev_throughput: 0,
            upload_rate_limit,
            min_slots,
            max_slots,
        }
    }

    /// Observe current aggregate throughput and adjust slot count.
    fn observe_throughput_inner(&mut self, throughput: u64) {
        // First observation with throughput > 0: just set baseline.
        if self.prev_throughput == 0 && throughput > 0 {
            self.prev_throughput = throughput;
            return;
        }

        if throughput > self.prev_throughput {
            // Throughput increased — consider adding a slot.
            // But not if we're already at capacity (>90% of upload rate limit).
            let at_capacity =
                self.upload_rate_limit > 0 && throughput > self.upload_rate_limit * 90 / 100;

            if !at_capacity && self.dynamic_slots < self.max_slots {
                self.dynamic_slots += 1;
            }
        } else if self.prev_throughput > 0 {
            // Check for >10% drop.
            let threshold = self.prev_throughput * 90 / 100;
            if throughput < threshold && self.dynamic_slots > self.min_slots {
                self.dynamic_slots -= 1;
            }
        }

        // Check for headroom: if we have spare capacity, try adding a slot.
        if self.upload_rate_limit > 0
            && throughput < self.upload_rate_limit
            && self.dynamic_slots > 0
        {
            let per_slot_avg = throughput / self.dynamic_slots as u64;
            if per_slot_avg > 0 {
                let headroom = self.upload_rate_limit - throughput;
                if headroom > per_slot_avg && self.dynamic_slots < self.max_slots {
                    self.dynamic_slots += 1;
                }
            }
        }

        self.prev_throughput = throughput;
    }

    /// Return the current dynamically-adjusted slot count.
    #[cfg(test)]
    pub fn current_slots(&self) -> usize {
        self.dynamic_slots
    }
}

impl ChokerStrategy for RateBasedStrategy {
    fn decide(
        &mut self,
        peers: &[PeerInfo],
        _unchoke_slots: usize,
        seed_mode: bool,
    ) -> ChokeDecision {
        // Ignore external unchoke_slots — use our dynamic count.
        self.inner.decide(peers, self.dynamic_slots, seed_mode)
    }

    fn rotate_optimistic(&mut self, peers: &[PeerInfo]) {
        self.inner.rotate_optimistic(peers);
    }

    fn observe_throughput(&mut self, throughput: u64) {
        self.observe_throughput_inner(throughput);
    }

    fn dynamic_slots(&self) -> Option<usize> {
        Some(self.dynamic_slots)
    }
}

// ---------------------------------------------------------------------------
// Choker (dispatcher)
// ---------------------------------------------------------------------------

/// Choking algorithm dispatcher.
///
/// Delegates to a pluggable [`ChokerStrategy`] implementation. Backward
/// compatible: `Choker::new(n)` creates a fixed-slots strategy with
/// `FastestUpload` seed algorithm (same behavior as the original `Choker`).
pub(crate) struct Choker {
    strategy: Box<dyn ChokerStrategy>,
    unchoke_slots: usize,
    seed_mode: bool,
    #[allow(dead_code)] // Read via #[cfg(test)] accessor.
    choking_algorithm: ChokingAlgorithm,
}

impl Choker {
    /// Create a choker with the given number of regular unchoke slots.
    ///
    /// Uses [`FixedSlotsStrategy`] with [`SeedChokingAlgorithm::FastestUpload`],
    /// matching the behavior of the original monolithic `Choker`.
    #[cfg(test)]
    pub fn new(unchoke_slots: usize) -> Self {
        Self {
            strategy: Box::new(FixedSlotsStrategy::new(SeedChokingAlgorithm::FastestUpload)),
            unchoke_slots,
            seed_mode: false,
            choking_algorithm: ChokingAlgorithm::FixedSlots,
        }
    }

    /// Create a choker with explicit algorithm configuration.
    pub fn with_algorithms(
        unchoke_slots: usize,
        seed_algorithm: SeedChokingAlgorithm,
        choking_algorithm: ChokingAlgorithm,
        upload_rate_limit: u64,
        min_slots: usize,
        max_slots: usize,
    ) -> Self {
        let strategy: Box<dyn ChokerStrategy> = match choking_algorithm {
            ChokingAlgorithm::FixedSlots => Box::new(FixedSlotsStrategy::new(seed_algorithm)),
            ChokingAlgorithm::RateBased => Box::new(RateBasedStrategy::new(
                seed_algorithm,
                upload_rate_limit,
                min_slots,
                max_slots,
            )),
        };

        Self {
            strategy,
            unchoke_slots,
            seed_mode: false,
            choking_algorithm,
        }
    }

    pub fn set_seed_mode(&mut self, seed_mode: bool) {
        self.seed_mode = seed_mode;
    }

    /// Update the number of regular unchoke slots (used by auto upload slot tuner).
    pub fn set_unchoke_slots(&mut self, n: usize) {
        self.unchoke_slots = n;
    }

    /// Return the current number of regular unchoke slots.
    #[allow(dead_code)] // Used by make_stats() in a later task.
    pub fn unchoke_slots(&self) -> usize {
        self.unchoke_slots
    }

    /// Observe current aggregate throughput for rate-based slot adjustment.
    pub fn observe_throughput(&mut self, throughput: u64) {
        self.strategy.observe_throughput(throughput);
    }

    /// Return the choking algorithm variant.
    #[cfg(test)]
    pub fn choking_algorithm(&self) -> ChokingAlgorithm {
        self.choking_algorithm
    }

    /// Decide which peers to unchoke and choke.
    pub fn decide(&mut self, peers: &[PeerInfo]) -> ChokeDecision {
        self.strategy
            .decide(peers, self.unchoke_slots, self.seed_mode)
    }

    /// Pick a new optimistic peer from interested peers.
    pub fn rotate_optimistic(&mut self, peers: &[PeerInfo]) {
        self.strategy.rotate_optimistic(peers);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        format!("127.0.0.1:{port}").parse().unwrap()
    }

    fn peer(port: u16, download_rate: u64, interested: bool) -> PeerInfo {
        PeerInfo {
            addr: addr(port),
            download_rate,
            upload_rate: 0,
            interested,
            upload_only: false,
            is_seed: false,
        }
    }

    fn seed_peer(port: u16, upload_rate: u64, interested: bool) -> PeerInfo {
        PeerInfo {
            addr: addr(port),
            download_rate: 0,
            upload_rate,
            interested,
            upload_only: false,
            is_seed: false,
        }
    }

    // -----------------------------------------------------------------------
    // Existing Choker tests (unchanged logic, updated PeerInfo construction)
    // -----------------------------------------------------------------------

    #[test]
    fn unchoke_top_n() {
        let mut choker = Choker::new(4);
        let peers = vec![
            peer(6881, 100, true),
            peer(6882, 500, true),
            peer(6883, 300, true),
            peer(6884, 200, true),
            peer(6885, 400, true),
            peer(6886, 50, true),
        ];

        let decision = choker.decide(&peers);

        // Top 4 by rate: 500, 400, 300, 200 (ports 6882, 6885, 6883, 6884).
        assert!(decision.to_unchoke.contains(&addr(6882)));
        assert!(decision.to_unchoke.contains(&addr(6885)));
        assert!(decision.to_unchoke.contains(&addr(6883)));
        assert!(decision.to_unchoke.contains(&addr(6884)));

        // Optimistic should add one more from the remaining interested peers,
        // so total unchoked should be 5 (4 regular + 1 optimistic).
        assert_eq!(decision.to_unchoke.len(), 5);

        // The remaining peer is choked.
        assert_eq!(decision.to_choke.len(), 1);
    }

    #[test]
    fn optimistic_rotation() {
        let mut choker = Choker::new(4);
        let peers = vec![
            peer(6881, 500, true),
            peer(6882, 400, true),
            peer(6883, 300, true),
            peer(6884, 200, true),
            peer(6885, 100, true),
            peer(6886, 50, true),
        ];

        let decision = choker.decide(&peers);

        // After decide(), there should be an optimistic peer among the unchoked.
        // The top 4 are 6881..6884. Optimistic is from remaining interested.
        assert_eq!(decision.to_unchoke.len(), 5);

        // The optimistic peer is one of the two not in the top-4 regular set.
        let regular = [addr(6881), addr(6882), addr(6883), addr(6884)];
        let opt: Vec<_> = decision
            .to_unchoke
            .iter()
            .filter(|a| !regular.contains(a))
            .copied()
            .collect();
        assert_eq!(opt.len(), 1);
        let first_opt = opt[0];
        assert!(first_opt == addr(6885) || first_opt == addr(6886));

        // After rotation, the optimistic peer should change.
        choker.rotate_optimistic(&peers);
        // Run decide again to reflect the new optimistic.
        let decision2 = choker.decide(&peers);
        let opt2: Vec<_> = decision2
            .to_unchoke
            .iter()
            .filter(|a| !regular.contains(a))
            .copied()
            .collect();
        assert_eq!(opt2.len(), 1);
        assert_ne!(opt2[0], first_opt);
    }

    #[test]
    fn fewer_peers_than_slots() {
        let mut choker = Choker::new(4);
        let peers = vec![peer(6881, 100, true), peer(6882, 200, true)];

        let decision = choker.decide(&peers);

        // Both interested peers should be unchoked.
        assert!(decision.to_unchoke.contains(&addr(6881)));
        assert!(decision.to_unchoke.contains(&addr(6882)));
        assert_eq!(decision.to_unchoke.len(), 2);
        assert!(decision.to_choke.is_empty());
    }

    #[test]
    fn no_interested_peers() {
        let mut choker = Choker::new(4);
        let peers = vec![
            peer(6881, 100, false),
            peer(6882, 200, false),
            peer(6883, 300, false),
        ];

        let decision = choker.decide(&peers);

        assert!(decision.to_unchoke.is_empty());
        // All peers should be in to_choke.
        assert_eq!(decision.to_choke.len(), 3);
        assert!(decision.to_choke.contains(&addr(6881)));
        assert!(decision.to_choke.contains(&addr(6882)));
        assert!(decision.to_choke.contains(&addr(6883)));
    }

    #[test]
    fn choke_below_threshold() {
        let mut choker = Choker::new(2);
        let peers = vec![
            peer(6881, 500, true),
            peer(6882, 400, true),
            peer(6883, 100, true),
            peer(6884, 50, true),
            peer(6885, 200, false), // not interested
        ];

        let decision = choker.decide(&peers);

        // Regular unchokes: top 2 = ports 6881 (500), 6882 (400).
        assert!(decision.to_unchoke.contains(&addr(6881)));
        assert!(decision.to_unchoke.contains(&addr(6882)));

        // Optimistic adds one more interested peer (6883 or 6884).
        assert_eq!(decision.to_unchoke.len(), 3);

        // The non-unchoked peers should be in to_choke.
        // That's 2 from the remaining: one interested + the uninterested peer.
        assert_eq!(decision.to_choke.len(), 2);
        // Uninterested peer is always choked.
        assert!(decision.to_choke.contains(&addr(6885)));
    }

    #[test]
    fn set_unchoke_slots_changes_capacity() {
        let mut choker = Choker::new(2);
        let peers = vec![
            peer(6881, 500, true),
            peer(6882, 400, true),
            peer(6883, 300, true),
            peer(6884, 200, true),
            peer(6885, 100, true),
        ];

        let decision = choker.decide(&peers);
        // 2 regular + 1 optimistic = 3 unchoked
        assert_eq!(decision.to_unchoke.len(), 3);

        // Increase slots to 4
        choker.set_unchoke_slots(4);
        let decision = choker.decide(&peers);
        // 4 regular + 1 optimistic = 5 unchoked
        assert_eq!(decision.to_unchoke.len(), 5);
    }

    #[test]
    fn seed_mode_unchokes_by_upload_rate() {
        let mut choker = Choker::new(2);
        choker.set_seed_mode(true);

        // In seed mode, peers are ranked by upload_rate (how fast we upload TO them)
        let peers = vec![
            seed_peer(6881, 100, true),
            seed_peer(6882, 500, true),
            seed_peer(6883, 300, true),
            seed_peer(6884, 200, true),
        ];

        let decision = choker.decide(&peers);

        // Top 2 by upload rate: 500 (6882), 300 (6883)
        assert!(decision.to_unchoke.contains(&addr(6882)));
        assert!(decision.to_unchoke.contains(&addr(6883)));

        // Plus one optimistic
        assert_eq!(decision.to_unchoke.len(), 3);
    }

    #[test]
    fn upload_only_excluded_from_optimistic() {
        let mut choker = Choker::new(2);
        let peers = vec![
            peer(6881, 500, true),
            peer(6882, 400, true),
            // These two are interested but upload-only — should never get optimistic slot
            PeerInfo {
                addr: addr(6883),
                download_rate: 100,
                upload_rate: 0,
                interested: true,
                upload_only: true,
                is_seed: false,
            },
            PeerInfo {
                addr: addr(6884),
                download_rate: 50,
                upload_rate: 0,
                interested: true,
                upload_only: true,
                is_seed: false,
            },
        ];

        let decision = choker.decide(&peers);

        // Regular unchokes: 6881, 6882 (top 2 by rate)
        // No optimistic: both remaining interested peers are upload-only
        assert_eq!(decision.to_unchoke.len(), 2);
        assert!(decision.to_unchoke.contains(&addr(6881)));
        assert!(decision.to_unchoke.contains(&addr(6882)));

        // Upload-only peers are choked
        assert!(decision.to_choke.contains(&addr(6883)));
        assert!(decision.to_choke.contains(&addr(6884)));
    }

    #[test]
    fn upload_only_still_regular_unchoked() {
        // In seed mode, upload-only peers can earn regular unchoke by upload rate
        let mut choker = Choker::new(2);
        choker.set_seed_mode(true);

        let peers = vec![
            PeerInfo {
                addr: addr(6881),
                download_rate: 0,
                upload_rate: 500,
                interested: true,
                upload_only: true, // upload-only but high upload rate
                is_seed: false,
            },
            seed_peer(6882, 300, true),
            seed_peer(6883, 100, true),
        ];

        let decision = choker.decide(&peers);

        // Upload-only peer at 6881 has highest upload rate, should be in regular unchokes
        assert!(decision.to_unchoke.contains(&addr(6881)));
        assert!(decision.to_unchoke.contains(&addr(6882)));
    }

    // -----------------------------------------------------------------------
    // New tests: algorithm enums
    // -----------------------------------------------------------------------

    #[test]
    fn seed_choking_algorithm_default() {
        assert_eq!(
            SeedChokingAlgorithm::default(),
            SeedChokingAlgorithm::FastestUpload
        );
    }

    #[test]
    fn choking_algorithm_default() {
        assert_eq!(ChokingAlgorithm::default(), ChokingAlgorithm::FixedSlots);
    }

    #[test]
    fn seed_choking_algorithm_serde_round_trip() {
        for variant in [
            SeedChokingAlgorithm::FastestUpload,
            SeedChokingAlgorithm::RoundRobin,
            SeedChokingAlgorithm::AntiLeech,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let decoded: SeedChokingAlgorithm = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, variant);
        }
    }

    #[test]
    fn choking_algorithm_serde_round_trip() {
        for variant in [ChokingAlgorithm::FixedSlots, ChokingAlgorithm::RateBased] {
            let json = serde_json::to_string(&variant).unwrap();
            let decoded: ChokingAlgorithm = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, variant);
        }
    }

    // -----------------------------------------------------------------------
    // New tests: FixedSlotsStrategy
    // -----------------------------------------------------------------------

    #[test]
    fn fixed_slots_strategy_leech_mode() {
        let mut strategy = FixedSlotsStrategy::new(SeedChokingAlgorithm::FastestUpload);
        let peers = vec![
            peer(6881, 100, true),
            peer(6882, 500, true),
            peer(6883, 300, true),
            peer(6884, 200, true),
            peer(6885, 400, true),
            peer(6886, 50, true),
        ];

        let decision = strategy.decide(&peers, 4, false);

        // Top 4 by download_rate: 500 (6882), 400 (6885), 300 (6883), 200 (6884).
        assert!(decision.to_unchoke.contains(&addr(6882)));
        assert!(decision.to_unchoke.contains(&addr(6885)));
        assert!(decision.to_unchoke.contains(&addr(6883)));
        assert!(decision.to_unchoke.contains(&addr(6884)));

        // 4 regular + 1 optimistic = 5
        assert_eq!(decision.to_unchoke.len(), 5);
        assert_eq!(decision.to_choke.len(), 1);
    }

    #[test]
    fn fixed_slots_round_robin_rotates() {
        let mut strategy = FixedSlotsStrategy::new(SeedChokingAlgorithm::RoundRobin);
        // 5 interested peers, 2 unchoke slots — should rotate through them.
        let peers = vec![
            seed_peer(6881, 100, true),
            seed_peer(6882, 200, true),
            seed_peer(6883, 300, true),
            seed_peer(6884, 400, true),
            seed_peer(6885, 500, true),
        ];

        let d1 = strategy.decide(&peers, 2, true);
        let d2 = strategy.decide(&peers, 2, true);

        // The two rounds should select different regular unchoke sets because
        // the offset advances by unchoke_slots each round.
        // Extract the first two from each (regular unchokes before optimistic).
        // We just need to verify they are not identical sets.
        let set1: Vec<SocketAddr> = d1.to_unchoke.iter().copied().collect();
        let set2: Vec<SocketAddr> = d2.to_unchoke.iter().copied().collect();
        assert_ne!(set1, set2, "round-robin should rotate unchoke set");
    }

    #[test]
    fn fixed_slots_anti_leech_prefers_non_seeds() {
        let mut strategy = FixedSlotsStrategy::new(SeedChokingAlgorithm::AntiLeech);
        let peers = vec![
            // Two seed peers (is_seed = true)
            PeerInfo {
                addr: addr(6881),
                download_rate: 0,
                upload_rate: 500,
                interested: true,
                upload_only: false,
                is_seed: true,
            },
            PeerInfo {
                addr: addr(6882),
                download_rate: 0,
                upload_rate: 400,
                interested: true,
                upload_only: false,
                is_seed: true,
            },
            // Two leecher peers (is_seed = false) with lower upload rates
            PeerInfo {
                addr: addr(6883),
                download_rate: 0,
                upload_rate: 100,
                interested: true,
                upload_only: false,
                is_seed: false,
            },
            PeerInfo {
                addr: addr(6884),
                download_rate: 0,
                upload_rate: 50,
                interested: true,
                upload_only: false,
                is_seed: false,
            },
        ];

        // 2 slots: anti-leech should prefer the 2 non-seed peers over seeds.
        let decision = strategy.decide(&peers, 2, true);

        // The regular unchokes should be the two leechers (non-seeds).
        assert!(
            decision.to_unchoke.contains(&addr(6883)),
            "non-seed peer 6883 should be unchoked"
        );
        assert!(
            decision.to_unchoke.contains(&addr(6884)),
            "non-seed peer 6884 should be unchoked"
        );
    }

    // -----------------------------------------------------------------------
    // New tests: RateBasedStrategy
    // -----------------------------------------------------------------------

    #[test]
    fn rate_based_starts_at_min_slots() {
        let strategy = RateBasedStrategy::new(
            SeedChokingAlgorithm::FastestUpload,
            0,  // unlimited
            3,  // min_slots
            10, // max_slots
        );
        // min_slots=3, max(3,2)=3
        assert_eq!(strategy.current_slots(), 3);

        // If min_slots < 2, should start at 2.
        let strategy2 = RateBasedStrategy::new(
            SeedChokingAlgorithm::FastestUpload,
            0,
            1, // min_slots=1
            10,
        );
        assert_eq!(strategy2.current_slots(), 2);
    }

    #[test]
    fn rate_based_increases_slots_on_throughput_increase() {
        let mut strategy = RateBasedStrategy::new(
            SeedChokingAlgorithm::FastestUpload,
            0,  // unlimited
            2,  // min_slots
            10, // max_slots
        );
        assert_eq!(strategy.current_slots(), 2);

        // First observation sets baseline.
        strategy.observe_throughput_inner(1000);
        assert_eq!(strategy.current_slots(), 2);

        // Throughput increased — should add a slot.
        strategy.observe_throughput_inner(1500);
        assert_eq!(strategy.current_slots(), 3);
    }

    #[test]
    fn rate_based_decreases_slots_on_throughput_drop() {
        let mut strategy = RateBasedStrategy::new(
            SeedChokingAlgorithm::FastestUpload,
            0,  // unlimited
            2,  // min_slots
            10, // max_slots
        );

        // Build up to 4 slots.
        strategy.observe_throughput_inner(1000);
        strategy.observe_throughput_inner(2000);
        assert_eq!(strategy.current_slots(), 3);
        strategy.observe_throughput_inner(3000);
        assert_eq!(strategy.current_slots(), 4);

        // Drop >10%: 3000 -> 2000 (33% drop).
        strategy.observe_throughput_inner(2000);
        assert_eq!(strategy.current_slots(), 3);
    }

    #[test]
    fn rate_based_respects_min_max() {
        let mut strategy = RateBasedStrategy::new(
            SeedChokingAlgorithm::FastestUpload,
            0, // unlimited
            2, // min_slots
            3, // max_slots = 3
        );

        // Build up.
        strategy.observe_throughput_inner(1000);
        strategy.observe_throughput_inner(2000);
        assert_eq!(strategy.current_slots(), 3);

        // Try to exceed max — should stay at 3.
        strategy.observe_throughput_inner(5000);
        assert_eq!(strategy.current_slots(), 3);

        // Drop to bring down.
        strategy.observe_throughput_inner(1000);
        assert_eq!(strategy.current_slots(), 2);

        // Drop more — should not go below min.
        strategy.observe_throughput_inner(100);
        assert_eq!(strategy.current_slots(), 2);
    }

    #[test]
    fn rate_based_does_not_add_at_capacity() {
        let mut strategy = RateBasedStrategy::new(
            SeedChokingAlgorithm::FastestUpload,
            10_000, // 10 KB/s limit
            2,
            10,
        );

        // Baseline.
        strategy.observe_throughput_inner(5_000);
        assert_eq!(strategy.current_slots(), 2);

        // Throughput increased but >90% of capacity (9500/10000 = 95%).
        strategy.observe_throughput_inner(9_500);
        // Should NOT add slot because we're at capacity.
        // However, we might still be at 2 because throughput increase check
        // fails the at_capacity guard. The headroom check also shouldn't fire
        // because headroom (500) < per_slot_avg (4750).
        assert_eq!(strategy.current_slots(), 2);
    }

    #[test]
    fn rate_based_ignores_external_unchoke_slots() {
        let mut strategy = RateBasedStrategy::new(SeedChokingAlgorithm::FastestUpload, 0, 2, 10);

        let peers = vec![
            peer(6881, 500, true),
            peer(6882, 400, true),
            peer(6883, 300, true),
            peer(6884, 200, true),
            peer(6885, 100, true),
        ];

        // dynamic_slots = 2 (min). Pass unchoke_slots=10 externally — should be ignored.
        let decision = strategy.decide(&peers, 10, false);

        // Should use dynamic_slots=2, not the external 10.
        // 2 regular + 1 optimistic = 3 unchoked.
        assert_eq!(decision.to_unchoke.len(), 3);
    }

    // -----------------------------------------------------------------------
    // New tests: Choker dispatcher
    // -----------------------------------------------------------------------

    #[test]
    fn choker_with_round_robin() {
        let mut choker = Choker::with_algorithms(
            2,
            SeedChokingAlgorithm::RoundRobin,
            ChokingAlgorithm::FixedSlots,
            0,
            2,
            10,
        );
        choker.set_seed_mode(true);

        let peers = vec![
            seed_peer(6881, 100, true),
            seed_peer(6882, 200, true),
            seed_peer(6883, 300, true),
            seed_peer(6884, 400, true),
            seed_peer(6885, 500, true),
        ];

        let d1 = choker.decide(&peers);
        let d2 = choker.decide(&peers);

        // Round-robin should produce different unchoke sets on successive calls.
        let set1: Vec<SocketAddr> = d1.to_unchoke.iter().copied().collect();
        let set2: Vec<SocketAddr> = d2.to_unchoke.iter().copied().collect();
        assert_ne!(set1, set2, "round-robin dispatcher should rotate");
    }

    #[test]
    fn choker_with_rate_based() {
        let mut choker = Choker::with_algorithms(
            4, // this should be ignored by rate-based
            SeedChokingAlgorithm::FastestUpload,
            ChokingAlgorithm::RateBased,
            0,
            2,
            10,
        );
        assert_eq!(choker.choking_algorithm(), ChokingAlgorithm::RateBased);

        let peers = vec![
            peer(6881, 500, true),
            peer(6882, 400, true),
            peer(6883, 300, true),
            peer(6884, 200, true),
            peer(6885, 100, true),
        ];

        // Rate-based starts at min_slots=2, so 2 regular + 1 optimistic = 3.
        let decision = choker.decide(&peers);
        assert_eq!(decision.to_unchoke.len(), 3);

        // Feed throughput to increase slots.
        choker.observe_throughput(1000);
        choker.observe_throughput(2000);
        // Now dynamic_slots should be 3 (increased).
        let decision = choker.decide(&peers);
        assert_eq!(decision.to_unchoke.len(), 4); // 3 regular + 1 optimistic
    }

    #[test]
    fn choker_unchoke_slots_getter() {
        let mut choker = Choker::new(4);
        assert_eq!(choker.unchoke_slots(), 4);

        choker.set_unchoke_slots(7);
        assert_eq!(choker.unchoke_slots(), 7);

        choker.set_unchoke_slots(0);
        assert_eq!(choker.unchoke_slots(), 0);
    }

    #[test]
    fn choker_new_is_backward_compatible() {
        let mut choker = Choker::new(4);
        assert_eq!(choker.choking_algorithm(), ChokingAlgorithm::FixedSlots);

        let peers = vec![
            peer(6881, 500, true),
            peer(6882, 400, true),
            peer(6883, 300, true),
            peer(6884, 200, true),
            peer(6885, 100, true),
            peer(6886, 50, true),
        ];

        let decision = choker.decide(&peers);

        // Same behavior as old Choker: top 4 + 1 optimistic = 5.
        assert_eq!(decision.to_unchoke.len(), 5);
        assert!(decision.to_unchoke.contains(&addr(6882)));
        assert!(decision.to_unchoke.contains(&addr(6885)));
        assert!(decision.to_unchoke.contains(&addr(6883)));
        assert!(decision.to_unchoke.contains(&addr(6884)));

        // set_seed_mode and set_unchoke_slots still work.
        choker.set_seed_mode(true);
        choker.set_unchoke_slots(2);
        let decision = choker.decide(&peers);
        assert_eq!(decision.to_unchoke.len(), 3); // 2 regular + 1 optimistic
    }
}
