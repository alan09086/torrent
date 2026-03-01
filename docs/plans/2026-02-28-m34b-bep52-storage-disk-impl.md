# M34b: BEP 52 Storage v2 + Disk I/O Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Extend ferrite-storage and ferrite-session disk I/O to support v2 SHA-256 piece verification, per-block hashing, and Merkle-aware chunk tracking.

**Architecture:** Add `verify_piece_v2()` and `hash_block()` default methods to the `TorrentStorage` trait. Extend `ChunkTracker` with optional per-block Merkle verification state. Add `HashV2` and `BlockHash` job variants to the disk actor. All changes are additive — v1 torrents are unaffected.

**Tech Stack:** Rust edition 2024, `ferrite-core` hash types (`Id32`, `sha256`), existing `TorrentStorage` trait, `DiskJob` enum, `DiskHandle`.

**Design doc:** `docs/plans/2026-02-28-m34-bep52-v2-wire-storage-design.md`

**Depends on:** M34a complete (v0.38.0) — needs `Id32` re-exported through ferrite-core.

---

### Task 1: Extend TorrentStorage Trait with v2 Methods

**Files:**
- Modify: `crates/ferrite-storage/src/storage.rs`

**Step 1: Write the failing tests**

Create a new test file `crates/ferrite-storage/tests/v2_storage_test.rs`:

```rust
use ferrite_core::{sha256, Id20, Id32, Lengths};
use ferrite_storage::memory::MemoryStorage;
use ferrite_storage::TorrentStorage;

#[test]
fn verify_piece_v2_correct_hash() {
    let data = vec![0xABu8; 32768]; // 2 blocks
    let lengths = Lengths::new(32768, 32768, 16384);
    let storage = MemoryStorage::new(lengths);
    storage.write_chunk(0, 0, &data[..16384]).unwrap();
    storage.write_chunk(0, 16384, &data[16384..]).unwrap();

    let expected = sha256(&data);
    assert!(storage.verify_piece_v2(0, &expected).unwrap());
}

#[test]
fn verify_piece_v2_wrong_hash() {
    let data = vec![0xABu8; 16384];
    let lengths = Lengths::new(16384, 16384, 16384);
    let storage = MemoryStorage::new(lengths);
    storage.write_chunk(0, 0, &data).unwrap();

    let wrong = Id32::ZERO;
    assert!(!storage.verify_piece_v2(0, &wrong).unwrap());
}

#[test]
fn hash_block_returns_correct_sha256() {
    let data = vec![0xCDu8; 16384];
    let lengths = Lengths::new(16384, 16384, 16384);
    let storage = MemoryStorage::new(lengths);
    storage.write_chunk(0, 0, &data).unwrap();

    let hash = storage.hash_block(0, 0, 16384).unwrap();
    assert_eq!(hash, sha256(&data));
}

#[test]
fn hash_block_partial_last_block() {
    // File is 20000 bytes: piece 0 = 20000 bytes, last block is 3616 bytes
    let data = vec![0xEFu8; 20000];
    let lengths = Lengths::new(20000, 20000, 16384);
    let storage = MemoryStorage::new(lengths);
    storage.write_chunk(0, 0, &data[..16384]).unwrap();
    storage.write_chunk(0, 16384, &data[16384..]).unwrap();

    let hash = storage.hash_block(0, 16384, 3616).unwrap();
    assert_eq!(hash, sha256(&data[16384..]));
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p ferrite-storage --test v2_storage_test`
Expected: FAIL — `verify_piece_v2` and `hash_block` methods don't exist.

**Step 3: Implement the trait methods**

In `crates/ferrite-storage/src/storage.rs`, add the `Id32` import and new methods:

```rust
use ferrite_core::{Id20, Id32};
```

Add to the `TorrentStorage` trait:

```rust
    /// Verify a piece by comparing its SHA-256 hash against `expected` (v2).
    ///
    /// Default implementation reads the full piece and hashes it.
    fn verify_piece_v2(&self, piece: u32, expected: &Id32) -> Result<bool> {
        let data = self.read_piece(piece)?;
        Ok(ferrite_core::sha256(&data) == *expected)
    }

    /// Hash a single 16 KiB block with SHA-256 for Merkle verification.
    ///
    /// Default implementation reads the chunk and hashes it.
    fn hash_block(&self, piece: u32, begin: u32, length: u32) -> Result<Id32> {
        let data = self.read_chunk(piece, begin, length)?;
        Ok(ferrite_core::sha256(&data))
    }
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p ferrite-storage --test v2_storage_test`
Expected: All 4 tests pass.

**Step 5: Commit**

```bash
git add crates/ferrite-storage/src/storage.rs crates/ferrite-storage/tests/v2_storage_test.rs
git commit -m "feat: add verify_piece_v2() and hash_block() to TorrentStorage (M34b)"
```

---

### Task 2: ChunkTracker Block Verification State

**Files:**
- Modify: `crates/ferrite-storage/src/chunk_tracker.rs`

**Step 1: Write the failing tests**

Add to the existing `#[cfg(test)] mod tests` in `chunk_tracker.rs`:

```rust
#[test]
fn v2_tracking_disabled_by_default() {
    let ct = make_tracker();
    assert!(!ct.is_block_verified(0, 0));
    assert!(!ct.all_blocks_verified(0));
}

#[test]
fn enable_v2_and_mark_blocks() {
    let mut ct = make_tracker();
    ct.enable_v2_tracking();

    assert!(!ct.is_block_verified(0, 0));
    ct.mark_block_verified(0, 0);
    assert!(ct.is_block_verified(0, 0));
    assert!(!ct.is_block_verified(0, 1));
}

#[test]
fn all_blocks_verified_complete() {
    let mut ct = make_tracker();
    ct.enable_v2_tracking();

    // Piece 0 has 4 chunks (from make_tracker: 50000 byte pieces, 16384 chunks)
    for i in 0..4 {
        ct.mark_block_verified(0, i);
    }
    assert!(ct.all_blocks_verified(0));
}

#[test]
fn all_blocks_verified_incomplete() {
    let mut ct = make_tracker();
    ct.enable_v2_tracking();

    ct.mark_block_verified(0, 0);
    ct.mark_block_verified(0, 2);
    assert!(!ct.all_blocks_verified(0));
}

#[test]
fn mark_failed_clears_v2_state() {
    let mut ct = make_tracker();
    ct.enable_v2_tracking();

    ct.mark_block_verified(0, 0);
    ct.mark_block_verified(0, 1);
    ct.mark_failed(0);
    assert!(!ct.is_block_verified(0, 0));
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p ferrite-storage -- chunk_tracker::tests::v2`
Expected: FAIL — methods don't exist.

**Step 3: Implement block verification tracking**

Add new field to `ChunkTracker`:

```rust
pub struct ChunkTracker {
    have: Bitfield,
    in_progress: HashMap<u32, Bitfield>,
    lengths: Lengths,
    /// Per-block Merkle verification state (v2 only). None for v1 torrents.
    block_verified: Option<HashMap<u32, Bitfield>>,
}
```

Initialize `block_verified: None` in both `new()` and `from_bitfield()`.

Add new methods:

```rust
    /// Enable v2 per-block Merkle verification tracking.
    pub fn enable_v2_tracking(&mut self) {
        self.block_verified = Some(HashMap::new());
    }

    /// Whether v2 block tracking is enabled.
    pub fn has_v2_tracking(&self) -> bool {
        self.block_verified.is_some()
    }

    /// Mark a specific block as Merkle-verified (v2).
    pub fn mark_block_verified(&mut self, piece: u32, block_index: u32) {
        if let Some(ref mut bv) = self.block_verified {
            let num_chunks = self.lengths.chunks_in_piece(piece);
            let bf = bv.entry(piece).or_insert_with(|| Bitfield::new(num_chunks));
            bf.set(block_index);
        }
    }

    /// Check if a specific block is Merkle-verified (v2).
    pub fn is_block_verified(&self, piece: u32, block_index: u32) -> bool {
        self.block_verified
            .as_ref()
            .and_then(|bv| bv.get(&piece))
            .is_some_and(|bf| bf.get(block_index))
    }

    /// Check if all blocks in a piece are Merkle-verified (v2).
    pub fn all_blocks_verified(&self, piece: u32) -> bool {
        let Some(ref bv) = self.block_verified else {
            return false;
        };
        let num_chunks = self.lengths.chunks_in_piece(piece);
        bv.get(&piece)
            .is_some_and(|bf| bf.count_ones() == num_chunks)
    }
```

Modify `mark_failed()` to also clear block verification state:

```rust
    pub fn mark_failed(&mut self, piece: u32) {
        self.in_progress.remove(&piece);
        if let Some(ref mut bv) = self.block_verified {
            bv.remove(&piece);
        }
    }
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p ferrite-storage -- chunk_tracker`
Expected: All existing + 5 new tests pass.

**Step 5: Commit**

```bash
git add crates/ferrite-storage/src/chunk_tracker.rs
git commit -m "feat: add v2 per-block Merkle verification to ChunkTracker (M34b)"
```

---

### Task 3: Disk I/O — HashV2 and BlockHash Jobs

**Files:**
- Modify: `crates/ferrite-session/src/disk.rs`

**Step 1: Write the failing tests**

Add to the existing test module in `crates/ferrite-session/src/disk.rs` (or create `crates/ferrite-session/tests/disk_v2_test.rs` if tests are external):

Check where disk tests live first. If inline, add there. Key tests:

```rust
#[tokio::test]
async fn verify_piece_v2_via_disk_handle() {
    let (manager, _join) = DiskManagerHandle::new(DiskConfig::default());
    let data = vec![0xABu8; 16384];
    let expected = ferrite_core::sha256(&data);
    let storage = Arc::new(MemoryStorage::new(Lengths::new(16384, 16384, 16384)));
    storage.write_chunk(0, 0, &data).unwrap();

    let disk = manager.register_torrent(Id20::ZERO, storage).await;
    let result = disk.verify_piece_v2(0, expected, DiskJobFlags::empty()).await;
    assert!(result.unwrap());
    manager.shutdown().await;
}

#[tokio::test]
async fn hash_block_via_disk_handle() {
    let (manager, _join) = DiskManagerHandle::new(DiskConfig::default());
    let data = vec![0xCDu8; 16384];
    let storage = Arc::new(MemoryStorage::new(Lengths::new(16384, 16384, 16384)));
    storage.write_chunk(0, 0, &data).unwrap();

    let disk = manager.register_torrent(Id20::ZERO, storage).await;
    let hash = disk.hash_block(0, 0, 16384, DiskJobFlags::empty()).await;
    assert_eq!(hash.unwrap(), ferrite_core::sha256(&data));
    manager.shutdown().await;
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p ferrite-session -- disk_v2` (or the appropriate test filter)
Expected: FAIL — `verify_piece_v2` and `hash_block` don't exist on `DiskHandle`.

**Step 3: Implement new DiskJob variants**

Add to the `DiskJob` enum:

```rust
    HashV2 {
        info_hash: Id20,
        piece: u32,
        expected: Id32,
        #[allow(dead_code)]
        flags: DiskJobFlags,
        reply: oneshot::Sender<ferrite_storage::Result<bool>>,
    },
    BlockHash {
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
        #[allow(dead_code)]
        flags: DiskJobFlags,
        reply: oneshot::Sender<ferrite_storage::Result<Id32>>,
    },
```

Add the import at the top of `disk.rs`:

```rust
use ferrite_core::{Id20, Id32};
```

(Replace the existing `use ferrite_core::Id20;` line.)

Add methods to `DiskHandle`:

```rust
    /// Verify a piece hash against an expected SHA-256 value (v2).
    pub async fn verify_piece_v2(
        &self,
        piece: u32,
        expected: Id32,
        flags: DiskJobFlags,
    ) -> ferrite_storage::Result<bool> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .tx
            .send(DiskJob::HashV2 {
                info_hash: self.info_hash,
                piece,
                expected,
                flags,
                reply: tx,
            })
            .await;
        rx.await.unwrap_or(Err(ferrite_storage::Error::Io(
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "disk actor gone"),
        )))
    }

    /// Hash a single block with SHA-256 for Merkle verification (v2).
    pub async fn hash_block(
        &self,
        piece: u32,
        begin: u32,
        length: u32,
        flags: DiskJobFlags,
    ) -> ferrite_storage::Result<Id32> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .tx
            .send(DiskJob::BlockHash {
                info_hash: self.info_hash,
                piece,
                begin,
                length,
                flags,
                reply: tx,
            })
            .await;
        rx.await.unwrap_or(Err(ferrite_storage::Error::Io(
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "disk actor gone"),
        )))
    }
```

Add handling to `DiskActor::process_job()`:

```rust
            DiskJob::HashV2 {
                info_hash,
                piece,
                expected,
                reply,
                ..
            } => {
                let result = self.handle_hash_v2(info_hash, piece, expected).await;
                let _ = reply.send(result);
            }
            DiskJob::BlockHash {
                info_hash,
                piece,
                begin,
                length,
                reply,
                ..
            } => {
                let result = self.handle_block_hash(info_hash, piece, begin, length).await;
                let _ = reply.send(result);
            }
```

Add the handler methods to `DiskActor`:

```rust
    async fn handle_hash_v2(
        &mut self,
        info_hash: Id20,
        piece: u32,
        expected: Id32,
    ) -> ferrite_storage::Result<bool> {
        self.flush_piece(info_hash, piece).await.ok();
        let storage = self.get_storage(info_hash)?;
        let permit = self.semaphore.clone().acquire_owned().await.unwrap();
        let result =
            tokio::task::spawn_blocking(move || storage.verify_piece_v2(piece, &expected))
                .await
                .unwrap();
        drop(permit);
        result
    }

    async fn handle_block_hash(
        &mut self,
        info_hash: Id20,
        piece: u32,
        begin: u32,
        length: u32,
    ) -> ferrite_storage::Result<Id32> {
        self.flush_piece(info_hash, piece).await.ok();
        let storage = self.get_storage(info_hash)?;
        let permit = self.semaphore.clone().acquire_owned().await.unwrap();
        let result =
            tokio::task::spawn_blocking(move || storage.hash_block(piece, begin, length))
                .await
                .unwrap();
        drop(permit);
        result
    }
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p ferrite-session -- disk_v2`
Expected: Both tests pass.

**Step 5: Run full workspace test + clippy**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: All tests pass, zero clippy warnings.

**Step 6: Commit**

```bash
git add crates/ferrite-session/src/disk.rs
git commit -m "feat: add HashV2 and BlockHash disk I/O jobs (M34b)"
```

---

### Task 4: Version Bump + Documentation

**Files:**
- Modify: `Cargo.toml` (workspace version → `0.39.0`)
- Modify: `README.md`
- Modify: `CHANGELOG.md`
- Modify: `CLAUDE.md`

**Step 1: Bump version**

In root `Cargo.toml`, change workspace version from `"0.38.0"` to `"0.39.0"`.

**Step 2: Run full test suite**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: All tests pass, zero warnings.

**Step 3: Update documentation**

README.md:
- Update test counts for ferrite-storage and ferrite-session
- Update total test count
- Update roadmap: M34b status

CHANGELOG.md — add v0.39.0 entry:

```markdown
## v0.39.0 — M34b: BEP 52 Storage v2 + Disk I/O

### Added
- `TorrentStorage::verify_piece_v2()` — SHA-256 piece verification for v2 torrents
- `TorrentStorage::hash_block()` — per-block SHA-256 hashing for Merkle verification
- `ChunkTracker` v2 per-block verification tracking (`enable_v2_tracking()`, `mark_block_verified()`, `all_blocks_verified()`)
- `DiskJob::HashV2` and `DiskJob::BlockHash` disk I/O variants
- `DiskHandle::verify_piece_v2()` and `DiskHandle::hash_block()` async methods

### Notes
- All changes are additive — v1 torrents unaffected
- Default trait implementations work with all existing storage backends (MemoryStorage, FilesystemStorage, MmapStorage)
```

CLAUDE.md:
- Add v2 storage methods to Key Types & Patterns Reference section

**Step 4: Commit and push**

```bash
git add Cargo.toml README.md CHANGELOG.md CLAUDE.md
git commit -m "feat: version bump to 0.39.0 and docs (M34b)"
git push origin main && git push github main
```

---

## Summary

| Task | What | Tests | Files |
|------|------|-------|-------|
| 1 | TorrentStorage v2 trait methods | 4 | storage.rs, v2_storage_test.rs |
| 2 | ChunkTracker block verification | 5 | chunk_tracker.rs |
| 3 | DiskJob HashV2 + BlockHash | 2 | disk.rs |
| 4 | Version bump + docs | 0 | Cargo.toml, README, CHANGELOG, CLAUDE.md |
| **Total** | | **11** | |
