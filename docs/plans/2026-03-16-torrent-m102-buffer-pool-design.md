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
    write_cache: Mutex<HashMap<(InfoHash, u32), PieceWriteBuffer>>,

    // Read side: ARC cache keyed by (info_hash, piece_index)
    // Values are full pieces (Bytes), not individual blocks
    read_cache: Mutex<ArcCache<(InfoHash, u32), Bytes>>,

    // Storage backends for prefetch reads
    storages: RwLock<HashMap<InfoHash, Arc<dyn TorrentStorage>>>,

    // Channel to M101's batched writer task
    write_tx: mpsc::Sender<WriteJob>,

    // Statistics (all atomic, lock-free)
    stats: BufferPoolStats,
}

struct PieceWriteBuffer {
    blocks: BTreeMap<u32, Bytes>,  // begin_offset -> data (sorted for sequential flush)
    total_bytes: usize,
    piece_size: usize,             // expected total for completion detection
}
```

**Key properties:**
- Read cache stores **full pieces** as `Bytes`, not individual blocks
- Write cache stores blocks per piece in BTreeMap for completion detection
- `write_used + read_used` must never exceed `total_capacity`
- Writes always get priority over reads (same as libtorrent 1.x)
- ARC algorithm for reads: adaptive recency/frequency balance with ghost lists

### Write Path

```rust
pub enum WriteStatus {
    PieceComplete,    // all blocks received — triggers flush + verify
    Buffered,         // block cached, piece still incomplete
    BackPressure,     // pool full, flushed oldest incomplete piece to make room
}

fn write_chunk(&self, info_hash: InfoHash, piece: u32, begin: u32, data: Bytes)
    -> Result<WriteStatus>
```

**Flow:**

1. Insert block into `write_cache[(info_hash, piece)].blocks`
2. Check completion — if `total_bytes == piece_size`:
   - Drain all blocks from `PieceWriteBuffer`
   - Send as `WriteJob`s through `write_tx` to M101's batched writer
   - Free the `write_used` bytes
   - Return `PieceComplete` (caller triggers verification)
3. Check capacity — if `write_used + read_used > total_capacity`:
   - First try evicting from read cache (least valuable ARC entries)
   - If read cache is at its 8 MiB floor, evict the oldest incomplete piece from write cache — flush its blocks to M101's writer (partial flush, re-read from disk during verification)
   - Return `BackPressure`

**Zero-copy:** Accept `Bytes` directly from the wire. `Bytes` is reference-counted — storing it in BTreeMap is an Arc bump, no data copy.

**Flush ordering:** BTreeMap sorts blocks by offset, so flushed blocks go to disk in order — sequential writes friendly to both HDD and SSD.

### Read Path + Prefetching

```rust
fn read_chunk(&self, info_hash: InfoHash, piece: u32, begin: u32, length: usize)
    -> Result<Bytes>
```

**Flow:**

1. **Check write cache** — if block exists in `write_cache[(info_hash, piece)].blocks`, return it directly (hot data, not yet flushed)
2. **Check read cache** — if ARC has the full piece, slice `[begin..begin+length]` and return
3. **Cache miss → prefetch entire piece:**
   - Read full piece from storage via `storage.read_piece(piece)` (one I/O per file segment)
   - Insert full piece into ARC read cache
   - ARC evicts least valuable entry if over capacity
   - Slice out requested block and return
4. Update ARC hit/miss stats for dynamic split decisions

**Seeding scenario (why this works):**
- Peer A requests block 0 of piece 42 → cache miss → read full piece → cache it → return block 0
- Peer A requests block 1 of piece 42 → cache hit → instant, zero I/O
- Peer B also wants piece 42 (rarest-first convergence) → cache hit → instant
- 16 blocks served from 1 disk read instead of 16

**VOLATILE_READ flag:** For force-recheck, bypass the cache entirely. M101's streaming verify handles this path — sequential hash checking should not pollute the read cache.

### Cache-Aware BEP 6 Suggest

```rust
fn hot_pieces(&self, info_hash: InfoHash) -> Vec<u32>
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
    let read_hit_rate = stats.read_hits / (stats.read_hits + stats.read_misses);
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
| `torrent-session/src/buffer_pool.rs` | **New** — BufferPool, PieceWriteBuffer, BufferPoolStats, config | ~400 |
| `torrent-session/src/disk.rs` | Wire BufferPool into DiskIoBackend read/write paths | ~60 |
| `torrent-session/src/disk_backend.rs` | Route reads/writes through BufferPool | ~40 |
| `torrent-session/src/torrent.rs` | Register storage with BufferPool on torrent add | ~10 |
| `torrent-session/src/settings.rs` | Add BufferPoolConfig fields to SessionSettings | ~15 |
| `torrent-wire/src/handler.rs` | Send SuggestPiece from hot_pieces() on peer connect | ~20 |
| `torrent-storage/src/cache.rs` | Expose ARC T2 entries for hot_pieces query | ~15 |
| **Total** | | **~560 lines** |

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
