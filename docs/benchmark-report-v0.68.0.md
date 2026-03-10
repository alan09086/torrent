---
title: Benchmark Report
version: 0.68.0
date: 2026-03-10
---

# Benchmark Report v0.68.0

## Summary

| Client | Time (s) | Speed (MB/s) | CPU (s) | RSS (MiB) |
|--------|----------|--------------|---------|-----------|
| torrent | 44.2 +/- 1.5 | 32.8 +/- 1.1 | 22.0 +/- 0.3 | 66.0 +/- 1.2 |
| rqbit | 24.7 +/- 1.9 | 59.0 +/- 4.3 | 10.6 +/- 0.7 | 41.7 +/- 0.3 |
| qbittorrent | 68.2 +/- 31.0 | 23.9 +/- 8.7 | 20.3 +/- 8.0 | 1530.7 +/- 4.1 |

## Relative Performance

- **torrent vs rqbit**: 1.8x slower (was 2.8x in v0.67.0)
- **torrent vs qbittorrent**: 1.5x faster

## Per-Trial Data

| Client | Trial | Time (s) | CPU (s) | Speed (MB/s) | RSS (MiB) |
|--------|-------|----------|---------|--------------|-----------|
| torrent | 1 | 44.24 | 21.77 | 32.81 | 64.7 |
| rqbit | 1 | 24.14 | 11.19 | 60.13 | 41.8 |
| qbittorrent | 1 | 52.98 | 15.71 | 27.40 | 1535.2 |
| torrent | 2 | 45.73 | 22.38 | 31.74 | 66.2 |
| rqbit | 2 | 23.20 | 9.76 | 62.57 | 41.3 |
| qbittorrent | 2 | 103.87 | 29.56 | 13.98 | 1527.0 |
| torrent | 3 | 42.71 | 21.91 | 33.99 | 67.1 |
| rqbit | 3 | 26.77 | 10.77 | 54.22 | 42.0 |
| qbittorrent | 3 | 47.80 | 15.71 | 30.37 | 1530.0 |

## Changes Since v0.67.0

**M65: CPU Efficiency Optimizations**

- **SHA-NI hardware acceleration**: Enabled `asm` feature on `sha1`/`sha2` crates for runtime
  CPU-dispatched SHA-NI instructions — accelerates piece verification and MSE/PE encryption
  handshakes. Zero code changes; `cpufeatures` crate handles runtime detection.
- **Batch dispatch threshold**: Added `BATCH_DISPATCH_THRESHOLD = 32` to gate the 5-layer piece
  picker in `handle_piece_data()`. The picker now only runs when 32+ pipeline slots are free
  (or during end-game), reducing ~91k picker invocations to ~2.9k per 1.4 GiB download (~97%
  reduction in hot-path CPU).
- **Pipeline tick safety net widened**: The 1-second pipeline tick now catches both fully idle
  peers (any queue depth) and peers crossing the batch threshold, preventing stalls for peers
  with low adaptive queue depths.

## Improvement vs v0.67.0

| Metric | v0.67.0 | v0.68.0 | Change |
|--------|---------|---------|--------|
| Speed | 20.7 MB/s | 32.8 MB/s | **+58%** |
| Wall time | 74.2s | 44.2s | **-40%** |
| CPU time | 28.8s | 22.0s | **-24%** |
| RSS | 83 MiB | 66 MiB | **-20%** |
| Speed variance | +/- 6.1 MB/s | +/- 1.1 MB/s | **5.5x more consistent** |

## Architectural Gap Analysis

### Why rqbit is 1.8x faster

| Gap | torrent | rqbit | Impact |
|-----|---------|-------|--------|
| **Request scheduling latency** | Tick-based: 1s pipeline tick + batch threshold gate. Up to 1s delay before refilling a peer. | Semaphore-based: `add_permits(1)` per block receipt. Zero-latency reactive dispatch. | ~30% of speed gap |
| **RC4 cipher overhead** | Pure Rust RC4 in MSE/PE data stream (~12.75% CPU per profiling) | OpenSSL optimized RC4 | ~5s CPU |
| **Wire message allocation** | Per-message Vec allocations in codec (~17% CPU) | 32 KB ring buffer, minimal allocations | ~4s CPU |
| **Piece picker complexity** | 5-layer rarest-first (speed affinity, extent affinity, streaming) | Simple sequential + time-based stealing | Higher per-call cost |
| **EWMA negative feedback** | Throughput dip -> reduced queue depth -> less data in-flight -> further dip | Fixed 128 permits, no feedback loop | Occasional rate dips |

### Why torrent beats qbittorrent

- **Speed**: 32.8 vs 23.9 MB/s (1.4x faster)
- **Memory**: 66 MiB vs 1531 MiB (23x less) — no Qt/GUI/WebUI overhead
- **Consistency**: torrent +/- 1.1 MB/s vs qbittorrent +/- 8.7 MB/s

### Lessons from libtorrent for M66+

1. **Application-level slow-start**: Double queue depth per RTT (like TCP) instead of EWMA
2. **O(1) rarity buckets**: Swap-based bucket boundaries instead of O(n) scan
3. **Buffer pools**: Reuse disk and receive buffers to reduce allocation pressure
4. **Dynamic queue sizing**: `rate * RTT / block_size` formula adapts per-peer
5. **Struct packing**: Keep hot peer fields on same cache line

### Key areas for improvement (next milestones)

1. **Adaptive queue depth** (M66): Dynamic `rate * RTT / block_size` permit sizing
2. **Zero-copy wire messages**: Pass `Bytes` references through codec instead of owned copies
3. **Bitfield reference passing**: Avoid cloning bitfields in picker calls
4. **RC4 optimization**: Consider OpenSSL or hardware-accelerated cipher for MSE/PE data stream
