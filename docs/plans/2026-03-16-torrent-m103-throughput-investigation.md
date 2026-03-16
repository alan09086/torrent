# M103 Throughput Investigation — v0.102.0 vs rqbit 8.1.1

## Benchmark Data (2026-03-16, Arch ISO 1452 MB)

### torrent v0.102.0 (10 trials)
| Metric | Mean | Median | StdDev | Min | Max |
|--------|------|--------|--------|-----|-----|
| Speed | 37.0 MB/s | 34.1 MB/s | 5.7 MB/s | 30.9 | 49.9 |
| CPU | 9.6s | — | — | 6.6 | 12.2 |
| RSS | 57 MiB | — | — | 31 | 77 |
| Major faults | 0 | — | — | — | — |
| Minor faults | 47,677 | — | — | — | — |
| Vol ctx sw | 135,902 | — | — | — | — |
| Invol ctx sw | 434 | — | — | — | — |

Perf HW counters (trial 3, 49.9 MB/s):
- 8.9B cycles, 20.1B instructions → 2.35 IPC
- 125M cache refs, 1.2M cache misses → 0.95% miss rate
- 16.8K page faults (minor)

### rqbit 8.1.1 (2 trials)
| Trial | Speed | CPU | RSS | Minor faults | Vol ctx sw | Invol ctx sw |
|-------|-------|-----|-----|-------------|-----------|-------------|
| 1 | 67.5 MB/s | 10.4s | 42 MiB | 10,390 | 127,770 | 1,007 |
| 2 | 68.5 MB/s | 10.3s | 44 MiB | 10,694 | 124,015 | 1,507 |

Perf HW counters (trial 1):
- 13.8B cycles, 23.0B instructions → 1.67 IPC
- 254M cache refs, 2.9M cache misses → 1.14% miss rate

### Head-to-head comparison
| Metric | torrent | rqbit | Ratio | Interpretation |
|--------|---------|-------|-------|---------------|
| Speed | 37.0 MB/s | 68.0 MB/s | **1.84x** | rqbit moves 84% more data |
| CPU total | 9.6s | 10.4s | 0.92x | We use LESS CPU |
| RSS | 57 MiB | 43 MiB | 1.33x | rqbit 25% leaner |
| Minor faults | 47,677 | 10,542 | **4.5x** | Our page fault overhead |
| Vol ctx sw | 135,902 | 125,893 | 1.08x | ~same |
| IPC | 2.35 | 1.67 | 1.41x | We're more efficient per-cycle |
| Cycles | 8.9B | 13.7B | 0.65x | rqbit uses more (moving more data) |

**Key insight: We are NOT CPU-bound. We use less CPU than rqbit but get half the speed.**
**The bottleneck is network pipeline utilisation — we don't keep the pipe full.**

## Flamegraph Analysis (trial 2, profile run)

Top CPU consumers (excluding hash workers):
| Function | CPU % | Notes |
|----------|-------|-------|
| tokio-runtime-w (scheduling) | 55.3% | Event loop — expected |
| [unknown] (kernel) | 19.4% | Syscall overhead |
| **EndGame::pick_block** | **6.3%** | Piece picker thrashing |
| tokio task::poll | 4.9% | Task scheduling |
| **try_connect_peers** | **2.9%** | Connection management |
| run_peer | 2.8% | Peer I/O |
| **BytesMut::reserve + realloc** | **3.7%** | Codec buffer churn |
| realloc | 1.9% | Heap allocation |

## Heaptrack Analysis (trial 2)

- Top allocator: `BytesMut::reserve_inner` via `MessageCodec::decode` — 84K calls, 7.85M peak
- 83% temporary allocations (high churn)
- Total peak heap: 9.74M (very lean)
- Secondary: `RawVecInner::finish_grow` — 120K calls total

## Root Cause Analysis

### 1. Piece-level reservation blocks parallelism (PRIMARY)
- **Our model**: One peer owns an entire piece. If peer A is slow on piece 7, peer B must pick a different piece.
- **rqbit model**: Per-block stealing. If peer A is slow, peer B steals remaining blocks mid-piece.
- **Evidence**: EndGame::pick_block at 6.3% CPU — thrashing trying to find available work when pieces are locked by slow peers.
- **Impact**: With 128 peers and piece-level locks, fast peers starve waiting for slow peers to finish.

### 2. 500ms snapshot rebuild adds latency
- **Our model**: `AvailabilitySnapshot::build()` every 500ms — O(n) bucket sort of all pieces.
- **rqbit model**: Reactive — piece ordering on-demand during acquisition. No timer.
- **Impact**: 500ms stale data means fast peers can't react to newly available pieces for up to half a second.

### 3. End-game mode overhead
- **Our model**: Explicit EndGame state machine with dedicated pick_block — 6.3% CPU.
- **rqbit model**: Dynamic completion detection via natural per-block stealing. No separate end-game.
- **Impact**: CPU wasted on piece selection rather than data transfer.

### 4. Page fault overhead (4.5x)
- 47K minor faults vs rqbit's 10K — BufferPool and codec buffer allocations hitting unmapped pages.
- Not the primary bottleneck but contributes to latency.

## Architectural Comparison

| Aspect | rqbit | Our Engine | Impact |
|--------|-------|-----------|--------|
| Peer storage | DashMap (lock-free) | HashMap + AtomicPieceStates | rqbit: less contention |
| Piece selection | Natural order + theft | Bucket-sort rarest-first (500ms) | rqbit: O(1); ours: O(n) periodic |
| Piece ownership | Per-block stealing | Per-piece reservation | rqbit: better parallelism |
| Snapshot updates | Implicit in acquisition | Explicit 500ms timer | rqbit: reactive |
| Block dispatch | Concurrent within piece | Sequential within piece | rqbit: faster for stragglers |
| Steal handling | Cancels inflight requests | Piece-level release | rqbit: fine-grained |
| End-game | Dynamic completion | Explicit state transition | rqbit: adaptive |
| Queue depth | Adaptive semaphore (128 base) | Fixed 128 | rqbit: responsive to choke/unchoke |
| I/O buffering | Fixed 32KB ring + lazy | 64 MiB BufferPool + ARC | Different tradeoffs |
| Message framing | DoubleBufHelper (ring) | tokio_util codec | Both zero-copy |

## Historical Context

We had better speeds in earlier milestones (M83: 56.8 MB/s, M95: 66.9 MB/s). The speed regression came with M93 (lock-free piece dispatch) which changed the dispatch architecture. While M93 improved reliability and correctness, the piece-level reservation model is more conservative than the previous approach.

## Profiling artifacts

- Flamegraph: `/mnt/TempNVME/bench-results/20260316-112959/flamegraph.svg`
- Heaptrack: `/mnt/TempNVME/bench-results/20260316-112959/heaptrack.txt`
- Heaptrack raw: `/mnt/TempNVME/bench-results/20260316-112959/heaptrack.zst`
- Per-trial perf stat: `/mnt/TempNVME/bench-results/20260316-112959/trial-*/perf-stat.txt`
- Summary: `/mnt/TempNVME/bench-results/20260316-112959/summary.txt`
- CSV: `/mnt/TempNVME/bench-results/20260316-112959/results.csv`
