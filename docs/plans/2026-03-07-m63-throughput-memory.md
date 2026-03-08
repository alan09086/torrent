# M63: Throughput & Memory Improvements — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Close throughput and memory gaps with rqbit by gating hashing concurrency, batching block dispatch, and eliminating the in-memory store buffer.

**Architecture:** Three independent changes to the disk I/O and request dispatch subsystems. Hashing gets a semaphore to prevent CPU saturation. Request drivers batch permit acquisitions to reduce channel pressure. Store buffer is replaced with direct disk I/O — blocks write to disk on receipt, verification reads from disk.

**Tech Stack:** Rust 2024, tokio, Bytes, FxHashMap

---

### Task 1: Gate Hashing Concurrency with Semaphore

**Files:**
- Modify: `crates/torrent-session/src/disk.rs:274-291` (DiskHandle struct)
- Modify: `crates/torrent-session/src/disk.rs:235-255` (register_torrent)
- Modify: `crates/torrent-session/src/disk.rs:543-596` (enqueue_verify)
- Modify: `crates/torrent-session/src/disk.rs:601-631` (enqueue_verify_v2)
- Modify: `crates/torrent-session/src/disk.rs:205-230` (DiskManagerHandle)

**Step 1: Add verify_semaphore to DiskHandle**

Add a shared semaphore to DiskHandle to cap concurrent hash verification tasks:

```rust
// disk.rs — DiskHandle struct (line ~276)
#[derive(Clone)]
pub struct DiskHandle {
    tx: mpsc::Sender<DiskJob>,
    info_hash: Id20,
    store_buffer: Arc<StoreBuffer>,
    verify_semaphore: Arc<tokio::sync::Semaphore>,
}
```

**Step 2: Add verify_semaphore to DiskManagerHandle**

```rust
// disk.rs — DiskManagerHandle struct (line ~208)
#[derive(Clone)]
pub struct DiskManagerHandle {
    tx: mpsc::Sender<DiskJob>,
    store_buffer: Arc<StoreBuffer>,
    verify_semaphore: Arc<tokio::sync::Semaphore>,
}
```

Create it in `new_with_backend()` (line ~222):

```rust
let verify_semaphore = Arc::new(tokio::sync::Semaphore::new(2));
```

Pass into the returned struct. The permit count `2` means at most 2 concurrent hash tasks.

**Step 3: Wire verify_semaphore into register_torrent**

In `register_torrent()` (line ~250), pass the semaphore to DiskHandle:

```rust
DiskHandle {
    tx: self.tx.clone(),
    info_hash,
    store_buffer: Arc::clone(&self.store_buffer),
    verify_semaphore: Arc::clone(&self.verify_semaphore),
}
```

**Step 4: Also update DiskHandle::new() (test constructor)**

In `DiskHandle::new()` (line ~286):

```rust
pub(crate) fn new(tx: mpsc::Sender<DiskJob>, info_hash: Id20) -> Self {
    Self {
        tx,
        info_hash,
        store_buffer: Arc::new(Mutex::new(HashMap::new())),
        verify_semaphore: Arc::new(tokio::sync::Semaphore::new(2)),
    }
}
```

**Step 5: Gate enqueue_verify with semaphore**

In `enqueue_verify()` (line ~560), acquire a permit before spawning:

```rust
pub fn enqueue_verify(
    &self,
    piece: u32,
    expected: Id20,
    result_tx: &mpsc::Sender<VerifyResult>,
) {
    let blocks = {
        self.store_buffer
            .lock()
            .unwrap()
            .remove(&(self.info_hash, piece))
    };
    let result_tx = result_tx.clone();
    let verify_sem = self.verify_semaphore.clone();
    tokio::spawn(async move {
        // Gate: at most 2 concurrent hash tasks
        let _permit = verify_sem.acquire().await.unwrap();
        let passed = tokio::task::spawn_blocking(move || {
            // ... existing hashing logic unchanged ...
        })
        .await
        .unwrap();
        // _permit dropped here, freeing slot for next hash task
        let _ = result_tx.send(VerifyResult { piece, passed }).await;
    });
}
```

**Step 6: Gate enqueue_verify_v2 with semaphore**

Same pattern in `enqueue_verify_v2()` (line ~601):

```rust
pub fn enqueue_verify_v2(
    &self,
    piece: u32,
    expected: Id32,
    result_tx: &mpsc::Sender<VerifyResult>,
) {
    let blocks = {
        self.store_buffer
            .lock()
            .unwrap()
            .remove(&(self.info_hash, piece))
    };
    let result_tx = result_tx.clone();
    let verify_sem = self.verify_semaphore.clone();
    tokio::spawn(async move {
        let _permit = verify_sem.acquire().await.unwrap();
        let passed = tokio::task::spawn_blocking(move || {
            // ... existing v2 hashing logic unchanged ...
        })
        .await
        .unwrap();
        let _ = result_tx.send(VerifyResult { piece, passed }).await;
    });
}
```

**Step 7: Run tests**

Run: `cargo test --workspace 2>&1 | grep "test result:"`
Expected: all 1402+ pass

**Step 8: Commit**

```bash
git add crates/torrent-session/src/disk.rs
git commit -m "perf: gate hash verification concurrency with Semaphore(2) (M63)"
```

---

### Task 2: Batch Dispatch (N Blocks Per Driver Wake)

**Files:**
- Modify: `crates/torrent-session/src/request_driver.rs` (driver loop)
- Modify: `crates/torrent-session/src/torrent.rs:1916-1921` (NeedBlocks select arm)
- Modify: `crates/torrent-session/src/torrent.rs:5186+` (dispatch_single_block)

**Step 1: Change DriverMessage to carry batch count**

In `request_driver.rs` (line ~18):

```rust
#[derive(Debug)]
pub(crate) enum DriverMessage {
    /// The driver acquired N permits and needs the torrent actor to pick +
    /// dispatch N blocks.
    NeedBlocks(u32),
}
```

**Step 2: Modify driver loop to batch permit acquisition**

In the driver loop function (line ~37), replace the single `semaphore.acquire()` with batch acquisition. Acquire up to 8 permits using `acquire_many`, with fallback to whatever is available:

Find the section where the driver acquires a permit and sends NeedBlocks. Replace:
- Single `semaphore.acquire().await` + `permit.forget()` + `send(NeedBlocks)`

With:
- Try `semaphore.acquire_many(8)` first. If not enough permits, try `acquire()` for at least 1.
- Use `semaphore.acquire().await` to get the first permit (blocks until available)
- Then `semaphore.try_acquire_many(7)` to grab up to 7 more (non-blocking)
- Forget all acquired permits
- Send `NeedBlocks(count)` with the total count

```rust
// Acquire at least 1 permit (blocking), then try to grab up to 7 more
let permit = semaphore.acquire().await.map_err(|_| ())?;
permit.forget();
let mut count: u32 = 1;
// Opportunistically grab more permits without blocking
if let Ok(extra) = semaphore.try_acquire_many(7) {
    count += extra.num_permits() as u32;
    extra.forget();
}
let _ = driver_tx.send((peer_addr, DriverMessage::NeedBlocks(count))).await;
```

**Step 3: Update the NeedBlocks handler in torrent.rs select! loop**

In `torrent.rs` (line ~1916):

```rust
Some((peer_addr, msg)) = self.driver_msg_rx.recv() => {
    match msg {
        DriverMessage::NeedBlocks(count) => {
            for _ in 0..count {
                self.dispatch_single_block(peer_addr).await;
            }
        }
    }
}
```

This loops `dispatch_single_block` N times per message. The function already handles early-exit cases (peer gone, choked, no blocks) by releasing the permit, so extra iterations are safe.

**Step 4: Run tests**

Run: `cargo test --workspace 2>&1 | grep "test result:"`
Expected: all pass

**Step 5: Commit**

```bash
git add crates/torrent-session/src/request_driver.rs crates/torrent-session/src/torrent.rs
git commit -m "perf: batch dispatch up to 8 blocks per driver wake (M63)"
```

---

### Task 3: Eliminate Store Buffer (Direct Disk I/O for Verification)

**Files:**
- Modify: `crates/torrent-session/src/disk.rs` (major changes)
- Modify: `crates/torrent-session/src/disk_backend.rs` (add read_piece)
- Test: existing tests should continue passing

This is the largest task. The store buffer (`Mutex<HashMap<(Id20, u32), BTreeMap<u32, Bytes>>>`) holds all blocks for in-flight pieces in RAM. We replace it with direct disk reads for verification.

**Step 1: Add `read_piece()` to DiskIoBackend trait**

In `crates/torrent-session/src/disk_backend.rs`, add a default method to the `DiskIoBackend` trait (after `read_chunk` at line ~65):

```rust
/// Read an entire piece as a single contiguous buffer.
/// Default implementation reads chunk-by-chunk using `read_chunk()`.
fn read_piece(
    &self,
    info_hash: Id20,
    piece: u32,
    piece_length: u32,
) -> crate::Result<Bytes> {
    self.read_chunk(info_hash, piece, 0, piece_length, true)
}
```

**Step 2: Add `DiskJob::VerifyPiece` variant**

In `disk.rs`, add a new DiskJob variant that routes verification through the DiskActor (where the backend is available):

```rust
DiskJob::VerifyPiece {
    info_hash: Id20,
    piece: u32,
    piece_length: u32,
    expected_v1: Option<Id20>,
    expected_v2: Option<Id32>,
    result_tx: mpsc::Sender<VerifyResult>,
}
```

**Step 3: Remove store buffer insertion from enqueue_write**

In `enqueue_write()` (line ~500), remove the store buffer insertion block (lines 510-515):

```rust
// DELETE these lines:
// self.store_buffer
//     .lock()
//     .unwrap()
//     .entry((self.info_hash, piece))
//     .or_default()
//     .insert(begin, data.clone());
```

Also remove the `data.clone()` — the original `data: Bytes` is passed directly to the `DiskJob::WriteAsync` without cloning.

**Step 4: Rewrite enqueue_verify to send DiskJob::VerifyPiece**

Replace the entire body of `enqueue_verify()` (line ~543):

```rust
pub fn enqueue_verify(
    &self,
    piece: u32,
    piece_length: u32,
    expected: Id20,
    result_tx: &mpsc::Sender<VerifyResult>,
) {
    let _ = self.tx.try_send(DiskJob::VerifyPiece {
        info_hash: self.info_hash,
        piece,
        piece_length,
        expected_v1: Some(expected),
        expected_v2: None,
        result_tx: result_tx.clone(),
    });
}
```

Note: signature now takes `piece_length: u32`. The caller (`torrent.rs:3666`) needs to pass it.

**Step 5: Similarly rewrite enqueue_verify_v2**

```rust
pub fn enqueue_verify_v2(
    &self,
    piece: u32,
    piece_length: u32,
    expected: Id32,
    result_tx: &mpsc::Sender<VerifyResult>,
) {
    let _ = self.tx.try_send(DiskJob::VerifyPiece {
        info_hash: self.info_hash,
        piece,
        piece_length,
        expected_v1: None,
        expected_v2: Some(expected),
        result_tx: result_tx.clone(),
    });
}
```

**Step 6: Handle DiskJob::VerifyPiece in DiskActor::dispatch_job**

Add a new match arm in `dispatch_job()` (after line ~720):

```rust
DiskJob::VerifyPiece {
    info_hash,
    piece,
    piece_length,
    expected_v1,
    expected_v2,
    result_tx,
} => {
    let backend = Arc::clone(&self.backend);
    let verify_sem = self.verify_semaphore.clone();
    tokio::spawn(async move {
        let _permit = verify_sem.acquire().await.unwrap();
        let passed = tokio::task::spawn_blocking(move || {
            match backend.read_piece(info_hash, piece, piece_length) {
                Ok(data) => {
                    if let Some(expected) = expected_v1 {
                        let actual = torrent_core::sha1_chunks(
                            std::iter::once(data.as_ref()),
                        );
                        actual == expected
                    } else if let Some(expected) = expected_v2 {
                        let actual = torrent_core::sha256_chunks(
                            std::iter::once(data.as_ref()),
                        );
                        actual == expected
                    } else {
                        false
                    }
                }
                Err(e) => {
                    tracing::warn!(piece, %e, "verify: disk read failed");
                    false
                }
            }
        })
        .await
        .unwrap();
        let _ = result_tx.send(VerifyResult { piece, passed }).await;
    });
}
```

Note: the verify_semaphore needs to be added to DiskActor too (from Task 1).

**Step 7: Add verify_semaphore to DiskActor**

Add `verify_semaphore: Arc<tokio::sync::Semaphore>` to DiskActor struct and constructor, passing it from DiskManagerHandle through the actor creation.

**Step 8: Update callers of enqueue_verify to pass piece_length**

In `torrent.rs` (line ~3666), the caller needs piece length. It has access to `self.meta` which contains `Lengths`. Update:

```rust
// torrent.rs — find the call to disk.enqueue_verify (line ~3666)
if let Some(ref disk) = self.disk
    && let Some(expected) = self
        .meta
        .as_ref()
        .and_then(|m| m.info.piece_hash(index as usize))
{
    let piece_length = self.meta.as_ref()
        .map(|m| m.info.lengths().piece_length(index))
        .unwrap_or(0) as u32;
    self.pending_verify.insert(index);
    disk.enqueue_verify(index, piece_length, expected, &self.verify_result_tx);
}
```

Do the same for v2 verification callers.

**Step 9: Remove StoreBuffer type and all references**

1. Delete the `StoreBuffer` type alias (line 52)
2. Remove `store_buffer` field from `DiskHandle`, `DiskManagerHandle`, and `DiskActor`
3. Remove store buffer creation in `DiskManagerHandle::new_with_backend()` (line ~226)
4. Remove store buffer from `register_torrent()` (line ~253)
5. Remove store buffer cloning in `DiskHandle::new()`
6. Remove the store buffer lock in `DiskJob::ClearPiece` handler (lines 717-720) — keep `backend.clear_piece()`
7. Remove the store buffer lock in `DiskJob::Unregister` handler (lines 710-713) — keep `backend.unregister()`
8. Clean up any remaining `store_buffer` imports (`HashMap`, `BTreeMap`, `Mutex` if unused)

**Step 10: Ensure DiskJob::WriteAsync flushes before verify**

The write must be flushed to disk before verify reads it back. In `enqueue_write()`, set the `FLUSH_PIECE` flag when the piece will be verified. Alternatively, in the `DiskJob::VerifyPiece` handler, call `backend.flush_piece(info_hash, piece)` before reading. Add before the `read_piece` call:

```rust
let _ = backend.flush_piece(info_hash, piece);
```

Check if `flush_piece` exists on the backend trait. If not, use `flush_all()` or ensure writes are synchronous.

**Step 11: Run tests**

Run: `cargo test --workspace 2>&1 | grep "test result:"`
Expected: all pass. Some tests may need updates if they directly reference the store buffer.

**Step 12: Commit**

```bash
git add crates/torrent-session/src/disk.rs crates/torrent-session/src/disk_backend.rs crates/torrent-session/src/torrent.rs
git commit -m "perf: eliminate store buffer, verify from disk (M63)"
```

---

### Task 4: Benchmark (5 Trials)

**Step 1: Build release binary**

```bash
cargo build --release -p torrent-cli
```

**Step 2: Run 5-trial benchmark**

```bash
MAGNET="magnet:?xt=urn:btih:a4373c326657898d0c588c3ff892a0fac97ffa20&dn=archlinux-2026.03.01-x86_64.iso"
bash benchmarks/run_benchmark.sh "$MAGNET" 5
```

**Step 3: Generate report**

```bash
bash benchmarks/generate_report.sh
```

**Step 4: Commit results**

```bash
git add benchmarks/results.csv docs/benchmark-report-v0.69.0.md
git commit -m "bench: v0.69.0 benchmark results (M63)"
```

---

### Task 5: Version Bump to 0.69.0

**Step 1: Bump version in root Cargo.toml**

Change `version = "0.68.0"` to `version = "0.69.0"` in `Cargo.toml` line 7.

**Step 2: Update CHANGELOG.md**

Add entry for `0.69.0 — M63: Throughput & Memory Improvements` covering:
- Gated hash verification concurrency (Semaphore(2))
- Batch dispatch (up to 8 blocks per driver wake)
- Eliminated store buffer (direct disk I/O for verification)
- Benchmark results

**Step 3: Update README.md**

Update version badge and dependency example to 0.69.0.

**Step 4: Commit and push**

```bash
git add Cargo.toml CHANGELOG.md README.md
git commit -m "feat: version bump to 0.69.0, changelog for M63"
git push origin main && git push github main
```
