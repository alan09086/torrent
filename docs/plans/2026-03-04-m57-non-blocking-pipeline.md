# M57: Non-Blocking Transfer Pipeline — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Unblock the TorrentActor select! loop from disk I/O by making writes fire-and-forget and verification async, targeting 50–80 MB/s (currently ~15 MB/s).

**Architecture:** Add async DiskJob variants (WriteAsync, VerifyAsync, VerifyV2Async) that report results via mpsc channels instead of oneshot reply. TorrentActor gets new select! branches for write errors and verify results. The hot path (`handle_piece_data`) switches from blocking `write_chunk().await` to non-blocking `enqueue_write()`, and piece verification switches from inline `verify_piece().await` to `enqueue_verify()` with callback via select! branch.

**Tech Stack:** Rust, tokio (mpsc, oneshot, spawn_blocking), bytes::Bytes, ferrite-session crate

**Base commit:** `c677d23` — TCP 5s timeout, BT verified working (1862 log lines, DHT peers, uTP connections)

---

## CRITICAL: BT Smoke Test Gate

**After EVERY commit**, run:

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
bash /tmp/bt-smoke4.sh
```

The BT smoke test (`/tmp/bt-smoke4.sh`) downloads the March 2026 Arch ISO for 45 seconds. **PASS criteria:**
- "Stdout lines:" must be **600+** (indicates DHT, tracker, peer activity)
- "PEER/PIECE ACTIVITY" section must show "DHT v4 returned peers" and "connection established" lines

**FAIL pattern:** Exactly 81 lines, only NAT/PCP/UPnP timeouts, zero peer activity → **STOP. Revert the commit. Investigate.**

The smoke test script is at `/tmp/bt-smoke4.sh`. If it doesn't exist, create it:

```bash
#!/bin/bash
rm -rf /tmp/ferrite-bench
mkdir -p /tmp/ferrite-bench
MAGNET='magnet:?xt=urn:btih:a4373c326657898d0c588c3ff892a0fac97ffa20&dn=archlinux-2026.03.01-x86_64.iso&tr=udp://tracker.opentrackr.org:1337/announce&tr=udp://open.stealth.si:80/announce'
timeout 45 /mnt/TempNVME/projects/ferrite/target/release/ferrite \
  -l debug download "$MAGNET" \
  --output /tmp/ferrite-bench \
  --quiet \
  1>/tmp/ferrite-out4.log 2>/tmp/ferrite-err4.log || true
echo "Downloaded: $(du -sh /tmp/ferrite-bench/)"
echo "Stdout lines: $(tr '\r' '\n' < /tmp/ferrite-out4.log | wc -l)"
echo "=== PEER/PIECE ACTIVITY ==="
tr '\r' '\n' < /tmp/ferrite-out4.log | sed 's/\x1b\[[0-9;]*m//g' | grep -iE "peer|piece|connect|handshake|unchoke|request|tracker.*resp|got_peers|add_peer" | head -30
echo "=== LAST 10 LOG LINES ==="
tr '\r' '\n' < /tmp/ferrite-out4.log | sed 's/\x1b\[[0-9;]*m//g' | grep -v bootstrap | tail -10
```

---

## Task 1: Add async disk types and DiskJob variants

**Files:**
- Modify: `crates/ferrite-session/src/disk.rs:11-106` (DiskJobFlags area + DiskJob enum)

**Context:** The DiskJob enum (lines 26-106) defines all disk operation variants. Each current variant uses a `oneshot::Sender` for reply. We add new variants that use `mpsc::Sender` for fire-and-forget error/result reporting.

**Step 1: Add the DiskWriteError and VerifyResult types**

Add these types after the `DiskJobFlags` definition (after line 24), before the `DiskJob` enum:

```rust
/// Error report from an async (fire-and-forget) write operation.
#[derive(Debug)]
pub struct DiskWriteError {
    /// Piece index that failed.
    pub piece: u32,
    /// Block offset within the piece.
    pub begin: u32,
    /// The storage error.
    pub error: ferrite_storage::Error,
}

/// Result of an async piece hash verification.
#[derive(Debug)]
pub struct VerifyResult {
    /// Piece index that was verified.
    pub piece: u32,
    /// Whether the hash matched.
    pub passed: bool,
}
```

**Step 2: Add the WriteAsync, VerifyAsync, and VerifyV2Async variants to DiskJob**

Add these variants inside the `DiskJob` enum, after the existing `Write` variant (after line 43):

```rust
    /// Fire-and-forget write: errors reported via mpsc instead of oneshot.
    WriteAsync {
        info_hash: Id20,
        piece: u32,
        begin: u32,
        data: Bytes,
        flags: DiskJobFlags,
        error_tx: mpsc::Sender<DiskWriteError>,
    },
```

And after the existing `HashV2` variant (after line 73):

```rust
    /// Async piece verification (v1 SHA-1): result reported via mpsc.
    VerifyAsync {
        info_hash: Id20,
        piece: u32,
        expected: Id20,
        result_tx: mpsc::Sender<VerifyResult>,
    },

    /// Async piece verification (v2 SHA-256): result reported via mpsc.
    VerifyV2Async {
        info_hash: Id20,
        piece: u32,
        expected: Id32,
        result_tx: mpsc::Sender<VerifyResult>,
    },
```

**Step 3: Add enqueue_write, enqueue_verify, and enqueue_verify_v2 methods to DiskHandle**

Add these methods at the end of the `impl DiskHandle` block (before the closing `}`), after the `move_storage` method:

```rust
    /// Fire-and-forget write: enqueue a write job without waiting for completion.
    /// Errors are reported asynchronously via `error_tx`.
    /// Returns `Err` if the disk channel is full (backpressure).
    pub fn enqueue_write(
        &self,
        piece: u32,
        begin: u32,
        data: Bytes,
        flags: DiskJobFlags,
        error_tx: &mpsc::Sender<DiskWriteError>,
    ) -> Result<(), Bytes> {
        match self.tx.try_send(DiskJob::WriteAsync {
            info_hash: self.info_hash,
            piece,
            begin,
            data,
            flags,
            error_tx: error_tx.clone(),
        }) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(job)) => {
                // Extract data back from the job for caller to retry or fallback
                if let DiskJob::WriteAsync { data, .. } = job {
                    Err(data)
                } else {
                    unreachable!()
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Disk actor gone — silently drop
                Ok(())
            }
        }
    }

    /// Fire-and-forget piece verification (v1 SHA-1).
    /// Result reported asynchronously via `result_tx`.
    pub fn enqueue_verify(
        &self,
        piece: u32,
        expected: Id20,
        result_tx: &mpsc::Sender<VerifyResult>,
    ) {
        let _ = self.tx.try_send(DiskJob::VerifyAsync {
            info_hash: self.info_hash,
            piece,
            expected,
            result_tx: result_tx.clone(),
        });
    }

    /// Fire-and-forget piece verification (v2 SHA-256).
    /// Result reported asynchronously via `result_tx`.
    pub fn enqueue_verify_v2(
        &self,
        piece: u32,
        expected: Id32,
        result_tx: &mpsc::Sender<VerifyResult>,
    ) {
        let _ = self.tx.try_send(DiskJob::VerifyV2Async {
            info_hash: self.info_hash,
            piece,
            expected,
            result_tx: result_tx.clone(),
        });
    }
```

**Step 4: Run tests**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

Expected: All tests pass, no clippy warnings. (No BT smoke test needed for this task — no behaviour changed, only new types and methods added.)

**Step 5: Commit**

```bash
git add crates/ferrite-session/src/disk.rs
git commit -m "feat: add async disk write/verify types and enqueue methods (M57)"
```

---

## Task 2: Handle WriteAsync in DiskActor

**Files:**
- Modify: `crates/ferrite-session/src/disk.rs:500-530` (DiskActor::process_job match arms)

**Context:** The `process_job()` method (starting ~line 500) has a `match job { ... }` that handles each DiskJob variant. We add a new arm for `WriteAsync`.

**Step 1: Add the WriteAsync match arm**

In `process_job()`, add this arm after the existing `DiskJob::Write` arm (after the `let _ = reply.send(to_storage_result(result));` line):

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
                let _ = error_tx.try_send(DiskWriteError { piece, begin, error: e });
            }
        }
```

**Step 2: Run tests**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

Expected: All pass. No BT smoke test needed — no behaviour changed.

**Step 3: Commit**

```bash
git add crates/ferrite-session/src/disk.rs
git commit -m "feat: handle WriteAsync in DiskActor (M57)"
```

---

## Task 3: Handle VerifyAsync and VerifyV2Async in DiskActor

**Files:**
- Modify: `crates/ferrite-session/src/disk.rs:500-650` (DiskActor::process_job match arms)

**Context:** Add match arms for the two async verification variants, after the existing `DiskJob::HashV2` arm.

**Step 1: Add the VerifyAsync match arm**

After the existing `DiskJob::HashV2` arm:

```rust
        DiskJob::VerifyAsync {
            info_hash,
            piece,
            expected,
            result_tx,
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
            let _ = result_tx.try_send(VerifyResult { piece, passed });
        }
```

**Step 2: Add the VerifyV2Async match arm**

Immediately after the VerifyAsync arm:

```rust
        DiskJob::VerifyV2Async {
            info_hash,
            piece,
            expected,
            result_tx,
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
            let _ = result_tx.try_send(VerifyResult { piece, passed });
        }
```

**Step 3: Run tests**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

Expected: All pass. No BT smoke test needed — no behaviour changed.

**Step 4: Commit**

```bash
git add crates/ferrite-session/src/disk.rs
git commit -m "feat: handle VerifyAsync and VerifyV2Async in DiskActor (M57)"
```

---

## Task 4: Wire async channels into TorrentActor + select! branches

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs` — struct fields (~line 1155), constructor, select! loop (~line 1516)

**Context:** This is the first task that touches `torrent.rs`. We add channel fields and select! branches, but nothing writes to these channels yet, so BT behaviour is unchanged. **This is a critical BT smoke test checkpoint.**

**Step 1: Add channel fields to TorrentActor struct**

Add these fields after the `event_rx` field (around line 1260):

```rust
    // Async disk result channels (M57 non-blocking pipeline)
    write_error_rx: mpsc::Receiver<crate::disk::DiskWriteError>,
    write_error_tx: mpsc::Sender<crate::disk::DiskWriteError>,
    verify_result_rx: mpsc::Receiver<crate::disk::VerifyResult>,
    verify_result_tx: mpsc::Sender<crate::disk::VerifyResult>,
```

**Step 2: Initialize channels in constructor**

Find the TorrentActor constructor (the function that creates `TorrentActor { ... }`). Add channel creation and pass to the struct. Use bounded channels with reasonable capacity:

```rust
let (write_error_tx, write_error_rx) = mpsc::channel(64);
let (verify_result_tx, verify_result_rx) = mpsc::channel(64);
```

And in the struct literal:
```rust
write_error_rx,
write_error_tx,
verify_result_rx,
verify_result_tx,
```

**Step 3: Add select! branches**

In the main `tokio::select!` loop, add two new branches. Place them after the `event_rx` branch (peer events) and before the timer branches. They should be near the top since they're high-priority I/O results:

```rust
        // Async disk write error (M57 non-blocking pipeline)
        Some(err) = self.write_error_rx.recv() => {
            warn!(piece = err.piece, begin = err.begin, "async disk write failed: {}", err.error);
        }

        // Async piece verification result (M57 non-blocking pipeline)
        Some(result) = self.verify_result_rx.recv() => {
            if result.passed {
                self.on_piece_verified(result.piece).await;
            } else {
                self.on_piece_hash_failed(result.piece).await;
            }
        }
```

**Step 4: Run tests + BT smoke test**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo build --release
bash /tmp/bt-smoke4.sh
```

Expected: All tests pass. BT smoke test shows **600+ lines** with DHT peers and connections. The new channels exist but nothing writes to them, so behaviour is identical to base.

**If BT smoke test fails (81 lines):** STOP. The struct field additions or select! branches broke the actor loop. Revert and investigate.

**Step 5: Commit**

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "feat: wire async disk channels into TorrentActor select loop (M57)"
```

---

## Task 5: Rewrite handle_piece_data() for non-blocking writes

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs:2890-2897` (select! dispatch) and `3115-3195` (handle_piece_data method)

**Context:** This is THE critical change. We change the method signature from `data: &[u8]` to `data: Bytes`, replace `disk.write_chunk().await` with `disk.enqueue_write()`, and eliminate the second Bytes copy. **Highest risk task — BT smoke test is mandatory.**

**Step 1: Update the select! dispatch site**

Change the PieceData arm in the select! loop (around line 2890) from:

```rust
            PeerEvent::PieceData {
                peer_addr,
                index,
                begin,
                data,
            } => {
                self.handle_piece_data(peer_addr, index, begin, &data)
                    .await;
            }
```

To:

```rust
            PeerEvent::PieceData {
                peer_addr,
                index,
                begin,
                data,
            } => {
                self.handle_piece_data(peer_addr, index, begin, data)
                    .await;
            }
```

(Remove the `&` — pass owned `Bytes` instead of borrowed `&[u8]`.)

**Step 2: Rewrite handle_piece_data()**

Replace the entire method (lines ~3115-3195) with:

```rust
    async fn handle_piece_data(
        &mut self,
        peer_addr: SocketAddr,
        index: u32,
        begin: u32,
        data: Bytes,
    ) {
        let data_len = data.len();

        // Fire-and-forget write: enqueue without blocking the select! loop.
        // Zero-copy: Bytes moved directly from wire codec → disk actor.
        if let Some(ref disk) = self.disk {
            match disk.enqueue_write(
                index,
                begin,
                data,
                DiskJobFlags::empty(),
                &self.write_error_tx,
            ) {
                Ok(()) => {}
                Err(_data) => {
                    // Channel full (backpressure) — fall back to blocking write
                    warn!(index, begin, "disk channel full, blocking write");
                    if let Err(e) = disk
                        .write_chunk(index, begin, _data, DiskJobFlags::empty())
                        .await
                    {
                        warn!(index, begin, "failed to write chunk: {e}");
                        return;
                    }
                }
            }
        }

        self.downloaded += data_len as u64;
        self.total_download += data_len as u64 + 13; // payload + message header
        self.last_download = now_unix();
        self.need_save_resume = true;

        // Smart banning: track which peers contribute to each piece
        self.piece_contributors
            .entry(index)
            .or_default()
            .insert(peer_addr.ip());

        if let Some(peer) = self.peers.get_mut(&peer_addr) {
            if let Some(pos) = peer
                .pending_requests
                .iter()
                .position(|&(i, b, _)| i == index && b == begin)
            {
                peer.pending_requests.swap_remove(pos);
            }
            peer.download_bytes_window += data_len as u64;

            // Update pipeline state (M28)
            peer.pipeline
                .block_received(index, begin, data_len as u32, std::time::Instant::now());
            peer.last_data_received = Some(std::time::Instant::now());
            // Clear snub if snubbed
            if peer.snubbed {
                peer.snubbed = false;
                peer.pipeline.reset_to_slow_start();
            }
        }

        // Remove block assignment from InFlightPiece
        if let Some(ifp) = self.in_flight_pieces.get_mut(&index) {
            ifp.assigned_blocks.remove(&(index, begin));
        }

        // End-game: cancel this block on all other peers
        if self.end_game.is_active() {
            let cancels = self.end_game.block_received(index, begin, peer_addr);
            for (cancel_addr, ci, cb, cl) in cancels {
                if let Some(cancel_peer) = self.peers.get_mut(&cancel_addr) {
                    let _ = cancel_peer
                        .cmd_tx
                        .send(PeerCommand::Cancel {
                            index: ci,
                            begin: cb,
                            length: cl,
                        })
                        .await;
                    if let Some(pos) = cancel_peer
                        .pending_requests
                        .iter()
                        .position(|&(i, b, _)| i == ci && b == cb)
                    {
                        cancel_peer.pending_requests.swap_remove(pos);
                    }
                }
            }
        }

        // Track chunk completion
        let piece_complete = if let Some(ref mut ct) = self.chunk_tracker {
            ct.chunk_received(index, begin)
        } else {
            false
        };

        if piece_complete {
            // NOTE: Piece verification is still inline (blocking) for now.
            // Task 6 will convert this to async dispatch via enqueue_verify().
            self.verify_and_mark_piece(index).await;
        }
    }
```

**Important:** This preserves the existing `verify_and_mark_piece()` call for now. Task 6 will convert verification to async. This keeps the change focused on writes only.

**Step 3: Run tests + BT smoke test**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo build --release
bash /tmp/bt-smoke4.sh
```

Expected: All tests pass. BT smoke test shows **600+ lines** with peer activity. Downloads should be noticeably faster since writes no longer block the select! loop.

**If BT smoke test fails (81 lines):** STOP. This is the highest-risk change. The method signature change from `&[u8]` to `Bytes` broke BT in the previous attempt. Check:
1. Did the select! dispatch site correctly remove `&`?
2. Is there another call site to `handle_piece_data` that wasn't updated?
3. Does the `enqueue_write` fallback path work correctly?

**Step 4: Commit**

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "perf: non-blocking disk writes in handle_piece_data with zero-copy (M57)"
```

---

## Task 6: Convert piece verification to async dispatch

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs` — the piece completion section inside `handle_piece_data()` and potentially `verify_and_mark_piece()`

**Context:** After Task 5, writes are non-blocking but verification is still inline. We change the piece completion path to use `enqueue_verify()` / `enqueue_verify_v2()` instead of blocking `verify_piece().await`. Results arrive via the `verify_result_rx` select! branch added in Task 4.

**Step 1: Find the piece completion code**

Inside `handle_piece_data()` (written in Task 5), the piece completion section currently calls:
```rust
if piece_complete {
    self.verify_and_mark_piece(index).await;
}
```

Find the `verify_and_mark_piece()` method. It likely calls `disk.flush_piece().await` then `disk.verify_piece().await` or `disk.verify_piece_v2().await`, then calls `on_piece_verified()` or `on_piece_hash_failed()`.

**Step 2: Replace inline verification with async dispatch**

Replace the piece completion block in `handle_piece_data()`:

```rust
        if piece_complete {
            // Flush the piece to disk before verifying
            if let Some(ref disk) = self.disk {
                if let Err(e) = disk.flush_piece(index).await {
                    warn!(index, "failed to flush piece before verify: {e}");
                }

                // Dispatch async verification based on torrent version
                match self.version {
                    ferrite_core::TorrentVersion::V2 | ferrite_core::TorrentVersion::Hybrid => {
                        if let Some(ref hp) = self.hash_picker {
                            if let Some(expected) = hp.piece_root(index) {
                                disk.enqueue_verify_v2(index, expected, &self.verify_result_tx);
                            }
                        } else if let Some(ref meta) = self.meta {
                            if let Some(hash) = meta.piece_hash(index) {
                                disk.enqueue_verify(index, hash, &self.verify_result_tx);
                            }
                        }
                    }
                    ferrite_core::TorrentVersion::V1 => {
                        if let Some(ref meta) = self.meta {
                            if let Some(hash) = meta.piece_hash(index) {
                                disk.enqueue_verify(index, hash, &self.verify_result_tx);
                            }
                        }
                    }
                }
            }

            // M44: Predictive piece announce — send Have before verification
            if self.config.predictive_piece_announce > 0 {
                self.send_predictive_have(index).await;
            }
        }
```

**Important notes for the implementer:**
- Look at the existing `verify_and_mark_piece()` method to understand how it currently determines the expected hash and dispatches v1 vs v2 verification. Mirror that logic here using `enqueue_verify` / `enqueue_verify_v2` instead.
- The `flush_piece().await` call remains blocking — this is intentional. Flushing ensures data is on disk before we hash it. This is a single call per piece (~50ms for a 4MB piece), not per block (~1ms × 256 blocks).
- The `on_piece_verified()` / `on_piece_hash_failed()` calls are now handled by the `verify_result_rx` select! branch from Task 4. Do NOT call them inline.
- Keep the predictive Have announcement — it should still fire immediately on piece completion.

**Step 3: Run tests + BT smoke test**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo build --release
bash /tmp/bt-smoke4.sh
```

Expected: All tests pass. BT smoke test shows 600+ lines. With both writes AND verification non-blocking, throughput should be significantly improved.

**Step 4: Commit**

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "perf: async piece verification via enqueue_verify (M57)"
```

---

## Task 7: HashSet peer dedup in handle_add_peers()

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs` — struct fields (~line 1240), constructor, `handle_add_peers()` (~line 1961)

**Context:** The current `handle_add_peers()` does `O(n)` linear scan: `self.available_peers.iter().any(|(a, _)| *a == addr)`. With hundreds of peers, this is wasteful. Add a `HashSet<SocketAddr>` for O(1) dedup.

**Step 1: Add the field**

Add to `TorrentActor` struct, near the `available_peers` field:

```rust
    /// O(1) dedup set for available_peers (M57).
    available_peers_set: HashSet<SocketAddr>,
```

**Step 2: Initialize in constructor**

```rust
available_peers_set: HashSet::new(),
```

**Step 3: Update handle_add_peers()**

Replace the current `handle_add_peers()` method (lines ~1961-1984):

```rust
    fn handle_add_peers(&mut self, peers: Vec<SocketAddr>, source: PeerSource) {
        let mut added = false;
        {
            let ban_mgr = self.ban_manager.read().unwrap();
            let ip_flt = self.ip_filter.read().unwrap();
            for addr in peers {
                if ban_mgr.is_banned(&addr.ip()) {
                    continue;
                }
                if ip_flt.is_blocked(addr.ip()) {
                    continue;
                }
                if !self.peers.contains_key(&addr)
                    && self.available_peers_set.insert(addr)
                {
                    self.available_peers.push((addr, source));
                    added = true;
                }
            }
        }
        if added {
            self.sort_available_peers();
        }
    }
```

The key change: `self.available_peers_set.insert(addr)` returns `false` if already present (O(1)), replacing the `O(n)` `.iter().any()`.

**Step 4: Keep the set in sync**

Search for all places that remove from `available_peers` (e.g., when connecting to a peer, when a peer is banned). At each removal site, also remove from `available_peers_set`. Common patterns to find:

```bash
# Find all sites that modify available_peers
grep -n "available_peers" crates/ferrite-session/src/torrent.rs
```

For each `available_peers.remove()`, `available_peers.swap_remove()`, `available_peers.retain()`, `available_peers.pop()`, or `available_peers.clear()`, add the corresponding `available_peers_set.remove(&addr)` or `available_peers_set.clear()`.

**Step 5: Run tests + BT smoke test**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo build --release
bash /tmp/bt-smoke4.sh
```

Expected: All pass. BT smoke test shows 600+ lines.

**Step 6: Commit**

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "perf: O(1) peer dedup with HashSet in handle_add_peers (M57)"
```

---

## Task 8: Unit tests for async disk paths

**Files:**
- Modify: `crates/ferrite-session/src/disk.rs` (tests module at bottom of file)

**Context:** Add tests that verify the async write and verify paths work correctly through the DiskActor.

**Step 1: Write tests**

Add these tests to the `#[cfg(test)] mod tests` block at the bottom of `disk.rs`:

```rust
    #[tokio::test]
    async fn test_enqueue_write_success() {
        // Set up a real DiskActor with in-memory storage
        let (disk_manager, _handle) = create_test_disk_manager();
        let info_hash = Id20::default();
        let storage = Arc::new(ferrite_storage::MemoryStorage::new());
        let disk = disk_manager.register(info_hash, storage).await;

        let (error_tx, mut error_rx) = mpsc::channel(16);

        // Enqueue a write
        let data = Bytes::from(vec![0u8; 16384]);
        let result = disk.enqueue_write(0, 0, data, DiskJobFlags::empty(), &error_tx);
        assert!(result.is_ok());

        // Give DiskActor time to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Should have no errors
        assert!(error_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_enqueue_verify_reports_result() {
        let (disk_manager, _handle) = create_test_disk_manager();
        let info_hash = Id20::default();
        let storage = Arc::new(ferrite_storage::MemoryStorage::new());
        let disk = disk_manager.register(info_hash, storage).await;

        let (result_tx, mut result_rx) = mpsc::channel(16);

        // Enqueue verification with a dummy hash (will fail since no data written)
        disk.enqueue_verify(0, Id20::default(), &result_tx);

        // Wait for result
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            result_rx.recv(),
        )
        .await
        .expect("timeout waiting for verify result")
        .expect("channel closed");

        assert_eq!(result.piece, 0);
        // Result depends on storage implementation — the important thing is we got a result
    }
```

**Important for implementer:** The test helper `create_test_disk_manager()` may not exist. Check what test infrastructure already exists in the `tests` module. If needed, create a minimal helper that spawns a DiskActor with an in-memory backend. Look at existing tests in the file for patterns.

If `MemoryStorage` doesn't exist, use the existing test patterns — check what storage backend the existing tests use. The key thing is that `enqueue_write` returns `Ok(())` and `enqueue_verify` delivers a `VerifyResult` through the channel.

**Step 2: Run tests**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

Expected: All pass including new tests.

**Step 3: Commit**

```bash
git add crates/ferrite-session/src/disk.rs
git commit -m "test: add unit tests for async disk write and verify (M57)"
```

---

## Task 9: Version bump, benchmark, CHANGELOG, README

**Files:**
- Modify: `Cargo.toml` (root workspace version)
- Modify: `CHANGELOG.md`
- Modify: `README.md` (if benchmark numbers change significantly)

**Step 1: Bump version**

In the root `Cargo.toml`, change the workspace version from `"0.63.0"` to `"0.64.0"`.

**Step 2: Run benchmark**

```bash
cargo build --release
bash /tmp/bt-smoke4.sh
```

Record the download speed. If the Arch ISO (1.42 GiB) downloads within 45 seconds, calculate throughput.

**Step 3: Update CHANGELOG.md**

Add a new section at the top:

```markdown
## v0.64.0 — M57: Non-Blocking Transfer Pipeline

### Performance
- **Non-blocking disk writes:** `handle_piece_data()` uses fire-and-forget `enqueue_write()` instead of blocking `write_chunk().await`, unblocking the TorrentActor select! loop
- **Async piece verification:** Hash verification dispatched via `enqueue_verify()` with results returned through select! branch
- **Zero-copy piece data:** `Bytes` moved directly from wire codec to disk actor without intermediate copy
- **O(1) peer dedup:** `HashSet` replaces linear scan in `handle_add_peers()`
- **TCP connect timeout:** 5-second timeout replaces OS default (~2 minutes)

### Internal
- New `DiskJob::WriteAsync`, `DiskJob::VerifyAsync`, `DiskJob::VerifyV2Async` variants
- New `DiskHandle::enqueue_write()`, `enqueue_verify()`, `enqueue_verify_v2()` methods
- New `DiskWriteError` and `VerifyResult` types for async result reporting
- TorrentActor `write_error_rx` and `verify_result_rx` select! branches
- Backpressure: bounded channel with fallback to blocking write when full
```

**Step 4: Run full test suite**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

**Step 5: Commit**

```bash
git add Cargo.toml crates/*/Cargo.toml CHANGELOG.md README.md
git commit -m "feat: version bump to 0.64.0, changelog for M57 non-blocking pipeline"
```

**Step 6: Final BT smoke test**

```bash
cargo build --release
bash /tmp/bt-smoke4.sh
```

Confirm everything still works with the version bump.
