# M35: Hybrid v1+v2 Torrents — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Enable ferrite to parse, verify, create, and resume hybrid BitTorrent v1+v2 torrents, matching libtorrent-rasterbar's dual-verification behaviour.

**Architecture:** A hybrid .torrent has one info dict with keys from both v1 and v2. The same raw bytes are hashed with SHA-1 (v1 info hash) and SHA-256 (v2 info hash). ferrite detects hybrids at parse time, stores both metadata representations, and verifies pieces with both hash types simultaneously. Conflict (one passes, other fails) is a hard error that pauses the torrent.

**Tech Stack:** Rust edition 2024, serde, sha1/sha2, ferrite-bencode, tokio

---

## Task 1: TorrentVersion Enum and Detection Refactor

**Files:**
- Create: `crates/ferrite-core/src/torrent_version.rs`
- Modify: `crates/ferrite-core/src/lib.rs:1-40`
- Modify: `crates/ferrite-core/src/detect.rs:1-122`

**Step 1: Write tests for TorrentVersion enum and hybrid detection**

Add new test module and tests in `crates/ferrite-core/src/torrent_version.rs`:

```rust
//! Torrent protocol version indicator.

use serde::{Deserialize, Serialize};

/// Indicates which BitTorrent protocol version(s) a torrent supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TorrentVersion {
    /// BitTorrent v1 only (BEP 3). SHA-1 piece hashes.
    V1Only,
    /// BitTorrent v2 only (BEP 52). SHA-256 Merkle per-file trees.
    V2Only,
    /// Hybrid v1+v2 (BEP 52). Both hash types in a single info dict.
    Hybrid,
}

impl TorrentVersion {
    /// Whether this version includes v1 (SHA-1) hashes.
    pub fn has_v1(&self) -> bool {
        matches!(self, Self::V1Only | Self::Hybrid)
    }

    /// Whether this version includes v2 (SHA-256 Merkle) hashes.
    pub fn has_v2(&self) -> bool {
        matches!(self, Self::V2Only | Self::Hybrid)
    }

    /// Whether this is a hybrid torrent with both v1 and v2 hashes.
    pub fn is_hybrid(&self) -> bool {
        matches!(self, Self::Hybrid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_only_flags() {
        let v = TorrentVersion::V1Only;
        assert!(v.has_v1());
        assert!(!v.has_v2());
        assert!(!v.is_hybrid());
    }

    #[test]
    fn v2_only_flags() {
        let v = TorrentVersion::V2Only;
        assert!(!v.has_v1());
        assert!(v.has_v2());
        assert!(!v.is_hybrid());
    }

    #[test]
    fn hybrid_flags() {
        let v = TorrentVersion::Hybrid;
        assert!(v.has_v1());
        assert!(v.has_v2());
        assert!(v.is_hybrid());
    }
}
```

**Step 2: Run tests to verify they pass**

Run: `cargo test -p ferrite-core torrent_version -- --nocapture`
Expected: 3 tests PASS

**Step 3: Refactor detect.rs — replace is_v2 with detect_version, add Hybrid variant**

Replace the private `is_v2()` function and update `torrent_from_bytes_any()` in `crates/ferrite-core/src/detect.rs`:

```rust
//! Auto-detection of torrent format (v1, v2, or hybrid).
//!
//! Inspects the info dict for `meta version` and `pieces` to distinguish:
//! - V1: no `meta version = 2`
//! - V2: `meta version = 2` without v1 `pieces` key
//! - Hybrid: `meta version = 2` AND v1 `pieces` key present

use ferrite_bencode::BencodeValue;

use crate::error::Error;
use crate::info_hashes::InfoHashes;
use crate::metainfo::TorrentMetaV1;
use crate::metainfo_v2::TorrentMetaV2;
use crate::torrent_version::TorrentVersion;

/// A parsed torrent file — v1, v2, or hybrid.
#[derive(Debug, Clone)]
pub enum TorrentMeta {
    /// BitTorrent v1 (BEP 3).
    V1(TorrentMetaV1),
    /// BitTorrent v2 (BEP 52).
    V2(TorrentMetaV2),
    /// Hybrid v1+v2 (BEP 52). Contains both metadata representations.
    Hybrid(TorrentMetaV1, TorrentMetaV2),
}

impl TorrentMeta {
    /// Get the unified info hashes.
    pub fn info_hashes(&self) -> InfoHashes {
        match self {
            TorrentMeta::V1(t) => InfoHashes::v1_only(t.info_hash),
            TorrentMeta::V2(t) => t.info_hashes.clone(),
            TorrentMeta::Hybrid(v1, v2) => InfoHashes::hybrid(
                v1.info_hash,
                v2.info_hashes.v2.expect("v2 torrent must have v2 hash"),
            ),
        }
    }

    /// Whether this is a v1 torrent.
    pub fn is_v1(&self) -> bool {
        matches!(self, TorrentMeta::V1(_))
    }

    /// Whether this is a v2 torrent.
    pub fn is_v2(&self) -> bool {
        matches!(self, TorrentMeta::V2(_))
    }

    /// Whether this is a hybrid v1+v2 torrent.
    pub fn is_hybrid(&self) -> bool {
        matches!(self, TorrentMeta::Hybrid(_, _))
    }

    /// Get the protocol version enum.
    pub fn version(&self) -> TorrentVersion {
        match self {
            TorrentMeta::V1(_) => TorrentVersion::V1Only,
            TorrentMeta::V2(_) => TorrentVersion::V2Only,
            TorrentMeta::Hybrid(_, _) => TorrentVersion::Hybrid,
        }
    }
}

/// Detected version of a .torrent file's info dict.
enum DetectedVersion {
    V1Only,
    V2Only,
    Hybrid,
}

/// Auto-detect and parse a .torrent file as v1, v2, or hybrid.
///
/// Detection:
/// - `meta version = 2` AND `pieces` key present → Hybrid
/// - `meta version = 2` only → V2
/// - Otherwise → V1
pub fn torrent_from_bytes_any(data: &[u8]) -> Result<TorrentMeta, Error> {
    match detect_version(data)? {
        DetectedVersion::V1Only => {
            Ok(TorrentMeta::V1(crate::metainfo::torrent_from_bytes(data)?))
        }
        DetectedVersion::V2Only => {
            Ok(TorrentMeta::V2(crate::metainfo_v2::torrent_v2_from_bytes(data)?))
        }
        DetectedVersion::Hybrid => {
            let v1 = crate::metainfo::torrent_from_bytes(data)?;
            let mut v2 = crate::metainfo_v2::torrent_v2_from_bytes(data)?;
            // Override the truncated v1 hash with the REAL SHA-1 info hash.
            // In hybrid torrents, the v1 hash is SHA-1 of the raw info dict,
            // not a truncation of the SHA-256 hash.
            v2.info_hashes.v1 = Some(v1.info_hash);
            Ok(TorrentMeta::Hybrid(v1, v2))
        }
    }
}

/// Detect the version of a torrent from its raw bencode bytes.
fn detect_version(data: &[u8]) -> Result<DetectedVersion, Error> {
    let root: BencodeValue = ferrite_bencode::from_bytes(data)?;
    let root_dict = root
        .as_dict()
        .ok_or_else(|| Error::InvalidTorrent("torrent must be a dict".into()))?;
    let info = root_dict
        .get(b"info".as_ref())
        .and_then(|v| v.as_dict())
        .ok_or_else(|| Error::InvalidTorrent("missing or invalid 'info' dict".into()))?;

    let has_v2 = info
        .get(b"meta version".as_ref())
        .and_then(|v| v.as_int())
        == Some(2);

    let has_v1_pieces = info.get(b"pieces".as_ref()).is_some();

    Ok(match (has_v2, has_v1_pieces) {
        (true, true) => DetectedVersion::Hybrid,
        (true, false) => DetectedVersion::V2Only,
        _ => DetectedVersion::V1Only,
    })
}
```

**Step 4: Update detect.rs tests — add hybrid detection test**

Add to the `#[cfg(test)] mod tests` block in `detect.rs`:

```rust
#[test]
fn auto_detect_hybrid() {
    use std::collections::BTreeMap;

    // Build a hybrid torrent: has both `pieces` (v1) and `meta version = 2` + `file tree` (v2)
    let mut info_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();

    // v2 keys
    let mut ft_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
    let mut attr_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
    attr_map.insert(b"length".to_vec(), BencodeValue::Integer(16384));
    let mut file_node: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
    file_node.insert(b"".to_vec(), BencodeValue::Dict(attr_map));
    ft_map.insert(b"test.dat".to_vec(), BencodeValue::Dict(file_node));

    info_map.insert(b"file tree".to_vec(), BencodeValue::Dict(ft_map));
    info_map.insert(b"meta version".to_vec(), BencodeValue::Integer(2));

    // v1 keys
    info_map.insert(b"name".to_vec(), BencodeValue::Bytes(b"test".to_vec()));
    info_map.insert(b"piece length".to_vec(), BencodeValue::Integer(16384));
    info_map.insert(b"length".to_vec(), BencodeValue::Integer(16384));
    // 1 piece = 20 bytes of SHA-1 hash
    info_map.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0xAA; 20]));

    let mut root_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
    root_map.insert(b"info".to_vec(), BencodeValue::Dict(info_map));

    let data = ferrite_bencode::to_bytes(&BencodeValue::Dict(root_map)).unwrap();
    let meta = torrent_from_bytes_any(&data).unwrap();
    assert!(meta.is_hybrid());
    assert!(!meta.is_v1());
    assert!(!meta.is_v2());

    // Verify info_hashes has both v1 and v2
    let hashes = meta.info_hashes();
    assert!(hashes.has_v1());
    assert!(hashes.has_v2());
    assert!(hashes.is_hybrid());

    // Verify the v1 hash is the REAL SHA-1, not a truncation of SHA-256
    if let TorrentMeta::Hybrid(ref v1, ref v2) = meta {
        // v1 info hash is SHA-1 of raw info dict bytes
        assert_eq!(hashes.v1.unwrap(), v1.info_hash);
        // v2 info hash is SHA-256 of same bytes — different from v1
        assert!(hashes.v2.is_some());
        assert_ne!(
            &v1.info_hash.0[..],
            &v2.info_hashes.v2.unwrap().0[..20],
            "v1 hash should NOT be a truncation in hybrid — it's SHA-1 not SHA-256[:20]"
        );
    } else {
        panic!("expected Hybrid variant");
    }
}

#[test]
fn hybrid_version_accessor() {
    // Re-use the v1 test data (TorrentMeta::V1)
    let data = b"d4:infod6:lengthi100e4:name4:test12:piece lengthi256e6:pieces20:\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00ee";
    let meta = torrent_from_bytes_any(data).unwrap();
    assert_eq!(meta.version(), crate::torrent_version::TorrentVersion::V1Only);
}
```

**Step 5: Register module in lib.rs**

Add `mod torrent_version;` and `pub use torrent_version::TorrentVersion;` to `crates/ferrite-core/src/lib.rs`.

**Step 6: Run all tests**

Run: `cargo test -p ferrite-core -- --nocapture`
Expected: All existing tests pass + 6 new tests (3 TorrentVersion + 2 detect + 1 version accessor)

**Step 7: Run clippy**

Run: `cargo clippy -p ferrite-core -- -D warnings`
Expected: Zero warnings

**Step 8: Commit**

```bash
git add crates/ferrite-core/src/torrent_version.rs crates/ferrite-core/src/detect.rs crates/ferrite-core/src/lib.rs
git commit -m "feat: TorrentVersion enum and hybrid torrent detection (M35)"
```

---

## Task 2: TorrentActor v2→TorrentVersion Migration

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs` (lines with `is_v2`)

This task replaces `is_v2: bool` with `version: TorrentVersion` in TorrentActor and updates all references.

**Step 1: Replace is_v2 field**

In `crates/ferrite-session/src/torrent.rs`:

1. Change the struct field at line ~693:
   - Old: `is_v2: bool,`
   - New: `version: ferrite_core::TorrentVersion,`

2. Update both constructor initializations (line ~266 and ~421):
   - Old: `is_v2: false,`
   - New: `version: ferrite_core::TorrentVersion::V1Only,`

3. Update the verification dispatch at line ~1739:
   - Old:
     ```rust
     if self.is_v2 {
         self.verify_and_mark_piece_v2(index).await;
     } else {
         self.verify_and_mark_piece_v1(index).await;
     }
     ```
   - New:
     ```rust
     match self.version {
         ferrite_core::TorrentVersion::V1Only => {
             self.verify_and_mark_piece_v1(index).await;
         }
         ferrite_core::TorrentVersion::V2Only => {
             self.verify_and_mark_piece_v2(index).await;
         }
         ferrite_core::TorrentVersion::Hybrid => {
             self.verify_and_mark_piece_hybrid(index).await;
         }
     }
     ```

4. Add a stub `verify_and_mark_piece_hybrid` method after `verify_and_mark_piece_v2`:
   ```rust
   /// Dual SHA-1 + SHA-256 verification for hybrid torrents.
   async fn verify_and_mark_piece_hybrid(&mut self, index: u32) {
       // TODO: M35 Task 4 — full hybrid verification
       // For now, fall through to v1 verification
       self.verify_and_mark_piece_v1(index).await;
   }
   ```

**Step 2: Run tests**

Run: `cargo test --workspace`
Expected: All 883 tests pass (no behaviour change — only v1 torrents exist in tests)

**Step 3: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: Zero warnings

**Step 4: Commit**

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "refactor: replace is_v2 bool with TorrentVersion enum (M35)"
```

---

## Task 3: AlertKind::InconsistentHashes and on_inconsistent_hashes

**Files:**
- Modify: `crates/ferrite-session/src/alert.rs:53-208`
- Modify: `crates/ferrite-session/src/torrent.rs`

**Step 1: Add InconsistentHashes alert variant**

In `crates/ferrite-session/src/alert.rs`, add after the `TorrentError` variant (line ~103):

```rust
    // ── Hybrid hash conflict (ERROR) ──
    /// v1 and v2 hashes disagree — the .torrent file is inconsistent.
    InconsistentHashes { info_hash: Id20, piece: u32 },
```

In the `category()` match (after `TorrentError`):

```rust
            InconsistentHashes { .. } => AlertCategory::ERROR,
```

**Step 2: Add on_inconsistent_hashes method to TorrentActor**

In `crates/ferrite-session/src/torrent.rs`, add after `on_piece_hash_failed()`:

```rust
    /// Handle v1/v2 hash inconsistency — the torrent data itself is corrupt.
    ///
    /// Matching libtorrent: destroy hash picker, pause the torrent, post error alert.
    async fn on_inconsistent_hashes(&mut self, piece: u32) {
        let info_hash = self.info_hash;
        tracing::error!(
            piece,
            info_hash = %info_hash,
            "v1 and v2 hashes are inconsistent — torrent data is corrupt"
        );

        // Destroy hash picker (Merkle tree state is untrustworthy)
        self.hash_picker = None;

        // Post alert
        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::InconsistentHashes { info_hash, piece },
        );

        // Post error alert for broader consumers
        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::TorrentError {
                info_hash,
                message: format!(
                    "v1 and v2 hashes do not describe the same data (piece {piece})"
                ),
            },
        );

        // Pause the torrent
        self.paused = true;
    }
```

**Step 3: Run tests**

Run: `cargo test --workspace`
Expected: All tests pass

**Step 4: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: Zero warnings

**Step 5: Commit**

```bash
git add crates/ferrite-session/src/alert.rs crates/ferrite-session/src/torrent.rs
git commit -m "feat: add InconsistentHashes alert and handler (M35)"
```

---

## Task 4: Hybrid Verification Logic

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs` (the stub from Task 2)

**Step 1: Implement verify_and_mark_piece_hybrid**

Replace the stub `verify_and_mark_piece_hybrid` with the full dual-verification implementation:

```rust
    /// Dual SHA-1 + SHA-256 verification for hybrid torrents.
    ///
    /// Runs both v1 (whole-piece SHA-1) and v2 (per-block SHA-256 Merkle) verification.
    /// Decision matrix (matching libtorrent tribool logic):
    /// - Both pass or one passes + other N/A → on_piece_verified()
    /// - Both fail → on_piece_hash_failed()
    /// - One passes + other fails → on_inconsistent_hashes() (fatal)
    /// - One fails + other N/A → on_piece_hash_failed()
    async fn verify_and_mark_piece_hybrid(&mut self, index: u32) {
        // --- v1 verification (SHA-1 of entire piece) ---
        let v1_result = {
            let expected_hash = self
                .meta
                .as_ref()
                .and_then(|m| m.info.piece_hash(index as usize));

            if let (Some(disk), Some(expected)) = (&self.disk, expected_hash) {
                match disk
                    .verify_piece(index, expected, DiskJobFlags::empty())
                    .await
                {
                    Ok(true) => HashResult::Passed,
                    Ok(false) => HashResult::Failed,
                    Err(_) => HashResult::Failed,
                }
            } else {
                HashResult::NotApplicable
            }
        };

        // --- v2 verification (per-block SHA-256 Merkle) ---
        let v2_result = self.run_v2_block_verification(index).await;

        // --- Decision matrix ---
        match (v1_result, v2_result) {
            // Both pass, or one passes with other not applicable
            (HashResult::Passed, HashResult::Passed)
            | (HashResult::Passed, HashResult::NotApplicable)
            | (HashResult::NotApplicable, HashResult::Passed) => {
                self.on_piece_verified(index).await;
            }
            // Both fail
            (HashResult::Failed, HashResult::Failed) => {
                self.on_piece_hash_failed(index).await;
            }
            // Inconsistency — one passes, other fails (fatal)
            (HashResult::Passed, HashResult::Failed)
            | (HashResult::Failed, HashResult::Passed) => {
                self.on_inconsistent_hashes(index).await;
            }
            // One fails, other not applicable
            (HashResult::Failed, HashResult::NotApplicable)
            | (HashResult::NotApplicable, HashResult::Failed) => {
                self.on_piece_hash_failed(index).await;
            }
            // Both not applicable — shouldn't happen for hybrid (at minimum v1 hashes exist)
            (HashResult::NotApplicable, HashResult::NotApplicable) => {
                tracing::warn!(index, "hybrid verification: both hash types unavailable");
            }
        }
    }

    /// Run v2 block-level SHA-256 verification, returning a HashResult.
    ///
    /// Extracted from verify_and_mark_piece_v2 to return a result instead of
    /// directly calling on_piece_verified/on_piece_hash_failed.
    async fn run_v2_block_verification(&mut self, index: u32) -> HashResult {
        let disk = match &self.disk {
            Some(d) => d.clone(),
            None => return HashResult::NotApplicable,
        };

        let lengths = match &self.lengths {
            Some(l) => l.clone(),
            None => return HashResult::NotApplicable,
        };

        if let Err(e) = disk.flush_piece(index).await {
            tracing::warn!(index, "failed to flush piece for v2 verification: {e}");
            return HashResult::Failed;
        }

        let num_chunks = lengths.chunks_in_piece(index);
        let blocks_per_piece = (lengths.piece_length() as u32) / DEFAULT_CHUNK_SIZE;

        for chunk_idx in 0..num_chunks {
            let (begin, length) = match lengths.chunk_info(index, chunk_idx) {
                Some(info) => info,
                None => continue,
            };

            let block_hash = match disk
                .hash_block(index, begin, length, DiskJobFlags::empty())
                .await
            {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!(index, chunk_idx, "failed to hash block: {e}");
                    return HashResult::Failed;
                }
            };

            if let Some(ref mut picker) = self.hash_picker {
                let file_index = 0; // single-file assumption — multi-file mapping in future
                let global_block = index * blocks_per_piece + chunk_idx;
                match picker.set_block_hash(file_index, global_block, block_hash) {
                    ferrite_core::SetBlockResult::Ok => {
                        if let Some(ref mut ct) = self.chunk_tracker {
                            ct.mark_block_verified(index, chunk_idx);
                        }
                    }
                    ferrite_core::SetBlockResult::Unknown => {
                        return HashResult::NotApplicable;
                    }
                    ferrite_core::SetBlockResult::HashFailed => {
                        return HashResult::Failed;
                    }
                }
            } else {
                return HashResult::NotApplicable;
            }
        }

        if self
            .chunk_tracker
            .as_ref()
            .is_some_and(|ct| ct.all_blocks_verified(index))
        {
            HashResult::Passed
        } else {
            HashResult::NotApplicable
        }
    }
```

**Step 2: Add HashResult enum**

Add near the top of `torrent.rs` (after the imports, before the struct):

```rust
/// Result of a single hash verification attempt (v1 SHA-1 or v2 Merkle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HashResult {
    /// Hash matched — piece data is correct.
    Passed,
    /// Hash mismatched — piece data is corrupt.
    Failed,
    /// Hash type not available (e.g., Merkle hashes not yet received).
    NotApplicable,
}
```

**Step 3: Refactor verify_and_mark_piece_v2 to use run_v2_block_verification**

Replace the body of `verify_and_mark_piece_v2` to delegate to the shared method:

```rust
    async fn verify_and_mark_piece_v2(&mut self, index: u32) {
        match self.run_v2_block_verification(index).await {
            HashResult::Passed => self.on_piece_verified(index).await,
            HashResult::Failed => self.on_piece_hash_failed(index).await,
            HashResult::NotApplicable => {
                // Blocks stored, will resolve when piece-layer hashes arrive
            }
        }
    }
```

**Step 4: Run tests**

Run: `cargo test --workspace`
Expected: All 883 tests pass

**Step 5: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: Zero warnings

**Step 6: Commit**

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "feat: hybrid dual-verification with HashResult tribool logic (M35)"
```

---

## Task 5: Hybrid Torrent Creation

**Files:**
- Modify: `crates/ferrite-core/src/create.rs:1-1038`
- Modify: `crates/ferrite-core/src/lib.rs` (re-export)

**Step 1: Write failing test for hybrid creation**

Add to `crates/ferrite-core/src/create.rs` tests module:

```rust
    #[test]
    fn hybrid_torrent_round_trip() {
        let f = make_test_file();
        let result = CreateTorrent::new()
            .set_version(crate::TorrentVersion::Hybrid)
            .add_file(f.path())
            .set_piece_size(16384)
            .set_creation_date(1000000)
            .generate()
            .unwrap();

        // Result should have both hashes
        assert!(result.info_hashes.has_v1());
        assert!(result.info_hashes.has_v2());
        assert!(result.info_hashes.is_hybrid());

        // Re-parse as hybrid
        let parsed = crate::torrent_from_bytes_any(&result.bytes).unwrap();
        assert!(parsed.is_hybrid());

        // Info hashes should match
        let parsed_hashes = parsed.info_hashes();
        assert_eq!(parsed_hashes.v1, result.info_hashes.v1);
        assert_eq!(parsed_hashes.v2, result.info_hashes.v2);
    }

    #[test]
    fn v2_only_torrent_round_trip() {
        let f = make_test_file();
        let result = CreateTorrent::new()
            .set_version(crate::TorrentVersion::V2Only)
            .add_file(f.path())
            .set_piece_size(16384)
            .set_creation_date(1000000)
            .generate()
            .unwrap();

        assert!(!result.info_hashes.has_v1());
        assert!(result.info_hashes.has_v2());

        let parsed = crate::torrent_from_bytes_any(&result.bytes).unwrap();
        assert!(parsed.is_v2());
    }
```

**Step 2: Run tests — verify they fail**

Run: `cargo test -p ferrite-core hybrid_torrent_round_trip -- --nocapture`
Expected: FAIL — `set_version` method doesn't exist

**Step 3: Implement hybrid/v2 torrent creation**

Modifications to `crates/ferrite-core/src/create.rs`:

1. Add `version` field to `CreateTorrent` struct:
   ```rust
   version: crate::TorrentVersion,
   ```
   Initialize to `crate::TorrentVersion::V1Only` in `new()`.

2. Add `set_version` method:
   ```rust
   pub fn set_version(mut self, version: crate::TorrentVersion) -> Self {
       self.version = version;
       self
   }
   ```

3. Add imports at top of create.rs:
   ```rust
   use crate::hash::Id32;
   use crate::merkle::MerkleTree;
   use crate::detect::TorrentMeta;
   use crate::info_hashes::InfoHashes;
   use crate::metainfo_v2::{InfoDictV2, TorrentMetaV2};
   use crate::file_tree::{FileTreeNode, V2FileAttr};
   ```

4. Update `CreateTorrentResult`:
   ```rust
   pub struct CreateTorrentResult {
       /// Parsed torrent metadata (v1, v2, or hybrid).
       pub meta: TorrentMeta,
       /// Raw `.torrent` file bytes.
       pub bytes: Vec<u8>,
       /// Convenience: unified info hashes.
       pub info_hashes: InfoHashes,
   }
   ```

5. In `generate_with_progress()`, after the piece hashing loop, branch on `self.version`:

   For `V1Only` (existing path): no changes, wrap in `TorrentMeta::V1(meta)`.

   For `V2Only` and `Hybrid`:
   - During the piece hashing loop, also compute SHA-256 of each 16 KiB block:
     ```rust
     let compute_v2 = self.version.has_v2();
     let block_size = 16384usize;
     // Per-file: Vec<Vec<Id32>> — block hashes for each file
     let mut v2_file_block_hashes: Vec<Vec<Id32>> = Vec::new();
     ```
   - Inside the existing piece data loop, after reading each block-sized chunk,
     compute `crate::sha256(&chunk)` and push to file block hashes.
   - After the piece loop, build Merkle trees from block hashes:
     ```rust
     let mut piece_layers = std::collections::BTreeMap::new();
     for file_hashes in &v2_file_block_hashes {
         let tree = MerkleTree::from_leaves(file_hashes);
         let root = tree.root();
         // Extract piece-layer hashes
         if file_hashes.len() > 1 {
             let piece_hashes = tree.piece_layer(piece_size as u32 / block_size as u32);
             let layer_bytes: Vec<u8> = piece_hashes.iter()
                 .flat_map(|h| h.0.iter().copied())
                 .collect();
             piece_layers.insert(root, layer_bytes);
         }
     }
     ```
   - Build the v2 file tree and info dict:
     ```rust
     let v2_info = InfoDictV2 { name, piece_length, meta_version: 2, file_tree };
     ```
   - Serialize the combined .torrent (one info dict with both v1 and v2 keys).
   - Compute `info_hash_v1 = sha1(info_bytes)` and `info_hash_v2 = sha256(info_bytes)`.

   **Note:** The serialization of a hybrid info dict requires manual construction as a
   `BencodeValue::Dict` because `InfoDict` (v1) and `InfoDictV2` (v2) are separate structs.
   Build a `BTreeMap<Vec<u8>, BencodeValue>` containing all keys from both.

6. For v2/hybrid, enforce pad file alignment on multi-file torrents:
   ```rust
   if self.version.has_v2() && !is_single_file && self.pad_file_limit.is_none() {
       // v2 requires file alignment — implicitly enable padding
       self.pad_file_limit = Some(0);
   }
   ```

**Step 4: Run tests**

Run: `cargo test -p ferrite-core -- --nocapture`
Expected: All tests pass including 2 new creation tests

**Step 5: Run clippy**

Run: `cargo clippy -p ferrite-core -- -D warnings`
Expected: Zero warnings

**Step 6: Commit**

```bash
git add crates/ferrite-core/src/create.rs crates/ferrite-core/src/lib.rs
git commit -m "feat: hybrid and v2 torrent creation with dual hashing (M35)"
```

---

## Task 6: Resume Data Hybrid Support

**Files:**
- Modify: `crates/ferrite-core/src/resume_data.rs`

**Step 1: Write test for hybrid resume data**

Add to `crates/ferrite-core/src/resume_data.rs` tests:

```rust
    #[test]
    fn resume_data_hybrid_round_trip() {
        let mut resume = FastResumeData::new(
            vec![0xAA; 20],
            "hybrid-torrent".into(),
            "/downloads".into(),
        );
        // Set both v1 and v2 hashes (hybrid)
        resume.info_hash2 = Some(vec![0xBB; 32]);
        resume.trees.insert(
            hex::encode([0xCC; 32]),
            vec![0xDD; 64],
        );

        let encoded = ferrite_bencode::to_bytes(&resume).unwrap();
        let decoded: FastResumeData = ferrite_bencode::from_bytes(&encoded).unwrap();

        // Both hashes present
        assert_eq!(decoded.info_hash, vec![0xAA; 20]);
        assert_eq!(decoded.info_hash2, Some(vec![0xBB; 32]));
        assert_eq!(decoded.trees.len(), 1);

        // Verify this is recognizable as hybrid (both hashes present)
        assert!(decoded.info_hash2.is_some(), "hybrid should have v2 hash");
    }
```

**Step 2: Run test**

Run: `cargo test -p ferrite-core resume_data_hybrid -- --nocapture`
Expected: PASS (FastResumeData already has `info_hash2` and `trees` fields from M34)

This test validates that the existing resume data structure works for hybrid torrents
without changes. The hybrid nature is inferred from having both `info_hash` (always present)
and `info_hash2` (present for v2/hybrid).

**Step 3: Commit**

```bash
git add crates/ferrite-core/src/resume_data.rs
git commit -m "test: validate resume data works for hybrid torrents (M35)"
```

---

## Task 7: Facade Re-exports and Version Bump

**Files:**
- Modify: `crates/ferrite/src/core.rs` (re-export TorrentVersion)
- Modify: `crates/ferrite/src/prelude.rs` (add TorrentVersion to prelude)
- Modify: `Cargo.toml` (version bump 0.40.0 → 0.41.0)

**Step 1: Add TorrentVersion to facade re-exports**

In `crates/ferrite/src/core.rs`, add:
```rust
pub use ferrite_core::TorrentVersion;
```

In `crates/ferrite/src/prelude.rs`, add `TorrentVersion` to the prelude imports.

**Step 2: Bump version**

In root `Cargo.toml`, change:
```toml
version = "0.41.0"
```

**Step 3: Run full test suite**

Run: `cargo test --workspace`
Expected: All tests pass (~893)

**Step 4: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: Zero warnings

**Step 5: Commit**

```bash
git add Cargo.toml crates/ferrite/src/core.rs crates/ferrite/src/prelude.rs
git commit -m "feat: re-export TorrentVersion, bump to v0.41.0 (M35)"
```

---

## Task 8: Documentation Updates

**Files:**
- Modify: `README.md`
- Modify: `CHANGELOG.md`
- Modify: `CLAUDE.md`

**Step 1: Update README.md**

- Update test count in the crates table (ferrite-core: +6, ferrite-session: +0 direct but adjust)
- Update total test count
- Update Phase 6 status: `M33-M35 done`

**Step 2: Update CHANGELOG.md**

Add v0.41.0 section:

```markdown
## v0.41.0 — M35: Hybrid v1+v2 Torrents

### Added
- `TorrentVersion` enum (`V1Only`, `V2Only`, `Hybrid`) in `ferrite-core`
- `TorrentMeta::Hybrid(v1, v2)` variant for dual-format torrent parsing
- Hybrid torrent detection: info dicts with both `pieces` and `meta version = 2`
- Dual verification: SHA-1 + SHA-256 Merkle for every piece in hybrid torrents
- `HashResult` tribool: `Passed`/`Failed`/`NotApplicable` (matching libtorrent)
- `AlertKind::InconsistentHashes` — fatal alert when v1 and v2 disagree
- `on_inconsistent_hashes()` — destroys hash picker, pauses torrent
- `CreateTorrent::set_version()` — create v1, v2, or hybrid .torrent files
- Hybrid torrent creation with dual SHA-1 + SHA-256 hashing in single pass
- v2-only torrent creation
- Resume data validation for hybrid torrents (both info hashes)

### Changed
- `TorrentActor.is_v2: bool` → `TorrentActor.version: TorrentVersion`
- `CreateTorrentResult.meta` is now `TorrentMeta` (was `TorrentMetaV1`)
- `CreateTorrentResult` gains `info_hashes: InfoHashes` convenience field
- `verify_and_mark_piece_v2` refactored to use shared `run_v2_block_verification`
```

**Step 3: Update CLAUDE.md**

Add `TorrentVersion` to the Key Types section.

**Step 4: Commit**

```bash
git add README.md CHANGELOG.md CLAUDE.md
git commit -m "docs: update README, changelog, CLAUDE.md for M35 (M35)"
```

---

## Task 9: Memory Updates and Push

**Files:**
- Modify: `/home/alan/.claude/projects/-mnt-TempNVME-projects/memory/MEMORY.md`
- Modify: `/home/alan/.claude/projects/-mnt-TempNVME-projects/memory/ferrite.md`

**Step 1: Update MEMORY.md**

Update the ferrite status line:
```
- **Status**: M35 complete (v0.41.0, ~893 tests). Next: M36 (BEP 53)
```

**Step 2: Update ferrite.md**

Add M35 entry with key details: TorrentVersion enum, hybrid detection, dual verification, hybrid creation.

**Step 3: Push to both remotes**

```bash
git push origin main && git push github main
```

---

## Verification Checklist

After all tasks:

1. `cargo test --workspace` — 883 + ~10 new = ~893 tests passing
2. `cargo clippy --workspace -- -D warnings` — zero warnings
3. Hybrid .torrent parsed → `TorrentMeta::Hybrid(v1, v2)` with correct hashes
4. v1 info hash is real SHA-1 (not truncated SHA-256)
5. Hybrid verification runs both SHA-1 and SHA-256, conflicts are fatal
6. `CreateTorrent::set_version(Hybrid)` → round-trips correctly
7. `TorrentVersion` enum replaces all `is_v2: bool` usage
8. Resume data works for hybrid (both hashes)
9. v1-only and v2-only torrents completely unaffected
10. Both remotes updated
