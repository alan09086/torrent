# M103: Per-Block Stealing & Reactive Dispatch

## Context

After M102's unified buffer pool, our disk I/O path is solid. The remaining 1.84x speed gap to rqbit (37 MB/s vs 68 MB/s) is **not CPU-bound** — we use less CPU (9.6s vs 10.4s) with better IPC (2.35 vs 1.67). The bottleneck is **network pipeline underutilisation**: we don't keep the pipe full.

Three architectural gaps identified in profiling:
1. **Piece-level reservation** — one peer owns an entire piece; fast peers starve waiting for slow peers
2. **500ms periodic snapshot rebuild** — stale availability data delays piece selection
3. **EndGame::pick_block at 6.3% CPU** — dedicated end-game state machine thrashes on piece selection

rqbit solves all three with one design: **per-block stealing with reactive dispatch**. No separate end-game mode — stealing IS end-game behaviour by default.

## Architecture

```
                        ┌─────────────────────────────────────────┐
                        │         TorrentActor                     │
                        │                                         │
                        │  piece_progress: Vec<PieceProgress>     │
                        │    ┌─ blocks_received: BitVec           │
                        │    ├─ blocks_requested: BitVec          │
                        │    ├─ owner: Option<u16> (primary peer) │
                        │    └─ total_blocks: u16                 │
                        │                                         │
                        │  atomic_states: AtomicPieceStates       │
                        │    (unchanged: Available/Reserved/       │
                        │     Complete/Unwanted/Endgame)           │
                        │                                         │
                        │  availability_snapshot: Arc<...>        │
                        │    (event-driven rebuild, no 500ms)     │
                        │                                         │
                        │  piece_notify: Arc<Notify>              │
                        │    (wake peers on new work)             │
                        └────────────┬────────────────────────────┘
                                     │ PeerCommand::BlockMap
                                     │ (sparse: only changed pieces)
                                     ▼
    ┌──────────────────────────────────────────────────────────────┐
    │  Peer Task (per peer)                                        │
    │                                                              │
    │  PeerDispatchState                                           │
    │    ├─ snapshot: Arc<AvailabilitySnapshot>                    │
    │    ├─ cursor: usize                                          │
    │    ├─ current_piece: Option<CurrentPiece>                    │
    │    │    ├─ piece: u32                                         │
    │    │    ├─ next_block: u32  (local sequential cursor)        │
    │    │    └─ blocks_requested: BitVec  (from actor's map)      │
    │    └─ lengths: Lengths                                       │
    │                                                              │
    │  next_block() flow:                                          │
    │    1. Current piece has unrequested blocks? → return block   │
    │    2. CAS try_reserve new piece? → set current, return b0   │
    │    3. Steal: scan snapshot for pieces with unrequested       │
    │       blocks owned by other peers → return stolen block      │
    │    4. None → wait on piece_notify                            │
    └──────────────────────────────────────────────────────────────┘
```

### State transitions (unchanged at piece level)

```
    Available ──── try_reserve() CAS ────► Reserved
        ▲                                      │
        │ release()                            │ mark_complete()
        │                                      ▼
        └──────────────────────────────── Complete

    Reserved ──── transition_to_endgame() ──► Endgame
                                                 │
                                            try_reserve() succeeds
                                            (multiple peers allowed)
```

### Block-level tracking (NEW)

```
    Per-piece PieceProgress:

    blocks_received:   [1 1 1 1 0 0 0 0 0 0]  ← disk-confirmed
    blocks_requested:  [1 1 1 1 1 1 0 0 0 0]  ← in-flight to peers
                                    ▲ ▲
                            peer A ─┘ └─ peer B (stolen)

    Stealable blocks = requested AND NOT received
    Unrequested blocks = NOT requested (highest priority)
```

## Design

### 1. PieceProgress — block-level tracking in TorrentActor

Replace the current per-piece `Option<u16>` owner with rich block-level state:

```rust
// In torrent.rs (TorrentActor fields)
pub(crate) struct PieceProgress {
    /// Which blocks have been received and written to disk/cache.
    pub blocks_received: BitVec,
    /// Which blocks have been requested from any peer (superset of received).
    pub blocks_requested: BitVec,
    /// Primary peer (first to reserve). Used for steal threshold comparison.
    pub owner: Option<u16>,
    /// Total blocks in this piece.
    pub total_blocks: u16,
}
```

**Stored in:** `TorrentActor::piece_progress: Vec<Option<PieceProgress>>` — `None` for Available/Complete/Unwanted pieces. `Some(...)` for Reserved/Endgame.

**Size:** ~40 bytes per active piece. With 256 max in-flight pieces × 40 bytes = ~10 KiB total. Negligible.

### 2. Block dispatch with stealing in PeerDispatchState

Modify `PeerDispatchState::next_block()` to add a steal phase:

```rust
// piece_reservation.rs — PeerDispatchState::next_block()
pub(crate) fn next_block(
    &mut self,
    peer_bitfield: &Bitfield,
    atomic_states: &AtomicPieceStates,
    block_maps: &BlockMaps,         // NEW: shared block-level state
) -> Option<BlockRequest> {
    // Phase 1: Current piece — sequential unrequested blocks (hot path, no atomics)
    if let Some(ref mut cp) = self.current_piece {
        if let Some(block) = cp.next_unrequested_block(block_maps) {
            return Some(block);
        }
        // Current piece exhausted (all blocks requested) — drop it
        self.current_piece = None;
    }

    // Phase 2: Reserve new piece (CAS, same as today)
    while self.cursor < self.snapshot.order.len() {
        let piece = self.snapshot.order[self.cursor];
        self.cursor += 1;
        if !peer_bitfield.get(piece) { continue; }
        if atomic_states.try_reserve(piece) {
            self.current_piece = Some(CurrentPiece::new(piece, &self.lengths));
            return self.current_piece.as_mut().unwrap()
                .next_unrequested_block(block_maps);
        }
    }

    // Phase 3: Steal — find Reserved/Endgame pieces with unrequested blocks
    for &piece in &self.snapshot.order {
        if !peer_bitfield.get(piece) { continue; }
        let state = atomic_states.get(piece);
        if state != PieceState::Reserved && state != PieceState::Endgame {
            continue;
        }
        if let Some(block) = block_maps.next_unrequested(piece, &self.lengths) {
            return Some(block);
        }
    }

    None
}
```

### 3. BlockMaps — shared block state accessible from peer tasks

The block-level state lives in TorrentActor but peer tasks need read access for steal decisions. Two options:

**Option A: Atomic BitVec per piece (lock-free)**
```rust
pub(crate) struct BlockMaps {
    /// Per-piece requested bitmap. AtomicU64 array — each bit = one block.
    /// For 256 KiB piece with 16 KiB blocks = 16 blocks = 1 AtomicU16.
    /// For 4 MiB piece with 16 KiB blocks = 256 blocks = 4 AtomicU64.
    maps: Vec<Option<AtomicBitVec>>,
}
```
Pro: Zero contention, peers read freely. Con: Needs atomic bit operations, complex for variable piece sizes.

**Option B: Arc<RwLock<BlockMaps>> with try_read()**
```rust
pub(crate) struct SharedBlockMaps {
    inner: Arc<RwLock<Vec<Option<PieceBlockState>>>>,
}
```
Pro: Simple. Con: Reader contention under high peer count.

**Option C: Actor broadcasts block maps to peers (message-passing)**
```rust
// TorrentActor sends PeerCommand::BlockMapUpdate { piece, requested_bitmap }
// Peer tasks maintain local copies, eventually consistent
```
Pro: No shared state, fits actor model. Con: Stale data, message overhead.

**RECOMMENDATION: Option A (atomic)**. The block maps are tiny (4 MiB piece = 32 bytes), read-heavy (every peer reads every dispatch), and write-rare (only on new request or block receipt). AtomicU64 arrays with `fetch_or` for marking requested, `load` for reading — no locks, no contention.

### 4. Reactive snapshot rebuild (replace 500ms timer)

Remove the 500ms `snapshot_rebuild_interval`. Instead, rebuild on events:

```rust
// In TorrentActor — events that trigger snapshot rebuild:
fn on_peer_bitfield_received(&mut self, ...) {
    self.invalidate_snapshot();  // New peer has pieces → rebuild
}
fn on_piece_verified(&mut self, piece: u32) {
    self.invalidate_snapshot();  // Piece complete → remove from ordering
}
fn on_piece_hash_failed(&mut self, piece: u32) {
    self.invalidate_snapshot();  // Piece released → re-add to ordering
}
fn on_peer_disconnected(&mut self, ...) {
    self.invalidate_snapshot();  // Availability changed
}

// Debounce: coalesce multiple events within 10ms into one rebuild
fn invalidate_snapshot(&mut self) {
    if !self.snapshot_dirty {
        self.snapshot_dirty = true;
        // Will rebuild on next tick of the actor loop (not a separate timer)
    }
}

// In the select! loop, check dirty flag on every iteration:
if self.snapshot_dirty {
    self.rebuild_availability_snapshot();
    self.snapshot_dirty = false;
}
```

This gives us **<1ms** reaction time instead of up to 500ms.

### 5. Simplify end-game (steal replaces end-game)

With per-block stealing, end-game behaviour emerges naturally:
- When few pieces remain, most pieces are Reserved with some blocks still unrequested
- Phase 3 (steal) in `next_block()` finds these blocks and assigns them to idle peers
- No separate EndGame state machine needed

**Migration path:**
1. Keep EndGame module initially but disable transition_to_endgame()
2. Instead, keep pieces in Reserved state — stealing handles the same work
3. After verification that steal covers all end-game scenarios, delete EndGame module

**For this milestone:** Keep EndGame as fallback. Add a setting `use_block_stealing: bool` (default true). When true, Phase 3 (steal) runs before end-game. When false, fall through to EndGame as today.

### 6. Block receipt flow

When a block is received:

```rust
// In TorrentActor::handle_block_received()
fn handle_block_received(&mut self, piece: u32, begin: u32, data: Bytes) {
    let block_index = begin / DEFAULT_CHUNK_SIZE as u32;

    // Update block maps (atomic)
    if let Some(ref maps) = self.block_maps {
        maps.mark_received(piece, block_index);
    }

    // Existing flow: write to disk, check completion
    self.disk.write_chunk(piece, begin, data, false);

    // Check if all blocks received
    if self.piece_progress[piece].all_received() {
        // Piece complete — enqueue hash verification
        self.enqueue_verify(piece, ...);
    }
}
```

### 7. Piece completion detection (BlockMaps authoritative)

**Change from M102:** The plan says "BufferPool authoritative for completion" but we passed piece_size=0. Now BlockMaps becomes the authority:

```rust
impl PieceProgress {
    fn all_received(&self) -> bool {
        self.blocks_received.all()  // All bits set
    }
}
```

This replaces ChunkTracker's completion detection for the pieces we're tracking. ChunkTracker remains for initial bitfield verification (which pieces we already have on disk).

## Files changed

| File | Change | Est. Lines |
|------|--------|------------|
| `torrent-session/src/piece_reservation.rs` | Add BlockMaps (atomic bit arrays), modify PeerDispatchState::next_block() with steal phase, add PieceProgress | ~200 |
| `torrent-session/src/torrent.rs` | Add piece_progress tracking, reactive snapshot (remove 500ms timer), block receipt updates, PieceProgress completion detection | ~150 |
| `torrent-session/src/peer.rs` | Pass block_maps to next_block(), handle BlockMapUpdate commands | ~30 |
| `torrent-session/src/end_game.rs` | Add use_block_stealing bypass, keep as fallback | ~20 |
| `torrent-session/src/settings.rs` | Add use_block_stealing: bool (default true) | ~5 |
| **Net total** | | **~405 new lines** |

## Existing code reused

- `AtomicPieceStates` — unchanged. Piece-level CAS reservation still works. Stealing doesn't change piece state.
- `AvailabilitySnapshot::build()` — unchanged algorithm, just triggered differently (event-driven vs timer).
- `PeerDispatchState` — extended with steal phase, same cursor/snapshot model.
- `PeerSlab` — unchanged arena allocation.
- `BufferPool` (M102) — unchanged, benefits from more parallel block writes.
- `HashPool` (M96) — unchanged, benefits from faster piece completion.

## Configuration

```rust
// In Settings, under piece picker section:
/// Enable per-block stealing from slow peers (default: true).
/// When enabled, idle peers steal unrequested blocks from pieces owned
/// by other peers, improving parallelism and eliminating the need for
/// explicit end-game mode.
#[serde(default = "default_true")]
pub use_block_stealing: bool,
```

## Testing (20 tests)

### Unit tests (12)

| # | Test | Validates |
|---|------|-----------|
| 1 | `block_maps_mark_requested` | AtomicBitVec mark + read roundtrip |
| 2 | `block_maps_mark_received` | Received bit set correctly |
| 3 | `block_maps_next_unrequested` | Returns first unrequested block index |
| 4 | `block_maps_all_requested` | Returns None when all blocks requested |
| 5 | `piece_progress_all_received` | Completion detection on all bits set |
| 6 | `piece_progress_partial` | Not complete with gaps |
| 7 | `next_block_phase1_current_piece` | Hot path: sequential block from current piece |
| 8 | `next_block_phase2_reserve_new` | Cold path: CAS reserve new piece |
| 9 | `next_block_phase3_steal` | Steal: find unrequested block in reserved piece |
| 10 | `next_block_steal_skips_complete` | Steal doesn't touch Complete pieces |
| 11 | `next_block_steal_requires_peer_has` | Steal only from pieces peer has |
| 12 | `reactive_snapshot_rebuild` | Dirty flag triggers rebuild on next tick |

### Integration tests (8)

| # | Test | Validates |
|---|------|-----------|
| 13 | `steal_completes_slow_peer_piece` | Fast peer steals blocks, piece completes faster |
| 14 | `steal_coexists_with_owner` | Owner and stealer both contribute blocks |
| 15 | `steal_handles_duplicate_blocks` | Two peers request same block, first write wins |
| 16 | `reactive_snapshot_faster_than_timer` | Event-driven rebuild responds in <10ms |
| 17 | `no_steal_when_disabled` | use_block_stealing=false → no Phase 3 |
| 18 | `endgame_fallback_when_steal_disabled` | EndGame still works as fallback |
| 19 | `block_maps_concurrent_access` | Multiple peers read maps simultaneously |
| 20 | `full_download_with_stealing` | Complete download with steal enabled |

## Success criteria

- **Mean speed ≥55 MB/s** (from 37 MB/s — closing ~60% of the gap to rqbit's 68 MB/s)
- **Speed StdDev ≤4 MB/s** (from 5.7 MB/s — more consistent)
- **EndGame::pick_block CPU <1%** (from 6.3% — steal replaces most end-game work)
- **No regression** in reliability (10/10 complete downloads)
- **RSS under 60 MiB** (no significant memory increase from BlockMaps)
- **Snapshot rebuild latency <1ms** (event-driven vs 500ms timer)

## Verification

```bash
# Build and test
cargo test --workspace
cargo clippy --workspace -- -D warnings

# Run specific tests
cargo test -p torrent-session block_maps
cargo test -p torrent-session steal

# Benchmark (10 trials)
./benchmarks/analyze.sh "<arch iso magnet>" -n 10

# Compare: check speed, stddev, EndGame CPU% in flamegraph
```

## NOT in scope

- **DashMap for peer storage** — lock-free peer map. Possible future optimisation but not the primary bottleneck. AtomicPieceStates already eliminates the piece-level lock.
- **Adaptive queue depth** — dynamic per-peer request depth based on choke/unchoke. Useful but secondary to block stealing.
- **io_uring / O_DIRECT** — kernel I/O optimisation. Separate milestone, orthogonal to network pipeline.
- **Vectored reads (readv)** — scatter-gather I/O for disk. Separate milestone.
- **Remove EndGame module entirely** — keep as fallback for this milestone, delete in a follow-up once steal is proven.

## Key decisions

| # | Decision | Rationale |
|---|----------|-----------|
| 1 | Atomic BitVec for block maps (not RwLock) | Zero contention on read-heavy path; block maps are tiny |
| 2 | Steal in Phase 3 of next_block() (not separate pass) | Minimal code change; steal is just another fallback |
| 3 | Event-driven snapshot (not timer) | <1ms reaction vs 500ms; reduces stale data |
| 4 | Keep EndGame as fallback | De-risks the change; can delete after validation |
| 5 | PieceProgress in TorrentActor (not peer-local) | Central source of truth; peers get read-only atomic view |
| 6 | No change to AtomicPieceStates | Piece-level CAS still works; steal doesn't change piece state |
| 7 | use_block_stealing setting | Easy A/B comparison for benchmarking |
