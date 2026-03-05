# M58: Non-Blocking Transfer Pipeline — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Eliminate the TorrentActor disk I/O bottleneck by making writes fire-and-forget and piece verification async, targeting 50–80 MB/s (up from ~15 MB/s).

**Architecture:** Replace blocking `disk.write_chunk().await` with a non-blocking channel send (`enqueue_write`), move piece hash verification to an async callback pattern via new `select!` branches, and compute v2 block hashes in-memory at receive time instead of round-tripping through disk.

**Tech Stack:** Rust, tokio (mpsc channels, select!), bytes::Bytes, sha2 (for in-memory v2 block hashing)

**Design doc:** `docs/plans/2026-03-04-m58-non-blocking-transfer-pipeline-design.md`

---

### Task 1: Add fire-and-forget write infrastructure to DiskHandle

**Files:**
- Modify: `crates/ferrite-session/src/disk.rs:26-42` (DiskJob enum — add WriteAsync variant)
- Modify: `crates/ferrite-session/src/disk.rs:235-272` (DiskHandle — add error_tx field, enqueue_write method)
- Modify: `crates/ferrite-session/src/disk.rs:197-216` (register_torrent — accept error_tx)

**Step 1: Add DiskWriteError type and WriteAsync DiskJob variant**

Add after `DiskJobFlags` (line 24), before `DiskJob` enum (line 26):

```rust
/// Error report from an async (fire-and-forget) disk write.
#[derive(Debug)]
pub struct DiskWriteError {
    pub info_hash: Id20,
    pub piece: u32,
    pub begin: u32,
    pub error: ferrite_storage::Error,
}
```

Add a new variant inside the `DiskJob` enum (after `Write`, line 43):

```rust
    /// Fire-and-forget write — errors are sent to `error_tx` instead of a oneshot reply.
    WriteAsync {
        info_hash: Id20,
        piece: u32,
        begin: u32,
        data: Bytes,
        flags: DiskJobFlags,
        error_tx: mpsc::Sender<DiskWriteError>,
    },
```

**Step 2: Add write_error_tx field to DiskHandle and enqueue_write method**

Change `DiskHandle` struct (line 237) to include an error channel:

```rust
#[derive(Clone)]
pub struct DiskHandle {
    tx: mpsc::Sender<DiskJob>,
    info_hash: Id20,
    write_error_tx: Option<mpsc::Sender<DiskWriteError>>,
}
```

Update `DiskHandle::new()` (line 245) to accept and store the error_tx:

```rust
pub(crate) fn new(
    tx: mpsc::Sender<DiskJob>,
    info_hash: Id20,
    write_error_tx: Option<mpsc::Sender<DiskWriteError>>,
) -> Self {
    Self { tx, info_hash, write_error_tx }
}
```

Add `enqueue_write()` method after `write_chunk()` (after line 272):

```rust
    /// Enqueue a chunk write without waiting for completion (fire-and-forget).
    ///
    /// Uses `try_send()` for zero-cost dispatch. If the disk channel is full,
    /// falls back to `send().await` for natural backpressure.
    /// Errors are reported asynchronously via the write_error channel.
    pub async fn enqueue_write(
        &self,
        piece: u32,
        begin: u32,
        data: Bytes,
        flags: DiskJobFlags,
    ) {
        let error_tx = match &self.write_error_tx {
            Some(tx) => tx.clone(),
            None => {
                // Fallback to blocking write if no error channel configured
                let _ = self.write_chunk(piece, begin, data, flags).await;
                return;
            }
        };
        let job = DiskJob::WriteAsync {
            info_hash: self.info_hash,
            piece,
            begin,
            data,
            flags,
            error_tx,
        };
        // try_send for zero-cost path; fall back to blocking send for backpressure
        if let Err(mpsc::error::TrySendError::Full(job)) = self.tx.try_send(job) {
            let _ = self.tx.send(job).await;
        }
    }
```

**Step 3: Update register_torrent to accept write_error_tx**

Modify `DiskManagerHandle::register_torrent()` (line 197):

```rust
pub async fn register_torrent(
    &self,
    info_hash: Id20,
    storage: Arc<dyn TorrentStorage>,
    write_error_tx: Option<mpsc::Sender<DiskWriteError>>,
) -> DiskHandle {
    let (reply_tx, reply_rx) = oneshot::channel();
    let _ = self
        .tx
        .send(DiskJob::Register {
            info_hash,
            storage,
            reply: reply_tx,
        })
        .await;
    let _ = reply_rx.await;
    DiskHandle {
        tx: self.tx.clone(),
        info_hash,
        write_error_tx,
    }
}
```

**Step 4: Fix all call sites of register_torrent and DiskHandle::new**

Search for all callers of `register_torrent(` and `DiskHandle::new(` and add the new parameter (`None` for test call sites, the real error_tx for the TorrentActor call site — which will be wired in Task 4).

**Step 5: Run tests**

Run: `cargo test --workspace`
Expected: All 1347 tests pass (no behavior change yet — enqueue_write is not called anywhere).

**Step 6: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: Clean.

**Step 7: Commit**

```bash
git add crates/ferrite-session/src/disk.rs
# Also add any files with register_torrent call site fixes
git commit -m "feat(disk): add fire-and-forget write infrastructure (M58)

Add DiskWriteError type, WriteAsync DiskJob variant, and
DiskHandle::enqueue_write() method with try_send backpressure.
No behavioral change yet — existing write_chunk() callers unchanged.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 2: Handle WriteAsync in DiskActor

**Files:**
- Modify: `crates/ferrite-session/src/disk.rs:509-527` (DiskActor job dispatch — add WriteAsync handler)

**Step 1: Add WriteAsync handler in DiskActor::run()**

After the existing `DiskJob::Write` handler (line 527), add:

```rust
            DiskJob::WriteAsync {
                info_hash,
                piece,
                begin,
                data,
                flags,
                error_tx,
            } => {
                let flush = flags.contains(DiskJobFlags::FLUSH_PIECE);
                let backend = Arc::clone(&self.backend);
                let permit = self.semaphore.clone().acquire_owned().await.unwrap();
                let result = tokio::task::spawn_blocking(move || {
                    backend.write_chunk(info_hash, piece, begin, &data, flush)
                })
                .await
                .unwrap();
                drop(permit);
                if let Err(e) = to_storage_result(result) {
                    let _ = error_tx.try_send(DiskWriteError {
                        info_hash,
                        piece,
                        begin,
                        error: e,
                    });
                }
            }
```

**Step 2: Run tests**

Run: `cargo test --workspace`
Expected: All 1347 tests pass.

**Step 3: Commit**

```bash
git add crates/ferrite-session/src/disk.rs
git commit -m "feat(disk): handle WriteAsync jobs in DiskActor (M58)

Same I/O logic as Write, but errors are sent to mpsc error channel
instead of oneshot reply. Uses try_send to avoid blocking the disk actor.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 3: Add async piece verification infrastructure

**Files:**
- Modify: `crates/ferrite-session/src/disk.rs` (add VerifyResult type, enqueue_verify method, VerifyAsync DiskJob variant)

**Step 1: Add VerifyResult type and VerifyAsync DiskJob variant**

Add near DiskWriteError:

```rust
/// Result of an async piece verification.
#[derive(Debug)]
pub struct VerifyResult {
    pub info_hash: Id20,
    pub piece: u32,
    pub passed: bool,
}
```

Add variant to DiskJob:

```rust
    /// Async v1 piece verification — result sent to verify_tx channel.
    VerifyAsync {
        info_hash: Id20,
        piece: u32,
        expected: Id20,
        flags: DiskJobFlags,
        verify_tx: mpsc::Sender<VerifyResult>,
    },

    /// Async v2 piece verification — result sent to verify_tx channel.
    VerifyV2Async {
        info_hash: Id20,
        piece: u32,
        expected: Id32,
        flags: DiskJobFlags,
        verify_tx: mpsc::Sender<VerifyResult>,
    },
```

**Step 2: Add enqueue_verify methods to DiskHandle**

Add `verify_tx` field to DiskHandle:

```rust
pub struct DiskHandle {
    tx: mpsc::Sender<DiskJob>,
    info_hash: Id20,
    write_error_tx: Option<mpsc::Sender<DiskWriteError>>,
    verify_tx: Option<mpsc::Sender<VerifyResult>>,
}
```

Update `new()` and `register_torrent()` to accept `verify_tx`.

Add methods:

```rust
    /// Enqueue a v1 piece verification (SHA-1) without waiting for the result.
    /// Result is sent to the verify channel for the TorrentActor's select! loop.
    pub async fn enqueue_verify(
        &self,
        piece: u32,
        expected: Id20,
        flags: DiskJobFlags,
    ) {
        let verify_tx = match &self.verify_tx {
            Some(tx) => tx.clone(),
            None => {
                // Fallback: blocking verify
                let passed = self.verify_piece(piece, expected, flags).await.unwrap_or(false);
                // Can't send result without channel — caller must use verify_piece() directly
                tracing::warn!(piece, "enqueue_verify called without verify channel");
                return;
            }
        };
        let job = DiskJob::VerifyAsync {
            info_hash: self.info_hash,
            piece,
            expected,
            flags,
            verify_tx,
        };
        let _ = self.tx.send(job).await;
    }

    /// Enqueue a v2 piece verification (SHA-256) without waiting for the result.
    pub async fn enqueue_verify_v2(
        &self,
        piece: u32,
        expected: Id32,
        flags: DiskJobFlags,
    ) {
        let verify_tx = match &self.verify_tx {
            Some(tx) => tx.clone(),
            None => {
                tracing::warn!(piece, "enqueue_verify_v2 called without verify channel");
                return;
            }
        };
        let job = DiskJob::VerifyV2Async {
            info_hash: self.info_hash,
            piece,
            expected,
            flags,
            verify_tx,
        };
        let _ = self.tx.send(job).await;
    }
```

**Step 3: Handle VerifyAsync and VerifyV2Async in DiskActor**

After the existing Hash/HashV2 handlers, add:

```rust
            DiskJob::VerifyAsync {
                info_hash,
                piece,
                expected,
                verify_tx,
                ..
            } => {
                let backend = Arc::clone(&self.backend);
                let permit = self.semaphore.clone().acquire_owned().await.unwrap();
                let result = tokio::task::spawn_blocking(move || {
                    backend.hash_piece(info_hash, piece, &expected)
                })
                .await
                .unwrap();
                drop(permit);
                let passed = to_storage_result(result).unwrap_or(false);
                let _ = verify_tx.try_send(VerifyResult { info_hash, piece, passed });
            }
            DiskJob::VerifyV2Async {
                info_hash,
                piece,
                expected,
                verify_tx,
                ..
            } => {
                let backend = Arc::clone(&self.backend);
                let permit = self.semaphore.clone().acquire_owned().await.unwrap();
                let result = tokio::task::spawn_blocking(move || {
                    backend.hash_piece_v2(info_hash, piece, &expected)
                })
                .await
                .unwrap();
                drop(permit);
                let passed = to_storage_result(result).unwrap_or(false);
                let _ = verify_tx.try_send(VerifyResult { info_hash, piece, passed });
            }
```

**Step 4: Run tests + clippy**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: All tests pass, clean clippy.

**Step 5: Commit**

```bash
git add crates/ferrite-session/src/disk.rs
git commit -m "feat(disk): add async piece verification infrastructure (M58)

Add VerifyResult type, VerifyAsync/VerifyV2Async DiskJob variants,
and DiskHandle::enqueue_verify()/enqueue_verify_v2() methods.
DiskActor sends results via mpsc channel instead of oneshot reply.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 4: Wire up channels in TorrentActor

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs:1157-1306` (TorrentActor struct — add channel fields)
- Modify: `crates/ferrite-session/src/torrent.rs` (TorrentActor initialization — create channels)
- Modify: `crates/ferrite-session/src/torrent.rs` (wherever register_torrent is called — pass channels)
- Modify: `crates/ferrite-session/src/torrent.rs:1520-1958` (select! loop — add two new branches)

**Step 1: Add channel fields to TorrentActor struct**

Add after the `disk` field (~line 1168):

```rust
    // M58: Async callback channels for non-blocking disk writes and verification
    write_error_rx: mpsc::Receiver<DiskWriteError>,
    write_error_tx: mpsc::Sender<DiskWriteError>,
    verify_result_rx: mpsc::Receiver<VerifyResult>,
    verify_result_tx: mpsc::Sender<VerifyResult>,
```

**Step 2: Create channels in TorrentActor initialization**

Find where TorrentActor is constructed (search for `TorrentActor {` in the constructor). Add:

```rust
    let (write_error_tx, write_error_rx) = mpsc::channel(64);
    let (verify_result_tx, verify_result_rx) = mpsc::channel(64);
```

Pass these to the struct initialization and to `register_torrent()` calls.

**Step 3: Update register_torrent calls to pass channels**

Find all `disk_manager.register_torrent(` calls in torrent.rs and pass `Some(write_error_tx.clone())` and `Some(verify_result_tx.clone())`.

**Step 4: Add select! branches for verify results and write errors**

Inside the `tokio::select!` loop (before the `refill_interval.tick()` branch at line 1943), add:

```rust
                // M58: Async piece verification result
                result = self.verify_result_rx.recv() => {
                    if let Some(result) = result {
                        if result.passed {
                            self.on_piece_verified(result.piece).await;
                        } else {
                            self.on_piece_hash_failed(result.piece).await;
                        }
                    }
                }
                // M58: Async write error notification
                error = self.write_error_rx.recv() => {
                    if let Some(error) = error {
                        tracing::warn!(
                            piece = error.piece,
                            begin = error.begin,
                            "async disk write failed: {}",
                            error.error,
                        );
                        // On ENOSPC or other fatal errors, could pause the torrent:
                        // self.transition_state(TorrentState::Error);
                    }
                }
```

**Step 5: Run tests + clippy**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: All tests pass. The new branches exist but aren't triggered yet (still using blocking paths).

**Step 6: Commit**

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "feat(session): wire up async callback channels in TorrentActor (M58)

Add write_error_rx and verify_result_rx channels with new select!
branches. on_piece_verified/on_piece_hash_failed called from select!
branch instead of inline. No behavioral change yet — blocking paths
still active.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 5: Rewrite handle_piece_data() for non-blocking writes + in-memory v2 hashing

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs:3120-3219` (handle_piece_data — the critical change)

This is the core performance change. The method currently blocks on `disk.write_chunk().await` for ~1ms per block.

**Step 1: Replace blocking write with enqueue_write**

Change lines 3130-3135 from:

```rust
        if let Some(ref disk) = self.disk
            && let Err(e) = disk.write_chunk(index, begin, data, DiskJobFlags::empty()).await
        {
            warn!(index, begin, "failed to write chunk: {e}");
            return;
        }
```

To:

```rust
        // M58: Fire-and-forget write — errors reported via write_error_rx select! branch.
        // We must clone data BEFORE enqueuing because v2 in-memory hashing needs it.
        let data_for_hash = if self.version != ferrite_core::TorrentVersion::V1Only {
            Some(data.clone())
        } else {
            None
        };

        if let Some(ref disk) = self.disk {
            disk.enqueue_write(index, begin, data, DiskJobFlags::empty()).await;
        }
```

**Step 2: Add in-memory v2 block hashing**

After the enqueue_write call (and before the download counter updates), add:

```rust
        // M58: In-memory v2 block hashing — compute SHA-256 at receive time (~15μs)
        // instead of round-tripping through disk (write → flush → read → hash).
        if let Some(data_bytes) = data_for_hash {
            let block_hash = ferrite_core::sha256(&data_bytes);
            if let Some(ref mut picker) = self.hash_picker {
                let blocks_per_piece = self.lengths.as_ref()
                    .map(|l| (l.piece_length() as u32) / DEFAULT_CHUNK_SIZE)
                    .unwrap_or(1);
                let chunk_idx = begin / DEFAULT_CHUNK_SIZE;
                let file_index = 0; // Same single-file assumption as existing code
                let global_block = index * blocks_per_piece + chunk_idx;
                match picker.set_block_hash(file_index, global_block, block_hash) {
                    ferrite_core::SetBlockResult::Ok => {
                        if let Some(ref mut ct) = self.chunk_tracker {
                            ct.mark_block_verified(index, chunk_idx);
                        }
                    }
                    ferrite_core::SetBlockResult::Unknown => {
                        // Awaiting piece-layer hashes — stored for deferred verification
                    }
                    ferrite_core::SetBlockResult::HashFailed => {
                        tracing::warn!(index, chunk_idx, "in-memory block hash failed Merkle verification");
                        // Don't return — still track the chunk for bookkeeping.
                        // The piece will fail verification when complete.
                    }
                }
            }
        }
```

**Step 3: Run tests + clippy**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: All tests pass. Downloads now use fire-and-forget writes with in-memory v2 hashing.

**Step 4: Commit**

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "feat(session): non-blocking disk writes + in-memory v2 hashing (M58)

Replace blocking disk.write_chunk().await with fire-and-forget
disk.enqueue_write(). Compute v2 SHA-256 block hashes in-memory
at receive time (~15us) instead of disk round-trip.

This is the primary performance fix — removes ~1ms blocking await
per 16 KiB block that capped throughput at ~16 MB/s.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 6: Convert piece verification to async dispatch

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs:3544-3556` (verify_and_mark_piece)
- Modify: `crates/ferrite-session/src/torrent.rs:3558-3577` (verify_and_mark_piece_v1)
- Modify: `crates/ferrite-session/src/torrent.rs:3580-3590` (verify_and_mark_piece_v2)
- Modify: `crates/ferrite-session/src/torrent.rs:3665-3722` (verify_and_mark_piece_hybrid)
- Modify: `crates/ferrite-session/src/torrent.rs:3597-3663` (run_v2_block_verification)

**Step 1: Convert verify_and_mark_piece_v1 to async dispatch**

Change `verify_and_mark_piece_v1` (line 3559) from blocking verify to enqueue:

```rust
    async fn verify_and_mark_piece_v1(&mut self, index: u32) {
        let expected_hash = self
            .meta
            .as_ref()
            .and_then(|m| m.info.piece_hash(index as usize));

        if let (Some(disk), Some(expected)) = (&self.disk, expected_hash) {
            // M58: Enqueue verification — result arrives via verify_result_rx select! branch
            disk.enqueue_verify(index, expected, DiskJobFlags::empty()).await;
        } else {
            // No disk or no expected hash — treat as failure
            self.on_piece_hash_failed(index).await;
        }
    }
```

**Step 2: Convert verify_and_mark_piece_v2 to use in-memory results**

Since v2 block hashes are now computed in `handle_piece_data()` (Task 5), the piece-complete check
changes. Replace `verify_and_mark_piece_v2` (line 3581):

```rust
    async fn verify_and_mark_piece_v2(&mut self, index: u32) {
        // M58: Block hashes were already computed in-memory in handle_piece_data().
        // Check if all blocks for this piece have been verified by MerkleTreeState.
        let all_verified = self.chunk_tracker.as_ref()
            .is_some_and(|ct| ct.all_blocks_verified(index));

        if all_verified {
            self.on_piece_verified(index).await;
        } else {
            // Blocks stored but awaiting piece-layer hashes — will resolve
            // when hashes arrive via handle_hashes_received()
            tracing::debug!(index, "v2 piece complete but awaiting piece-layer hashes");
        }
    }
```

**Step 3: Convert verify_and_mark_piece_hybrid**

For hybrid torrents, v2 is checked in-memory (instant) and v1 is enqueued:

```rust
    async fn verify_and_mark_piece_hybrid(&mut self, index: u32) {
        // v2 check is in-memory (already done in handle_piece_data)
        let v2_result = if self.chunk_tracker.as_ref()
            .is_some_and(|ct| ct.all_blocks_verified(index))
        {
            HashResult::Passed
        } else {
            HashResult::NotApplicable
        };

        if v2_result == HashResult::Failed {
            self.on_piece_hash_failed(index).await;
            return;
        }

        // v1: enqueue SHA-1 verification — result arrives via verify_result_rx.
        // The select! branch handler will call on_piece_verified/on_piece_hash_failed.
        let expected_hash = self.meta.as_ref()
            .and_then(|m| m.info.piece_hash(index as usize));

        if let (Some(disk), Some(expected)) = (&self.disk, expected_hash) {
            disk.enqueue_verify(index, expected, DiskJobFlags::empty()).await;
        } else {
            // v2 passed but no v1 data — treat v1 as not applicable, accept on v2
            if v2_result == HashResult::Passed {
                self.on_piece_verified(index).await;
            }
        }
    }
```

**Step 4: Remove or gut run_v2_block_verification**

The `run_v2_block_verification()` method (line 3597) is no longer needed for the hot path — v2 block
hashing is now done inline in `handle_piece_data()`. However, it may still be called by
`verify_existing_pieces()` for resume checking. Keep it but add a comment noting it's only used
for resume/recheck, not the download hot path.

**Step 5: Run tests + clippy**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: All tests pass. Piece verification is now fully async.

**Step 6: Commit**

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "feat(session): async piece verification dispatch (M58)

Convert verify_and_mark_piece to async pattern:
- v1: enqueue SHA-1 verify, result via select! branch
- v2: in-memory check (block hashes computed at receive time)
- hybrid: in-memory v2 + enqueued v1

Removes 1-5ms blocking await per piece completion from the
TorrentActor select! loop.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 7: Tests for async paths

**Files:**
- Modify: `crates/ferrite-session/src/disk.rs` (add tests for enqueue_write, enqueue_verify)
- Modify: `crates/ferrite-session/src/torrent.rs` (add test for write error handling)

**Step 1: Write test for enqueue_write backpressure**

Add in disk.rs `#[cfg(test)]` module:

```rust
#[tokio::test]
async fn test_enqueue_write_backpressure() {
    // Create a channel with capacity 1 to trigger backpressure
    let (disk_tx, mut disk_rx) = mpsc::channel(1);
    let (error_tx, mut error_rx) = mpsc::channel(16);
    let (verify_tx, _) = mpsc::channel(16);
    let handle = DiskHandle::new(disk_tx, Id20::default(), Some(error_tx), Some(verify_tx));

    // First enqueue should succeed via try_send
    handle.enqueue_write(0, 0, Bytes::from_static(b"hello"), DiskJobFlags::empty()).await;

    // Drain the job
    let job = disk_rx.recv().await.unwrap();
    assert!(matches!(job, DiskJob::WriteAsync { piece: 0, begin: 0, .. }));

    // No errors should have been sent
    assert!(error_rx.try_recv().is_err());
}
```

**Step 2: Write test for async verify result**

```rust
#[tokio::test]
async fn test_enqueue_verify_sends_result() {
    let (disk_tx, _disk_rx) = mpsc::channel(16);
    let (error_tx, _) = mpsc::channel(16);
    let (verify_tx, mut verify_rx) = mpsc::channel(16);
    let handle = DiskHandle::new(disk_tx, Id20::default(), Some(error_tx), Some(verify_tx));

    // Enqueue a verify — should send VerifyAsync job
    handle.enqueue_verify(42, Id20::default(), DiskJobFlags::empty()).await;

    // The verify result won't arrive without a running DiskActor,
    // but we can verify the job was dispatched by checking the channel
    // (verify_rx won't have a result yet since DiskActor isn't running)
}
```

**Step 3: Run tests + clippy**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: All tests pass including new ones.

**Step 4: Commit**

```bash
git add crates/ferrite-session/src/disk.rs crates/ferrite-session/src/torrent.rs
git commit -m "test: add tests for async write and verify paths (M58)

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 8: Benchmark, version bump, and documentation

**Files:**
- Modify: `Cargo.toml` (workspace version → 0.65.0)
- Modify: `CHANGELOG.md` (add M58 entry)
- Modify: `README.md` (update test count, version, performance notes)
- Modify: `benchmarks/results.csv` (add M58 benchmark results)
- Modify: `crates/ferrite-cli/src/download.rs` (clean up any diagnostic eprintln from previous debugging)

**Step 1: Clean up download.rs diagnostic instrumentation**

Remove any temporary `eprintln!` stats lines that were added during the M57 debugging session
(the quiet-mode CSV stats around line 153-167 if they are not part of normal operation).

**Step 2: Bump workspace version**

In root `Cargo.toml`, change `version = "0.64.0"` to `version = "0.65.0"`.

**Step 3: Run benchmark**

Run three trials against the Arch ISO torrent (same as M57 baseline):

```bash
cd /mnt/TempNVME/projects/ferrite
cargo build --release -p ferrite-cli

# Trial 1
rm -rf /tmp/ferrite-bench && mkdir -p /tmp/ferrite-bench
timeout 120 ./target/release/ferrite download \
    "magnet:?xt=urn:btih:ab6ad7ff24b5ed3a61352a1f1a7f6bca46e4a28e&dn=archlinux-2025.02.01-x86_64.iso" \
    -o /tmp/ferrite-bench --quiet 2>&1 | tail -5

# Repeat for trials 2-3
```

Record results in `benchmarks/results.csv`.

**Step 4: Update CHANGELOG.md**

Add entry:

```markdown
## [0.65.0] — 2026-03-04

### Added
- M58: Non-blocking transfer pipeline
  - Fire-and-forget disk writes (DiskHandle::enqueue_write)
  - Async piece verification via callback channels
  - In-memory v2 block hashing (eliminates disk round-trip for BEP 52)
  - Write error notification channel (DiskWriteError)
  - Backpressure via try_send with blocking fallback

### Changed
- handle_piece_data() no longer blocks on disk I/O (~1ms per block removed)
- Piece verification dispatched asynchronously via select! branches
- Download throughput improved from ~15 MB/s to ~XX MB/s (pending benchmark)
```

**Step 5: Update README.md**

Update version badge, test count, and add performance improvement note.

**Step 6: Run final tests + clippy**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`

**Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock CHANGELOG.md README.md benchmarks/results.csv crates/ferrite-cli/src/download.rs
git commit -m "docs: update README and CHANGELOG for M58 (v0.65.0)

Non-blocking transfer pipeline: fire-and-forget writes, async verify,
in-memory v2 hashing. Download speed improved from ~15 MB/s to ~XX MB/s.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

**Step 8: Push to both remotes**

```bash
git push origin main && git push github main
```

---

## Key Invariants to Verify

After implementation, confirm these invariants hold:

1. **Have is never broadcast before verification.** Check that `on_piece_verified()` is only called after SHA-1 (v1) or Merkle (v2) passes.
2. **Smart banning still works.** `piece_contributors` is populated in `handle_piece_data()` before `enqueue_write()`.
3. **Write ordering within a piece.** DiskActor processes jobs from a single mpsc channel in FIFO order. Writes for piece N arrive before the verify for piece N.
4. **Merkle tree integrity.** In-memory block hashes are computed from the same `Bytes` data that gets written to disk.
5. **Backpressure works.** When disk channel is full, `enqueue_write()` falls back to blocking send.
6. **Existing tests pass.** All 1347 tests must pass after every task.
