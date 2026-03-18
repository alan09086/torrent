# Peer Scoring System — Implementation Plan (M106)

## Context

The torrent engine currently uses simple heuristics for peer lifecycle management: tit-for-tat choking by download_rate, 8% turnover every 120s sorted by download_rate, and binary snub detection at 15s idle with immediate disconnect (M104). These are fragmented — three independent systems using a single signal each.

This plan implements a unified peer scoring system that computes a composite score from multiple signals (bandwidth, RTT, reliability, availability) and uses it to drive all peer lifecycle decisions: eviction, admission, and snub handling. The goal is faster ramp-up to peak throughput and better peer pool quality in large, well-seeded swarms.

**Spec:** `docs/superpowers/specs/2026-03-18-peer-scoring-system-design.md`

---

## Eng Review Amendments

The following amendments were decided during `/plan-eng-review` and MUST be followed:

1. **Add types.rs** to files list — update TorrentConfig fields + From<&Settings> + Default
2. **Use self.parole_pieces** directly (no parole_ips() method — it doesn't exist on BanManager)
3. **Hybrid snub**: score-driven + fast-evict after 10s snub in turnover tick
4. **Extract disconnect_peer()** helper — all disconnect cleanup call sites become 1-liners (DRY)
5. **Vec::sort_by** instead of itertools sorted_by (itertools not a dependency)
6. **Guard build_swarm_context** for empty iterator (return safe default SwarmContext)
7. **3 additional tests**: RTT EWMA convergence, snub fast-path, blocks_timed_out increment
8. **Update settings validation** for new fields (replace old peer_turnover validation)
9. **PeerScorer::new() takes &TorrentConfig** (not Settings — TorrentActor uses self.config)

---

## Task 1: Create `peer_scorer.rs` module

**File:** `crates/torrent-session/src/peer_scorer.rs` (NEW)
**Also:** `crates/torrent-session/src/lib.rs` — add `pub(crate) mod peer_scorer;` after line 31 (`pub(crate) mod peer_priority;`)

### Structs & enums to create:

```rust
pub(crate) enum SwarmPhase {
    Discovery,  // first 60s — aggressive churn
    Steady,     // after 60s — conservative churn
}

pub(crate) struct SwarmContext {
    pub max_rate: f64,
    pub min_rtt: f64,
    pub median_score: f64,
}

pub(crate) struct PeerScorer {
    probation_duration: Duration,
    discovery_phase_duration: Duration,
    discovery_churn_interval: Duration,
    discovery_churn_percent: f64,
    steady_churn_interval: Duration,
    steady_churn_percent: f64,
    min_score_threshold: f64,
    download_start: Option<Instant>,
    last_churn: Option<Instant>,
}
```

### Key methods:

- `new(config: &TorrentConfig) -> Self` — construct from TorrentConfig fields
- `start_download(&mut self)` — record download start time
- `phase(&self) -> SwarmPhase` — check elapsed since download_start
- `churn_interval(&self) -> Duration` — based on current phase
- `churn_percent(&self, median_score: f64) -> f64` — base percent, halved if median > 0.7
- `should_churn(&self, now: Instant) -> bool` — check if churn_interval has elapsed since last_churn
- `mark_churned(&mut self, now: Instant)` — update last_churn
- `is_in_probation(&self, connected_at: Instant) -> bool` — check probation window
- `min_score_threshold(&self) -> f64` — getter
- `compute_score(ewma_rate: f64, avg_rtt: Option<f64>, blocks_completed: u64, blocks_timed_out: u64, piece_count: u32, total_pieces: u32, snubbed: bool, ctx: &SwarmContext) -> f64` — pure function, compute composite score:
  - If snubbed, return 0.0
  - bandwidth_norm = ewma_rate / ctx.max_rate (guard div-by-zero)
  - rtt_norm = ctx.min_rtt / avg_rtt (lower = better, default 0.5 when no RTT)
  - reliability = blocks_completed / (blocks_completed + blocks_timed_out) (default 1.0)
  - availability = piece_count / total_pieces
  - Weights: bandwidth=0.4, rtt=0.2, reliability=0.25, availability=0.15
  - Return weighted sum clamped to 0.0..=1.0
- `build_swarm_context(peers: iterator of (ewma_rate, avg_rtt, score)) -> SwarmContext` — single pass to find max_rate, min_rtt, median_score (using SmallVec<[f64; 128]> + select_nth_unstable for median). Guard for empty iterator → return safe defaults.

### Tests (~10-11):
- Score computation with known inputs → verify weights
- Snubbed peers score exactly 0.0
- Probation check returns true/false based on elapsed time
- Phase transitions at correct elapsed time
- Churn dampening when median_score > 0.7
- Normalisation edge cases: all same rate, single peer, zero rates, no RTT data
- SwarmContext computation: max_rate, min_rtt, median correctness
- SwarmContext with empty iterator returns safe defaults
- RTT EWMA convergence (test formula standalone)
- should_churn timing logic
- churn_percent returns correct base values per phase

---

## Task 1B: Add new fields to `PeerState`

**File:** `crates/torrent-session/src/peer_state.rs`

### Changes to struct (after line 136, before closing brace):
```rust
pub score: f64,
pub blocks_completed: u64,
pub blocks_timed_out: u64,
pub avg_rtt: Option<f64>,
```

### Changes to `new()` constructor (lines 141-176):
Add defaults:
```rust
score: 0.0,
blocks_completed: 0,
blocks_timed_out: 0,
avg_rtt: None,
```

No tests needed — struct fields are exercised by Task 1 and integration tests.

---

## Task 2: Update Settings + TorrentConfig

**File:** `crates/torrent-session/src/settings.rs`

### Remove (default fns + struct fields + Default impl entries + validation):
- `peer_turnover` (default fn ~line 160, struct field ~line 648, Default impl ~line 854, validation ~line 993)
- `peer_turnover_cutoff` (default fn ~line 163, struct field ~line 651, Default impl ~line 855, validation ~line 997)
- `peer_turnover_interval` (default fn ~line 166, struct field ~line 654, Default impl ~line 856)

### Add (following existing pattern: default fn + `#[serde(default = "...")]` field + Default impl):
```rust
// Default functions
fn default_probation_duration_secs() -> u64 { 20 }
fn default_discovery_phase_secs() -> u64 { 60 }
fn default_discovery_churn_interval_secs() -> u64 { 30 }
fn default_discovery_churn_percent() -> f64 { 0.10 }
fn default_steady_churn_interval_secs() -> u64 { 120 }
fn default_steady_churn_percent() -> f64 { 0.05 }
fn default_min_score_threshold() -> f64 { 0.15 }

// Struct fields (in peer turnover section, replacing old fields)
pub probation_duration_secs: u64,
pub discovery_phase_secs: u64,
pub discovery_churn_interval_secs: u64,
pub discovery_churn_percent: f64,
pub steady_churn_interval_secs: u64,
pub steady_churn_percent: f64,
pub min_score_threshold: f64,
```

### Add validation (replacing old peer_turnover validation):
```rust
if !(0.0..=1.0).contains(&self.discovery_churn_percent) {
    errors.push("discovery_churn_percent must be between 0.0 and 1.0".into());
}
if !(0.0..=1.0).contains(&self.steady_churn_percent) {
    errors.push("steady_churn_percent must be between 0.0 and 1.0".into());
}
if !(0.0..=1.0).contains(&self.min_score_threshold) {
    errors.push("min_score_threshold must be between 0.0 and 1.0".into());
}
```

**File:** `crates/torrent-session/src/types.rs`

### Update TorrentConfig:
- Remove: `peer_turnover`, `peer_turnover_cutoff`, `peer_turnover_interval` fields
- Add: `probation_duration_secs`, `discovery_phase_secs`, `discovery_churn_interval_secs`, `discovery_churn_percent`, `steady_churn_interval_secs`, `steady_churn_percent`, `min_score_threshold`
- Update `From<&Settings>` impl to map new fields
- Update `Default` impl with correct defaults
- Update any tests that reference old fields

### Also update any references to removed settings:
- Search for `peer_turnover` across the codebase — used in `run_peer_turnover()` (torrent.rs) and TorrentConfig. References in torrent.rs will be replaced in Tasks 5.

---

## Task 3: Extract disconnect_peer() helper + RTT capture

**File:** `crates/torrent-session/src/torrent.rs`

### Extract disconnect_peer() helper:

The peer disconnect/cleanup pattern is duplicated 3 times:
1. Snub handler (lines ~2345-2394)
2. run_peer_turnover() (lines ~6125-6170)
3. PeerEvent::Disconnected handler

Extract into:
```rust
fn disconnect_peer(&mut self, addr: SocketAddr, reason: &str) {
    // BEP 16: super-seed cleanup
    if let Some(ref mut ss) = self.super_seed {
        ss.peer_disconnected(addr);
    }
    if let Some(peer) = self.peers.remove(&addr) {
        // End-game cleanup
        if self.end_game.is_active() {
            self.end_game.peer_disconnected(addr);
        }
        // M93: Release all pieces owned by this peer
        if let Some(slab_idx) = self.peer_slab.remove_by_addr(&addr) {
            for piece in 0..self.num_pieces {
                if self.piece_owner[piece as usize] == Some(slab_idx) {
                    self.piece_owner[piece as usize] = None;
                    if let Some(ref states) = self.atomic_states {
                        states.release(piece);
                    }
                }
            }
        }
        // Update availability
        for piece in 0..self.num_pieces {
            if peer.bitfield.get(piece) {
                self.availability[piece as usize] =
                    self.availability[piece as usize].saturating_sub(1);
            }
        }
        // Send shutdown command
        let _ = peer.cmd_tx.try_send(PeerCommand::Shutdown);
        post_alert(&self.alert_tx, &self.alert_mask,
            AlertKind::PeerDisconnected {
                info_hash: self.info_hash,
                addr,
                reason: Some(reason.into()),
            },
        );
    }
    self.suggested_to_peers.remove(&addr);
}
```

Replace all 3 existing disconnect patterns with calls to `self.disconnect_peer(addr, "reason")`.

### Capture RTT at block receipt (~line 4074):

**Before:**
```rust
peer.pipeline.block_received(index, begin, length, now);
```

**After:**
```rust
if let Some(rtt) = peer.pipeline.block_received(index, begin, length, now) {
    let rtt_secs = rtt.as_secs_f64();
    const ALPHA: f64 = 0.3;
    peer.avg_rtt = Some(match peer.avg_rtt {
        Some(prev) => ALPHA * rtt_secs + (1.0 - ALPHA) * prev,
        None => rtt_secs,
    });
}
peer.blocks_completed += 1;
```

### Increment blocks_timed_out:
Find where `timed_out_blocks()` is called and increment `peer.blocks_timed_out` by the count.

---

## Task 4: Integrate scoring into tick loop + snub revert

**File:** `crates/torrent-session/src/torrent.rs`

### Add PeerScorer field to TorrentActor:
```rust
scorer: PeerScorer,
```
Initialize in constructor from config.

### Call `scorer.start_download()`:
In the state transition to `TorrentState::Downloading` (~lines 3434, 4264, 5374), call `self.scorer.start_download()`.

### Modify tick handler (lines 2323-2410):
After the existing `peer.pipeline.tick()` loop, add scoring:

**Pass 1 — collect swarm context:**
```rust
let ctx = PeerScorer::build_swarm_context(
    self.peers.values()
        .filter(|p| !self.scorer.is_in_probation(p.connected_at))
        .map(|p| (p.pipeline.ewma_rate(), p.avg_rtt, p.score))
);
```

**Pass 2 — score each peer:**
```rust
for peer in self.peers.values_mut() {
    if self.scorer.is_in_probation(peer.connected_at) {
        peer.score = 0.5;
    } else {
        peer.score = PeerScorer::compute_score(
            peer.pipeline.ewma_rate(),
            peer.avg_rtt,
            peer.blocks_completed,
            peer.blocks_timed_out,
            peer.bitfield.count_ones(),
            self.num_pieces,
            peer.snubbed,
            &ctx,
        );
    }
}
```

### Revert M104 snub handling:
**Keep:** The detection loop that sets `peer.snubbed = true` (lines 2330-2340).
**Remove:** The entire snub cleanup block (lines ~2343-2394) that does piece release, peer removal, slab cleanup, availability decrement. The scoring system zeros their score, and the turnover cycle evicts them.

---

## Task 5: Replace `run_peer_turnover()` with `run_scored_turnover()` + admission

**File:** `crates/torrent-session/src/torrent.rs`

### Replace `run_peer_turnover()` (lines 6052-6188):

New `run_scored_turnover()`:

```rust
async fn run_scored_turnover(&mut self) {
    // 1. Only during Downloading
    if self.state != TorrentState::Downloading { return; }

    let now = Instant::now();

    // 2. Snub fast-path: always evict peers snubbed for >10s regardless of churn timer
    let snub_evict: Vec<SocketAddr> = self.peers.iter()
        .filter(|(_, p)| p.snubbed && p.score < self.scorer.min_score_threshold())
        .map(|(addr, _)| *addr)
        .collect();
    for addr in &snub_evict {
        self.disconnect_peer(*addr, "snubbed low-score eviction");
    }
    if !snub_evict.is_empty() {
        self.recalc_max_in_flight();
        self.mark_snapshot_dirty();
        self.try_connect_peers();
    }

    // 3. Check if it's time for regular churn
    if !self.scorer.should_churn(now) { return; }
    self.scorer.mark_churned(now);

    // 4. Skip during endgame
    if self.end_game.is_active() { return; }

    // 5. Skip if < 10 scored peers (small swarm protection)
    let scored_count = self.peers.values()
        .filter(|p| !self.scorer.is_in_probation(p.connected_at))
        .count();
    if scored_count < 10 { return; }

    // 6. Collect swarm context for dampening
    let ctx = PeerScorer::build_swarm_context(
        self.peers.values()
            .filter(|p| !self.scorer.is_in_probation(p.connected_at))
            .map(|p| (p.pipeline.ewma_rate(), p.avg_rtt, p.score))
    );
    let churn_pct = self.scorer.churn_percent(ctx.median_score);

    // 7. Build eligible set (exclude probation, seeds, parole)
    let parole_ips: std::collections::HashSet<std::net::IpAddr> = self
        .parole_pieces.values()
        .filter_map(|p| p.parole_peer)
        .collect();

    let mut eligible: Vec<(SocketAddr, f64)> = self.peers.iter()
        .filter(|(_, p)| {
            !self.scorer.is_in_probation(p.connected_at)
            && p.bitfield.count_ones() != self.num_pieces  // not a seed
            && !parole_ips.contains(&p.addr.ip())
        })
        .filter(|(_, p)| p.score < self.scorer.min_score_threshold())
        .map(|(addr, p)| (*addr, p.score))
        .collect();

    if eligible.is_empty() { return; }

    // Sort by score ascending (worst first) — use Vec::sort_by, NOT itertools
    eligible.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    // 8. Disconnect bottom N
    let n = ((eligible.len() as f64 * churn_pct).floor() as usize).max(1);
    let to_disconnect: Vec<SocketAddr> = eligible.into_iter().take(n).map(|(addr, _)| addr).collect();

    for addr in &to_disconnect {
        self.disconnect_peer(*addr, "scored turnover");
    }
    self.recalc_max_in_flight();
    self.mark_snapshot_dirty();

    // 9. Connect replacements
    let peers_before = self.peers.len();
    self.try_connect_peers();
    let replaced = self.peers.len().saturating_sub(peers_before);

    post_alert(&self.alert_tx, &self.alert_mask,
        AlertKind::PeerTurnover {
            info_hash: self.info_hash,
            disconnected: to_disconnect.len(),
            replaced,
        },
    );
}
```

### Update the caller:
Replace the turnover timer interval to 1 second (checking frequently since `should_churn()` gates actual work). Replace `run_peer_turnover()` call with `run_scored_turnover()`.

### Modify `try_connect_peers()` for score-based admission:

At the top of the while loop (lines ~6449-6457), when at capacity:
```rust
if self.peers.len() >= self.effective_max_connections() {
    // Find lowest-scoring non-probation peer
    let worst = self.peers.iter()
        .filter(|(_, p)| !self.scorer.is_in_probation(p.connected_at))
        .min_by(|(_, a), (_, b)| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal));

    if let Some((worst_addr, worst_peer)) = worst {
        if worst_peer.score < self.scorer.min_score_threshold() {
            let worst_addr = *worst_addr;
            self.disconnect_peer(worst_addr, "score-based admission");
            self.recalc_max_in_flight();
            self.mark_snapshot_dirty();
        } else {
            break; // Pool is strong enough
        }
    } else {
        break;
    }
}
```

---

## Task 6: Integration tests

**File:** `crates/torrent-session/src/torrent.rs` (or peer_scorer.rs test module)

### Tests (~8-10):
- Scored turnover evicts lowest-scoring peers (not highest-rated ones)
- Probation peers survive turnover
- Score-based admission replaces low-scorer when at capacity
- Small swarm (< 10 scored peers) disables eviction
- Endgame suppresses eviction
- SwarmPhase Discovery → Steady transition changes churn interval
- Snubbed peer gets low score and is evicted on next churn (not immediately)
- Snub fast-path: snubbed peer below threshold evicted regardless of churn timer
- Adaptive dampening: high median score reduces churn percent
- blocks_timed_out counter increments on timeout detection

---

## Task 7: Benchmark validation & cleanup

### Benchmark:
```bash
./benchmarks/analyze.sh "<current arch iso magnet>" -n 10
```
Run before and after. Compare average throughput, peak throughput, time-to-peak, RSS, CPU.

### Post-merge docs:
- Update README.md (test count, version, milestone entry)
- Update CHANGELOG.md (v0.106.0 entry)
- Update CLAUDE.md milestone section

---

## Verification

1. `cargo test --workspace` — all 1565+ tests pass (plus ~18-20 new)
2. `cargo clippy --workspace -- -D warnings` — zero warnings
3. Benchmark: `./benchmarks/analyze.sh <magnet> -n 10` — no regression, ideally faster ramp-up
4. Manual smoke test: download a well-seeded torrent, observe peer churn in logs

## Files Summary

| File | Change |
|------|--------|
| `crates/torrent-session/src/peer_scorer.rs` | **NEW** — PeerScorer, SwarmPhase, SwarmContext, scoring logic |
| `crates/torrent-session/src/peer_state.rs` | Add score, blocks_completed, blocks_timed_out, avg_rtt fields |
| `crates/torrent-session/src/settings.rs` | Remove 3 old turnover settings, add 7 new scoring settings |
| `crates/torrent-session/src/types.rs` | Update TorrentConfig: remove 3 old, add 7 new fields |
| `crates/torrent-session/src/torrent.rs` | disconnect_peer helper, RTT capture, tick scoring, snub revert, scored turnover, admission |
| `crates/torrent-session/src/lib.rs` | Add `mod peer_scorer` |
