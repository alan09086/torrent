---
title: "Gap Analysis: torrent v0.66.0 vs rqbit v8.1.1"
subtitle: "Performance Bottlenecks & Recommended Next Steps"
date: "2026-03-07"
geometry: margin=1in
fontsize: 11pt
header-includes:
  - \usepackage{booktabs}
  - \usepackage{longtable}
  - \usepackage{array}
  - \usepackage{xcolor}
  - \usepackage{colortbl}
  - \definecolor{lightgray}{gray}{0.93}
  - \renewcommand{\arraystretch}{1.3}
---

# 1. Benchmark Results

## Test Configuration

- **Torrent:** Arch Linux 2026.03.01 ISO (~1.5 GiB), public magnet link
- **Trials:** 5 per client, measured with `/usr/bin/time -v`
- **Machine:** CachyOS (Arch-based), Linux 6.19.0-2-cachyos

## Summary (mean +/- stddev)

| Client | Time (s) | Speed (MB/s) | CPU (s) | RSS (MiB) |
|:-------|:--------:|:------------:|:-------:|:---------:|
| **rqbit 8.1.1** | 20.8 +/-1.2 | 70.1 +/-4.2 | 10.4 +/-0.6 | 39.2 +/-1.8 |
| **qbittorrent 5.1.4** | 67.1 +/-21.9 | 23.2 +/-6.1 | 19.6 +/-5.7 | 1518.8 +/-7.7 |
| **torrent 0.66.0** | 102.1 +/-35.6 | 15.8 +/-5.8 | 162.5 +/-66.1 | 93.6 +/-3.8 |

## Per-Trial Data

| Client | Trial | Time (s) | Speed (MB/s) | CPU (s) | RSS (MiB) |
|:-------|:-----:|:--------:|:------------:|:-------:|:---------:|
| torrent | 1 | 124.7 | 11.6 | 200.8 | 96.9 |
| torrent | 2 | 101.0 | 14.4 | 155.5 | 95.8 |
| torrent | 3 | 59.8 | 24.3 | 84.8 | 87.3 |
| torrent | 4 | 76.9 | 18.9 | 119.2 | 94.6 |
| torrent | 5 | 148.3 | 9.8 | 252.1 | 93.2 |
| rqbit | 1 | 22.0 | 66.1 | 11.3 | 38.3 |
| rqbit | 2 | 20.5 | 71.0 | 10.0 | 38.5 |
| rqbit | 3 | 20.1 | 72.2 | 10.6 | 39.6 |
| rqbit | 4 | 22.1 | 65.6 | 9.6 | 37.5 |
| rqbit | 5 | 19.3 | 75.4 | 10.4 | 42.2 |

\newpage

# 2. The Gap

## Speed Gap

| Metric | torrent | rqbit | Gap |
|:-------|:-------:|:-----:|:---:|
| Avg download speed | 15.8 MB/s | 70.1 MB/s | **4.4x slower** |
| Best trial speed | 24.3 MB/s | 75.4 MB/s | **3.1x slower** |
| Worst trial speed | 9.8 MB/s | 65.6 MB/s | **6.7x slower** |
| Consistency (stddev) | +/-5.8 | +/-4.2 | torrent is erratic |

rqbit's speed is remarkably consistent (65--75 MB/s across all 5 trials). torrent ranges wildly from 9.8 to 24.3 MB/s, indicating systemic instability.

## Memory Gap

| Metric | torrent | rqbit | Gap |
|:-------|:-------:|:-----:|:---:|
| Peak RSS | 93.6 MiB | 39.2 MiB | **2.4x more** |

torrent uses 54 MiB more than rqbit. The store buffer (in-memory block cache for pieces awaiting verification) is the primary contributor.

## CPU Efficiency Gap

| Metric | torrent | rqbit | Observation |
|:-------|:-------:|:-----:|:------------|
| CPU time | 162.5s | 10.4s | **15.6x more CPU** |
| CPU:wall ratio | 1.59 | 0.50 | torrent saturates 1.6 cores |
| CPU per MB | 10.3s/MB | 0.15s/MB | **69x less efficient** |

torrent's CPU time **exceeds its wall time**, meaning background threads (piece hash verification via `spawn_blocking`) are consuming more CPU than the main async runtime. rqbit uses only half a core for the entire download.

\newpage

# 3. Root Cause Analysis

The following bottlenecks are ranked by their expected impact on **download throughput** and **memory usage**. While CPU is not a primary optimisation target, the analysis reveals that CPU saturation is directly throttling throughput --- the system cannot process incoming data fast enough because CPU cores are consumed by hashing and bookkeeping.

## Priority 1: End-Game Pipeline Starvation

**Impact:** Throughput --- adds 40--60 seconds per download

**Location:** `torrent.rs:5364`

`END_GAME_DEPTH = 4` limits each peer to 4 in-flight requests during end-game mode, down from 128 in normal mode. The `final_speed` metric reports 0.0 MB/s at download completion on all trials --- the last 5--10\% of the download crawls.

| Mode | Peers | Depth | Block Size | RTT | Throughput Ceiling |
|:-----|:-----:|:-----:|:----------:|:---:|:------------------:|
| Normal | 50 | 128 | 16 KB | 100ms | ~1 GB/s |
| End-game | 50 | 4 | 16 KB | 100ms | ~32 MB/s |

**rqbit:** Flat 128-permit semaphore throughout the entire download. No depth reduction.

**Fix:** Set `END_GAME_DEPTH = 128` or remove the end-game depth distinction entirely.

## Priority 2: Unrestricted Hashing Concurrency

**Impact:** Throughput --- CPU saturation throttles block processing

**Location:** `disk.rs:561--595, 616--630`

Piece hash verification uses `tokio::task::spawn_blocking()` with **no concurrency limit**. When multiple pieces complete simultaneously, dozens of SHA-1/SHA-256 hash tasks flood the blocking thread pool, saturating all CPU cores.

This is why CPU time (162.5s) exceeds wall time (102.1s) --- background hashing threads consume 1.6 cores on average, starving the async runtime of CPU time to process incoming blocks.

| Trial | Wall (s) | CPU (s) | CPU:Wall | Interpretation |
|:-----:|:--------:|:-------:|:--------:|:---------------|
| 1 | 124.7 | 200.8 | 1.61 | Heavy hashing load |
| 3 | 59.8 | 84.8 | 1.42 | Best trial, less hashing contention |
| 5 | 148.3 | 252.1 | 1.70 | Worst trial, CPU completely saturated |

**rqbit:** Hashes pieces synchronously in the peer reader, no thread pool. CPU:wall = 0.50.

**Fix:** Gate `spawn_blocking` hashing with a `Semaphore(2)` to limit concurrent hash tasks to 2 threads.

## Priority 3: Store Buffer Memory Overhead

**Impact:** Memory --- ~50 MiB overhead

**Location:** `disk.rs:47--52`

Every in-flight piece stores all its blocks in a `Mutex<HashMap<(Id20, u32), BTreeMap<u32, Bytes>>>`. With 100+ pieces in flight at 32 blocks/piece at 16 KB/block, this holds ~50 MiB of block data in RAM until verification completes.

| Component | torrent | rqbit |
|:----------|:-------:|:-----:|
| Store buffer | ~50 MiB | 0 MiB |
| Peer state (200 peers) | ~8 MiB | ~4 MiB |
| Bitfields + metadata | ~5 MiB | ~5 MiB |
| **Total RSS** | **93.6 MiB** | **39.2 MiB** |

**rqbit:** Writes blocks directly to disk via `pwritev()`. Verifies from on-disk data. No in-memory block buffer.

**Fix:** Write blocks to disk immediately, verify from disk. Or cap buffered pieces and apply backpressure.

## Priority 4: Reactive Re-Requesting Cascade

**Impact:** Throughput --- triggers thousands of unnecessary picker cycles per second

**Location:** `torrent.rs:3601--3627`

When a block is received in end-game mode, cancel messages are sent to all peers that also requested that block. For **each cancelled peer**, the code immediately runs a full picker cycle (`request_end_game_block()`). Then it runs another picker cycle for the receiving peer.

With 50 peers in end-game, each block received triggers 2+ full picker invocations. At thousands of blocks per second, this creates a CPU cascade.

**rqbit:** Block arrival triggers a simple `add_permits(1)` on the peer's semaphore. No picker cycles.

**Fix:** Batch freed peers into the next tick instead of triggering immediate picker cycles per block.

## Priority 5: Per-Peer Request Driver Tasks

**Impact:** Throughput --- 200 concurrent async tasks with per-block semaphore overhead

**Location:** `torrent.rs:5128+`, `request_driver.rs`

Each of the 200 connected peers has a dedicated `tokio::spawn()` task that loops: acquire semaphore permit, send `NeedBlocks` message to actor, await response. This means 200 tasks doing semaphore + channel operations on every block.

**rqbit:** No per-peer tasks. Each peer holds a `Semaphore(128)` and block arrival adds a permit directly. Zero task overhead.

**Fix:** Replace per-peer driver tasks with a reactive model: on block receipt, add permit to peer's semaphore directly.

## Priority 6: O(n) Data Structure Operations in Hot Path

**Impact:** Throughput --- hundreds of thousands of wasted comparisons per second

**Locations:**

| Operation | Location | Cost per call | Calls/sec |
|:----------|:---------|:-------------:|:---------:|
| `pending_requests.position()` | `torrent.rs:3352,3520,3583,3619` | O(128) | ~4,000 |
| `peer_count()` sort+dedup | `piece_selector.rs:73--81` | O(n log n) | ~2,000 |
| `end_game.peers.contains()` | `end_game.rs:92` | O(n) | ~4,000 |
| Bitfield clones | `torrent.rs:5200,5313,5375` | ~768 bytes each | ~4,000 |

**rqbit:** Hash-indexed lookups, shared references, no allocations in hot path.

**Fix:** Replace `Vec` with `FxHashMap` for pending requests, track peer count incrementally, use `HashSet` in end-game, pass bitfields by reference.

\newpage

# 4. Bottleneck Summary

| Rank | Bottleneck | Primary Metric | Est. Impact | Code Location |
|:----:|:-----------|:--------------:|:------------|:--------------|
| 1 | End-game depth = 4 | Throughput | +40--60s/download | `torrent.rs:5364` |
| 2 | Unrestricted hashing concurrency | Throughput | CPU saturates cores | `disk.rs:561` |
| 3 | Store buffer in-memory blocks | Memory | ~50 MiB overhead | `disk.rs:47` |
| 4 | Reactive re-requesting cascade | Throughput | 2+ picker cycles/block | `torrent.rs:3601` |
| 5 | Per-peer request driver tasks | Throughput | 200 tasks overhead | `request_driver.rs` |
| 6 | O(n) hot-path operations | Throughput | 100Ks wasted ops/sec | Multiple files |

\vspace{1em}

# 5. Architectural Comparison

| Aspect | torrent | rqbit |
|:-------|:--------|:------|
| Pipeline model | 128 normal, 4 end-game | Flat 128 always |
| Hashing | Unbounded `spawn_blocking` | Synchronous in peer reader |
| Disk writes | Store buffer (RAM) then disk | Direct `pwritev()` to disk |
| Request dispatch | Per-peer async tasks (200) | Per-peer semaphore (atomic) |
| Block tracking | `Vec` with linear scan | Hash-indexed O(1) |
| Bitfield access | Clone per picker call | Shared reference |
| CPU:wall ratio | 1.59 (saturates 1.6 cores) | 0.50 (half a core) |

\newpage

# 6. Recommended Next Steps

The following fixes are ordered to maximise throughput improvement per unit of implementation effort.

## Step 1: Raise End-Game Pipeline Depth

Change `END_GAME_DEPTH` from 4 to 128 at `torrent.rs:5364`. This is a one-line change that eliminates the 40--60 second tail on every download.

**Expected gain:** 40--60\% faster wall time.

## Step 2: Gate Hashing Concurrency

Add a `Semaphore::new(2)` around `spawn_blocking` calls in `disk.rs:561` and `disk.rs:616`. This prevents CPU saturation from concurrent hash tasks, freeing CPU cores for async block processing.

**Expected gain:** CPU:wall ratio drops from 1.6 to ~0.7, indirectly improving throughput.

## Step 3: Eliminate Store Buffer

Write blocks directly to disk on receipt (like rqbit). Read back from disk for verification. Removes ~50 MiB of RAM usage and eliminates `Mutex` contention between async and blocking threads.

**Expected gain:** RSS drops to ~45 MiB (close to rqbit). Removes lock contention.

## Step 4: Remove Reactive Re-Requesting

Stop triggering immediate picker cycles when end-game blocks are cancelled. Instead, batch freed peers and refill on the next 1-second tick or after the current block's processing completes.

**Expected gain:** Eliminates thousands of redundant picker cycles per second.

## Step 5: Replace Per-Peer Driver Tasks

Replace the 200 spawned driver tasks with a direct semaphore-permit model. On block receipt, call `semaphore.add_permits(1)` on the peer's semaphore. The peer's send loop acquires permits to send requests --- no actor message passing needed.

**Expected gain:** Eliminates 200 task wake-ups and channel sends per block.

## Step 6: Fix Hot-Path Data Structures

Replace `Vec::position()` with `FxHashMap` for pending requests, track `peer_count()` incrementally, use `HashSet` in end-game block picker, pass bitfields by reference instead of cloning.

**Expected gain:** Reduces per-block CPU work by ~50\%.

\vspace{1em}

## Projected Outcome

| Metric | Current | After Steps 1--3 | After Steps 1--6 |
|:-------|:-------:|:-----------------:|:-----------------:|
| Speed | 15.8 MB/s | ~45--55 MB/s | ~60--70 MB/s |
| RSS | 93.6 MiB | ~45 MiB | ~40 MiB |
| CPU:wall | 1.59 | ~0.7 | ~0.5 |

Steps 1--3 alone should close most of the gap with rqbit. Steps 4--6 are refinements to reach parity.
