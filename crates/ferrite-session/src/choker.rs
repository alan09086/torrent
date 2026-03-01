use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Algorithm enums
// ---------------------------------------------------------------------------

/// Choking algorithm used when we are seeding.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)] // consumed by torrent module (not yet implemented)
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
#[allow(dead_code)] // consumed by torrent module (not yet implemented)
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
#[allow(dead_code)]
pub(crate) trait ChokerStrategy: Send {
    /// Given the current peer list, decide who to unchoke/choke.
    fn decide(
        &mut self,
        peers: &[PeerInfo],
        unchoke_slots: usize,
        seed_mode: bool,
    ) -> ChokeDecision;

    /// Rotate the optimistic unchoke peer.
    fn rotate_optimistic(&mut self, peers: &[PeerInfo]);
}

// ---------------------------------------------------------------------------
// FixedSlotsStrategy
// ---------------------------------------------------------------------------

/// Fixed-slots choking strategy.
///
/// Leech mode: tit-for-tat (sort by download rate descending).
/// Seed mode: configurable via [`SeedChokingAlgorithm`].
#[allow(dead_code)]
pub(crate) struct FixedSlotsStrategy {
    optimistic_peer: Option<SocketAddr>,
    seed_algorithm: SeedChokingAlgorithm,
    round_robin_offset: usize,
}

#[allow(dead_code)]
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
                    self.round_robin_offset = self
                        .round_robin_offset
                        .wrapping_add(unchoke_slots)
                        % interested.len();
                }
            }
            SeedChokingAlgorithm::AntiLeech => {
                // Non-seed peers first (sorted by upload rate desc), then seeds.
                interested.sort_by(|a, b| {
                    match (a.is_seed, b.is_seed) {
                        (false, true) => std::cmp::Ordering::Less,
                        (true, false) => std::cmp::Ordering::Greater,
                        _ => b.upload_rate.cmp(&a.upload_rate),
                    }
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
// Choker (existing — kept intact for Task 3-4)
// ---------------------------------------------------------------------------

/// Tit-for-tat choking algorithm.
///
/// Unchokes the top-N peers by download rate plus one optimistic unchoke
/// to discover faster peers.
#[allow(dead_code)] // consumed by torrent module (not yet implemented)
pub(crate) struct Choker {
    unchoke_slots: usize,
    optimistic_peer: Option<SocketAddr>,
    seed_mode: bool,
}

#[allow(dead_code)]
impl Choker {
    /// Create a choker with the given number of regular unchoke slots.
    pub fn new(unchoke_slots: usize) -> Self {
        Self {
            unchoke_slots,
            optimistic_peer: None,
            seed_mode: false,
        }
    }

    pub fn set_seed_mode(&mut self, seed_mode: bool) {
        self.seed_mode = seed_mode;
    }

    /// Update the number of regular unchoke slots (used by auto upload slot tuner).
    pub fn set_unchoke_slots(&mut self, n: usize) {
        self.unchoke_slots = n;
    }

    /// Decide which peers to unchoke and choke.
    ///
    /// Algorithm:
    /// 1. Filter to interested peers only.
    /// 2. Sort by download rate descending.
    /// 3. Regular unchokes = top `unchoke_slots`.
    /// 4. Optimistic unchoke = existing optimistic peer if still interested and
    ///    not already unchoked, otherwise first remaining interested peer.
    /// 5. Everyone else gets choked.
    pub fn decide(&mut self, peers: &[PeerInfo]) -> ChokeDecision {
        let all_addrs: Vec<SocketAddr> = peers.iter().map(|p| p.addr).collect();

        // Interested peers sorted by rate descending.
        let mut interested: Vec<&PeerInfo> = peers.iter().filter(|p| p.interested).collect();
        if self.seed_mode {
            // Seed mode: unchoke peers we upload to fastest (maximize distribution)
            interested.sort_by(|a, b| b.upload_rate.cmp(&a.upload_rate));
        } else {
            // Leech mode: unchoke peers that upload to us fastest (tit-for-tat)
            interested.sort_by(|a, b| b.download_rate.cmp(&a.download_rate));
        }

        // Regular unchokes: top N by download rate.
        let regular_count = self.unchoke_slots.min(interested.len());
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

    /// Pick a new optimistic peer from interested peers.
    ///
    /// Selects the interested peer with the lowest download rate that is not
    /// already the optimistic peer.
    pub fn rotate_optimistic(&mut self, peers: &[PeerInfo]) {
        let mut interested: Vec<&PeerInfo> = peers.iter().filter(|p| p.interested && !p.upload_only).collect();
        // Sort ascending by download rate so the first non-optimistic is picked.
        interested.sort_by(|a, b| a.download_rate.cmp(&b.download_rate));

        self.optimistic_peer = interested
            .iter()
            .find(|p| Some(p.addr) != self.optimistic_peer)
            .map(|p| p.addr);
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

        // After decide(), an optimistic peer should be set.
        assert!(choker.optimistic_peer.is_some());

        // The optimistic peer must be in the interested set but not in the
        // top 4 (regular unchokes are 6881, 6882, 6883, 6884).
        let opt = choker.optimistic_peer.unwrap();
        assert!(opt == addr(6885) || opt == addr(6886));

        // Optimistic peer should be in the unchoke list.
        assert!(decision.to_unchoke.contains(&opt));

        // After rotation, the optimistic peer should change.
        choker.rotate_optimistic(&peers);
        let new_opt = choker.optimistic_peer.unwrap();
        assert_ne!(new_opt, opt);
    }

    #[test]
    fn fewer_peers_than_slots() {
        let mut choker = Choker::new(4);
        let peers = vec![
            peer(6881, 100, true),
            peer(6882, 200, true),
        ];

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
        assert_eq!(SeedChokingAlgorithm::default(), SeedChokingAlgorithm::FastestUpload);
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
}
