# M58: Non-Blocking Transfer Pipeline

## Problem

Ferrite downloads at ~15 MB/s while rqbit achieves ~85 MB/s on the same torrent (Arch ISO, 1.42 GiB). Three milestones (M55–M57) optimized peer discovery, queue depth, allocations, and zero-copy writes with zero measurable improvement. Root cause analysis revealed the bottleneck is architectural: the single-threaded TorrentActor blocks on disk I/O for every 16 KiB block received.

## Root Cause

`handle_piece_data()` executes three blocking awaits in series:

1. `disk.write_chunk().await` — sends to DiskActor, waits ~1ms for reply
2. `verify_and_mark_piece().await` — on piece completion, waits 1–5ms for SHA-1/SHA-256
3. `request_pieces_from_peer().await` — computation + channel send

While any await is in flight, the TorrentActor's `select!` loop is blocked. All other peer events, timer ticks (choker, refill, connect), DHT responses, and tracker replies queue up. With ~1ms per disk write and 16 KiB blocks, the theoretical maximum throughput is ~16 MB/s — exactly what benchmarks measure.

rqbit avoids this by running per-peer async tasks that fire off disk writes without waiting. Each peer independently requests blocks and writes to disk in parallel.

## Design

### Architecture Change

```
BEFORE (serial):
  PieceData → await write → update state → await verify → request more
  (one event at a time, ~1000 blocks/sec max)

AFTER (pipelined):
  PieceData → enqueue write → update state → enqueue verify → request more
                   ↓                              ↓
              DiskActor writes              DiskActor verifies
                   ↓                              ↓
              error_rx ──────────────── verify_rx ──→ select! loop
  (parallel across all peers, limited only by network bandwidth)
```

### Changes

#### 1. Non-blocking disk writes in `handle_piece_data()`

Replace `disk.write_chunk().await` with `disk.enqueue_write()` — a channel send that returns immediately. The DiskActor's WriteBuffer already accumulates chunks before flushing to disk, so no write ordering changes are needed.

For backpressure: `enqueue_write()` uses `try_send()`. If the channel is full, it falls back to `send().await` — naturally throttling the fastest peers when disk can't keep up.

#### 2. In-memory v2 block hashing

For BEP 52 v2 torrents, compute `sha256(&data)` in memory at receive time (~15μs) and insert into `MerkleTreeState` immediately. This eliminates the `DiskJob::BlockHash` round-trip (write block to disk, read it back, hash it) and is both faster and compatible with fire-and-forget writes.

#### 3. Async piece verification

Replace `verify_and_mark_piece().await` with an enqueued verify request. Add a `verify_result_rx` channel as a new branch in the TorrentActor's `select!` loop.

The existing `on_piece_verified()` / `on_piece_hash_failed()` handlers are called from the new select! branch instead of inline. All smart banning, Have broadcast, and completion logic remains identical.

#### 4. Write error callback channel

DiskActor sends write errors back via `write_error_tx: mpsc::Sender<DiskWriteError>`. TorrentActor handles them in a new select! branch — logs the error, optionally pauses the torrent on ENOSPC.

### What Does NOT Change

- 16 KiB block size (BEP 3)
- SHA-1 piece verification before Have broadcast
- SHA-256 Merkle tree verification (BEP 52) — enhanced, not changed
- Smart banning (contributor tracking is in-memory, before write)
- ChunkTracker state management
- DiskActor internals (WriteBuffer, semaphore, spawn_blocking)
- All wire protocol messages and peer task logic
- All 27 BEP implementations
- Request pipelining logic in `request_pieces_from_peer()`

### Data Flow

```
Peer wire → PeerEvent::PieceData → TorrentActor::handle_piece_data()
  1. Record contributor (piece_contributors HashMap)     — in-memory, instant
  2. Mark chunk received (ChunkTracker)                  — in-memory, instant
  3. [v2 only] sha256(&data) → MerkleTreeState           — ~15μs, no disk
  4. disk.enqueue_write(index, begin, data)              — channel send, <1μs
  5. Update download counters                            — in-memory, instant
  6. If piece complete: disk.enqueue_verify(index)       — channel send, <1μs
  7. request_pieces_from_peer(peer_addr)                 — computation + send

DiskActor (separate task, parallel):
  - Receives write job → WriteBuffer → flush → pwrite
  - Receives verify job → flush_piece → read_piece → SHA-1 → send result
  - On write error → send to error channel

TorrentActor select! loop (new branches):
  - verify_result_rx.recv() → on_piece_verified() / on_piece_hash_failed()
  - write_error_rx.recv() → log warning, optionally pause torrent
```

### Error Handling

- **Write errors**: DiskActor sends `(Id20, u32, u32, StorageError)` (info_hash, piece, offset, error) via `write_error_tx`. TorrentActor logs and can pause the torrent.
- **Verify failures**: Same as current — `on_piece_hash_failed()` triggers smart banning, re-requests the piece from uninvolved peers.
- **Channel full (backpressure)**: `enqueue_write()` uses `try_send()`. If full, falls back to blocking `send().await` — throttles the fastest peers when disk can't keep up.
- **ENOSPC**: Surfaces via write error channel. TorrentActor pauses.

### Correctness Invariants Preserved

1. **Have is never broadcast before verification passes.** ChunkTracker's "received" state is internal bookkeeping. The Have message and bitfield update only happen in `on_piece_verified()`, which is called after the DiskActor confirms the SHA-1/SHA-256 hash.

2. **Smart banning still works.** `piece_contributors` is recorded in `handle_piece_data()` before the write is enqueued — the same moment as before.

3. **Write ordering within a piece.** The DiskActor processes jobs from a single channel in order. Writes for piece N arrive before the verify for piece N, so `flush_piece()` will see all data.

4. **Merkle tree integrity (BEP 52).** Block hashes are computed from the in-memory `Bytes` data at receive time — identical bits to what gets written to disk. The MerkleTreeState sees the same hashes regardless of write timing.

### Scope

- **Files modified**: `torrent.rs`, `disk.rs` (both in `ferrite-session`)
- **Files unchanged**: All wire, codec, peer, tracker, DHT, storage code
- **Tests**: Existing tests pass. Add tests for async verify path and write error handling.
- **Version**: 0.64.0

### Expected Impact

Based on the analysis:
- Removing the write await unblocks ~1ms per block → estimated 2–3x speedup
- Async verification removes 1–5ms per piece completion → estimated 1.5x speedup
- Combined: ferrite should reach 50–80 MB/s, approaching rqbit's 85 MB/s
