# M77: Context Switch Reduction

## Problem

Torrent generates 3.7x more context switches than rqbit (412K vs 112K) for the same download. This contributes to the 1.9x CPU time gap. The actor model creates many cross-task wake-ups: each block received by a peer task triggers a channel send to the actor, which wakes the actor task, which processes the event, which may wake other peer tasks.

## Benchmark Baseline (post-M76)

Expected after M76: RSS ~70 MB, cache misses ~6M, wall time ~35s, context switches still ~400K.

## Analysis

### Context Switch Sources

1. **PeerEvent channel (peer → actor)**: Every block receipt, bitfield, have, choke/unchoke event sends a message through `event_rx`. At 100 MB/s with 16 KB blocks: ~6,250 blocks/sec = ~6,250 channel sends/sec across all peers.

2. **PeerCommand channel (actor → peer)**: End-game requests, Have broadcasts, shutdown signals. Lower frequency but still significant during end-game.

3. **Verify result channel (disk → actor)**: Each piece verification result wakes the actor. ~5-10/sec at full speed.

4. **Notify signals (reservation state → peers)**: `piece_notify.notify_waiters()` wakes ALL peer tasks waiting for new work. Called on every piece completion, peer connect/disconnect, have message. At ~5-10 pieces/sec, this wakes 128 peers ~5-10 times/sec = 640-1280 wakes/sec.

5. **Tokio runtime overhead**: 160+ tasks competing for a thread pool (default: num_cpus threads).

### rqbit Comparison

rqbit has fewer context switches because:
- Peer tasks write directly to shared state (`DashMap`, `RwLock`) instead of channel-messaging an actor
- No separate actor task processing events — peers are self-sufficient
- `notify_waiters()` equivalent is less frequent (piece-tracker state changes)

## Changes

### Part 1: Batch Event Coalescing

**Current state:** The actor's `event_rx` arm drains up to 256 events per select! iteration, but each event individually wakes the actor from `select!`. The tokio mpsc channel wakes the receiver on every send.

**Change:** Replace the bounded `mpsc::channel` for peer events with a coalescing pattern. Options:
- **Option A**: Use `tokio::sync::mpsc` with a larger buffer (current: 256, increase to 2048) — reduces wake frequency since sends only wake when buffer was empty
- **Option B**: Add a `tokio::time::interval(1ms)` coalescing timer — process events in 1ms batches instead of per-event wakes
- **Option C**: Use `flume` or `crossbeam` channel which has different wake semantics

**Recommended: Option A** — simplest, lowest risk. The channel already batches reads (drain up to 256), but a larger buffer means fewer wake-on-empty events. Combined with the existing `biased;` select, the actor will naturally batch-process events.

**Files:**
- `crates/torrent-session/src/torrent.rs` — Increase event channel capacity
- `crates/torrent-session/src/session.rs` — If channel creation is there

### Part 2: Reduce Notify Wake Storm

**Current state:** `piece_notify.notify_waiters()` wakes ALL waiting peer tasks every time a piece completes, a peer connects, or a have message arrives. With 128 peers, each piece completion causes 128 task wakes — but only a few peers actually have new work available.

**Change:** Replace the broadcast `notify_waiters()` with targeted notification. When a piece completes, only notify peers that were blocked on that specific piece or have exhausted their piece queue. Implementation options:
- **Option A**: Per-peer Notify — each peer gets its own `Notify`, actor signals only relevant peers
- **Option B**: `Notify::notify_one()` instead of `notify_waiters()` — one peer wakes, checks for work, wakes next if needed (chain reaction)
- **Option C**: Conditional notify — only call `notify_waiters()` on high-impact events (piece complete, peer disconnect releasing pieces), skip low-impact events (individual have messages)

**Recommended: Option C** — lowest risk, biggest win. Most `peer_have()` calls add availability for pieces that peers aren't blocked on. Only `complete_piece()`, `fail_piece()`, `release_peer_pieces()`, and `set_endgame(false)` meaningfully unblock waiting peers. Remove `notify_waiters()` from `add_peer()`, `peer_have()`, and `update_wanted()`.

**Files:**
- `crates/torrent-session/src/piece_reservation.rs` — Remove `notify_waiters()` from low-impact events

### Part 3: Move Chunk Tracking to Peer Task (Stretch Goal)

**Current state:** Every block received follows: peer task receives Piece message → sends `PeerEvent::PieceData` to actor → actor writes to disk → actor updates chunk tracker → actor checks piece completion. This is 2 context switches per block (peer→actor, then actor→disk).

**Change:** Have the peer task update a shared chunk tracker directly, only notifying the actor when a piece is complete (all chunks received). This reduces actor wakes from ~6250/sec (per-block) to ~5-10/sec (per-piece).

**Risk:** High — chunk tracker is currently not thread-safe, and piece verification must happen on the actor side. This would require making `ChunkTracker` thread-safe or using a per-piece lock. **Only attempt if Parts 1-2 don't achieve target.**

**Files:**
- `crates/torrent-storage/src/chunk_tracker.rs` — Add thread-safe wrapper
- `crates/torrent-session/src/peer.rs` — Direct chunk tracking updates
- `crates/torrent-session/src/torrent.rs` — Change to piece-complete-only events

## Task Breakdown

### Task 1: Increase event channel capacity
- Increase from 256 to 2048 (or higher)
- Benchmark to measure context switch reduction

### Task 2: Reduce notify wake storms
- Remove `notify_waiters()` from `add_peer()`, `peer_have()`, `update_wanted()`
- Keep only in `complete_piece()`, `fail_piece()`, `release_peer_pieces()`, `set_endgame(false)`
- Verify peers still wake when they should

### Task 3: Verification and benchmark
- `cargo clippy --workspace -- -D warnings`
- `cargo test --workspace`
- 3-trial Arch ISO benchmark comparing context switches, CPU time
- Version bump to 0.79.0

### Task 4 (Stretch): Shared chunk tracking
- Only if Tasks 1-2 don't hit target
- Make ChunkTracker thread-safe, move block tracking to peer task

## Success Criteria

- Context switches < 250K (down from 412K)
- CPU time < 15s (down from 19.3s)
- No speed regression
- All tests pass, zero clippy warnings
