# M57: Non-Blocking Transfer Pipeline — Design

**Goal:** Unblock the TorrentActor select! loop from disk I/O, achieving 50–80 MB/s downloads (currently ~15 MB/s). Combines the useful parts of the old M57/M58 into a single milestone with BT smoke testing after every commit.

**Base commit:** `c677d23` (TCP 5s timeout — BT verified working)

**Version target:** v0.64.0

## Background & Lessons Learned

The original M57 (micro-optimizations) and M58 (non-blocking pipeline) were separate milestones. Both M57 commits touching `torrent.rs` broke BT functionality in ways that were hard to diagnose:

- **missing_chunks cache** (`db96994`): Captured `self.chunk_tracker.as_ref()` in a closure inside `request_pieces_from_peer()`, killed the select! loop — zero DHT/peer activity.
- **zero-copy Bytes** (`3b76a58`): Changed `handle_piece_data` signature from `&[u8]` to `Bytes`, also broke BT in the same way (81 log lines, zero peer activity).

Both were small changes that looked correct in isolation but caused the TorrentActor to stop functioning. The root cause was never fully identified — possibly related to how the borrow checker interacts with the select! loop's implicit Future state machine.

**Key decision:** Instead of applying fragile micro-optimizations incrementally, we do a single cohesive architectural rewrite where the zero-copy path falls out naturally from the new non-blocking design.

## Architecture

### Current (blocking, ~15 MB/s)

```
PieceData → handle_piece_data(&[u8])
  → Bytes::copy_from_slice(data)        // copy #2 (unnecessary)
  → disk.write_chunk(...).await          // BLOCKS ~1ms per 16 KiB block
  → update counters
  → if piece complete:
      disk.flush_piece().await           // BLOCKS
      disk.verify_piece().await          // BLOCKS 1-5ms
      on_piece_verified()
```

The select! loop is stalled for the entire write+verify duration. With ~1000 blocks/sec max throughput (16 KiB each), this caps at ~16 MB/s.

### New (non-blocking, target 50–80 MB/s)

```
PieceData → handle_piece_data(data: Bytes)    // zero-copy move from PeerEvent
  → disk.enqueue_write(data)                   // <1μs fire-and-forget
  → update counters
  → if piece complete:
      disk.enqueue_verify()                    // <1μs fire-and-forget

# Async results arrive via new select! branches:
write_error_rx  → log warning, optionally ban peer
verify_result_rx → on_piece_verified() / on_piece_hash_failed()
```

The select! loop returns immediately after enqueue, free to process the next peer event.

## Detailed Design

### DiskHandle additions (disk.rs)

New types:
```rust
pub struct DiskWriteError {
    pub piece: u32,
    pub begin: u32,
    pub error: ferrite_storage::Error,
}

pub struct VerifyResult {
    pub piece: u32,
    pub passed: bool,
}
```

New DiskJob variants:
```rust
WriteAsync {
    info_hash: Id20,
    piece: u32,
    begin: u32,
    data: Bytes,
    flags: DiskJobFlags,
    error_tx: mpsc::Sender<DiskWriteError>,
}

VerifyAsync {
    info_hash: Id20,
    piece: u32,
    expected: Id20,
    result_tx: mpsc::Sender<VerifyResult>,
}

VerifyV2Async {
    info_hash: Id20,
    piece: u32,
    expected: Id32,
    result_tx: mpsc::Sender<VerifyResult>,
}
```

New DiskHandle methods:
```rust
pub fn enqueue_write(&self, piece, begin, data, flags, error_tx) -> Result<(), ...>
pub fn enqueue_verify(&self, piece, expected, result_tx) -> Result<(), ...>
pub fn enqueue_verify_v2(&self, piece, expected, result_tx) -> Result<(), ...>
```

These use `try_send()` (non-blocking) to the existing disk mpsc channel.

### TorrentActor changes (torrent.rs)

New fields:
```rust
write_error_rx: mpsc::Receiver<DiskWriteError>,
write_error_tx: mpsc::Sender<DiskWriteError>,
verify_result_rx: mpsc::Receiver<VerifyResult>,
verify_result_tx: mpsc::Sender<VerifyResult>,
```

New select! branches:
```rust
Some(err) = self.write_error_rx.recv() => {
    warn!(piece = err.piece, begin = err.begin, "async write failed: {}", err.error);
}
Some(result) = self.verify_result_rx.recv() => {
    if result.passed {
        self.on_piece_verified(result.piece).await;
    } else {
        self.on_piece_hash_failed(result.piece).await;
    }
}
```

### handle_piece_data rewrite

Signature changes from `data: &[u8]` to `data: Bytes`. Key changes:
1. Capture `data.len()` before moving `data` into enqueue
2. `enqueue_write()` instead of `write_chunk().await`
3. Piece completion triggers `enqueue_verify()` instead of inline verify

### handle_add_peers HashSet dedup

Replace `O(n)` linear scan:
```rust
!self.available_peers.iter().any(|(a, _)| *a == addr)
```
With `O(1)` HashSet lookup via new `available_peers_set: HashSet<SocketAddr>` field.

## Task Ordering (9 tasks)

| # | Description | Risk | Touches torrent.rs? |
|---|-------------|------|---------------------|
| 1 | Add `DiskWriteError`, `VerifyResult`, `WriteAsync`/`VerifyAsync`/`VerifyV2Async` variants + `enqueue_*` methods | None | No |
| 2 | Handle `WriteAsync` in DiskActor `process_job()` | None | No |
| 3 | Handle `VerifyAsync`/`VerifyV2Async` in DiskActor | None | No |
| 4 | Wire channels into TorrentActor + add select! branches | Low | Yes (additive) |
| 5 | Rewrite `handle_piece_data()` for non-blocking writes + zero-copy | **High** | Yes (rewrite) |
| 6 | Convert piece verification to async dispatch | Medium | Yes (modify) |
| 7 | HashSet peer dedup in `handle_add_peers()` | Low | Yes (small) |
| 8 | Unit tests for async disk paths | None | No |
| 9 | Version bump to 0.64.0, benchmark, CHANGELOG, README | None | No |

## Safety Invariants

- **Have-broadcast after verify only:** `on_piece_verified()` called from `verify_result_rx` select! branch
- **Write ordering:** DiskActor mpsc is FIFO — blocks for same piece arrive in order
- **Backpressure:** Bounded channel; if disk falls behind, `try_send` fails and we fall back to blocking write or drop
- **Piece contributors recorded before enqueue:** Smart banning data captured before `enqueue_write()`
- **Existing blocking paths preserved:** `write_chunk()` and `verify_piece()` remain for callers that need synchronous results (force_recheck, move_storage, etc.)

## Testing Strategy

Every commit gets:
1. `cargo test --workspace` — all tests pass
2. `cargo clippy --workspace -- -D warnings` — no warnings
3. **BT smoke test** — `/tmp/bt-smoke4.sh` (March 2026 Arch ISO), must show 600+ log lines with DHT peers + uTP connections

If BT smoke test shows 81 lines / zero peer activity → **stop, revert, investigate**.
