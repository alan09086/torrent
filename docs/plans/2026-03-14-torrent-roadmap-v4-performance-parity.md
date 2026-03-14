# Torrent Roadmap v4 — Performance Parity with rqbit

**Date:** 2026-03-14
**Target:** Close the 2.7x speed gap with rqbit through systematic optimization
**Current State:** v0.91.0, 1480 tests, M1-M91 complete
**Supersedes:** Nothing (extends roadmap v3 with performance milestones)

## Profiling Baseline (v0.91.0 vs rqbit)

| Metric | torrent v0.91.0 | rqbit | Gap | Target |
|--------|----------------|-------|-----|--------|
| Speed | 31 MB/s | 82.5 MB/s | 2.7x | ≥70 MB/s |
| Context switches | 462K-534K | 118K | 4x | <150K |
| CPU migrations | 132K-167K | 23K | 6x | <30K |
| Page faults | 60K | 7.7K | 8x | <15K |
| RSS | 130 MB | 38 MB | 3.4x | <60 MB |
| CPU time | 15s | 8.9s | 1.7x | <10s |

**Test torrent:** Arch Linux ISO (~1452 MB), DHT-only magnet, cold start.
**Test magnet:** Check `https://archlinux.org/download/` for current hash (old ISOs lose seeders).

### CPU Profile (perf report, top 15)

| % CPU | Function | Category |
|-------|----------|----------|
| 20.3% | `sha1_block_data_order_avx2` | Piece hashing |
| 4.5% | kernel syscalls | Network recv/send |
| 2.9% | kernel (epoll) | I/O multiplexing |
| 1.9% | libc (malloc/free) | Allocation |
| 1.6% | `tokio::..::Context::run` | Runtime scheduler |
| 1.0% | `tokio::..::Steal::steal_into` | Work stealing |
| 0.9% | `parking_lot::Condvar::wait` | Lock contention |
| 0.5% | `futex::Mutex::lock_contended` | Lock contention |
| 0.5% | `PieceReservationState::can_reserve` | Dispatch |
| 0.4% | `Condvar::notify_one_slow` | Lock notification |
| 0.4% | `peer::run_peer` | Peer task loop |
| 0.3% | `PieceReservationState::rebuild_candidates` | Dispatch sort |
| 0.3% | `DiskHandle::enqueue_write` | Disk queue |
| 0.3% | `hashbrown::RawTable::remove_entry` | HashMap ops |
| 0.3% | `WriteBuffer::write` | Write buffer |

---

## Phase 12: Performance Parity (M92-M100)

*Systematic optimization guided by profiling data. Each milestone targets a specific
bottleneck identified in the v0.91.0 flamegraph, perf stat, and heaptrack analysis.*

### Tier 1 — Structural bottlenecks (M92-M94)

These three milestones address the root causes of the 4x context switch gap,
the 1.8% lock contention, and the 8x page fault gap. Expected to close 60-70%
of the speed gap.

---

### M92: Peer Event Batching — Reduce Context Switches

**Problem:** Each 16KB block downloaded triggers a `PeerEvent::ChunkWritten` channel
send from the peer task to the TorrentActor. At 100 MB/s that's ~6K channel sends/sec
across ~12 peer tasks. Each send wakes the TorrentActor's `select!` loop, causing
a context switch. Result: 462K-534K context switches per download vs rqbit's 118K.

**Root cause:** The M78 "direct peer-to-disk write" optimization moved the actual
disk write into the peer task but still sends a lightweight `ChunkWritten` event
per block so the TorrentActor can track piece completion.

**Solution:** Batch `ChunkWritten` events per piece. Instead of sending 32 individual
`ChunkWritten` events (one per 16KB block in a 512KB piece), the peer task accumulates
block completions and sends a single `PieceBlocksBatch` event when either:
1. All blocks for a piece are written (piece complete), or
2. A batch timer fires (100ms) to flush partial progress

**Scope:**
- New `PeerEvent::PieceBlocksBatch { peer_addr, piece_index, blocks: Vec<(u32, u32)> }`
  variant that carries multiple block completions in one channel send
- Per-peer `PendingBlocks` accumulator in `run_peer()` — collects block completions,
  flushes on piece completion or 100ms timer
- `TorrentActor::handle_peer_event` handler for the batch variant — iterates blocks,
  updates chunk tracker, triggers verification when piece is complete
- Remove individual `ChunkWritten` variant (replaced entirely by batch)

**Expected impact:**
- Context switches: 462K → ~150K (32x fewer events per piece × typical piece count)
- Speed: +30-40% from reduced wakeup overhead

**Crates:** torrent-session (peer.rs, torrent.rs, types.rs)
**Tests:** ~4 (batch accumulation, timer flush, piece completion trigger, single-block piece)

---

### M93: Lock-Free Piece Dispatch — Eliminate Convoy Contention

**Problem:** `PieceReservationState` is behind a single `Arc<RwLock<>>` shared across
all peer tasks. When `candidates_dirty = true`, the first peer to call `try_reserve()`
takes a WRITE lock and runs `rebuild_candidates()` — an O(n log n) sort. All other
peers block on the lock. With ~12 concurrent peers calling `acquire_and_reserve()`,
this creates convoy effects visible in the profile as 1.8% lock contention.

**Root cause:** The `find_candidate()` hot path (called per block request, ~6K/sec)
falls back to O(n) scan when the candidate cache is dirty. The cache is invalidated
on every `add_peer()`, piece completion, and peer choke — which happen frequently.

**Solution:** Replace the RwLock with a two-tier design:
1. **Atomic piece state array** (`Vec<AtomicU8>`) — each piece has a state byte
   (Available/Reserved/Complete/Unwanted) that peers read without locking
2. **Per-peer candidate iterator** — each peer maintains its own rarest-first
   iterator backed by a shared immutable availability snapshot. No global lock
   needed for `find_candidate()`
3. **Reservation via CAS** — `try_reserve()` does `AtomicU8::compare_exchange`
   (Available → Reserved). No lock needed. Conflicts resolve via CAS retry.
4. **Periodic availability refresh** — a background task rebuilds the availability
   snapshot every 500ms (or on significant events like peer join/leave). Peers
   lazily pick up the new snapshot.

**Scope:**
- New `AtomicPieceStates` struct wrapping `Vec<AtomicU8>` with `try_reserve()`,
  `mark_complete()`, `release()` methods
- New `AvailabilitySnapshot` — immutable sorted piece list by rarity, cheaply cloneable
- Refactor `acquire_and_reserve()` to use atomic CAS instead of RwLock
- Remove `PieceReservationState`'s `sorted_candidates`, `candidates_dirty`,
  `rebuild_candidates()` — replaced by snapshot
- Keep `piece_owner` tracking in TorrentActor (single-threaded, no lock needed)

**Expected impact:**
- Lock contention: 1.8% → <0.1%
- Context switches: further reduction (no convoy wakeups)
- Speed: +10-15% from eliminating lock waits

**Crates:** torrent-session (piece_reservation.rs, peer.rs, torrent.rs)
**Tests:** ~6 (atomic reserve/release, CAS conflict, snapshot rebuild, endgame, priority pieces)

---

### M94: Memory Footprint Reduction — Cut RSS and Page Faults

**Problem:** RSS is 130 MB vs rqbit's 38 MB (3.4x). Page faults are 60K vs 7.7K (8x).
High page fault count means frequent TLB misses and cache-cold memory access, adding
latency to every operation.

**Root cause (heaptrack analysis needed for confirmation, but likely sources):**
- Per-peer write buffers: each peer task allocates a `WriteBuffer` for disk writes.
  With 50+ peers, these accumulate.
- `PieceReservationState` hashmaps: `piece_owner` (FxHashMap<u32, SocketAddr>),
  `peer_pieces` (FxHashMap<SocketAddr, Vec<u32>>), `current_progress` — all grow
  proportionally to piece count × peer count
- `Bitfield` clones: availability tracking creates per-peer bitfield copies
- Disk queue bounded at 512 entries × write buffers

**Solution:** Three-pronged memory reduction:
1. **Pooled write buffers** — Replace per-peer `WriteBuffer` with a shared buffer pool
   (`Arc<Mutex<Vec<BytesMut>>>`). Peers check out a buffer, write to disk, return it.
   Pool size capped at `min(peer_count, 16)`.
2. **Compact piece tracking** — Replace `FxHashMap<u32, SocketAddr>` piece_owner with
   a flat `Vec<Option<u16>>` indexed by piece (peer index, not full SocketAddr).
   16-bit peer index supports up to 65K peers. Eliminates hash overhead.
3. **Shared bitfield storage** — Instead of cloning bitfields per peer, store a single
   `Vec<AtomicU32>` availability counter array. Peers update atomically on Have messages.
   Eliminates per-peer `Vec<u32>` availability clones.

**Expected impact:**
- RSS: 130 MB → ~50-60 MB
- Page faults: 60K → ~15-20K
- Speed: +5-10% from better cache behavior

**Crates:** torrent-session (peer.rs, piece_reservation.rs, torrent.rs), torrent-storage
**Tests:** ~5 (buffer pool checkout/return, compact piece tracking, atomic availability)

---

### Tier 2 — Runtime efficiency (M95-M97)

These milestones optimize the async runtime and crypto overhead. Expected to
close another 15-20% of the gap.

---

### M95: Tokio Worker Affinity — Reduce CPU Migrations

**Problem:** 132K-167K CPU migrations per download vs rqbit's 23K (6x). The default
`#[tokio::main]` multi-threaded runtime has no thread affinity. The TorrentActor,
peer tasks, and disk tasks freely migrate between all CPU cores, destroying L1/L2
cache locality.

**Solution:**
- Configure tokio runtime with explicit worker thread count = `min(num_cpus, 8)`
- Use `tokio::task::LocalSet` or `runtime::Builder` thread naming for tracing
- Pin the TorrentActor to worker 0 using a dedicated single-threaded runtime
  for the main event loop (it's the bottleneck — all events funnel through it)
- Pin disk I/O tasks to workers 1-2 (I/O bound, benefit from cache locality)
- Let peer tasks float across remaining workers (they're I/O bound, less sensitive)

**Scope:**
- New `RuntimeConfig` in `Settings` — `worker_threads`, `actor_pinning` (bool)
- `SessionHandle::start_full()` creates a dedicated `LocalSet` for TorrentActor
  when `actor_pinning` is enabled
- `ClientBuilder::worker_threads()` fluent setter
- CLI `--workers` flag

**Expected impact:**
- CPU migrations: 140K → ~25K
- Speed: +5-10% from cache locality

**Crates:** torrent-session (session.rs, settings.rs), torrent (client.rs), torrent-cli
**Tests:** ~3 (custom worker count, actor pinning, settings validation)

---

### M96: Parallel Piece Verification — Offload SHA1

**Problem:** SHA1 hashing is 20.3% of CPU time. Currently runs inline on the disk
actor's blocking thread pool via `DiskJob::Hash`. Only one piece is hashed at a time
per disk actor. For a 1452 MB torrent with 2774 pieces, that's ~2774 sequential
SHA1 computations of 512KB each.

**Solution:**
- Create a dedicated `HashPool` with `num_cpus / 2` threads (CPU-bound work)
- `DiskActor` sends completed piece data to `HashPool` for verification instead
  of hashing inline
- `HashPool` returns results via a bounded channel to TorrentActor
- Multiple pieces can be hashed in parallel (up to pool size)
- SHA1 computation uses `aws-lc-rs` AVX2 assembly (already optimal per-call)

**Scope:**
- New `HashPool` struct in torrent-session — wraps `rayon::ThreadPool` or
  `std::thread` pool with crossbeam channel
- `DiskActor::dispatch_job` for `DiskJob::Hash` sends to HashPool instead of
  computing inline
- `TorrentActor` adds `hash_result_rx` arm to select! loop
- Remove synchronous `sha1_chunks` call from `PosixDiskIo::hash_piece()`

**Expected impact:**
- CPU time: 15s → ~12s (hashing parallelized across cores)
- Wall time: -1-2s (hashing no longer blocks disk write completion)

**Crates:** torrent-session (disk.rs, torrent.rs), torrent-core
**Tests:** ~4 (parallel hash correctness, pool shutdown, hash failure handling)

---

### M97: DHT Cold-Start Hardening

**Problem:** Trial 3 in the benchmark stalled for 270s (5.4 MB/s) due to DHT
cold-start issues. The BEP 42 fix (M91 post-fix) improved reliability from ~30%
to ~95%, but occasional stalls remain when the routing table has stale nodes.

**Solution:**
- **Delay initial `get_peers` until bootstrap completes** — Add a `bootstrap_complete`
  flag to DhtActor. `start_get_peers()` waits for bootstrap before querying. This
  ensures the routing table is populated before the first lookup.
- **Exponential backoff for V6 empty-table retries** — Currently retries every 100ms
  (connect timer tick) with a hard cap at 30. Change to exponential backoff:
  100ms, 200ms, 400ms, ..., 5s. Reduces V6 spam from 30 retries in 3s to ~8 retries.
- **Saved-state node verification** — When loading DHT state from disk, ping a
  random sample of 8 nodes before trusting the routing table. Mark unresponsive
  nodes as Questionable immediately.

**Expected impact:**
- Cold-start reliability: 95% → 99%+
- Eliminates 270s stall outliers
- Reduces V6 DHT log spam

**Crates:** torrent-dht (actor.rs), torrent-session (torrent.rs)
**Tests:** ~4 (bootstrap gate, backoff timing, node verification, V6 give-up)

---

### Tier 3 — Algorithmic refinements (M98-M100)

Final polish milestones. Each provides incremental gains. Expected to close
the remaining 5-10% gap.

---

### M98: Indexed Piece Selection — O(log n) Dispatch

**Problem:** `find_candidate()` does an O(n) scan when the candidate cache is dirty,
and `can_reserve()` does 4 checks (bitfield get, wanted check, have check, hashmap
lookup) per piece. For a 2774-piece torrent at ~6K calls/sec, this is measurable
(0.8% CPU combined).

**Solution:** Replace the sorted-candidates cache with a `BTreeSet<(u32, u32)>`
keyed by `(availability, piece_index)`. Insertion/removal is O(log n). Finding
the rarest available piece is O(log n) via `iter().next()`. Candidate invalidation
becomes targeted (remove/re-insert affected pieces) instead of full rebuild.

**Scope:**
- Replace `sorted_candidates: Vec<u32>` + `candidates_dirty: bool` with
  `candidate_set: BTreeSet<(u32, u32)>`
- `on_piece_complete()`: remove from set — O(log n)
- `on_availability_change()`: remove old entry, insert new — O(log n)
- `find_candidate()`: iterate set, check peer_has bitfield — O(k) where k is
  pieces checked before finding one the peer has

**Expected impact:**
- Dispatch CPU: 0.8% → <0.1%
- Eliminates rebuild_candidates O(n log n) spikes

**Crates:** torrent-session (piece_reservation.rs)
**Tests:** ~4 (insertion ordering, removal, availability update, rarest-first correctness)

---

### M99: io_uring Disk I/O

**Problem:** Disk writes use `pwrite()` syscalls (one per 16KB block write). Each
syscall is a user→kernel transition. With ~6K blocks/sec, that's ~6K syscalls/sec
just for writes, plus reads for piece verification.

**Solution:** Replace `pwrite()` calls with `io_uring` submission queue entries.
Batch multiple writes into a single `io_uring_enter()` call. CachyOS kernel 6.19
has full io_uring support.

**Scope:**
- New `IoUringDiskIo` implementing `DiskIoBackend` trait
- Uses `io-uring` crate for submission/completion ring management
- Batches up to 32 write SQEs before submitting
- Completion polling in a dedicated thread
- Feature-gated behind `io-uring` cargo feature (fallback to PosixDiskIo)
- Runtime detection: check `io_uring_setup` syscall availability

**Expected impact:**
- Syscall count: -50% for disk writes
- Speed: +5-10% from batched I/O
- Latency: lower tail latency for write completion

**Crates:** torrent-session (disk_backend.rs, new io_uring_backend.rs)
**Tests:** ~5 (write correctness, batch submission, completion handling, fallback)

---

### M100: Buffer Pooling & Final Polish

**Problem:** Residual memory and allocation overhead after M92-M99. `malloc`/`free`
show up as 1.9% in the profile. `hashbrown::RawTable::remove_entry` at 0.3% suggests
frequent hash map churn.

**Solution:**
- **BytesMut pool** for peer read buffers — reuse allocations across message reads
  instead of allocating fresh BytesMut per message
- **SmallVec for peer lists** — replace `Vec<SocketAddr>` in hot paths with
  `SmallVec<[SocketAddr; 8]>` to avoid heap allocation for common case
- **Reduce HashMap churn** — profile remaining FxHashMap operations and consider
  replacing hot-path maps with flat arrays where key space is bounded
- **Final benchmark** — 10-trial profiling run to verify all milestones achieved
  their targets

**Expected impact:**
- malloc/free CPU: 1.9% → <0.5%
- RSS: final reduction to ~40-50 MB
- Speed: within 15% of rqbit (≥70 MB/s target)

**Crates:** torrent-session, torrent-wire
**Tests:** ~3 (buffer pool, smallvec correctness, final benchmark)

---

## Milestone Summary

| Milestone | Title | Tier | Primary Target | Expected Δ Speed |
|-----------|-------|------|----------------|-----------------|
| M92 | Peer Event Batching | 1 | Context switches (462K→150K) | +30-40% |
| M93 | Lock-Free Piece Dispatch | 1 | Lock contention (1.8%→0.1%) | +10-15% |
| M94 | Memory Footprint Reduction | 1 | RSS (130MB→50MB), page faults | +5-10% |
| M95 | Tokio Worker Affinity | 2 | CPU migrations (140K→25K) | +5-10% |
| M96 | Parallel Piece Verification | 2 | SHA1 CPU (20%→parallel) | +5-10% |
| M97 | DHT Cold-Start Hardening | 2 | Reliability (95%→99%) | reliability |
| M98 | Indexed Piece Selection | 3 | Dispatch (0.8%→0.1%) | +2-3% |
| M99 | io_uring Disk I/O | 3 | Syscalls (-50%) | +5-10% |
| M100 | Buffer Pooling & Final Polish | 3 | Allocation (1.9%→0.5%) | +2-5% |

## Execution Order

Milestones are ordered by dependency and impact:

1. **M92** first — largest single impact, no dependencies
2. **M93** second — builds on M92's reduced event volume
3. **M94** third — independent, addresses memory bottleneck
4. **M95** fourth — runtime config, independent of dispatch changes
5. **M96** fifth — disk actor refactor, independent
6. **M97** sixth — DHT reliability, independent
7. **M98-M100** — incremental polish, order flexible

**Re-benchmark after M92, M94, and M97** to measure cumulative progress and
adjust remaining milestone scope.

## Verification

After each milestone:
1. `cargo test --workspace` — 1480+ tests, 0 failures
2. `cargo clippy --workspace -- -D warnings` — zero warnings
3. `benchmarks/quick_reliability.sh <magnet> 10 35` — 10/10 passes
4. `benchmarks/torrent_profile.sh <magnet> 5` — measure speed, ctx switches, RSS

After M100:
- Speed ≥70 MB/s (within 15% of rqbit)
- Context switches <150K
- CPU migrations <30K
- Page faults <15K
- RSS <60 MB
