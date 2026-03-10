---
title: "M65: CPU Efficiency Optimizations"
date: 2026-03-10
baseline: "v0.67.0 — 20.7 MB/s, 28.8s CPU, 83 MiB RSS"
target: "~17-20s CPU, proportional speed increase if CPU-bound"
---

# M65: CPU Efficiency Optimizations

## Summary

Two low-risk, high-impact changes to reduce CPU overhead on the download hot path.
Based on the post-revert benchmark analysis (benchmarks/report-v0.67.0.md).

## Task 1: SHA Hardware Acceleration

**File**: `crates/torrent-core/Cargo.toml`

Enable the `asm` feature on both SHA crates:

```toml
sha1 = { version = "0.10", features = ["asm"] }
sha2 = { version = "0.10", features = ["asm"] }
```

**Behaviour**: The `cpufeatures` crate performs runtime CPU detection. If SHA-NI
instructions are available (Intel Ice Lake+, AMD Zen+), uses hardware-accelerated
path (~2-3 GB/s). Otherwise falls back to optimised software assembly (~170-220 MB/s).
No code changes required. No new external dependencies.

**Impact**: Modest on current hardware (no SHA-NI), but free ~5x gain on newer CPUs.
Estimated ~1-2s CPU savings on current system.

**Risk**: Effectively zero. Automatic fallback on all platforms.

**Testing**: Existing 1390 tests exercise hashing through piece verification.

## Task 2: Batch Dispatch Threshold

**File**: `crates/torrent-session/src/torrent.rs`

### Problem

`request_pieces_from_peer()` is called after every non-duplicate block in
`handle_piece_data()` (~91,750 times for a 1.4 GiB download). Each call runs:

1. Bitfield clone (359 bytes)
2. PickContext construction (22 fields)
3. RefCell<Vec> + closure allocation
4. 5-layer piece selection algorithm

When the pipeline is 127/128 full, the full picker cycle fires to dispatch 1 block.

### Fix

At the call site in `handle_piece_data()` (~line 3669), add a threshold check:

```rust
let in_flight = peer.in_flight_count();
let queue_depth = peer.queue_depth();
if queue_depth - in_flight >= BATCH_DISPATCH_THRESHOLD {
    self.request_pieces_from_peer(peer_addr);
}
```

**Constant**: `BATCH_DISPATCH_THRESHOLD = 32` (25% of 128 pipeline depth).

- Pipeline 127/128 full → skip (1 free slot)
- Pipeline 96/128 full → dispatch (fills all 32 free slots in one pass)
- Reduces invocations from ~91,750 to ~2,900 (32x reduction)

### Safety Net

Other call sites remain unchanged — unchoke, bitfield, have, hash failure, and
post-metadata transitions all trigger on meaningful state changes.

The 1-second pipeline tick tops up any peer that drifts below capacity without
triggering the threshold. The 200ms end-game tick covers the download tail.

### Constant Placement

Define `BATCH_DISPATCH_THRESHOLD` near `END_GAME_DEPTH` (~line 1478).

**Impact**: Estimated ~10s CPU savings (30-40% reduction).

**Risk**: Low. Worst case is a 1-second stall on a single peer before the pipeline
tick refills. The 25% threshold is conservative.

**Testing**: Existing integration tests cover download completion. Add a unit test
confirming the threshold gate: mock a peer with 127/128 in-flight (dispatch skipped),
mock with 96/128 (dispatch fires).

## Out of Scope

- Adaptive queue depth (already matches rqbit at fixed 128) → M66
- Bitfield clone elimination (negligible with reduced invocations) → M66
- Zero-copy message parsing → M66
- Dispatch model restructuring (learned from M62/M63)

## Success Criteria

1. All 1390+ tests pass
2. `cargo clippy --workspace -- -D warnings` clean
3. Benchmark shows measurable CPU reduction vs v0.67.0 baseline
4. No download speed regression
