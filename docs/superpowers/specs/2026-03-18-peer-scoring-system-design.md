# Peer Scoring System Design

**Date:** 2026-03-18
**Status:** Approved
**Milestone:** M106 (tentative)

## Overview

An intelligent peer scoring system for IronTide that maximises download throughput in large, well-seeded swarms by maintaining high-quality connections and evicting low-quality ones. A single composite score drives all peer lifecycle decisions — connection admission, turnover/eviction, and steal-candidate filtering — through a centralised scorer in TorrentActor.

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Decision points | All three (admission, eviction, dispatch-adjacent) | Unified scoring avoids fragmented heuristics |
| Scoring emphasis | Bandwidth-dominant | Maximises raw throughput in well-seeded swarms |
| New peer handling | Probation window (20s) | Simple, uses existing `connected_at` field |
| Low-scorer handling | Hard cutoff (full disconnect) | No shortage of replacement peers in target swarms |
| Churn strategy | Adaptive (aggressive early, conservative later) | Finds optimal peers fast, then stabilises |
| Architecture | Centralised scorer in TorrentActor tick loop | Trivial cost for 128 peers, no sync overhead |

## Scoring Model

### Composite Score

A single `f64` per peer, recomputed every 500ms tick:

```
score = (0.70 × bandwidth_norm)
      + (0.15 × rtt_norm)
      + (0.10 × reliability_norm)
      + (0.05 × availability_norm)
```

### Component Definitions

| Component | Weight | Signal | Normalisation |
|-----------|--------|--------|---------------|
| `bandwidth_norm` | 0.70 | `pipeline.ewma_rate()` (bytes/sec) | `peer_rate / max_rate` across swarm. Best peer = 1.0. |
| `rtt_norm` | 0.15 | Average RTT from `pipeline.request_times` | `min_rtt / peer_rtt` (inverted). Fastest = 1.0. No data = 0.5. |
| `reliability_norm` | 0.10 | `blocks_timed_out / blocks_completed` | `1.0 - timeout_ratio`, clamped to [0.0, 1.0]. Zero timeouts = 1.0. |
| `availability_norm` | 0.05 | `bitfield.count_ones() / total_pieces` | Seed = 1.0, partial peers proportional. |

### Probation

Peers where `connected_at.elapsed() < probation_duration` (default 20s):
- Assigned a fixed score of `0.5` (median)
- Exempt from eviction
- Real score takes over after probation expires

## Infrastructure

### `PeerScorer` struct

Owned by `TorrentActor`. No channels, no Arc, no async.

```rust
pub struct PeerScorer {
    probation_duration: Duration,  // default 20s, from Settings
    swarm_phase: SwarmPhase,       // tracks download lifecycle
    download_start: Option<Instant>,
}

pub enum SwarmPhase {
    /// First 60s of download — aggressive churn
    Discovery,
    /// After 60s — conservative churn
    Steady,
}
```

### SwarmPhase Churn Parameters

| Phase | Duration | Churn Interval | Churn Percentage |
|-------|----------|----------------|------------------|
| Discovery | First 60s | Every 30s | 10% of scored peers |
| Steady | After 60s | Every 120s | 5% of scored peers |

Phase is determined by elapsed time since `TorrentState::Downloading` was entered.

### Swarm Context

Collected in a single O(n) pass over non-probation peers during the tick:

```rust
struct SwarmContext {
    max_rate: f64,    // highest EWMA rate across peers
    min_rtt: f64,     // lowest average RTT across peers
    median_score: f64, // for adaptive churn dampening
}
```

### New Fields on `PeerState`

```rust
pub score: f64,              // composite score, updated every tick
pub blocks_completed: u64,   // lifetime completed blocks
pub blocks_timed_out: u64,   // lifetime timed-out blocks
```

`blocks_completed` incremented on `PieceData`/`PieceBlocksBatch` events.
`blocks_timed_out` incremented when `timed_out_blocks()` detects stale requests.

### Settings Additions

```rust
pub probation_duration_secs: u64,       // default 20
pub discovery_phase_secs: u64,          // default 60
pub discovery_churn_interval_secs: u64, // default 30
pub discovery_churn_percent: f64,       // default 0.10
pub steady_churn_interval_secs: u64,    // default 120
pub steady_churn_percent: f64,          // default 0.05
pub min_score_threshold: f64,           // default 0.15
```

### Tick Loop Integration

On every 500ms pipeline tick in `TorrentActor`:

1. **Compute swarm context** — collect `max_rate`, `min_rtt` across all non-probation peers (single pass, can piggyback on existing EWMA update iteration)
2. **Score each peer** — `scorer.compute_score(peer, &swarm_ctx)` → writes `peer.score`
3. **Existing tick work continues** — EWMA updates, snub checks, pipeline management (unchanged)

## Decision Point Integration

### 1. Turnover/Eviction — replaces `run_peer_turnover()`

New `run_scored_turnover()`:

1. Check `SwarmPhase` to determine churn interval and percentage
2. Skip peers in probation window
3. Collect all scored peers below `min_score_threshold` (0.15)
4. Sort by `score` ascending (worst first)
5. Disconnect bottom N peers (N = `churn_percent × scored_peer_count`)
6. Existing cleanup — release owned pieces, update availability, free slab slots

**Adaptive dampening:** If the median peer score > 0.7, reduce churn percentage by half. The swarm is already high quality.

### 2. Connection Admission — modifies `try_connect_peers()`

When at `max_peers_per_torrent` and a new peer is available:

1. Find the lowest-scoring non-probation peer
2. If that peer's score < `min_score_threshold` (0.15), disconnect it and admit the new peer
3. Otherwise, skip — the current peer pool is strong enough

### 3. Dispatch — minimal, indirect influence

The lock-free `AtomicPieceStates` + `PeerDispatchState` dispatch system is untouched. Scoring influences dispatch indirectly:

- Low-scoring peers get evicted → fewer competitors for pieces
- Existing choker already unchokes best downloaders (tit-for-tat)
- **One targeted addition:** when building `StealCandidates`, skip blocks currently owned by high-scoring peers (score > 0.8). Don't steal from top contributors.

**Rationale:** The M93 lock-free dispatch is the engine's highest-throughput path. Adding score-based branching would reintroduce complexity in the exact place where the biggest performance gains were achieved.

## Snub Integration

Snubbing becomes a score event rather than a standalone disconnect system:

- When a peer is idle for `snub_timeout_secs`, set `peer.snubbed = true` as before
- Snubbed peers: `reliability_norm = 0.0`, `bandwidth_norm = 0.0`
- Score drops to ~`0.05 × availability_norm` — well below `min_score_threshold`
- Next turnover cycle evicts them through the normal scoring pipeline

**Benefit:** Single code path for all peer eviction. No separate snub disconnect logic.

## Edge Cases

### Small swarms (< 10 scored peers)

Disable score-based eviction entirely. Probation and scoring still compute (for stats/logging), but no eviction fires. Every connection matters when peers are scarce.

### All peers are slow

Normalisation is relative (`peer_rate / max_rate`), so even in a slow swarm the best peer scores 1.0. If the spread is narrow (worst = 0.6), nobody falls below `min_score_threshold`. This is correct: churning doesn't help in a uniformly slow swarm.

### Seed mode

Scoring is only active during `TorrentState::Downloading`. Once complete, the scorer stops and the existing seed-mode choker takes over.

### Endgame mode

During endgame (all pieces reserved or complete), disable eviction. Every peer is needed to finish the last blocks. Scoring continues to compute but eviction is suppressed.

### Reconnection after eviction

Evicted peers return to the `connect_backoff` map with their existing backoff timer. They can reconnect later but face the same probation window. No permanent ban.

## Testing Strategy

### Unit tests (in `peer_scorer.rs`)

- Score computation with known inputs → verify weights produce expected output
- Probation peers always return 0.5
- Snubbed peers score near zero
- SwarmPhase transitions at correct elapsed time
- Normalisation edge cases: all peers same rate, single peer, zero rates

### Integration tests (in `torrent.rs` or dedicated test module)

- Scored turnover evicts lowest-scoring peers
- Probation peers survive turnover
- Connection admission replaces low-scorer with new peer
- Small swarm (< 10) disables eviction
- Endgame suppresses eviction
- Steal-candidate filtering skips high-scorer blocks
- SwarmPhase Discovery → Steady transition changes churn parameters

### Benchmark validation

- Run `./benchmarks/analyze.sh <magnet> -n 10` before and after
- Compare: average throughput, peak throughput, time-to-completion
- Verify no regression in RSS or CPU usage
- Target: measurable throughput improvement in well-seeded swarms (Arch ISO)

## Files Modified

| File | Change |
|------|--------|
| `crates/torrent-session/src/peer_scorer.rs` | **New** — `PeerScorer`, `SwarmPhase`, `SwarmContext`, score computation |
| `crates/torrent-session/src/peer_state.rs` | Add `score`, `blocks_completed`, `blocks_timed_out` fields |
| `crates/torrent-session/src/torrent.rs` | Integrate scorer into tick loop, replace `run_peer_turnover()` with `run_scored_turnover()`, modify admission in `try_connect_peers()` |
| `crates/torrent-session/src/piece_reservation.rs` | Steal-candidate filtering for high-scoring peers |
| `crates/torrent-session/src/settings.rs` | Add scoring settings |
| `crates/torrent-session/src/lib.rs` | Add `mod peer_scorer` |

## Non-Goals

- No score persistence across sessions — scores are ephemeral
- No peer reputation system — each connection starts fresh
- No machine learning or complex adaptive weights — simple linear combination
- No changes to the wire protocol — scoring uses existing signals
- No dispatch hot-path modifications — scoring operates through eviction/admission only
