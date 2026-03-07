---
title: Benchmark Report
version: 0.68.0
date: 2026-03-07
---

# Benchmark Report v0.68.0

## Summary

| Client | Time (s) | Speed (MB/s) | CPU (s) | RSS (MiB) |
|--------|----------|--------------|---------|-----------|
| qbittorrent | 67.1 +/- 21.9 | 23.2 +/- 6.1 | 19.6 +/- 5.7 | 1518.8 +/- 7.7 |
| rqbit | 20.8 +/- 1.2 | 70.1 +/- 4.2 | 10.4 +/- 0.6 | 39.2 +/- 1.8 |
| torrent | 102.1 +/- 35.6 | 15.8 +/- 5.8 | 162.5 +/- 66.1 | 93.6 +/- 3.8 |

## Relative Performance

- **torrent vs qbittorrent**: 1.5x slower
- **torrent vs rqbit**: 4.9x slower

## Per-Trial Data

| Client | Trial | Time (s) | CPU (s) | Speed (MB/s) | RSS (MiB) |
|--------|-------|----------|---------|--------------|-----------|
| torrent | 1 | 124.69 | 200.82 | 11.64 | 96.9 |
| rqbit | 1 | 21.95 | 11.25 | 66.13 | 38.3 |
| qbittorrent | 1 | 65.56 | 20.01 | 22.14 | 1508.0 |
| torrent | 2 | 101.01 | 155.47 | 14.37 | 95.8 |
| rqbit | 2 | 20.45 | 9.99 | 70.98 | 38.5 |
| qbittorrent | 2 | 52.15 | 15.95 | 27.83 | 1514.8 |
| torrent | 3 | 59.79 | 84.76 | 24.28 | 87.3 |
| rqbit | 3 | 20.11 | 10.59 | 72.18 | 39.6 |
| qbittorrent | 3 | 48.85 | 13.64 | 29.72 | 1526.8 |
| torrent | 4 | 76.91 | 119.15 | 18.87 | 94.6 |
| rqbit | 4 | 22.14 | 9.56 | 65.56 | 37.5 |
| qbittorrent | 4 | 64.88 | 19.74 | 22.37 | 1525.1 |
| torrent | 5 | 148.32 | 252.14 | 9.79 | 93.2 |
| rqbit | 5 | 19.25 | 10.40 | 75.41 | 42.2 |
| qbittorrent | 5 | 103.83 | 28.61 | 13.98 | 1519.5 |

## Per-Client Details

### qbittorrent

- **Trials**: 5
- **Time**: 67.1 +/- 21.9 s
- **Speed**: 23.2 +/- 6.1 MB/s
- **CPU**: 19.6 +/- 5.7 s
- **RSS**: 1518.8 +/- 7.7 MiB

### rqbit

- **Trials**: 5
- **Time**: 20.8 +/- 1.2 s
- **Speed**: 70.1 +/- 4.2 MB/s
- **CPU**: 10.4 +/- 0.6 s
- **RSS**: 39.2 +/- 1.8 MiB

### torrent

- **Trials**: 5
- **Time**: 102.1 +/- 35.6 s
- **Speed**: 15.8 +/- 5.8 MB/s
- **CPU**: 162.5 +/- 66.1 s
- **RSS**: 93.6 +/- 3.8 MiB

## Changes Since v0.67.0

**M62: Semaphore-Based Reactive Pipeline**

- Replaced poll-based batch dispatch with per-peer `tokio::Semaphore` reactive scheduling
- Per-peer `request_driver` async task: acquire permit -> pick block -> dispatch (zero-latency)
- Unified normal and end-game dispatch through single `dispatch_single_block` path
- End-game mode: 1-redundant-copy limit (libtorrent parity), reactive cancel cascade
- Removed: 200ms end-game refill tick, batch `fill_requests()`, `queue_depth` counter
- Snub handling via `AtomicBool` flag (driver parks on `Notify`, no semaphore closure)
- Fixed completion exit bug: cancel all request drivers before Seeding transition
- Fixed state-dependent driver spawning: guard against spawning in non-Downloading state

## Architectural Gap Analysis

### Why rqbit is 4.9x faster

| Gap | torrent | rqbit | Impact |
|-----|---------|-------|--------|
| **Request dispatch overhead** | Per-peer tokio task with semaphore acquire/forget/channel send per block (3 async ops) | Direct function call from peer handler, inline picker access | ~3x latency per request cycle |
| **CPU efficiency** | 162.5 CPU-seconds for 102s wall (1.6x ratio) — 200 driver tasks spinning semaphores | 10.4 CPU-seconds for 20.8s wall (0.5x ratio) — no per-peer tasks | 15x CPU overhead |
| **Peer connection ramp-up** | All 200 peers get drivers immediately; many peers choke us for 30-60s before unchoke | Fewer concurrent connections, more selective peer management | Wasted work on non-contributing peers |
| **Channel bottleneck** | All 200 drivers funnel NeedBlocks through single mpsc(256) channel to torrent actor | No channel — picker called directly from peer event handler | Contention under load |
| **Adaptive queue depth** | Fixed 128 permits regardless of peer speed or RTT | Dynamic request queue based on throughput estimation | Fast peers underutilized, slow peers over-requested |
| **DHT warm start** | rqbit has persistent DHT routing table (warm cache from prior runs) | torrent also has DHT persistence, but peer discovery may be slower | Faster initial peer acquisition |

### Why qbittorrent is 1.5x slower than torrent (sometimes)

qbittorrent (libtorrent-rasterbar C++) has higher overhead from its Qt/GUI layer and C++ runtime:
- **RSS**: 1519 MiB vs 94 MiB (16x memory) — Qt framework, WebUI server, SQLite state
- **High variance**: 48s to 104s across trials (same swarm variability as torrent)
- **CPU**: 19.6s (efficient C++ core) vs 162.5s — libtorrent's adaptive pipeline is far more CPU-efficient

### Key areas for improvement (next milestones)

1. **Adaptive queue depth** (M63): Dynamic `rate * RTT / block_size` permit sizing — fast peers get more permits, slow peers fewer. This is the single biggest gap vs rqbit.
2. **Reduce per-request overhead**: Consider batching NeedBlocks (request N blocks per wake instead of 1) to reduce channel pressure and semaphore churn.
3. **Selective peer connection**: Don't spawn 200 drivers immediately — ramp up connections gradually based on available bandwidth.
4. **Inline dispatch path**: For hot path, consider bypassing the channel and allowing the driver to call the picker directly (requires careful lock design).
