# M43: Seed Choking Algorithms + Rate-Based Choker -- Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Refactor the monolithic `Choker` into a pluggable `ChokerStrategy` trait with four implementations: FixedSlots (current tit-for-tat), RoundRobin, AntiLeech, and RateBased. Add two new settings fields (`seed_choking_algorithm`, `choking_algorithm`) so users can select strategies at runtime.

**Architecture:** The existing `Choker` struct in `choker.rs` currently owns all logic -- sorting by download/upload rate, selecting optimistic unchoke, and producing `ChokeDecision`. We decompose this into:

1. **`SeedChokingAlgorithm`** enum -- controls how *seed-mode* peers are ranked for regular unchoke slots: `FastestUpload` (current), `RoundRobin`, `AntiLeech`.
2. **`ChokingAlgorithm`** enum -- controls how the *unchoke slot count* is determined: `FixedSlots` (current `unchoke_slots` from SlotTuner), `RateBased` (dynamically adjusts count based on upload capacity utilization).
3. **`ChokerStrategy`** trait -- `fn decide(&mut self, peers: &[PeerInfo], unchoke_slots: usize, seed_mode: bool) -> ChokeDecision` + `fn rotate_optimistic(&mut self, peers: &[PeerInfo])`.
4. **`Choker`** becomes a thin dispatcher that holds a `Box<dyn ChokerStrategy>` and delegates.

The `PeerInfo` struct gains one new field: `is_seed: bool` (for AntiLeech detection -- peers that have complete bitfield and never request data are likely seeds). The `TorrentConfig` gains the two algorithm fields, propagated from `Settings`.

**Tech Stack:** Rust edition 2024, serde, tokio

---

## Task 1: Algorithm Enums + PeerInfo Extension

**Files:**
- Modify: `crates/ferrite-session/src/choker.rs:1-17`

**Step 1: Write tests for the new enums and PeerInfo field**

Add the following at the top of `crates/ferrite-session/src/choker.rs`, replacing the existing content up to the `PeerInfo` struct definition:

```rust
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// Strategy for ranking peers during seed-mode choking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SeedChokingAlgorithm {
    /// Unchoke peers we upload to fastest — maximize throughput (current default).
    FastestUpload,
    /// Rotate unchoke slots periodically regardless of speed — maximize fairness.
    RoundRobin,
    /// Prefer peers that also seed other torrents — penalize hit-and-run leechers.
    AntiLeech,
}

impl Default for SeedChokingAlgorithm {
    fn default() -> Self {
        Self::FastestUpload
    }
}

/// Strategy for determining the number of unchoke slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChokingAlgorithm {
    /// Fixed slot count controlled by SlotTuner (current default).
    FixedSlots,
    /// Dynamically adjust unchoke count based on upload capacity utilization.
    RateBased,
}

impl Default for ChokingAlgorithm {
    fn default() -> Self {
        Self::FixedSlots
    }
}

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
    /// Whether the peer appears to be a full seed (has all pieces, never requests).
    pub is_seed: bool,
}
```

Then add these tests to the existing `#[cfg(test)] mod tests` block (at the bottom of the file):

```rust
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
        for variant in [
            ChokingAlgorithm::FixedSlots,
            ChokingAlgorithm::RateBased,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let decoded: ChokingAlgorithm = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, variant);
        }
    }
```

**Step 2: Update all existing PeerInfo construction sites to include `is_seed: false`**

In the existing test helpers (`peer()` and `seed_peer()` functions in `choker.rs` tests), add `is_seed: false` to the `PeerInfo` structs. Also in the `upload_only_excluded_from_optimistic` and `upload_only_still_regular_unchoked` tests, add `is_seed: false`.

In `crates/ferrite-session/src/torrent.rs`, find the two `PeerInfo` construction sites (in `run_choker()` around line 2837 and `rotate_optimistic()` around line 2990). Add the `is_seed` field:

```rust
    // In run_choker():
    let peer_infos: Vec<PeerInfo> = self
        .peers
        .values()
        .map(|p| PeerInfo {
            addr: p.addr,
            download_rate: p.download_rate,
            upload_rate: p.upload_rate,
            interested: p.peer_interested,
            upload_only: p.upload_only,
            is_seed: p.bitfield.count_ones() == self.num_pieces && p.pending_requests.is_empty(),
        })
        .collect();
```

```rust
    // In rotate_optimistic():
    let peer_infos: Vec<PeerInfo> = self
        .peers
        .values()
        .map(|p| PeerInfo {
            addr: p.addr,
            download_rate: p.download_rate,
            upload_rate: p.upload_rate,
            interested: p.peer_interested,
            upload_only: p.upload_only,
            is_seed: p.bitfield.count_ones() == self.num_pieces && p.pending_requests.is_empty(),
        })
        .collect();
```

**Step 3: Run tests**

Run: `cargo test -p ferrite-session choker -- --nocapture`
Expected: All existing tests pass + 4 new enum tests pass.

---

## Task 2: ChokerStrategy Trait + FixedSlots Implementation

**Files:**
- Modify: `crates/ferrite-session/src/choker.rs`

**Step 1: Define the ChokerStrategy trait and extract FixedSlots**

Add the trait definition and the FixedSlots implementation after the enum and PeerInfo definitions, before the existing `Choker` struct. The `FixedSlots` impl is a direct extraction of the current `Choker::decide()` and `Choker::rotate_optimistic()` logic:

```rust
/// Result of a choking decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChokeDecision {
    /// Peers that should be unchoked.
    pub to_unchoke: Vec<SocketAddr>,
    /// Peers that should be choked.
    pub to_choke: Vec<SocketAddr>,
}

/// Trait for pluggable choking strategies.
pub(crate) trait ChokerStrategy: Send {
    /// Decide which peers to unchoke and choke.
    ///
    /// `unchoke_slots` is the number of regular (non-optimistic) unchoke slots.
    /// `seed_mode` indicates whether the torrent is in seed mode.
    fn decide(
        &mut self,
        peers: &[PeerInfo],
        unchoke_slots: usize,
        seed_mode: bool,
    ) -> ChokeDecision;

    /// Rotate the optimistic unchoke peer.
    fn rotate_optimistic(&mut self, peers: &[PeerInfo]);
}

// ── FixedSlots strategy (default — tit-for-tat) ───────────────────────

/// FixedSlots: current tit-for-tat algorithm with optimistic unchoke.
///
/// Leech mode: unchoke top N by download rate.
/// Seed mode: unchoke top N by upload rate (FastestUpload), round-robin,
/// or anti-leech depending on `seed_choking_algorithm`.
pub(crate) struct FixedSlotsStrategy {
    optimistic_peer: Option<SocketAddr>,
    seed_algorithm: SeedChokingAlgorithm,
    /// For RoundRobin: current rotation offset.
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

    /// Sort interested peers by the seed-mode algorithm.
    fn sort_seed_mode(&mut self, interested: &mut Vec<&PeerInfo>, unchoke_slots: usize) {
        match self.seed_algorithm {
            SeedChokingAlgorithm::FastestUpload => {
                interested.sort_by(|a, b| b.upload_rate.cmp(&a.upload_rate));
            }
            SeedChokingAlgorithm::RoundRobin => {
                // Stable sort by address (deterministic order), then rotate
                interested.sort_by(|a, b| a.addr.cmp(&b.addr));
                if !interested.is_empty() {
                    let offset = self.round_robin_offset % interested.len();
                    interested.rotate_left(offset);
                    // Advance offset for next round
                    self.round_robin_offset =
                        self.round_robin_offset.wrapping_add(unchoke_slots);
                }
            }
            SeedChokingAlgorithm::AntiLeech => {
                // Prefer non-seed peers (actual leechers who need data).
                // Among non-seeds, sort by upload rate descending.
                // Seeds (hit-and-run leechers who have everything) go to the end.
                interested.sort_by(|a, b| {
                    match (a.is_seed, b.is_seed) {
                        (false, true) => std::cmp::Ordering::Less,    // non-seed first
                        (true, false) => std::cmp::Ordering::Greater, // seed last
                        _ => b.upload_rate.cmp(&a.upload_rate),       // within group: fastest first
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

        // Interested peers sorted by rate.
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
```

**Step 2: Write tests for FixedSlotsStrategy directly**

Add these tests in the `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn fixed_slots_strategy_leech_mode() {
        let mut strategy = FixedSlotsStrategy::new(SeedChokingAlgorithm::FastestUpload);
        let peers = vec![
            peer(6881, 100, true),
            peer(6882, 500, true),
            peer(6883, 300, true),
            peer(6884, 200, true),
            peer(6885, 400, true),
        ];

        let decision = strategy.decide(&peers, 3, false);
        // Top 3 by download rate: 500, 400, 300 (ports 6882, 6885, 6883)
        assert!(decision.to_unchoke.contains(&addr(6882)));
        assert!(decision.to_unchoke.contains(&addr(6885)));
        assert!(decision.to_unchoke.contains(&addr(6883)));
        // Plus optimistic = 4 total
        assert_eq!(decision.to_unchoke.len(), 4);
    }

    #[test]
    fn fixed_slots_round_robin_rotates() {
        let mut strategy = FixedSlotsStrategy::new(SeedChokingAlgorithm::RoundRobin);
        let peers = vec![
            seed_peer(6881, 100, true),
            seed_peer(6882, 200, true),
            seed_peer(6883, 300, true),
            seed_peer(6884, 400, true),
        ];

        let d1 = strategy.decide(&peers, 2, true);
        let d2 = strategy.decide(&peers, 2, true);

        // The two decisions should select different regular unchoke sets
        // because round-robin advances the offset.
        let r1: Vec<SocketAddr> = d1.to_unchoke.iter().copied().collect();
        let r2: Vec<SocketAddr> = d2.to_unchoke.iter().copied().collect();
        assert_ne!(r1, r2, "round-robin should rotate unchoke slots");
    }

    #[test]
    fn fixed_slots_anti_leech_prefers_non_seeds() {
        let mut strategy = FixedSlotsStrategy::new(SeedChokingAlgorithm::AntiLeech);
        let peers = vec![
            // Seed peer with high upload rate
            PeerInfo {
                addr: addr(6881),
                download_rate: 0,
                upload_rate: 1000,
                interested: true,
                upload_only: false,
                is_seed: true,
            },
            // Non-seed peer with lower upload rate
            PeerInfo {
                addr: addr(6882),
                download_rate: 0,
                upload_rate: 100,
                interested: true,
                upload_only: false,
                is_seed: false,
            },
            // Another non-seed
            PeerInfo {
                addr: addr(6883),
                download_rate: 0,
                upload_rate: 50,
                interested: true,
                upload_only: false,
                is_seed: false,
            },
        ];

        let decision = strategy.decide(&peers, 2, true);
        // Non-seed peers should be preferred over seed peers
        assert!(decision.to_unchoke.contains(&addr(6882)));
        assert!(decision.to_unchoke.contains(&addr(6883)));
    }
```

**Step 3: Run tests**

Run: `cargo test -p ferrite-session choker -- --nocapture`
Expected: All tests pass (existing + new).

---

## Task 3: RateBased Strategy Implementation

**Files:**
- Modify: `crates/ferrite-session/src/choker.rs`

**Step 1: Implement the RateBased strategy**

Add after the `FixedSlotsStrategy` implementation:

```rust
// ── RateBased strategy ─────────────────────────────────────────────────

/// RateBased: dynamically adjusts unchoke count based on upload capacity.
///
/// Instead of using a fixed slot count from SlotTuner, this strategy
/// observes the total upload throughput and adjusts the number of unchoked
/// peers. If all unchoked peers are saturated (upload capacity is fully
/// utilized), it adds a slot. If adding a slot doesn't increase throughput,
/// it removes the slot.
///
/// The seed-mode peer ranking still respects `SeedChokingAlgorithm`.
pub(crate) struct RateBasedStrategy {
    inner: FixedSlotsStrategy,
    /// Dynamically computed unchoke count.
    dynamic_slots: usize,
    /// Previous interval's total upload throughput.
    prev_throughput: u64,
    /// Upload rate limit (bytes/sec). 0 = unlimited (use heuristic).
    upload_rate_limit: u64,
    min_slots: usize,
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

    /// Update unchoke count based on observed upload throughput.
    ///
    /// Called before `decide()` with the bytes uploaded in the last interval.
    pub fn observe_throughput(&mut self, throughput: u64) {
        if self.prev_throughput == 0 && throughput > 0 {
            // First observation: baseline
            self.prev_throughput = throughput;
            return;
        }

        // Calculate per-slot throughput
        let per_slot = if self.dynamic_slots > 0 {
            throughput / self.dynamic_slots as u64
        } else {
            0
        };

        // Determine upload capacity
        let capacity = if self.upload_rate_limit > 0 {
            self.upload_rate_limit
        } else {
            // Heuristic: if throughput increased when we added a slot,
            // we haven't hit capacity yet
            u64::MAX
        };

        // If throughput is above 90% of capacity and we have a known limit,
        // don't add more slots
        let at_capacity = self.upload_rate_limit > 0
            && throughput > self.upload_rate_limit * 9 / 10;

        if throughput > self.prev_throughput && !at_capacity {
            // Throughput improved: try adding a slot
            self.dynamic_slots = (self.dynamic_slots + 1).min(self.max_slots);
        } else if throughput < self.prev_throughput * 9 / 10 && self.dynamic_slots > self.min_slots {
            // Throughput dropped significantly: remove a slot
            self.dynamic_slots = (self.dynamic_slots - 1).max(self.min_slots);
        } else if per_slot > 0 && capacity != u64::MAX {
            // Check if we can fit more peers within capacity
            let headroom = capacity.saturating_sub(throughput);
            if headroom > per_slot && self.dynamic_slots < self.max_slots {
                self.dynamic_slots = (self.dynamic_slots + 1).min(self.max_slots);
            }
        }

        self.prev_throughput = throughput;
    }

    /// Current dynamically computed slot count.
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
        // Ignore the external unchoke_slots — use our own dynamic count
        self.inner.decide(peers, self.dynamic_slots, seed_mode)
    }

    fn rotate_optimistic(&mut self, peers: &[PeerInfo]) {
        self.inner.rotate_optimistic(peers);
    }
}
```

**Step 2: Write tests for RateBased strategy**

```rust
    #[test]
    fn rate_based_starts_at_min_slots() {
        let strategy = RateBasedStrategy::new(
            SeedChokingAlgorithm::FastestUpload,
            0, // unlimited
            2,
            20,
        );
        assert_eq!(strategy.current_slots(), 2);
    }

    #[test]
    fn rate_based_increases_slots_on_throughput_increase() {
        let mut strategy = RateBasedStrategy::new(
            SeedChokingAlgorithm::FastestUpload,
            0, // unlimited
            2,
            20,
        );
        strategy.observe_throughput(100_000); // baseline
        strategy.observe_throughput(120_000); // improved
        assert_eq!(strategy.current_slots(), 3);
    }

    #[test]
    fn rate_based_decreases_slots_on_throughput_drop() {
        let mut strategy = RateBasedStrategy::new(
            SeedChokingAlgorithm::FastestUpload,
            0, // unlimited
            2,
            20,
        );
        strategy.observe_throughput(100_000); // baseline
        strategy.observe_throughput(120_000); // improved → 3 slots
        assert_eq!(strategy.current_slots(), 3);
        strategy.observe_throughput(50_000); // dropped significantly → 2 slots
        assert_eq!(strategy.current_slots(), 2);
    }

    #[test]
    fn rate_based_respects_min_max() {
        let mut strategy = RateBasedStrategy::new(
            SeedChokingAlgorithm::FastestUpload,
            0,
            2,
            3,
        );
        strategy.observe_throughput(100_000);
        strategy.observe_throughput(200_000); // → 3
        strategy.observe_throughput(300_000); // → 3 (capped at max)
        assert!(strategy.current_slots() <= 3);

        // Can't go below min
        strategy.observe_throughput(10_000); // big drop
        assert!(strategy.current_slots() >= 2);
    }

    #[test]
    fn rate_based_does_not_add_at_capacity() {
        let mut strategy = RateBasedStrategy::new(
            SeedChokingAlgorithm::FastestUpload,
            100_000, // 100 KB/s limit
            2,
            20,
        );
        strategy.observe_throughput(95_000); // baseline near capacity
        let slots_before = strategy.current_slots();
        strategy.observe_throughput(96_000); // still near capacity — should not add slot
        assert_eq!(strategy.current_slots(), slots_before);
    }

    #[test]
    fn rate_based_ignores_external_unchoke_slots() {
        let mut strategy = RateBasedStrategy::new(
            SeedChokingAlgorithm::FastestUpload,
            0,
            2,
            20,
        );
        let peers = vec![
            peer(6881, 500, true),
            peer(6882, 400, true),
            peer(6883, 300, true),
            peer(6884, 200, true),
            peer(6885, 100, true),
        ];

        // Pass external slots = 10, but RateBased should use its own dynamic_slots (2)
        let decision = strategy.decide(&peers, 10, false);
        // 2 regular + 1 optimistic = 3
        assert_eq!(decision.to_unchoke.len(), 3);
    }
```

**Step 3: Run tests**

Run: `cargo test -p ferrite-session choker -- --nocapture`
Expected: All tests pass.

---

## Task 4: Refactor Choker to Dispatch via ChokerStrategy

**Files:**
- Modify: `crates/ferrite-session/src/choker.rs`

**Step 1: Rewrite the Choker struct as a dispatcher**

Replace the existing `Choker` struct and its `impl` block with:

```rust
/// Choker — dispatcher that delegates to a pluggable `ChokerStrategy`.
///
/// The `Choker` is the public-facing API used by `TorrentActor`. It holds
/// configuration (unchoke slots, seed mode) and delegates the actual
/// choking decision to the selected strategy.
pub(crate) struct Choker {
    strategy: Box<dyn ChokerStrategy>,
    unchoke_slots: usize,
    seed_mode: bool,
    /// Reference to rate-based strategy for throughput observation, if active.
    /// This is a raw pointer to the same allocation as `strategy` — safe because
    /// we only access it while `strategy` is alive and we own both.
    choking_algorithm: ChokingAlgorithm,
}

impl Choker {
    /// Create a choker with the default strategies and the given number of unchoke slots.
    pub fn new(unchoke_slots: usize) -> Self {
        Self {
            strategy: Box::new(FixedSlotsStrategy::new(SeedChokingAlgorithm::FastestUpload)),
            unchoke_slots,
            seed_mode: false,
            choking_algorithm: ChokingAlgorithm::FixedSlots,
        }
    }

    /// Create a choker with specific algorithm selections.
    pub fn with_algorithms(
        unchoke_slots: usize,
        seed_algorithm: SeedChokingAlgorithm,
        choking_algorithm: ChokingAlgorithm,
        upload_rate_limit: u64,
        min_slots: usize,
        max_slots: usize,
    ) -> Self {
        let strategy: Box<dyn ChokerStrategy> = match choking_algorithm {
            ChokingAlgorithm::FixedSlots => {
                Box::new(FixedSlotsStrategy::new(seed_algorithm))
            }
            ChokingAlgorithm::RateBased => {
                Box::new(RateBasedStrategy::new(
                    seed_algorithm,
                    upload_rate_limit,
                    min_slots,
                    max_slots,
                ))
            }
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

    /// Observe upload throughput for rate-based strategy.
    ///
    /// Only has an effect when `ChokingAlgorithm::RateBased` is active.
    /// When using `FixedSlots`, this is a no-op (SlotTuner handles slot count).
    pub fn observe_throughput(&mut self, throughput: u64) {
        if self.choking_algorithm == ChokingAlgorithm::RateBased {
            // Downcast to RateBasedStrategy to call observe_throughput
            // Safety: we know the strategy is RateBasedStrategy when choking_algorithm is RateBased
            if let Some(rate_based) = (self.strategy.as_mut() as &mut dyn std::any::Any)
                .downcast_mut::<RateBasedStrategy>()
            {
                rate_based.observe_throughput(throughput);
            }
        }
    }

    /// Decide which peers to unchoke and choke.
    pub fn decide(&mut self, peers: &[PeerInfo]) -> ChokeDecision {
        self.strategy.decide(peers, self.unchoke_slots, self.seed_mode)
    }

    /// Rotate the optimistic unchoke peer.
    pub fn rotate_optimistic(&mut self, peers: &[PeerInfo]) {
        self.strategy.rotate_optimistic(peers);
    }
}
```

Wait -- `dyn ChokerStrategy` does not implement `Any`. We need a different approach for the downcast. Let's use a simpler pattern: store the `RateBasedStrategy` separately or make `observe_throughput` part of the trait:

Actually, the cleanest approach is to **add `observe_throughput` to the `ChokerStrategy` trait** with a default no-op:

Add to the `ChokerStrategy` trait:

```rust
    /// Observe upload throughput for the latest interval.
    ///
    /// Rate-based strategies use this to adjust slot count dynamically.
    /// Default implementation is a no-op (used by FixedSlots).
    fn observe_throughput(&mut self, _throughput: u64) {
        // No-op for strategies that don't need throughput observation
    }

    /// Return the dynamically computed slot count, if the strategy manages its own.
    ///
    /// Returns `None` for strategies that use the externally provided slot count.
    fn dynamic_slots(&self) -> Option<usize> {
        None
    }
```

Then override in `RateBasedStrategy`:

```rust
    fn observe_throughput(&mut self, throughput: u64) {
        // (delegate to the existing observe_throughput method)
        self.observe_throughput_inner(throughput);
    }

    fn dynamic_slots(&self) -> Option<usize> {
        Some(self.dynamic_slots)
    }
```

(Rename the existing `observe_throughput` on `RateBasedStrategy` to `observe_throughput_inner` to avoid name collision, or just inline the logic in the trait method implementation.)

And simplify `Choker::observe_throughput`:

```rust
    pub fn observe_throughput(&mut self, throughput: u64) {
        self.strategy.observe_throughput(throughput);
    }
```

**Step 2: Update existing Choker tests to use the new API**

The existing tests construct `Choker::new(4)` and call `decide()` / `rotate_optimistic()` -- these should work unchanged since `Choker::new()` creates a `FixedSlotsStrategy` with `FastestUpload`. Verify that all existing test patterns still compile and pass.

**Step 3: Add integration tests for the Choker dispatcher**

```rust
    #[test]
    fn choker_with_round_robin() {
        let mut choker = Choker::with_algorithms(
            2,
            SeedChokingAlgorithm::RoundRobin,
            ChokingAlgorithm::FixedSlots,
            0,
            2,
            20,
        );
        choker.set_seed_mode(true);

        let peers = vec![
            seed_peer(6881, 100, true),
            seed_peer(6882, 200, true),
            seed_peer(6883, 300, true),
            seed_peer(6884, 400, true),
        ];

        let d1 = choker.decide(&peers);
        let d2 = choker.decide(&peers);
        // Round-robin should produce different unchoke sets
        let r1: Vec<SocketAddr> = d1.to_unchoke.iter().copied().collect();
        let r2: Vec<SocketAddr> = d2.to_unchoke.iter().copied().collect();
        assert_ne!(r1, r2);
    }

    #[test]
    fn choker_with_rate_based() {
        let mut choker = Choker::with_algorithms(
            4,
            SeedChokingAlgorithm::FastestUpload,
            ChokingAlgorithm::RateBased,
            0,
            2,
            20,
        );
        let peers = vec![
            peer(6881, 500, true),
            peer(6882, 400, true),
            peer(6883, 300, true),
            peer(6884, 200, true),
            peer(6885, 100, true),
        ];

        // Rate-based uses its own slot count (starts at 2), not the 4 passed to constructor
        let decision = choker.decide(&peers);
        // 2 regular + 1 optimistic = 3
        assert_eq!(decision.to_unchoke.len(), 3);

        // After observing throughput increase, slots should go up
        choker.observe_throughput(100_000);
        choker.observe_throughput(120_000);
        let decision = choker.decide(&peers);
        // 3 regular + 1 optimistic = 4
        assert_eq!(decision.to_unchoke.len(), 4);
    }

    #[test]
    fn choker_new_is_backward_compatible() {
        // Choker::new(4) should behave exactly like the old implementation
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
        // Top 4 by rate: 500, 400, 300, 200 + 1 optimistic = 5
        assert_eq!(decision.to_unchoke.len(), 5);
        assert!(decision.to_unchoke.contains(&addr(6882)));
        assert!(decision.to_unchoke.contains(&addr(6885)));
        assert!(decision.to_unchoke.contains(&addr(6883)));
        assert!(decision.to_unchoke.contains(&addr(6884)));
    }
```

**Step 4: Run tests**

Run: `cargo test -p ferrite-session choker -- --nocapture`
Expected: All tests pass.

---

## Task 5: Settings + TorrentConfig Integration

**Files:**
- Modify: `crates/ferrite-session/src/settings.rs`
- Modify: `crates/ferrite-session/src/types.rs`

**Step 1: Add new settings fields**

In `crates/ferrite-session/src/settings.rs`, add two new imports at the top:

```rust
use crate::choker::{SeedChokingAlgorithm, ChokingAlgorithm};
```

Add default helper functions:

```rust
fn default_seed_choking_algorithm() -> SeedChokingAlgorithm {
    SeedChokingAlgorithm::FastestUpload
}

fn default_choking_algorithm() -> ChokingAlgorithm {
    ChokingAlgorithm::FixedSlots
}
```

Add two new fields to the `Settings` struct, in the "Seeding" section (after `have_send_delay_ms`):

```rust
    /// Algorithm for ranking peers during seed-mode choking.
    #[serde(default = "default_seed_choking_algorithm")]
    pub seed_choking_algorithm: SeedChokingAlgorithm,
    /// Algorithm for determining the number of unchoke slots.
    #[serde(default = "default_choking_algorithm")]
    pub choking_algorithm: ChokingAlgorithm,
```

Add the fields to `Default for Settings`:

```rust
            seed_choking_algorithm: SeedChokingAlgorithm::FastestUpload,
            choking_algorithm: ChokingAlgorithm::FixedSlots,
```

Add the fields to `Settings::min_memory()` and `Settings::high_performance()` (use `..Self::default()` so they inherit defaults -- they already do, no changes needed since these use `..Self::default()`).

Add the fields to the manual `PartialEq` implementation:

```rust
            && self.seed_choking_algorithm == other.seed_choking_algorithm
            && self.choking_algorithm == other.choking_algorithm
```

**Step 2: Add fields to TorrentConfig**

In `crates/ferrite-session/src/types.rs`, add import:

```rust
use crate::choker::{SeedChokingAlgorithm, ChokingAlgorithm};
```

Add two fields to `TorrentConfig` (after `share_mode`):

```rust
    /// Algorithm for ranking peers during seed-mode choking.
    pub seed_choking_algorithm: SeedChokingAlgorithm,
    /// Algorithm for determining the number of unchoke slots.
    pub choking_algorithm: ChokingAlgorithm,
```

Update `Default for TorrentConfig`:

```rust
            seed_choking_algorithm: SeedChokingAlgorithm::FastestUpload,
            choking_algorithm: ChokingAlgorithm::FixedSlots,
```

Update `From<&Settings> for TorrentConfig`:

```rust
            seed_choking_algorithm: s.seed_choking_algorithm,
            choking_algorithm: s.choking_algorithm,
```

**Step 3: Add tests for the new settings fields**

In `settings.rs` tests:

```rust
    #[test]
    fn default_choking_algorithms() {
        let s = Settings::default();
        assert_eq!(s.seed_choking_algorithm, SeedChokingAlgorithm::FastestUpload);
        assert_eq!(s.choking_algorithm, ChokingAlgorithm::FixedSlots);
    }

    #[test]
    fn choking_algorithm_json_round_trip() {
        let mut s = Settings::default();
        s.seed_choking_algorithm = SeedChokingAlgorithm::AntiLeech;
        s.choking_algorithm = ChokingAlgorithm::RateBased;
        let json = serde_json::to_string(&s).unwrap();
        let decoded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.seed_choking_algorithm, SeedChokingAlgorithm::AntiLeech);
        assert_eq!(decoded.choking_algorithm, ChokingAlgorithm::RateBased);
    }
```

In `types.rs` tests:

```rust
    #[test]
    fn torrent_config_choking_defaults() {
        let cfg = TorrentConfig::default();
        assert_eq!(cfg.seed_choking_algorithm, SeedChokingAlgorithm::FastestUpload);
        assert_eq!(cfg.choking_algorithm, ChokingAlgorithm::FixedSlots);
    }

    #[test]
    fn torrent_config_from_settings_choking() {
        let mut s = Settings::default();
        s.seed_choking_algorithm = SeedChokingAlgorithm::RoundRobin;
        s.choking_algorithm = ChokingAlgorithm::RateBased;
        let cfg = TorrentConfig::from(&s);
        assert_eq!(cfg.seed_choking_algorithm, SeedChokingAlgorithm::RoundRobin);
        assert_eq!(cfg.choking_algorithm, ChokingAlgorithm::RateBased);
    }
```

**Step 4: Run tests**

Run: `cargo test -p ferrite-session settings types -- --nocapture`
Expected: All tests pass.

---

## Task 6: Wire Choker into TorrentActor

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs`

**Step 1: Update Choker construction in TorrentActor**

In `crates/ferrite-session/src/torrent.rs`, find the two places where `Choker::new(4)` is called (approximately lines 244 and 399). Replace both with:

```rust
            choker: Choker::with_algorithms(
                4,
                config.seed_choking_algorithm,
                config.choking_algorithm,
                config.upload_rate_limit,
                2,  // min_slots (matches auto_upload_slots_min default)
                20, // max_slots (matches auto_upload_slots_max default)
            ),
```

**Step 2: Wire throughput observation into the unchoke timer**

In the unchoke timer arm (approximately line 857-863), add throughput observation for the rate-based choker:

```rust
                // Unchoke timer
                _ = unchoke_interval.tick() => {
                    self.update_peer_rates();
                    // Auto upload slot tuning
                    self.slot_tuner.observe(self.upload_bytes_interval);
                    // Rate-based choker: observe throughput independently
                    self.choker.observe_throughput(self.upload_bytes_interval);
                    self.upload_bytes_interval = 0;
                    self.choker.set_unchoke_slots(self.slot_tuner.current_slots());
                    self.run_choker().await;
                    // Update streaming cursors and piece priorities
                    self.update_streaming_cursors();
                }
```

Note: When `ChokingAlgorithm::RateBased` is active, the `set_unchoke_slots()` call still works (it sets the field on Choker, but `RateBasedStrategy` ignores it and uses its own `dynamic_slots`). This is clean separation -- SlotTuner and RateBased don't interfere with each other.

**Step 3: Run full test suite**

Run: `cargo test -p ferrite-session -- --nocapture`
Expected: All tests pass.

---

## Task 7: Export Enums from Facade

**Files:**
- Modify: `crates/ferrite-session/src/lib.rs`
- Modify: `crates/ferrite/src/session.rs`
- Modify: `crates/ferrite/src/prelude.rs`

**Step 1: Make enums public from ferrite-session**

In `crates/ferrite-session/src/lib.rs`, change `choker` visibility and add re-exports. The choker module itself stays `pub(crate)` but we re-export the public-facing enums:

Add to the existing re-exports at the bottom of `lib.rs`:

```rust
pub use choker::{SeedChokingAlgorithm, ChokingAlgorithm};
```

**Step 2: Re-export from facade**

In `crates/ferrite/src/session.rs`, add to the `pub use ferrite_session` block:

```rust
    // Choking algorithm selection (M43)
    SeedChokingAlgorithm,
    ChokingAlgorithm,
```

In `crates/ferrite/src/prelude.rs`, add:

```rust
// Choking algorithms (M43)
pub use crate::session::{SeedChokingAlgorithm, ChokingAlgorithm};
```

**Step 3: Run full workspace tests**

Run: `cargo test --workspace`
Expected: All tests pass, zero clippy warnings.

Run: `cargo clippy --workspace -- -D warnings`
Expected: Clean.

---

## Task 8: ClientBuilder Methods

**Files:**
- Modify: `crates/ferrite/src/client.rs`

**Step 1: Read the ClientBuilder implementation**

Read `crates/ferrite/src/client.rs` to find the pattern for existing builder methods.

**Step 2: Add builder methods**

Add two new methods to the `ClientBuilder` impl block, following the existing pattern (owned `self`, returns `Self`):

```rust
    /// Set the seed-mode choking algorithm.
    ///
    /// Controls how peers are ranked for upload slots when the torrent is seeding.
    /// Default: `FastestUpload`.
    pub fn seed_choking_algorithm(mut self, algorithm: SeedChokingAlgorithm) -> Self {
        self.settings.seed_choking_algorithm = algorithm;
        self
    }

    /// Set the choking algorithm.
    ///
    /// `FixedSlots` uses a fixed unchoke count (adjusted by auto-slot tuning).
    /// `RateBased` dynamically adjusts the unchoke count based on upload capacity.
    /// Default: `FixedSlots`.
    pub fn choking_algorithm(mut self, algorithm: ChokingAlgorithm) -> Self {
        self.settings.choking_algorithm = algorithm;
        self
    }
```

**Step 3: Run workspace tests**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: Clean.

---

## Task 9: Remove `#[allow(dead_code)]` Annotations

**Files:**
- Modify: `crates/ferrite-session/src/choker.rs`

**Step 1: Remove dead_code annotations from choker types**

The original `Choker`, `PeerInfo`, and `ChokeDecision` had `#[allow(dead_code)]` annotations because they were written before being wired in. Now that they are fully integrated, remove these annotations.

Remove `#[allow(dead_code)]` from:
- `PeerInfo` struct
- `ChokeDecision` struct
- `Choker` struct (if still present)
- The `impl Choker` block (if still present)

**Step 2: Run final check**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: All 903+ tests pass (895 existing + 8+ new), zero warnings.

---

## Summary

| Task | What | Tests Added |
|------|------|-------------|
| 1 | Algorithm enums + PeerInfo.is_seed | 4 |
| 2 | ChokerStrategy trait + FixedSlots | 3 |
| 3 | RateBased strategy | 6 |
| 4 | Choker dispatcher refactor | 3 |
| 5 | Settings + TorrentConfig fields | 4 |
| 6 | TorrentActor wiring | 0 (integration) |
| 7 | Facade re-exports | 0 |
| 8 | ClientBuilder methods | 0 |
| 9 | Cleanup dead_code | 0 |
| **Total** | | **~20** |

**Version bump:** 0.41.0 -> 0.42.0
**Commit:** `feat: pluggable choking algorithms (M43)`

### Files Modified

| File | Change |
|------|--------|
| `crates/ferrite-session/src/choker.rs` | Major rewrite: enums, trait, 3 strategies, dispatcher |
| `crates/ferrite-session/src/settings.rs` | 2 new fields + defaults + PartialEq |
| `crates/ferrite-session/src/types.rs` | 2 new TorrentConfig fields + From impl |
| `crates/ferrite-session/src/torrent.rs` | Choker construction + throughput wiring + PeerInfo.is_seed |
| `crates/ferrite-session/src/lib.rs` | Re-export enums |
| `crates/ferrite/src/session.rs` | Re-export enums |
| `crates/ferrite/src/prelude.rs` | Re-export enums |
| `crates/ferrite/src/client.rs` | 2 new builder methods |
