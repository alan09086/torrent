# Torrent vs rqbit: Detailed Architecture Comparison

Reference for post-bugfix architectural improvements. rqbit consistently achieves ~80 MB/s on well-seeded torrents. Torrent peaks at 116 MB/s but is inconsistent.

## Request Scheduling

### rqbit: Event-Driven Semaphore
- Per-peer `Semaphore(0)` — starts empty
- On unchoke: `add_permits(128)` — immediately unlocks 128 request slots
- Per received block: `add_permits(1)` — replenishes one slot instantly
- Chunk requester task: `loop { acquire_permit(); send_request(); }` — zero latency between receipt and next request
- Single-piece-at-a-time reservation, but full request pipelining across 128 permits
- **No tick-based evaluation. Fully reactive.**

### torrent: Tick-Based Pipeline + EWMA
- Pipeline evaluated on periodic ticks
- EWMA-based dynamic queue depth: `depth = (ewma_rate * 3.0) / 16384`
- Initial depth: 128, max: 250
- EWMA alpha: 0.3 (30% weight on recent data)
- Slow-start phase exits on plateau detection (delta < 10 KB/s)
- `pick_blocks()` called in a loop to fill slots (commit 9687704)

### Key Difference
Torrent has **latency gaps** between block receipt and next request due to tick scheduling. rqbit's semaphore fires instantly. The EWMA depth formula can also create negative feedback loops: brief throughput dip -> reduced depth -> less data in flight -> further dip. rqbit avoids this entirely with fixed 128 permits.

### Recommendation
Adopt semaphore-based per-peer request scheduling. Keep EWMA for monitoring/stats, but decouple it from request depth decisions. Fixed depth (128) with semaphore gating matches rqbit's proven model.

---

## Piece Selection

### rqbit: Adaptive Stealing (No Rarest-First)
- Three-stage selection per piece request:
  1. **Aggressive steal** (10x threshold): steal from peers 10x slower than requesting peer
  2. **Standard reservation**: reserve next needed piece (sequential/rarest order)
  3. **Conservative steal** (3x threshold): steal from peers 3x slower near completion
- Piece states: HAVE, QUEUED, IN_FLIGHT, NOT_NEEDED
- No explicit rarity tracking — time-based stealing handles distribution naturally
- Computationally cheaper than rarity-based selection

### torrent: Rarest-First with Layered Priority
- Per-piece availability counter (updated on peer connect/bitfield)
- Selection priority stack:
  1. Streaming pieces (file streaming window)
  2. Time-critical pieces (first/last of high-priority files)
  3. Suggested pieces (BEP 6)
  4. Partial pieces with unassigned blocks (speed affinity)
  5. New pieces (sequential/random/rarest-first)
- Speed-based affinity: Slow (<10 KB/s), Medium (10-100 KB/s), Fast (>100 KB/s)
- Extent affinity: fast peers prefer continuing pieces
- Endgame mode with duplicate requests + auto-cancel

### Key Difference
Torrent's piece picker is more sophisticated (better for swarm health and rare piece distribution) but has more overhead per selection. rqbit's stealing approach is simpler and optimizes for raw throughput by reassigning work from slow peers quickly. Torrent's endgame mode is a strength rqbit lacks.

### Recommendation
Keep torrent's rarest-first as the base algorithm. Add aggressive piece stealing as an overlay: if a peer is >5x slower than the median, reassign its in-flight blocks. This combines swarm-healthy selection with rqbit's adaptive speed optimization.

---

## Peer Management

### rqbit
- **Max peers**: 128 (hard semaphore limit)
- **Connection**: immediate spawn on discovery (no batching)
- **TCP timeout**: 10 seconds (configurable)
- **Retry**: exponential backoff (100ms-30s) via `backoff` crate, permanent drop on exhaustion
- **Keep-alive**: 120 seconds
- **Read/write timeout**: 10 seconds
- **No zombie tracking needed** — semaphore naturally limits connections

### torrent
- **Max peers**: 200 (configurable)
- **Connection**: `try_connect_peers()` triggered on peer discovery
- **TCP timeout**: 5 seconds
- **Snub timeout**: 60 seconds (queue depth forced to 1)
- **Zombie issue**: ~100 peers with empty bitfields observed consuming slots
- **Peer turnover**: 4% replacement every 300 seconds (30 peer changes/hour)

### Key Difference
rqbit's 128 peer limit with aggressive retry keeps the active peer pool healthy. Torrent's 200 limit allows more zombies, and the 60-second snub timeout is 6x longer than rqbit's effective timeout (10s read/write). Torrent's peer turnover is also very conservative (4% every 5 minutes).

### Recommendation
- Reduce snub timeout to 15-20 seconds
- Add zombie pruning: disconnect peers with empty bitfields after 30 seconds
- Consider reducing max peers to 150 (less overhead per tick evaluation)
- Increase peer turnover: 10% every 120 seconds for faster churn of underperformers

---

## Disk I/O

### rqbit: Zero-Copy Socket-to-File
- Direct I/O: data flows from socket read buffer -> file via `pwrite_all_vectored()`
- Vectored writes: multiple non-contiguous buffers in a single syscall
- No intermediate buffering layer
- Positioned I/O (`pread_exact`, `pwrite_all`) — no seeking
- 32 KB ring buffer (`ReadBuf`) for incoming messages
- Bitfield flushed every 16 MB

### torrent: Store Buffer + Fire-and-Forget
- Block arrives -> store buffer (Mutex<HashMap>) -> fire-and-forget async write
- Piece verification reads from store buffer (in-memory, avoids disk read-back)
- Write buffer: 16 MB max (oldest piece flushed first)
- ARC read cache: 48 MB
- Disk I/O threads: `(cores/2).clamp(4, 16)`
- Hashing threads: `(cores/4).clamp(2, 8)`

### Key Difference
rqbit eliminates intermediate copies entirely. Torrent's store buffer adds a layer (Mutex contention, memory copies) but enables in-memory verification which avoids reading pieces back from disk. Both approaches have merit — torrent's is better for verification speed, rqbit's is better for write throughput.

### Recommendation
Fix the immediate Mutex contention bug (hashing under lock). Long-term, consider a lock-free store buffer using `DashMap` or per-piece `RwLock` instead of a single global Mutex. The store buffer concept is sound (avoiding disk read-back for verification) — the implementation just needs to avoid contention.

---

## Choking Algorithm

### rqbit: Minimal
- No complex unchoking algorithm
- Unchoke on connection, send 128 permits
- Relies on peer selection and stealing for balance

### torrent: Full Implementation
- Fixed-slots: 4 regular + 1 optimistic unchoke (configurable)
- Rate-based alternative: dynamic slot allocation based on throughput
- Seed mode strategies: FastestUpload, RoundRobin, AntiLeech
- Evaluation interval: every 10 seconds

### Key Difference
rqbit's simplicity avoids overhead. Torrent's implementation is more protocol-correct and better for swarm health. This is a strength of torrent's, not a gap.

### Recommendation
Keep torrent's choking implementation. Consider reducing evaluation interval from 10s to 5s for faster adaptation to changing peer conditions.

---

## Buffer & Memory Management

### rqbit
- 32 KB ring buffer per peer (fixed allocation)
- ~16.4 KB reused write buffer per peer (stack-allocated)
- No pre-allocation for pieces — chunks written to disk immediately
- Minimal memory footprint: ~50 KB per active peer

### torrent
- Store buffer: up to 16 MB global
- ARC read cache: up to 48 MB
- Per-peer channel: 256 slots
- In-flight memory: ~128 slots x 16 KB x peers (e.g., 20 peers x 128 x 16 KB = 40 MB transient)
- Zero-copy wire codec (Bytes throughout)

### Key Difference
rqbit uses dramatically less memory per peer but requires disk read-back for verification. Torrent uses more memory but avoids verification I/O. Both are valid tradeoffs.

### Recommendation
No changes needed for the buffer architecture. The store buffer approach is a valid optimization. Just fix the contention issues.

---

## Network Layer

### rqbit
- Tokio-based with socket splits (read/write halves)
- Vectored I/O via `BoxAsyncReadVectored`
- 32 KB ring buffer with scatter-gather reads
- TCP preferred (1s head start over uTP)
- Speed estimator updates every 100ms

### torrent
- Tokio-based with `FramedRead`/`FramedWrite` + `MessageCodec`
- Zero-copy message decoding (Bytes)
- Direct-write encoding (no double-copy)
- Peer command channel capacity: 256
- uTP fallback with 5s timeout

### Key Difference
Both use tokio effectively. rqbit's ring buffer approach is slightly more efficient for scatter-gather reads. Torrent's framed codec approach is more idiomatic tokio but may have slightly higher overhead per message boundary.

### Recommendation
No immediate changes needed. Long-term, consider whether a ring buffer approach would reduce per-message overhead.

---

## Constants Comparison

| Parameter | torrent | rqbit | Notes |
|-----------|---------|-------|-------|
| Initial queue depth | 128 | 128 (semaphore) | Same |
| Max queue depth | 250 | 128 (fixed) | torrent allows more, but EWMA can reduce it |
| Chunk size | 16 KB | 16 KB | Same (BEP standard) |
| Max peers | 200 | 128 | torrent allows more |
| Connect timeout | 5s | 10s | torrent is more aggressive |
| Read/write timeout | N/A | 10s | torrent uses snub detection instead |
| Snub timeout | 60s | N/A | rqbit uses read timeout + stealing |
| Keep-alive | implicit | 120s | |
| Unchoke slots | 4+1 | N/A | rqbit doesn't implement traditional choking |
| Choking interval | 10s | N/A | |
| Peer turnover | 4%/300s | N/A | rqbit uses backoff + retry |
| Speed estimator | EWMA alpha=0.3 | atomic counters | Different approaches |
| Disk write buffer | 16 MB | 0 (direct write) | |
| Read cache | 48 MB | 0 | |
| Peer channel capacity | 256 | unbounded | |
| Disk I/O threads | (cores/2).clamp(4,16) | N/A (sync writes) | |

---

## Duplicate Block Prevention

### rqbit: Three-Layer Guard
1. **Per-peer inflight_requests HashSet** — `HashSet::insert()` returns false if block already requested from same peer; `HashSet::remove()` on receipt bails if block wasn't in flight
2. **Global inflight_pieces HashMap** — tracks which peer "owns" each piece; if stolen by another peer, data is discarded (`"was stolen by {peer}, ignoring"`)
3. **Chunk-level BitVector** — `mark_chunk_downloaded()` returns `PreviouslyCompleted` if all chunks in piece already set; early return skips disk write entirely

### libtorrent: Block State Machine
- Four states: `none → requested → writing → finished`
- State transitions prevent duplicate writes structurally — a block in `writing` or `finished` state cannot be overwritten
- End-game mode allows controlled redundancy (configurable via `strict_end_game` setting)
- Tracks redundant bytes received as a session statistic

### torrent (before fix): No Guard
- `handle_piece_data` called `enqueue_write` unconditionally — no check for already-received blocks
- In end-game mode or after timeout re-requests, duplicate blocks overwrote valid data in the store buffer
- This caused piece hash failures (SHA-1 mismatch) and forced re-downloads, hurting throughput

### torrent (after fix): Single-Layer Guard
- Added `chunk_tracker.has_chunk(index, begin)` check at top of `handle_piece_data`
- Returns early for already-received blocks, counting bytes as redundant download
- Equivalent to rqbit's Layer 3 (chunk-level BitVector)

### Future Consideration
- rqbit's Layer 2 (piece-level steal protection) could prevent wasted data transfers earlier
- Per-peer inflight tracking (Layer 1) would catch unsolicited data, but our actor model already validates via `pending_requests`

---

## Priority Improvements (Post-Bugfix)

1. **Semaphore-based request scheduling** — highest impact, replaces tick-based pipeline
2. **Aggressive piece stealing** — overlay on rarest-first picker
3. **Reduced snub/zombie timeouts** — faster recovery from stalled peers
4. **Queue depth floor** — prevent EWMA from dropping below 64
5. **Peer limit tuning** — reduce to 150, faster turnover cycle

## Architecture Improvements (Longer-Term)

1. **Per-peer chunk requester task** — dedicated task per peer (like rqbit) instead of centralized TorrentActor evaluation
2. **Lock-free store buffer** — DashMap or per-piece locks instead of global Mutex
3. **Ring buffer for wire reads** — reduce per-message framing overhead
4. **Connection retry with backoff** — exponential backoff instead of simple reconnect
