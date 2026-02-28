# M28: Advanced Piece Picker + Dynamic Request Queue + File Streaming

**Date:** 2026-02-28
**Status:** Design approved
**Crates:** ferrite-session (primary), ferrite (facade re-exports)
**Depends on:** M27 (multi-threaded hashing) merged
**Reference:** libtorrent-rasterbar piece_picker.cpp, streaming.rst, Arvid Norberg's blog posts

## Overview

M28 replaces the piece-granularity rarest-first picker with a layered priority system
operating at block level, adds per-peer dynamic request queue sizing with slow-start,
and introduces `FileStream` — an `AsyncRead + AsyncSeek` type over torrent file data
that drives piece priority for media streaming.

---

## 1. Advanced Piece Picker

### 1.1 Priority Layers (highest to lowest)

1. **Streaming window** — pieces within `readahead_pieces` of any active `FileStream`
   cursor. Picked sequentially from cursor position forward. Assigned to fastest peers
   first (sorted by estimated queue time).
2. **Time-critical** — first + last pieces of each file with `High` priority (media
   preview). Same fast-peer-first assignment.
3. **Partial piece completion** — in-flight pieces with unassigned blocks. Priority
   boost within tier: `effective_priority = priority * 2 + (is_partial as u8)`.
4. **Sequential mode** — when `sequential_download = true`, pick by ascending piece index.
5. **Normal rarest-first** — current behavior, but returns block-level assignments.

### 1.2 Block-Level Assignment

Replace `in_flight: HashSet<u32>` with:

```rust
struct InFlightPiece {
    assigned_blocks: HashMap<(u32, u32), SocketAddr>,  // (begin, length) → peer
    total_blocks: u32,
}
in_flight_pieces: HashMap<u32, InFlightPiece>
```

Multiple peers can work on different blocks of the same piece simultaneously.
`ChunkTracker::missing_chunks()` already supports this — the gap is only at the
TorrentActor level.

### 1.3 Speed Categories

```rust
enum PeerSpeed { Slow, Medium, Fast }
```

Thresholds: `Slow < 10 KB/s`, `Medium < 100 KB/s`, `Fast >= 100 KB/s`.

**Speed affinity**: when picking blocks from a partial piece, prefer peers in the same
speed category as existing block owners. Prevents a slow peer from holding the last
block of a fast peer's piece, stalling completion.

### 1.4 Snubbed Peer Handling

A peer is **snubbed** if no payload data received for `snub_timeout_secs` (default 60).
Snubbed peers:
- Restricted to **1 outstanding request**
- Use **reverse picking** (most common pieces, not rarest)
- Excluded from streaming/time-critical assignment
- Snub cleared when data arrives; peer re-enters slow-start

### 1.5 Seed Counter Optimization

Don't track seeds per-piece in the availability array. Maintain `seed_count: u32`.
When a seed connects, increment the counter instead of touching every entry.
Effective availability: `availability[i] + seed_count`.

### 1.6 Auto-Sequential on Partial Explosion

If `in_flight_pieces.len() > 1.5 * connected_peer_count`, temporarily override to
sequential picking to drain partial pieces. Deactivate when ratio drops below 1.0.

### 1.7 Initial Random Threshold

For the first `initial_picker_threshold` pieces (default 4), pick randomly from
available pieces. Avoids rarest-first stampede in new swarms where all peers compete
for the same rarest piece.

### 1.8 Whole-Piece Threshold (time-based)

`whole_pieces_threshold: u32 = 20` (seconds). If `piece_size / peer_download_rate <=
threshold`, assign all blocks of a new piece to that peer exclusively. Reduces disk
cache thrashing on the seeder side and improves piece completion rate.

Auto-activation: also triggered when partial pieces exceeds `1.5 * peer_count`
(complementary to auto-sequential).

### 1.9 BEP 6 Suggested Pieces

Suggested pieces from peers are checked after streaming/time-critical layers but
before rarest-first, as a hint to the picker.

---

## 2. Dynamic Request Queue Sizing

### 2.1 Formula: `request_queue_time`

Network RTT doesn't capture remote disk latency on saturated seeders (can add seconds).
Use time-based pipeline depth instead:

```
queue_depth = ewma_rate * request_queue_time / BLOCK_SIZE
clamped to [2, max_request_queue_depth]
```

Where `request_queue_time` defaults to 3.0 seconds and `BLOCK_SIZE = 16384` (16 KiB).
EWMA smoothing on rate with α = 0.3.

### 2.2 Per-Peer State

```rust
struct PeerPipelineState {
    ewma_rate_bytes_sec: f64,
    queue_depth: usize,
    in_slow_start: bool,
    last_second_bytes: u64,
    prev_second_bytes: u64,
    last_request_times: HashMap<(u32, u32), Instant>,  // (piece, begin) → sent time
}
```

### 2.3 Slow-Start Phase

1. Initial `queue_depth = 2`
2. Each received block increments `queue_depth` by 1 (exponential growth per RTT)
3. Exit slow-start when `last_second_bytes - prev_second_bytes < 10_240` (throughput
   plateaued, delta < 10 KB/s)
4. Switch to `request_queue_time` formula; never re-enter slow-start for this peer

A peer at 1 MB/s with 100ms RTT reaches optimal depth (~183) in under a second.

### 2.4 Block Timeout Re-Request

Per-block timeout with smart target selection:

- **Timeout:** `block_request_timeout_secs` (default 60)
- On timeout: re-request the **most recently requested** block from another peer
  (least likely to already be in-flight on the remote end)
- If timed-out block is the **last remaining block** in an otherwise complete piece:
  cancel and re-request immediately
- If other blocks in same piece still pending: postpone timeout by one block's worth
  of time before escalating

### 2.5 Snubbed Peer Override

When snubbed: `queue_depth = 1` regardless of formula. On data arrival: clear snub,
re-enter slow-start.

---

## 3. FileStream

### 3.1 Type

```rust
pub struct FileStream {
    disk: DiskHandle,
    lengths: Lengths,
    file_index: usize,
    file_offset: u64,           // absolute byte offset of file start in torrent
    file_length: u64,
    position: u64,              // current read position within file
    cursor_tx: watch::Sender<u64>,
    piece_ready_rx: broadcast::Receiver<u32>,
    have: watch::Receiver<Bitfield>,
    _read_permit: OwnedSemaphorePermit,  // concurrent read limiting
}
```

Implements `AsyncRead + AsyncSeek + Unpin + Send`.

### 3.2 Read Flow

```
poll_read(buf):
  1. Map (file_offset + position) → (piece_index, begin, length) via Lengths
  2. Check have bitfield for piece_index
  3. If available:
     → disk.read_chunk(piece, begin, min(length, buf.len()), SEQUENTIAL)
     → copy to buf, advance position
  4. If NOT available:
     → store waker
     → wait on piece_ready_rx for matching piece_index
     → wake and retry
```

### 3.3 Seek Flow

```
start_seek(SeekFrom):
  1. Compute new absolute position
  2. Update self.position
  3. Send position via cursor_tx → TorrentActor
  4. Actor recomputes streaming window
  5. Actor cancels non-priority outstanding requests (selective, not all)
```

### 3.4 Request Cancellation on Priority Change

When a FileStream opens or seeks:
1. Actor builds new priority piece set (streaming window + time-critical)
2. Scans peers' `pending_requests` for blocks not in priority set and not in
   partial pieces >75% complete
3. Sends `Cancel` for those blocks, freeing pipeline slots
4. Fills freed slots with streaming window blocks assigned to fastest peers

### 3.5 Streaming Peer Ranking

Blocks from the streaming window are assigned to peers sorted by estimated queue time:

```
est_queue_time = pending_bytes / ewma_rate
```

Lowest queue time first. After each assignment, recalculate and re-sort. Matches
libtorrent's `request_time_critical_pieces()` algorithm.

### 3.6 Streaming Timeout Escalation

For pieces in the streaming window, more aggressive than the standard 60s timeout:

- Track mean and deviation of piece download time (EWMA)
- Streaming piece times out when: `elapsed > mean_download_time + deviation / 2`
- On timeout: "busy request" — request same block from additional peer
- Multiple timeouts = multiple redundant peers
- Never request same block twice from same peer

### 3.7 Streaming Cursor Management

```rust
streaming_cursors: Vec<StreamingCursor>,

struct StreamingCursor {
    file_index: usize,
    cursor_piece: u32,
    readahead_pieces: u32,
    cursor_rx: watch::Receiver<u64>,
}
```

Before each pick cycle: update cursor positions, build `BTreeSet<u32>` of streaming
priority pieces (union of all cursor windows).

### 3.8 Multiple Concurrent Streams

Each FileStream is independent — different files, different cursors. Streaming window
is the union. On FileStream drop: `cursor_tx` drops → actor detects via
`cursor_rx.changed()` error → removes cursor. If no cursors remain, streaming layer
deactivates.

### 3.9 Peer Reconnection on Stream Open

When `open_file()` is called and peers are in `NotNeeded` state (disconnected because
download was complete for previously-wanted files), attempt to reconnect them.

### 3.10 Concurrent Read Limiting

Semaphore bounds concurrent blocking disk reads from FileStreams (default 8). Prevents
seek bursts from overwhelming the disk I/O thread pool.

---

## 4. Integration

### 4.1 Full Pick Cycle

```
request_pieces_from_peer(peer_addr):
  1. Compute slots: peer.pipeline.queue_depth - peer.pending_requests.len()
     If snubbed: slots = min(1, slots)
     If slots == 0: return

  2. While slots > 0:
     a. STREAMING — streaming window blocks, fastest-peer-first
        (skip if peer is snubbed)
     b. TIME-CRITICAL — first/last file pieces, fastest-peer-first
        (skip if peer is snubbed)
     c. SUGGESTED — BEP 6 suggested pieces from this peer
     d. PARTIAL — unassigned blocks in in_flight_pieces, speed affinity
     e. NEW PIECE:
        → if partials > 1.5 * peers: force sequential
        → if completed < initial_picker_threshold: random
        → if snubbed: reverse (most common)
        → if sequential_download: ascending index
        → else: rarest-first
        → whole-piece check: piece_size / rate <= threshold → exclusive
     f. END-GAME — duplicate blocks, streaming pieces first
     g. Decrement slots per block

  3. Batch-send all Request messages
```

### 4.2 End-Game Mode

- Activation: `have + in_flight_pieces.len() >= num_pieces` (unchanged)
- `EndGame.activate()` reads from `in_flight_pieces` (block-level structure)
- Streaming window pieces get duplicate-requested first
- Speed affinity applies — fast peers get streaming/rare duplicates
- Snubbed peers capped at 1 request

### 4.3 Feature Interactions

| Feature | Interaction |
|---------|-------------|
| Selective download (M12) | `wanted_pieces` filters all layers. `open_file()` rejects Skip files. |
| Bandwidth limiter (M14) | Independent — queue depth sizes pipeline, limiter gates I/O. |
| Smart banning (M25) | Parole bypasses block splitting. Speed affinity skips paroled peers. |
| Async disk I/O (M26) | FileStream uses `DiskHandle::read_chunk()` with `SEQUENTIAL` flag. |
| Multi-threaded hashing (M27) | Piece verification uses M27 parallel hasher. |
| Super seeding (M23) | No interaction — upload-only. |

---

## 5. Config Additions

```rust
// TorrentConfig
pub sequential_download: bool,              // default: false
pub initial_picker_threshold: u32,          // default: 4
pub whole_pieces_threshold: u32,            // default: 20 (seconds)
pub snub_timeout_secs: u32,                 // default: 60
pub readahead_pieces: u32,                  // default: 8
pub streaming_timeout_escalation: bool,     // default: true

// SessionConfig
pub max_request_queue_depth: usize,         // default: 250
pub request_queue_time: f64,                // default: 3.0 (seconds)
pub block_request_timeout_secs: u32,        // default: 60
pub max_concurrent_stream_reads: usize,     // default: 8
```

---

## 6. File Changes

| File | Changes |
|------|---------|
| `piece_selector.rs` | Major rewrite — priority layers, block-level returns, speed affinity, reverse picking, seed counter, random initial, auto-sequential |
| `torrent.rs` | `request_pieces_from_peer()` rewrite, `in_flight_pieces`, streaming cursors, snub detection, request cancellation, peer ranking |
| `end_game.rs` | Read from `in_flight_pieces`, streaming-aware duplicate ordering |
| `types.rs` | New config fields on TorrentConfig and SessionConfig |
| New: `pipeline.rs` | `PeerPipelineState` — slow-start, EWMA rate, queue depth, block timeouts |
| New: `streaming.rs` | `FileStream`, `StreamingCursor`, concurrent read semaphore |
| `lib.rs` / facade | Re-export FileStream, `TorrentHandle::open_file()`, ClientBuilder methods |

---

## 7. Test Plan (~20 tests)

### Piece Picker (11 tests)

| # | Test |
|---|------|
| 1 | Block-level picking: two peers get different blocks of same piece |
| 2 | Streaming window pieces picked before rarest-first |
| 3 | Streaming pieces assigned to fastest peer first |
| 4 | First/last file pieces get time-critical priority |
| 5 | Sequential mode picks ascending piece index |
| 6 | Initial random threshold: first N pieces not rarest-first |
| 7 | Whole-piece threshold: fast peer gets exclusive assignment (time-based) |
| 8 | Speed affinity: slow peer avoids fast peer's partial piece |
| 9 | Snubbed peer: restricted to 1 request, reverse picking |
| 10 | Auto-sequential activates when partials > 1.5 * peers |
| 11 | Seed counter: seed connect/disconnect doesn't touch availability array |

### Dynamic Request Queue (4 tests)

| # | Test |
|---|------|
| 12 | Slow-start: queue depth grows exponentially, exits on plateau |
| 13 | Steady-state: queue_depth = rate * request_queue_time / block_size |
| 14 | Queue depth clamped to [2, max] |
| 15 | Block timeout: re-requests most-recently-sent block from another peer |

### FileStream (5 tests)

| # | Test |
|---|------|
| 16 | Sequential read: reads file bytes in order, correct data |
| 17 | Seek: updates cursor, picker reprioritizes |
| 18 | Blocks on missing piece, wakes on completion |
| 19 | Request cancellation: seek cancels non-priority outstanding requests |
| 20 | Streaming timeout escalation: slow piece gets busy-requested |

---

## 8. References

- [Writing a fast piece picker](https://blog.libtorrent.org/2011/11/writing-a-fast-piece-picker/)
- [Requesting pieces](https://blog.libtorrent.org/2011/11/requesting-pieces/)
- [Block request time-outs](https://blog.libtorrent.org/2011/11/block-request-time-outs/)
- [Slow start](https://blog.libtorrent.org/2015/07/slow-start/)
- [libtorrent streaming.html](https://www.libtorrent.org/streaming.html)
- [libtorrent reference-Settings](https://www.libtorrent.org/reference-Settings.html)
- [rqbit streaming implementation](https://github.com/ikatson/rqbit)
