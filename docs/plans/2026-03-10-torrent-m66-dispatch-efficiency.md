---
title: "M66: Dispatch Path Efficiency"
date: 2026-03-10
depends_on: "M65 (batch threshold reduces invocation count; M66 reduces cost per invocation)"
target: "~12-14s CPU (from ~17s post-M65)"
---

# M66: Dispatch Path Efficiency

## Summary

Three changes to reduce the per-invocation cost of the request dispatch path.
M65 reduces how often the picker runs; M66 reduces how expensive each run is.

## Task 1: Adaptive Queue Depth (libtorrent BDP Model)

**File**: `crates/torrent-session/src/torrent.rs`, `crates/torrent-session/src/settings.rs`

### Problem

All peers get fixed 128 queue depth regardless of speed. A 5 KB/s peer holds 128
slots it'll never fill. A 10 MB/s peer could benefit from more.

### Fix

Compute per-peer queue depth using libtorrent's BDP formula:

```
queue = download_rate × request_queue_time / block_size
```

**Parameters**:
- `request_queue_time`: 3 seconds (libtorrent default), exposed in `Settings`
- Floor: 16 (minimum pipeline for any peer)
- Ceiling: 500 (libtorrent's max, prevents runaway)
- Recalculated on the 1-second pipeline tick using EWMA-smoothed per-peer rate

**Benefit**: Slow peers waste fewer slots. Fast peers get deeper pipelines.

**Risk**: Medium. Requires per-peer rate tracking (may already exist) and careful
interaction with the batch dispatch threshold from M65. The threshold constant may
need to become a percentage of queue depth rather than a fixed 32.

**Testing**: Unit tests for the formula with edge cases (zero rate, very fast peers,
rate exactly at floor/ceiling boundaries). Integration test confirming downloads
complete with adaptive sizing.

## Task 2: Eliminate Bitfield Clone

**File**: `crates/torrent-session/src/torrent.rs`

### Problem

Each `request_pieces_from_peer()` call clones the peer's bitfield (~359 bytes).
After M65's batch threshold, this is ~2,900 clones (~1 MB total) — modest but
the refactor enables cleaner code.

### Fix

Pass `&Bitfield` reference into the picker instead of cloning. Restructure borrows
in `request_pieces_from_peer()`:

1. **Gather phase** (shared borrow): read peer bitfield, peer rate, in-flight state
2. **Dispatch phase** (mutable borrow): call picker with gathered references, send requests

This splits the peer map access so the shared borrow is released before mutable
operations begin.

**Risk**: Medium. Borrow checker refactor — may require extracting peer state into
a temporary struct to satisfy lifetimes.

**Testing**: Existing tests. No behavioural change.

## Task 3: Zero-Copy Wire Message Parsing

**Files**: `crates/torrent-wire/src/message.rs`, `crates/torrent-wire/src/codec.rs`

### Problem

Every incoming message allocates an owned `Message` struct. For Piece messages
(~91,750 per 1.4 GiB download), this copies 16KB of payload data each time. rqbit
uses `MessageBorrowed<'_>` that references the receive buffer directly.

### Fix

Add a `MessageRef<'a>` enum variant that borrows from the codec's `BytesMut` buffer.
The Piece variant holds `Bytes` (zero-copy slice from buffer) instead of `Vec<u8>`.

**Scope**: Only the Piece message needs the zero-copy path. Control messages (Have,
Bitfield, Request, Cancel, etc.) are small — owned copies are fine.

**Implementation**:
- Codec produces `MessageRef<'_>` from `decode()`
- Piece data uses `BytesMut::split_to().freeze()` → `Bytes` (reference-counted, no copy)
- TorrentActor receives `Bytes` for piece payload, passes directly to DiskActor
- Existing `Message` enum remains for outbound messages (no change to send path)

**Risk**: Medium-high. Touches the codec layer and message type hierarchy. Requires
careful lifetime management to ensure the borrow doesn't outlive the buffer.

**Testing**: Wire protocol unit tests (encode/decode round-trip). Integration tests
for download completion. Verify no memory leaks via RSS comparison in benchmark.

## Out of Scope

- Dispatch model restructuring (no per-peer tasks, no semaphore drivers)
- `parking_lot` / `DashMap` migration (marginal gains, large diff)
- `block_in_place` for disk I/O (DiskActor already non-blocking)

## Success Criteria

1. All tests pass, clippy clean
2. Benchmark shows CPU reduction from M65 baseline (~17s → ~12-14s)
3. RSS stays stable or decreases (zero-copy should help)
4. No download speed regression
5. No data corruption (lesson from M62/M63)
