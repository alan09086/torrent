# M77: Context Switch Reduction

## Problem

Torrent generates 3.7x more context switches than rqbit (412K vs 112K) for the same download. This contributes to the 1.9x CPU time gap. The actor model creates many cross-task wake-ups: each block received by a peer task triggers a channel send to the actor, which wakes the actor task, which processes the event, which may wake other peer tasks.

## Benchmark Baseline

Must be re-measured after M76 ships. Success criteria are expressed as percentage improvements from actual post-M76 baseline, not absolute numbers. M77 depends on M76 completing first.

**Expected post-M76 baseline (approximate):** RSS ~80 MB, cache misses ~7M, context switches ~400K.

## Analysis

### Context Switch Sources (ranked by frequency)

1. **PeerEvent channel (peer → actor)**: Every block receipt sends `PeerEvent::PieceData`. At 100 MB/s with 16 KB blocks: ~6,250 channel sends/sec across all peers. Event channel capacity is already 2048 (not 256 — previously corrected).

2. **Notify wake storms (reservation state → peers)**: `piece_notify.notify_waiters()` wakes ALL 128 waiting peer tasks. Called from 9 sites in `piece_reservation.rs`:
   - Line 117: `add_peer()` — new peer connects
   - Line 141: `remove_peer()` — peer disconnects (releases pieces)
   - Line 154: `peer_have()` — peer announces a new piece
   - Line 169: `release_peer_pieces()` — peer choked/snubbed (releases pieces)
   - Line 191: `complete_piece()` — piece verified
   - Line 209: `fail_piece()` — piece hash failed (releases piece)
   - Line 222: `update_wanted()` — file priorities changed
   - Line 228: `set_priority_pieces()` — streaming/priority pieces changed
   - Line 238: `set_endgame(false)` — end-game deactivated

3. **PeerCommand channel (actor → peer)**: End-game requests, Have broadcasts, shutdown. Lower frequency.

4. **Verify result channel (disk → actor)**: ~5-10 piece verifications/sec.

5. **Tokio runtime overhead**: 160+ tasks competing for thread pool.

### rqbit Comparison

rqbit has fewer context switches because:
- Peer tasks write directly to shared state (`DashMap`, `RwLock`) — no actor channel
- No broadcast notify — piece-tracker state changes are localized
- Simpler task structure — fewer total tasks

## Changes

### Part 1: Reduce Notify Wake Storms

**Current state:** `piece_notify.notify_waiters()` wakes ALL 128 waiting peer tasks on every call. Many calls are for events that don't unblock any peer:
- `peer_have()` adds one piece of availability — unlikely to unblock peers blocked on OTHER pieces
- `add_peer()` signals that a new peer exists — doesn't change available pieces
- `update_wanted()` — only relevant if new files selected

**High-impact events (peers ARE waiting for these):**
- `complete_piece()` — releases piece for re-reservation if hash failed, frees capacity
- `fail_piece()` — piece becomes available for retry
- `release_peer_pieces()` — choked/snubbed peer's pieces become available
- `set_endgame(false)` — peers need to resume requesting
- `remove_peer()` — peer's pieces become available for others

**Low-impact events (peers NOT blocked on these):**
- `add_peer()` — no new pieces available
- `peer_have()` — single piece availability change, peers block on capacity/ownership not availability
- `update_wanted()` — rare (user changes file selection mid-download)
- `set_priority_pieces()` — rare (streaming cursor changes)

**Change:** Remove `notify_waiters()` from `add_peer()` and `peer_have()`. These are the two highest-frequency low-impact callers. `peer_have()` is called once per Have message — at 128 peers each announcing 2904 pieces, that's potentially thousands of broadcasts, each waking 128 tasks.

Keep notifications for: `remove_peer()`, `release_peer_pieces()`, `complete_piece()`, `fail_piece()`, `set_endgame(false)`, `update_wanted()`, `set_priority_pieces()`.

**Safety net:** Add a periodic `notify_waiters()` call on the pipeline_tick (every 1s) to prevent starvation. If a peer is blocked and a `peer_have()` made work available, it will be unblocked within 1s. This is cheap insurance — 1 broadcast/sec vs thousands.

**Accepted trade-off:** In poorly-seeded swarms where a new peer is the ONLY source of certain pieces, removing `add_peer()` notification delays unblocking by up to 1 second. In well-seeded swarms (the common case and benchmark scenario), this is negligible since most connected peers are seeds.

**Files:**
- `crates/torrent-session/src/piece_reservation.rs` — Remove `notify_waiters()` from `add_peer()` and `peer_have()`
- `crates/torrent-session/src/torrent.rs` — Add periodic `piece_notify.notify_waiters()` in pipeline_tick arm

### Part 2: Event Channel Batching Optimization

**Current state:** Event channel capacity is 2048. The actor drains up to 256 events per `select!` iteration using `try_recv()` after the first `recv()`. This is already efficient. However, each `send()` from a peer task still wakes the actor task if it was sleeping in `select!`.

**Change:** Increase the batch drain limit from 256 to 512. This reduces the number of `select!` loop iterations needed to process a burst of events, reducing the overhead of checking all 21 other select! arms (11 timers, 4 channels, 2 accept, 4 DHT discovery) between event batches. Start at 512; if context switches remain above target, increase to 1024 in a follow-up.

**Files:**
- `crates/torrent-session/src/torrent.rs` — Increase batch drain constant

### Part 3: Verification and Benchmark

- `cargo clippy --workspace -- -D warnings`
- `cargo test --workspace`
- 3-trial Arch ISO benchmark with `perf stat` comparing context switches, CPU time vs post-M76 baseline
- Version bump to 0.79.0

### Part 4 (Stretch): Move Block Tracking to Peer Task

**Only attempt if Parts 1-2 don't achieve target context switch reduction.**

Move chunk-level tracking to the peer task: instead of sending `PeerEvent::PieceData` for every block, the peer task writes the block to disk directly and only notifies the actor when a piece is complete (all chunks received). This reduces actor wakes from ~6,250/sec (per-block) to ~5-10/sec (per-piece).

**Risk: HIGH.** Requires thread-safe chunk tracker, changes disk I/O ownership, and piece verification must still happen actor-side. Lock contention on shared chunk tracker with 128 peers is a concern. Only pursue if Parts 1-2 fall short.

## Task Breakdown

### Task 1: Reduce notify wake storms
- Remove `notify_waiters()` from `add_peer()` and `peer_have()`
- Add periodic safety-net notification in pipeline_tick (1s)
- Verify peers still receive work promptly

### Task 2: Increase event batch drain limit
- Increase batch drain constant from 256 to 512
- Benchmark to measure context switch reduction

### Task 3: Verification and benchmark
- clippy + tests
- 3-trial perf stat benchmark
- Version bump to 0.79.0

### Task 4 (Stretch): Shared chunk tracking
- Only if Tasks 1-2 don't hit target
- Thread-safe ChunkTracker, per-block disk writes in peer task

## Success Criteria (relative to post-M76 baseline)

- Context switches reduced by > 30% from post-M76 baseline
- CPU time reduced by > 10% from post-M76 baseline
- No speed regression
- All tests pass, zero clippy warnings
