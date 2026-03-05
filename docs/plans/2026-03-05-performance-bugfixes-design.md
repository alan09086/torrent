# Performance Bugfixes Design

Date: 2026-03-05
Status: Approved
Goal: Fix 3 critical bugs causing download inconsistency + tune constants for reliable throughput

## Context

Ferrite v0.65.0 peaks at 116 MB/s but is inconsistent across runs. rqbit consistently achieves ~80 MB/s. Root cause analysis identified 3 bugs and 4 tuning gaps.

## Bug 1: Store Buffer Mutex Contention

**Problem**: SHA-1 hashing runs while holding the store buffer `Mutex`. Piece verification (CPU-bound, ~1ms for 512 KiB) blocks all incoming `enqueue_write()` calls. Under load with many pieces completing near-simultaneously, this creates cascading write stalls that back-pressure into peer I/O and cause disconnections.

**Fix**: Remove data from the store buffer *before* hashing. The verification task should:
1. Lock the Mutex
2. Remove the piece's blocks (`HashMap::remove`)
3. Drop the lock
4. Assemble piece data from the removed blocks
5. Hash outside the lock

This changes the lock hold time from "copy + hash" (~1-10ms) to "remove" (~microseconds).

**Risk**: Low. The piece data is consumed (removed) atomically. No other code path should access a piece's blocks after verification begins, since verification is triggered when all blocks for that piece have arrived.

## Bug 2: Disconnect Race in Re-Request Loop

**Problem**: When a peer disconnects, its pending requests are freed and `request_pieces_from_peer()` is called in a loop over all remaining peers. If any peer disconnects during this async loop, subsequent iterations may reference dead peers, and the freed blocks from the *original* disconnect are never reassigned.

**Fix**:
1. Collect the snapshot of peer addresses before the loop
2. Inside the loop, check `self.peers.contains_key(&addr)` before calling `request_pieces_from_peer(addr)`
3. If a peer is gone, skip it (the blocks will be reassigned when *that* peer's disconnect handler runs)

**Risk**: Low. This is a defensive check that prevents wasted work on dead peers.

## Bug 3: Stale Peer in Pipeline Fill Loop

**Problem**: The `pick_blocks` fill loop (commit 9687704) caches peer context (bitfield, rates, suggested pieces) at the start but doesn't re-validate the peer between iterations. If the peer disconnects after the first ~34 blocks, subsequent iterations silently fail (`try_send` to a dead channel), and `sent == 0` breaks the loop with ~94 unfilled slots.

**Fix**:
1. At the top of each loop iteration, verify the peer still exists in `self.peers`
2. If the peer is gone, break the loop immediately (blocks will be picked up by other peers)
3. Re-fetch `peer_bitfield` and `peer_rates` each iteration (they may change even without disconnect)

**Risk**: Low. Adds one HashMap lookup per iteration (negligible cost for correctness).

## Tuning Changes

### Snub Timeout: 60s -> 15s
A stalled peer holding 128 request slots for 60 seconds is too long. rqbit effectively handles this in ~10 seconds via read timeouts. 15 seconds is conservative while still being 4x faster than current.

### Zombie Peer Pruning
Peers with empty bitfields (bf_ones == 0) after 30 seconds of connection should be disconnected. They contribute nothing and consume connection slots. Check on each choking evaluation cycle.

### Queue Depth Floor: 64
Prevent EWMA-based depth from dropping below 64 blocks. This prevents the negative feedback loop where a brief throughput dip reduces queue depth, reducing throughput further. rqbit uses a fixed 128 — a floor of 64 is conservative.

### Peer Turnover: 4%/300s -> 8%/120s
Faster churn of underperforming peers. Current rate replaces ~30 peers/hour. New rate replaces ~120 peers/hour, cycling through bad peers 4x faster.

## Verification

- All 1378 existing tests must pass
- `cargo clippy --workspace -- -D warnings` clean
- Manual benchmark: 3 consecutive Arch Linux ISO downloads, record speed for each
- Success criteria: all 3 runs within 20% of each other (consistent), average >= 80 MB/s

## Files to Modify

1. `crates/torrent-session/src/disk.rs` — store buffer lock fix
2. `crates/torrent-session/src/torrent.rs` — disconnect race fix, pipeline loop fix, zombie pruning
3. `crates/torrent-session/src/pipeline.rs` — queue depth floor
4. `crates/torrent-session/src/defaults.rs` — snub timeout, peer turnover constants
