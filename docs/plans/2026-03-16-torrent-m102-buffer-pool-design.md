# M102: libtorrent 1.x-Style Unified Buffer Pool

**Date:** 2026-03-16
**Status:** Design
**Depends on:** M101 (batched writer, streaming verify, SmallVec segments)
**Motivation:** libtorrent-rasterbar 1.x's hand-rolled buffer pool was the gold standard for BitTorrent disk I/O. Version 2.0 replaced it with mmap and regressed catastrophically — loss of cache awareness, network storage failures, RAM confusion, slower hash checking. We implement the 1.x approach: a unified, domain-aware buffer pool with prefetching, cache-aware piece suggestion, and dynamic read/write split.

## Problem

After M100 simplified our I/O to direct per-block pwrite (rqbit-style), we have:

- **No read cache** — every upload block request hits disk
- **No write coalescing** — every download block is a separate write (M101 batches the spawns but not the data)
- **No cache awareness** — we can't suggest hot pieces to peers (BEP 6)
- **No prefetching** — serving 16 blocks from one piece costs 16 syscalls instead of 1

This mirrors libtorrent 2.0's deficiency: the OS page cache is a black box with no domain knowledge.

## Design

### Core Structure

```rust
pub struct BufferPool {
    // Shared pool, 64 MiB default
    total_capacity: usize,

    // Dynamic partition tracking
    write_used: AtomicUsize,
    read_used: AtomicUsize,
    read_ceiling: AtomicUsize,

    // Write side: blocks buffered per piece, awaiting completion
    write_cache: Mutex<HashMap<(Id20, u32), PieceWriteBuffer>>,

    // Read side: byte-aware ARC cache keyed by (info_hash, piece_index)
    // Values are full pieces (Bytes), not individual blocks
    read_cache: Mutex<ByteArcCache<(Id20, u32), Bytes>>,

    // Storage backends + piece lengths for prefetch reads and completion detection
    storages: RwLock<HashMap<Id20, (Arc<dyn TorrentStorage>, Lengths)>>,

    // Channel to M101's batched writer task
    write_tx: mpsc::Sender<WriteJob>,

    // Statistics (all atomic, lock-free)
    stats: BufferPoolStats,
}

struct PieceWriteBuffer {
    blocks: BTreeMap<u32, Bytes>,  // begin_offset -> data (sorted for sequential flush)
    total_bytes: usize,
    piece_size: usize,             // from Lengths, set at first write via storages lookup
}
```

**Type notes:**
- `Id20` (from `torrent_core`) is the info hash type used throughout the codebase — no `InfoHash` alias exists
- `Lengths` is stored alongside each `TorrentStorage` to provide `piece_size(idx)` for write completion detection

**ByteArcCache adapter (~60 lines):**

The existing `ArcCache<K, V>` is count-based (capacity = number of entries). We wrap it in a `ByteArcCache` that:
- Tracks `used_bytes: usize` based on `Bytes::len()` of inserted values
- Enforces a byte-budget ceiling instead of entry count
- Provides an `on_evict` callback so the `BufferPool` can call `munlock()` and decrement `read_used`
- Exposes `t2_keys() -> Vec<K>` for the hot_pieces query (entries in the frequency list)

This adapter lives in `torrent-storage/src/cache.rs` alongside the existing `ArcCache`.

**Key properties:**
- Read cache stores **full pieces** as `Bytes`, not individual blocks
- Write cache stores blocks per piece in BTreeMap for completion detection
- `write_used + read_used` must never exceed `total_capacity`
- Writes always get priority over reads (same as libtorrent 1.x)
- ARC algorithm for reads: adaptive recency/frequency balance with ghost lists
- Mutex contention on read/write paths is a known limitation (same as prior `PosixDiskIo`); follow-up milestone could explore `parking_lot::Mutex` or lock-free structures if profiling shows contention

### Write Path

```rust
pub enum WriteStatus {
    PieceComplete,    // all blocks received — triggers flush + verify
    Buffered,         // block cached, piece still incomplete
    BackPressure,     // pool full, flushed oldest incomplete piece to make room
}

fn write_chunk(&self, info_hash: Id20, piece: u32, begin: u32, data: Bytes)
    -> Result<WriteStatus>
```

**Flow:**

1. Insert block into `write_cache[(info_hash, piece)].blocks`. On first block for a new piece, look up `piece_size` from the `Lengths` stored in `storages[(info_hash)]`
2. Check completion — if `total_bytes == piece_size`:
   - Drain all blocks from `PieceWriteBuffer`
   - Send as `WriteJob`s through `write_tx` to M101's batched writer
   - Free the `write_used` bytes
   - Return `PieceComplete` (caller triggers verification)
3. Check capacity — if `write_used + read_used > total_capacity`:
   - First try evicting from read cache (least valuable ARC entries)
   - If read cache is at its 8 MiB floor, evict the oldest incomplete piece from write cache — flush its blocks to M101's writer (partial flush, re-read from disk during verification)
   - Return `BackPressure`

**Zero-copy (write path):** Accept `Bytes` directly from the wire. `Bytes` is reference-counted — storing it in BTreeMap is an Arc bump, no data copy.

**Flush ordering:** BTreeMap sorts blocks by offset, so flushed blocks go to disk in order — sequential writes friendly to both HDD and SSD.

**Relationship to existing code:** This replaces `PosixDiskIo`'s write buffering and the `WriteBuffer` struct (both from pre-M100, already deleted in M100). The BufferPool owns the write-side responsibility that `PosixDiskIo` formerly had.

### Read Path + Prefetching

```rust
fn read_chunk(&self, info_hash: Id20, piece: u32, begin: u32, length: usize)
    -> Result<Bytes>
```

**Flow:**

1. **Check write cache** — if block exists in `write_cache[(info_hash, piece)].blocks`, return it directly (hot data, not yet flushed)
2. **Check read cache** — if ARC has the full piece, slice `[begin..begin+length]` and return
3. **Cache miss → prefetch entire piece:**
   - Read full piece from storage via `storage.read_piece(piece)` — returns `Vec<u8>`, converted to `Bytes` via `Bytes::from()` (move, not copy — but the `Vec` itself was allocated by the storage backend, so this is one allocation per prefetch, not zero-copy)
   - Insert full piece into byte-aware ARC read cache
   - ARC evicts least valuable entry if over byte ceiling
   - Slice out requested block and return (`Bytes::slice()` is zero-copy — shares backing memory)
4. Update ARC hit/miss stats for dynamic split decisions

**Zero-copy scope:** The write path is truly zero-copy (wire `Bytes` → BTreeMap with no clone). The read cache *hit* path is zero-copy (`Bytes::slice()`). The read cache *miss* path has one allocation (storage reads into `Vec<u8>` → `Bytes`). This is inherent — data must be read from disk into memory.

**Seeding scenario (why this works):**
- Peer A requests block 0 of piece 42 → cache miss → read full piece → cache it → return block 0
- Peer A requests block 1 of piece 42 → cache hit → instant, zero I/O
- Peer B also wants piece 42 (rarest-first convergence) → cache hit → instant
- 16 blocks served from 1 disk read instead of 16

**VOLATILE_READ flag:** For force-recheck, bypass the cache entirely. M101's streaming verify handles this path — sequential hash checking should not pollute the read cache.

### Cache-Aware BEP 6 Suggest

```rust
fn hot_pieces(&self, info_hash: Id20) -> Vec<u32>
```

**Flow:**

1. Query ARC read cache for all entries matching `info_hash`
2. Return piece indices sorted by **frequency score** (ARC's T2 list — pieces accessed more than once)
3. Cap at 16 pieces

**Integration:**
- On new peer connection (or periodically, every ~30 seconds), session calls `hot_pieces()` and sends `SuggestPiece` messages
- When a peer requests a suggested piece → guaranteed cache hit → zero disk I/O
- No unsuggest on eviction (protocol has no unsuggest) — worst case is a normal disk read

**Why T2 (frequency) not T1 (recency):**
- T1: accessed once — likely prefetched but unproven
- T2: accessed multiple times — proven popular, likely requested again
- Suggesting T2 maximises cache hit probability

### Dynamic Split

Every 5 seconds (piggybacks on session tick):

```rust
fn rebalance(&self) {
    let total_reads = stats.read_hits + stats.read_misses;
    if total_reads == 0 { return; }  // no data yet, keep current split
    let read_hit_rate = stats.read_hits as f64 / total_reads as f64;
    let write_pressure = write_used as f64 / total_capacity as f64;

    if read_hit_rate < 0.5 && write_pressure < 0.3 {
        // Reads missing, writes have slack — shift toward reads
        read_ceiling += REBALANCE_STEP;  // 1 MiB per tick
    } else if write_pressure > 0.7 && read_hit_rate > 0.8 {
        // Writes under pressure, reads healthy — shift toward writes
        read_ceiling -= REBALANCE_STEP;
    }

    // Hard floors: never below 8 MiB for either side
    read_ceiling = read_ceiling.clamp(MIN_CACHE, total_capacity - MIN_CACHE);
}
```

**Behaviour:**
- **Pure downloading:** write cache dominates, read shrinks to 8 MiB floor
- **Pure seeding:** write cache empty, read gets full 56 MiB (64 - 8 floor)
- **Mixed:** proportional split based on actual pressure
- 5-second tick prevents oscillation

### mlock

On `#[cfg(unix)]` platforms, call `libc::mlock()` on cache entry backing memory at insert, `munlock()` on evict. Prevents OS from paging cache to swap under memory pressure.

Silently no-op on non-Unix. Cost: one syscall per piece insert/evict — ~245 calls across cache lifecycle for 64 MiB pool. Negligible.

**Caveat:** `mlock` is best-effort for `Bytes` values. `Bytes` uses reference-counted backing memory; `Bytes::slice()` shares the same allocation. We only `mlock`/`munlock` full-piece entries in the ARC (not slices returned to callers), so the lifecycle is predictable: lock on ARC insert, unlock on ARC evict via the `ByteArcCache` eviction callback. If the backing `Bytes` is dropped while a slice is alive, the memory remains valid (refcounted) but unlocked — acceptable since the slice is short-lived (in-flight to a peer).

### Statistics

```rust
pub struct BufferPoolStats {
    // Read cache
    pub read_hits: AtomicU64,
    pub read_misses: AtomicU64,
    pub read_prefetches: AtomicU64,
    pub read_bytes_served: AtomicU64,

    // Write cache
    pub write_buffered_bytes: AtomicUsize,
    pub pieces_completed: AtomicU64,
    pub back_pressure_evictions: AtomicU64,

    // Pool health
    pub read_used_bytes: AtomicUsize,
    pub write_used_bytes: AtomicUsize,
    pub read_ceiling: AtomicUsize,

    // Suggest
    pub suggest_messages_sent: AtomicU64,
    pub suggest_cache_hits: AtomicU64,
}
```

Exposed via existing `DiskStats` struct for CLI display.

## Configuration

```rust
pub struct BufferPoolConfig {
    pub total_capacity: usize,    // default: 64 MiB
    pub min_read_cache: usize,    // default: 8 MiB (floor)
    pub min_write_cache: usize,   // default: 8 MiB (floor)
    pub rebalance_interval: Duration,  // default: 5 seconds
    pub rebalance_step: usize,    // default: 1 MiB
    pub max_suggest_pieces: usize, // default: 16
    pub enable_mlock: bool,       // default: true on unix
}
```

## Files Changed

| File | Change | Est. Lines |
|------|--------|------------|
| `torrent-session/src/buffer_pool.rs` | **New** — BufferPool, PieceWriteBuffer, BufferPoolStats, BufferPoolConfig | ~600 |
| `torrent-storage/src/cache.rs` | **Add** ByteArcCache adapter (byte-budget, eviction callback, t2_keys) | ~80 |
| `torrent-session/src/disk.rs` | Wire BufferPool into write path, pass to DiskIoBackend | ~60 |
| `torrent-session/src/disk_backend.rs` | Route reads/writes through BufferPool, remove direct storage calls | ~80 |
| `torrent-session/src/torrent.rs` | Register storage+Lengths with BufferPool on torrent add; modify `suggest_cached_pieces()` to use `hot_pieces()` | ~30 |
| `torrent-session/src/settings.rs` | Add BufferPoolConfig fields to SessionSettings | ~15 |
| **Total** | | **~865 lines** |

**Note:** SuggestPiece sending already exists in `torrent-session/src/torrent.rs` (`suggest_cached_pieces()` method). M102 modifies it to call `BufferPool::hot_pieces()` instead of the old `cached_pieces()`. No changes needed in `torrent-wire` — the wire crate only defines `Message::SuggestPiece`.

## Testing

### Unit tests (~14)

| Test | Validates |
|------|-----------|
| `write_chunk_buffers_until_complete` | Blocks accumulate, PieceComplete on last block |
| `write_chunk_accepts_bytes_zero_copy` | No .to_vec() — Bytes refcount stays at 1 |
| `read_chunk_hits_write_cache` | Block in write buffer served without read cache |
| `read_chunk_miss_prefetches_full_piece` | Miss triggers full piece read, subsequent blocks hit |
| `read_cache_arc_eviction` | Fill beyond capacity, ARC evicts least valuable |
| `back_pressure_evicts_read_first` | Pool full → read entries evict before write pieces |
| `back_pressure_flushes_oldest_write` | Read at floor + pool full → oldest write piece flushes |
| `dynamic_split_shifts_toward_reads` | High miss + low write pressure → ceiling moves |
| `dynamic_split_shifts_toward_writes` | High write pressure + high hit rate → ceiling moves |
| `dynamic_split_respects_floors` | Neither side drops below 8 MiB |
| `hot_pieces_returns_t2_entries` | Multi-access pieces appear in hot_pieces() |
| `hot_pieces_caps_at_16` | 30 hot pieces → only 16 returned |
| `volatile_read_bypasses_cache` | Force-recheck reads don't pollute cache |
| `mlock_called_on_insert_and_evict` | cfg(unix) — verify mlock/munlock via counter |

### Integration tests (~4)

| Test | Validates |
|------|-----------|
| `piece_completion_flows_to_writer_channel` | All blocks → PieceComplete → WriteJobs in receiver |
| `back_pressure_flush_reaches_disk` | Overflow pool → evicted piece on disk via writer |
| `prefetch_then_suggest_then_serve` | Read piece → hot_pieces includes it → request → cache hit |
| `full_download_verify_cycle` | Write blocks → complete → flush → streaming verify passes |

### Benchmark test (~1)

| Test | Validates |
|------|-----------|
| `seeding_throughput_with_cache` | 1000 random reads with 64 MiB cache — hit rate and throughput vs direct disk |

**Total: ~19 new tests**

## Success Criteria

- Read cache hit rate >80% when seeding with ≤245 unique pieces requested
- Suggest cache hit rate >60% (suggested pieces actually requested from cache)
- Zero back-pressure evictions under normal download at ≤100 MB/s
- RSS stays under 130 MiB total (64 MiB cache + ~65 MiB everything else)
- No performance regression on download path vs M101 baseline
