# M104: Fixed-Depth Pipeline & Connection Overhaul

## Context

After M103 (per-block stealing + reactive snapshots), the torrent engine achieves **43.7 MB/s mean** vs rqbit's **72.3 MB/s** — a 1.65x gap. M103 improved speed +18% and reduced CPU/RSS, but the remaining 40% gap is in network pipeline utilisation, not CPU efficiency.

**M103 benchmark** (10 trials, Arch ISO 1.42 GiB):
- Mean: 43.7 MB/s | StdDev: 8.0 | Peak: 53.4 | RSS: 46 MiB | CPU: 8.5s
- **2/10 DHT cold-start failures** (0 MB/s timeouts)
- Cache miss rate 3.78% (was 0.95% in M102 — 3.97x worse)

**rqbit 8.1.1** (10 trials, same torrent):
- Mean: 72.3 MB/s | StdDev: 4.9 | Min: 64.1 | RSS: 41 MiB
- **10/10 reliability** | Consistent within 4.9 MB/s stddev

**rqbit's key architectural insight**: Fixed-depth pipeline with no dynamic sizing. Each peer gets `Semaphore(128)` permits, refilled instantly on block receipt. No AIMD, no slow-start, no tick-based evaluation. TCP's own flow control handles backpressure. This is simpler AND faster.

## Root Cause Analysis

### Gap 1: AIMD Pipeline Fights the Network (estimated 20-25% of gap)

```
Our AIMD cycle (1s ticks):
  Tick 0: depth=128 (slow-start)
  Tick 1: throughput↑ → depth=250 (cap)
  Tick 2: throughput↓5% (brief network burst) → EXIT slow-start PERMANENTLY
  Tick 3: consecutive_decrease=1
  Tick 4: consecutive_decrease=2
  Tick 5: consecutive_decrease=3 → depth=125 (HALVED!)
  Tick 6-35: +4/tick additive → 30 SECONDS to return to 250

rqbit: Semaphore(128), never changes. Block received → permit refilled →
next request sent. Zero latency. Zero false reductions.

Problem: AIMD penalises normal network behaviour (brief throughput dips).
Every halving event costs 30 seconds of recovery. On a 30-second download,
that's catastrophic — you never recover.
```

### Gap 2: Conservative Connection Strategy (estimated 10-15% of gap)

```
Current 3-phase connection ramp-up:
  0-15s:  100ms interval (aggressive)     ← good
  15-60s: 500ms interval (moderate)       ← too conservative
  60s+:   5000ms interval (maintenance)   ← starves peer recovery

rqbit: Immediate spawn on peer discovery, exponential backoff on failure
(200ms→1s→5s→30s). No time-based phases — demand-driven.

Problem: After 60s, replacing a disconnected peer takes 5 SECONDS.
```

### Gap 3: Max In-Flight Pieces Cap (estimated 5-10% of gap)

```rust
// Current: max(256, connected*3) capped at pieces/2
// Arch ISO: min(256, 2773) = 256 pieces max
// With 128 peers: ~2 pieces per peer — too low
```

With block stealing (M103), stolen blocks count against in-flight. The 256 cap is hit more often, blocking new reservations.

### Gap 4: DHT Cold-Start Failures (reliability — 2/10 trials)

```
Trials 6-7: 0.2 IPC, 98% idle, 0.6-0.7s CPU in 300s wall time.
Process couldn't find ANY peers.

Bootstrap flow:
  1. Ping saved nodes (from previous session)     ← may be stale
  2. FindNode to DNS bootstrap nodes               ← requires DNS resolution
  3. Node-count gate: opens at ≥8 nodes            ← blocks get_peers
  4. Bootstrap timeout: 10s forced open             ← fallback

Root cause unknown. Need diagnostic logging to determine:
- Are saved nodes stale? How many respond?
- Does DNS resolution succeed? How many bootstrap nodes found?
- What is routing table size at timeout? At get_peers time?
- Does the swarm have enough peers for this magnet at test time?
```

### Gap 5: Cache Miss Rate Regression (estimated 5% of gap)

M103 added BlockMaps atomic arrays scanned linearly by 128 peers. Cache miss rate 3.78% vs M102's 0.95%. May resolve with pipelining fixes (fewer dispatch cycles = less cache pressure).

## Architecture

```
    M104 Changes (4 focused improvements)
    ┌──────────────────────────────────────────────────┐
    │ 1. Fixed-Depth Pipeline (DELETE AIMD)             │
    │    Remove PeerPipeline tick-based AIMD entirely.  │
    │    Set fixed permit count per peer (128 or 256).  │
    │    Permits refill on block receipt (already how   │
    │    our semaphore works). No slow-start, no        │
    │    halving, no cooldown ticks.                    │
    │    Net effect: ~DELETE 100 lines from pipeline.rs │
    │                                                   │
    │ 2. Aggressive Connection + Backoff                │
    │    Replace ConnectPhase 3-phase timers with:      │
    │    - Immediate connect on peer discovery          │
    │    - Per-peer exponential backoff on failure:     │
    │      200ms → 1s → 5s → 30s → drop                │
    │    - Fixed 500ms scan interval for pending peers  │
    │    Net effect: DELETE ConnectPhase enum, simpler   │
    │    try_connect_peers() logic                      │
    │                                                   │
    │ 3. Raise max_in_flight_pieces: 256 → 512          │
    │    Default change + formula: max(512, connected*4) │
    │                                                   │
    │ 4. DHT Diagnostic Logging                         │
    │    Add structured logging at each bootstrap stage │
    │    to diagnose WHY 2/10 trials fail:              │
    │    - saved_nodes_pinged / saved_nodes_responded   │
    │    - dns_bootstrap_resolved / dns_nodes_found     │
    │    - routing_table_size at gate check + timeout   │
    │    - get_peers_issued / peers_found               │
    │    No behavioural changes — logging only.         │
    └──────────────────────────────────────────────────┘
```

## What already exists (reused)

| Component | File | Action |
|-----------|------|--------|
| Per-peer semaphore | peer.rs:627 | **Keep** — stop dynamically resizing it |
| `PeerPipeline` struct | pipeline.rs | **Simplify** — remove AIMD tick logic, keep rate tracking for stats |
| `PeerPipeline::tick()` | pipeline.rs:145-187 | **Gut** — remove all AIMD adjustment, keep EWMA for stats only |
| `ConnectPhase` enum | torrent.rs:67-70 | **Delete** — replace with backoff-based logic |
| `try_connect_peers()` | torrent.rs | **Simplify** — remove phase switching |
| `recalc_max_in_flight()` | torrent.rs | **Adjust** formula |
| `default_max_in_flight_pieces()` | settings.rs:217 | **Change** default 256→512 |
| DHT `bootstrap()` | dht/actor.rs:721 | **Add logging** around each phase |

## Files changed

| File | Change | Est. Lines |
|------|--------|------------|
| `pipeline.rs` | Remove AIMD adjustment from tick(), keep EWMA rate tracking + request_sent/block_received timing. Delete slow-start, consecutive_decrease, cooldown logic. | ~-80 |
| `peer.rs` | Remove pipeline tick resizing of semaphore. Fixed permit count on unchoke. Remove snub depth override path. | ~-30, +10 |
| `torrent.rs` | Delete ConnectPhase enum + 3-phase interval logic. Replace with fixed 500ms scan + per-peer backoff tracking. Adjust recalc_max_in_flight. | ~-40, +30 |
| `settings.rs` | Raise max_in_flight default 256→512. Remove deprecated AIMD settings (or mark deprecated). Add `fixed_pipeline_depth: usize` setting (default 128). | ~+10 |
| `dht/actor.rs` | Add structured diagnostic logging in bootstrap(), ping handler, node-count gate, timeout handler. | ~+30 |
| **Net total** | | **~-100 lines (net deletion!)** |

## Key decisions

| # | Decision | Rationale |
|---|----------|-----------|
| 1 | Delete AIMD, fixed-depth pipeline | rqbit proves fixed Semaphore(128) is faster than dynamic sizing. AIMD actively harms throughput on bursty networks. Simpler = faster. |
| 2 | Aggressive connect + per-peer backoff | Demand-driven, not time-based. Failed peers back off exponentially; healthy peers connect immediately. Matches rqbit's proven model. |
| 3 | max_in_flight 256→512 | More headroom for block stealing + parallel pieces. Formula: max(512, connected*4). |
| 4 | DHT logging, not retry logic | Investigate root cause before adding complexity. The 2/10 failure rate may be a stale-nodes bug, DNS issue, or swarm availability problem — each needs a different fix. |
| 5 | Keep PeerPipeline for rate stats | EWMA rate tracking is still useful for stats/alerts. Just remove the AIMD depth adjustment. |
| 6 | fixed_pipeline_depth setting | Allows A/B benchmarking with different depths (64, 128, 256). |

## Testing (12 tests)

### Unit (6)
| # | Test | Validates |
|---|------|-----------|
| 1 | `fixed_pipeline_depth_no_adjustment` | Pipeline tick() no longer changes queue_depth |
| 2 | `pipeline_ewma_still_tracks` | EWMA rate calculation still works after AIMD removal |
| 3 | `unchoke_sets_full_permits` | Unchoke event sets semaphore to fixed_pipeline_depth |
| 4 | `max_in_flight_512_default` | Default is 512, not 256 |
| 5 | `recalc_max_in_flight_formula` | max(512, connected*4) clamped to pieces/2 |
| 6 | `peer_backoff_on_connect_failure` | Failed connect → 200ms → 1s → 5s → 30s → drop |

### Integration (6)
| # | Test | Validates |
|---|------|-----------|
| 7 | `fixed_depth_sustained_throughput` | No depth reduction during download; permits stay at fixed level |
| 8 | `connection_recovery_after_churn` | After losing 20% peers, new connections happen within 1s (not 5s) |
| 9 | `backoff_prevents_hammering` | Failed peer not retried faster than backoff schedule |
| 10 | `high_in_flight_with_stealing` | 512 max_in_flight allows more parallel block steals |
| 11 | `pipeline_depth_configurable` | Setting fixed_pipeline_depth=256 produces 256 permits |
| 12 | `dht_diagnostic_logging_fires` | Bootstrap stages emit structured log events |

## Failure modes

| Codepath | Failure | Test | Handling | Silent? |
|----------|---------|------|----------|---------|
| Fixed depth: fast peer overwhelms slow link | TCP flow control (receive window) | Existing | Network-level backpressure | No |
| Fixed depth: too many requests queued | Peer chokes us | Existing | Choke handling clears pipeline | No |
| Backoff: all peers fail | All peers reach 30s backoff → dropped | T6 | New peers from tracker/DHT replace | No |
| max_in_flight=512: all pieces reserved | Phase 2 dispatch returns None for all peers | T10 | Phase 3 stealing kicks in | No |
| DHT logging: log volume too high | One-time bootstrap event | — | Bounded to ~10 log lines per bootstrap | No |

No critical gaps.

## Success criteria

- **Mean speed ≥55 MB/s** (from 43.7)
- **StdDev ≤6 MB/s** (from 8.0)
- **10/10 download reliability** (from 8/10) — or clear diagnosis of DHT failures
- **RSS <65 MiB** (from 46; allowing for higher in-flight)
- **Gap to rqbit ≤1.3x** (from 1.65x)

## NOT in scope

- **AIMD tuning** — we're deleting AIMD, not tuning it
- **Hysteresis on connection intervals** — replaced by backoff-based approach
- **DHT retry with backoff** — deferred until root cause investigation reveals what to fix
- **Cache optimization** (BlockMaps access patterns) — needs post-M104 profiling
- **io_uring / O_DIRECT** — separate milestone, orthogonal
- **Remove EndGame module** — still needed for last-pieces completion
- **Connection multiplexing / uTP improvements** — separate milestone
- **Delete pipeline.rs entirely** — tracked as TODO for post-M104 cleanup

## TODOS.md updates

1. **Delete pipeline.rs entirely** — After M104 benchmarks validate fixed-depth works, the AIMD machinery in pipeline.rs becomes dead code (~220 lines). The struct can be reduced to just EWMA rate tracking or inlined into peer.rs. Depends on: M104 benchmark validation.
2. **Cache access pattern optimization** — Profile BlockMaps access patterns after M104 pipeline fixes. If cache miss rate remains >2%, consider per-word scanning (trailing_ones), cache-line-aligned allocation, or separate hot/cold bitmap paths. Depends on: M104.
3. **Adaptive per-peer queue depth** — If fixed depth leaves performance on the table for heterogeneous peer speeds, consider per-peer adaptive depth based on measured RTT. Depends on: M104 results showing fixed depth is insufficient.

## Verification

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
./benchmarks/analyze.sh "<arch iso magnet>" -n 10
# Compare: speed, stddev, reliability, RSS vs M103 baseline
# Run rqbit comparison with same magnet for gap analysis
# Review DHT diagnostic logs from any failed trials
```

## Review summary

- Step 0: Scope Challenge — SMALL CHANGE selected
- Architecture Review: 1 issue (connection oscillation) — resolved with backoff instead of hysteresis
- Code Quality Review: 1 issue (AIMD constants as settings) — resolved by deleting AIMD entirely
- Test Review: diagram produced, 0 gaps
- Performance Review: 1 issue (max_in_flight memory) — acceptable, self-limiting
- NOT in scope: written
- What already exists: written
- TODOS.md updates: 3 items (1 accepted — pipeline.rs deletion)
- Failure modes: 0 critical gaps
