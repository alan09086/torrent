# M12: Selective File Download — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Download only specific files from a multi-file torrent via per-file priorities (`Skip`, `Low`, `Normal`, `High`).

**Architecture:** New `FilePriority` enum in ferrite-core. `PieceSelector::pick()` gains a `wanted` bitfield parameter to skip unwanted pieces. `TorrentActor` maintains `file_priorities` and a computed `wanted_pieces` bitfield. `FilesystemStorage::new()` skips creating files with `Skip` priority. Resume data round-trips `file_priority` via existing `FastResumeData.file_priority` field.

**Tech Stack:** Rust 2024 edition, serde, ferrite workspace crates

**Design doc:** `docs/plans/2026-02-26-m12-selective-download-design.md`

---

### Task 1: FilePriority Enum (ferrite-core)

**Files:**
- Create: `crates/ferrite-core/src/file_priority.rs`
- Modify: `crates/ferrite-core/src/lib.rs:1-17` — add `mod file_priority` and `pub use`

**Step 1: Write the failing test**

In `crates/ferrite-core/src/file_priority.rs`, write the full module with tests at the bottom. The test will fail because the file doesn't exist yet — but we write everything in one shot since it's a self-contained leaf type.

```rust
use serde::{Deserialize, Serialize};

/// Per-file download priority.
///
/// Matches libtorrent's priority scale. `Skip` means "do not download".
/// `PartialOrd`/`Ord` ordering: Skip < Low < Normal < High.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[derive(Serialize, Deserialize)]
#[repr(u8)]
pub enum FilePriority {
    Skip = 0,
    Low = 1,
    Normal = 4,
    High = 7,
}

impl Default for FilePriority {
    fn default() -> Self {
        Self::Normal
    }
}

impl From<u8> for FilePriority {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Skip,
            1 => Self::Low,
            7 => Self::High,
            _ => Self::Normal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_normal() {
        assert_eq!(FilePriority::default(), FilePriority::Normal);
    }

    #[test]
    fn ordering_skip_less_than_high() {
        assert!(FilePriority::Skip < FilePriority::Low);
        assert!(FilePriority::Low < FilePriority::Normal);
        assert!(FilePriority::Normal < FilePriority::High);
    }

    #[test]
    fn from_u8_known_values() {
        assert_eq!(FilePriority::from(0), FilePriority::Skip);
        assert_eq!(FilePriority::from(1), FilePriority::Low);
        assert_eq!(FilePriority::from(4), FilePriority::Normal);
        assert_eq!(FilePriority::from(7), FilePriority::High);
    }

    #[test]
    fn from_u8_unknown_defaults_to_normal() {
        assert_eq!(FilePriority::from(2), FilePriority::Normal);
        assert_eq!(FilePriority::from(255), FilePriority::Normal);
    }

    #[test]
    fn repr_u8_round_trip() {
        let p = FilePriority::High;
        let v = p as u8;
        assert_eq!(v, 7);
        assert_eq!(FilePriority::from(v), p);
    }

    #[test]
    fn serde_round_trip() {
        let p = FilePriority::Low;
        let encoded = ferrite_bencode::to_bytes(&p).unwrap();
        let decoded: FilePriority = ferrite_bencode::from_bytes(&encoded).unwrap();
        assert_eq!(p, decoded);
    }
}
```

**Step 2: Wire into ferrite-core lib.rs**

In `crates/ferrite-core/src/lib.rs`, add after `mod resume_data;` (line 9):

```rust
mod file_priority;
```

And add to the pub use section:

```rust
pub use file_priority::FilePriority;
```

**Step 3: Run tests to verify they pass**

Run: `cargo test -p ferrite-core -- file_priority`
Expected: 6 tests PASS

**Step 4: Commit**

```bash
git add crates/ferrite-core/src/file_priority.rs crates/ferrite-core/src/lib.rs
git commit -m "feat(core): add FilePriority enum for selective download (M12)"
```

---

### Task 2: PieceSelector::pick() — Add `wanted` Parameter (ferrite-session)

**Files:**
- Modify: `crates/ferrite-session/src/piece_selector.rs:64-99` — add `wanted: &Bitfield` param
- Modify: `crates/ferrite-session/src/torrent.rs:968-972` — update call site

**Step 1: Write a failing test**

Add this test at the end of the `mod tests` block in `crates/ferrite-session/src/piece_selector.rs` (before the closing `}`):

```rust
    #[test]
    fn pick_skips_unwanted() {
        let mut sel = PieceSelector::new(4);
        sel.availability[0] = 1;
        sel.availability[1] = 1;
        sel.availability[2] = 1;
        sel.availability[3] = 1;

        let mut peer_has = Bitfield::new(4);
        for i in 0..4 {
            peer_has.set(i);
        }
        let we_have = Bitfield::new(4);
        let in_flight = HashSet::new();

        // Only want pieces 2 and 3
        let mut wanted = Bitfield::new(4);
        wanted.set(2);
        wanted.set(3);

        let picked = sel.pick(&peer_has, &we_have, &in_flight, &wanted);
        assert_eq!(picked, Some(2)); // lowest-index wanted piece
    }

    #[test]
    fn pick_all_wanted_is_normal_behavior() {
        let mut sel = PieceSelector::new(4);
        sel.availability[0] = 3;
        sel.availability[1] = 1;
        sel.availability[2] = 2;
        sel.availability[3] = 1;

        let mut peer_has = Bitfield::new(4);
        for i in 0..4 {
            peer_has.set(i);
        }
        let we_have = Bitfield::new(4);
        let in_flight = HashSet::new();

        // All wanted — same as pre-M12 behavior
        let mut wanted = Bitfield::new(4);
        for i in 0..4 {
            wanted.set(i);
        }

        let picked = sel.pick(&peer_has, &we_have, &in_flight, &wanted);
        assert_eq!(picked, Some(1)); // rarest first
    }
```

**Step 2: Run to verify test fails**

Run: `cargo test -p ferrite-session -- pick_skips_unwanted`
Expected: FAIL — `pick()` doesn't accept 4th argument yet

**Step 3: Update `pick()` signature and logic**

In `crates/ferrite-session/src/piece_selector.rs`, change the `pick` method (lines 64-99) to:

```rust
    pub fn pick(
        &self,
        peer_has: &Bitfield,
        we_have: &Bitfield,
        in_flight: &HashSet<u32>,
        wanted: &Bitfield,
    ) -> Option<u32> {
        let mut best_index: Option<u32> = None;
        let mut best_avail: u32 = u32::MAX;

        for i in 0..self.num_pieces {
            // Peer must have it
            if !peer_has.get(i) {
                continue;
            }
            // We must not have it
            if we_have.get(i) {
                continue;
            }
            // Must not be in flight
            if in_flight.contains(&i) {
                continue;
            }
            // Must be wanted
            if !wanted.get(i) {
                continue;
            }
            // Must have non-zero availability
            let avail = self.availability[i as usize];
            if avail == 0 {
                continue;
            }
            // Rarest first, ties broken by lowest index
            if avail < best_avail {
                best_avail = avail;
                best_index = Some(i);
            }
        }

        best_index
    }
```

**Step 4: Fix existing tests — add all-ones `wanted` bitfield**

Every existing test that calls `sel.pick(...)` with 3 args must now pass 4 args. There are 4 existing tests that call `pick()`: `pick_rarest`, `pick_skips_have`, `pick_skips_inflight`, `pick_none_available`.

For each, add an all-ones wanted bitfield. Pattern:

```rust
// Add before the pick() call:
let mut wanted = Bitfield::new(N);  // N = num_pieces for that test
for i in 0..N {
    wanted.set(i);
}
// Change pick call to:
let picked = sel.pick(&peer_has, &we_have, &in_flight, &wanted);
```

Apply this to:
- `pick_rarest` (line ~206): N=4
- `pick_skips_have` (line ~231): N=4
- `pick_skips_inflight` (line ~254): N=4
- `pick_none_available` (line ~271, ~281, ~287): N=4 (three pick calls)

**Step 5: Fix call site in TorrentActor**

In `crates/ferrite-session/src/torrent.rs`, find the `pick()` call at line ~968:

```rust
            let picked = self.piece_selector.pick(
                &peer_bitfield,
                &we_have,
                &self.in_flight,
            );
```

Change to (using a temporary all-ones bitfield until Task 3 adds `wanted_pieces`):

```rust
            // TODO(M12-task3): replace with self.wanted_pieces
            let wanted = Bitfield::all_ones(self.num_pieces);
            let picked = self.piece_selector.pick(
                &peer_bitfield,
                &we_have,
                &self.in_flight,
                &wanted,
            );
```

**Note:** `Bitfield::all_ones` doesn't exist yet. Instead, create the bitfield manually:

```rust
            let mut wanted = Bitfield::new(self.num_pieces);
            for i in 0..self.num_pieces {
                wanted.set(i);
            }
            let picked = self.piece_selector.pick(
                &peer_bitfield,
                &we_have,
                &self.in_flight,
                &wanted,
            );
```

**Step 6: Run all tests to verify**

Run: `cargo test -p ferrite-session`
Expected: ALL tests pass (existing + 2 new)

**Step 7: Commit**

```bash
git add crates/ferrite-session/src/piece_selector.rs crates/ferrite-session/src/torrent.rs
git commit -m "feat(session): add wanted bitfield param to PieceSelector::pick() (M12)"
```

---

### Task 3: TorrentActor — file_priorities, wanted_pieces, rebuild_wanted_pieces()

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs:94-119` — add fields to TorrentActor struct
- Modify: `crates/ferrite-session/src/torrent.rs:248-293` — add fields to struct definition
- Modify: `crates/ferrite-session/src/torrent.rs:932-999` — use `self.wanted_pieces` in `request_pieces_from_peer`

**Step 1: Write failing tests**

Add these tests at the end of the `mod tests` block in `crates/ferrite-session/src/piece_selector.rs` (since `rebuild_wanted_pieces` is a pure function we can test standalone):

Actually, `rebuild_wanted_pieces` will live on TorrentActor which is private. We need a standalone helper we can test. Add a free function to `piece_selector.rs`:

```rust
/// Build a bitfield marking which pieces are wanted based on file priorities.
///
/// For each file with priority > Skip, compute its piece range via `Lengths::file_pieces()`
/// and set those bits. Shared pieces (spanning file boundaries) are wanted if **any**
/// overlapping file is non-Skip.
pub fn build_wanted_pieces(
    file_priorities: &[FilePriority],
    file_infos: &[(u64, u64)],  // (file_offset, file_length) pairs
    lengths: &Lengths,
) -> Bitfield {
    let mut wanted = Bitfield::new(lengths.num_pieces());
    let mut offset = 0u64;
    for (i, &(_, file_length)) in file_infos.iter().enumerate() {
        if file_priorities.get(i).copied().unwrap_or_default() > FilePriority::Skip {
            if let Some((first, last)) = lengths.file_pieces(offset, file_length) {
                for p in first..=last {
                    wanted.set(p);
                }
            }
        }
        offset += file_length;
    }
    wanted
}
```

Wait — `file_infos` should just be file lengths, since offsets are cumulative. Simplify:

```rust
pub fn build_wanted_pieces(
    file_priorities: &[FilePriority],
    file_lengths: &[u64],
    lengths: &Lengths,
) -> Bitfield {
    let mut wanted = Bitfield::new(lengths.num_pieces());
    let mut offset = 0u64;
    for (i, &file_len) in file_lengths.iter().enumerate() {
        if file_priorities.get(i).copied().unwrap_or_default() > FilePriority::Skip {
            if let Some((first, last)) = lengths.file_pieces(offset, file_len) {
                for p in first..=last {
                    wanted.set(p);
                }
            }
        }
        offset += file_len;
    }
    wanted
}
```

Add these tests in `piece_selector.rs` tests module:

```rust
    use ferrite_core::{FilePriority, Lengths};

    #[test]
    fn build_wanted_all_normal() {
        let priorities = vec![FilePriority::Normal; 2];
        let file_lengths = vec![100, 100];
        let lengths = Lengths::new(200, 100, 50);
        let wanted = super::build_wanted_pieces(&priorities, &file_lengths, &lengths);
        // All pieces wanted
        assert_eq!(wanted.count_ones(), 2);
        assert!(wanted.get(0));
        assert!(wanted.get(1));
    }

    #[test]
    fn build_wanted_skip_first_file() {
        let priorities = vec![FilePriority::Skip, FilePriority::Normal];
        let file_lengths = vec![100, 100];
        let lengths = Lengths::new(200, 100, 50);
        let wanted = super::build_wanted_pieces(&priorities, &file_lengths, &lengths);
        // File 0 → piece 0, File 1 → piece 1. Only piece 1 wanted.
        assert!(!wanted.get(0));
        assert!(wanted.get(1));
    }

    #[test]
    fn build_wanted_shared_boundary_piece() {
        // Two files share a piece at the boundary
        // File 0: 80 bytes (pieces 0), File 1: 80 bytes (pieces 0..1)
        // piece_length=100, total=160 → 2 pieces
        // File 0: offset=0, len=80 → pieces 0..0
        // File 1: offset=80, len=80 → pieces 0..1
        // If File 0 is Skip, File 1 is Normal: piece 0 is still wanted (shared)
        let priorities = vec![FilePriority::Skip, FilePriority::Normal];
        let file_lengths = vec![80, 80];
        let lengths = Lengths::new(160, 100, 50);
        let wanted = super::build_wanted_pieces(&priorities, &file_lengths, &lengths);
        // File 1 spans pieces 0-1, so both are wanted
        assert!(wanted.get(0));
        assert!(wanted.get(1));
    }

    #[test]
    fn build_wanted_all_skip() {
        let priorities = vec![FilePriority::Skip; 3];
        let file_lengths = vec![100, 100, 100];
        let lengths = Lengths::new(300, 100, 50);
        let wanted = super::build_wanted_pieces(&priorities, &file_lengths, &lengths);
        assert_eq!(wanted.count_ones(), 0);
    }
```

**Step 2: Add imports and the function to piece_selector.rs**

At the top of `crates/ferrite-session/src/piece_selector.rs`, add:

```rust
use ferrite_core::{FilePriority, Lengths};
```

Add the `build_wanted_pieces` function after the `impl PieceSelector` block (before `#[cfg(test)]`).

**Step 3: Run tests**

Run: `cargo test -p ferrite-session -- build_wanted`
Expected: 4 tests PASS

**Step 4: Wire into TorrentActor**

In `crates/ferrite-session/src/torrent.rs`:

a) Add to `TorrentActor` struct (after `in_flight: HashSet<u32>` at line ~262):

```rust
    file_priorities: Vec<FilePriority>,
    wanted_pieces: Bitfield,
```

Add import at the top of the file (line ~17-18 area):

```rust
use ferrite_core::{
    torrent_from_bytes, FilePriority, Id20, Lengths, Magnet, PeerId, TorrentMetaV1, DEFAULT_CHUNK_SIZE,
};
```

b) In `from_torrent()` (line ~94-119), initialize the new fields. Compute file_lengths from the meta info. Add after `let piece_selector = PieceSelector::new(num_pieces);` (line ~61):

```rust
        let file_lengths: Vec<u64> = meta.info.files().iter().map(|f| f.length).collect();
        let file_priorities = vec![FilePriority::Normal; file_lengths.len()];
        let wanted_pieces = crate::piece_selector::build_wanted_pieces(
            &file_priorities, &file_lengths, &lengths,
        );
```

In the TorrentActor struct initializer (line ~94), add after `in_flight: HashSet::new(),`:

```rust
            file_priorities,
            wanted_pieces,
```

c) In `from_magnet()` (line ~161-186), add defaults. After `in_flight: HashSet::new(),`:

```rust
            file_priorities: Vec::new(),
            wanted_pieces: Bitfield::new(0),
```

d) In `try_assemble_metadata()` (line ~908-910 area), after metadata is assembled and piece_selector is set, add:

```rust
                        let file_lengths: Vec<u64> = meta.info.files().iter().map(|f| f.length).collect();
                        self.file_priorities = vec![FilePriority::Normal; file_lengths.len()];
                        self.wanted_pieces = crate::piece_selector::build_wanted_pieces(
                            &self.file_priorities, &file_lengths, &lengths,
                        );
```

e) In `request_pieces_from_peer()` (line ~968), replace the temp all-ones bitfield with:

```rust
            let picked = self.piece_selector.pick(
                &peer_bitfield,
                &we_have,
                &self.in_flight,
                &self.wanted_pieces,
            );
```

**Step 5: Run all tests**

Run: `cargo test -p ferrite-session`
Expected: ALL pass

**Step 6: Commit**

```bash
git add crates/ferrite-session/src/piece_selector.rs crates/ferrite-session/src/torrent.rs
git commit -m "feat(session): add file_priorities and wanted_pieces to TorrentActor (M12)"
```

---

### Task 4: TorrentHandle API — set_file_priority / file_priorities

**Files:**
- Modify: `crates/ferrite-session/src/types.rs:149-158` — add new TorrentCommand variants
- Modify: `crates/ferrite-session/src/torrent.rs` — add handle methods + actor handlers

**Step 1: Add TorrentCommand variants**

In `crates/ferrite-session/src/types.rs`, add to the `TorrentCommand` enum (after `SaveResumeData` variant, around line 157):

```rust
    SetFilePriority {
        index: usize,
        priority: ferrite_core::FilePriority,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    FilePriorities {
        reply: oneshot::Sender<Vec<ferrite_core::FilePriority>>,
    },
```

**Step 2: Add TorrentHandle methods**

In `crates/ferrite-session/src/torrent.rs`, add after the `save_resume_data` method (around line 241):

```rust
    /// Set the download priority for a specific file.
    ///
    /// Setting `Skip` prevents the file from being downloaded.
    /// Changing from `Skip` to a non-Skip value will start downloading
    /// the file's pieces.
    pub async fn set_file_priority(
        &self,
        index: usize,
        priority: ferrite_core::FilePriority,
    ) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::SetFilePriority { index, priority, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get the current per-file priorities.
    pub async fn file_priorities(&self) -> crate::Result<Vec<ferrite_core::FilePriority>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::FilePriorities { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        Ok(rx.await.map_err(|_| crate::Error::Shutdown)?)
    }
```

**Step 3: Handle commands in TorrentActor::run()**

In the `run()` method's `match cmd` block (around line 324-345), add before the `Shutdown` arm:

```rust
                        Some(TorrentCommand::SetFilePriority { index, priority, reply }) => {
                            let result = self.handle_set_file_priority(index, priority);
                            let _ = reply.send(result);
                        }
                        Some(TorrentCommand::FilePriorities { reply }) => {
                            let _ = reply.send(self.file_priorities.clone());
                        }
```

**Step 4: Implement handle_set_file_priority on TorrentActor**

Add this method to the `impl TorrentActor` block (after `build_resume_data`):

```rust
    fn handle_set_file_priority(
        &mut self,
        index: usize,
        priority: FilePriority,
    ) -> crate::Result<()> {
        if index >= self.file_priorities.len() {
            return Err(crate::Error::InvalidFileIndex {
                index,
                count: self.file_priorities.len(),
            });
        }

        self.file_priorities[index] = priority;

        // Rebuild wanted_pieces bitfield
        if let Some(ref meta) = self.meta {
            let file_lengths: Vec<u64> = meta.info.files().iter().map(|f| f.length).collect();
            if let Some(ref lengths) = self.lengths {
                self.wanted_pieces = crate::piece_selector::build_wanted_pieces(
                    &self.file_priorities, &file_lengths, lengths,
                );
            }
        }

        Ok(())
    }
```

**Step 5: Add the `InvalidFileIndex` error variant**

In `crates/ferrite-session/src/error.rs`, add to the Error enum:

```rust
    #[error("file index {index} out of range (torrent has {count} files)")]
    InvalidFileIndex { index: usize, count: usize },
```

**Step 6: Write tests**

Add these tests to `crates/ferrite-session/src/torrent.rs` in the existing `mod tests` block:

```rust
    #[tokio::test]
    async fn set_file_priority_and_read_back() {
        // Create a multi-file torrent with known file structure
        let info_bytes = b"d5:filesld6:lengthi100e4:pathl5:a.bineed6:lengthi100e4:pathl5:b.bineee4:name4:test12:piece lengthi100e6:pieces40:AAAAAAAAAAAAAAAAAAAABBBBBBBBBBBBBBBBBBBBe";
        let mut torrent_bytes = b"d4:info".to_vec();
        torrent_bytes.extend_from_slice(info_bytes);
        torrent_bytes.push(b'e');

        let meta = ferrite_core::torrent_from_bytes(&torrent_bytes).unwrap();
        let lengths = Lengths::new(200, 100, DEFAULT_CHUNK_SIZE);
        let storage: Arc<dyn TorrentStorage> = Arc::new(MemoryStorage::new(lengths));
        let config = TorrentConfig {
            listen_port: 0,
            ..Default::default()
        };

        let handle = TorrentHandle::from_torrent(meta, storage, config, None)
            .await
            .unwrap();

        // Default priorities should all be Normal
        let prios = handle.file_priorities().await.unwrap();
        assert_eq!(prios.len(), 2);
        assert!(prios.iter().all(|p| *p == FilePriority::Normal));

        // Set file 0 to Skip
        handle
            .set_file_priority(0, FilePriority::Skip)
            .await
            .unwrap();

        let prios = handle.file_priorities().await.unwrap();
        assert_eq!(prios[0], FilePriority::Skip);
        assert_eq!(prios[1], FilePriority::Normal);

        // Invalid index should error
        let result = handle.set_file_priority(99, FilePriority::High).await;
        assert!(result.is_err());

        handle.shutdown().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
```

**Step 7: Run all tests**

Run: `cargo test -p ferrite-session`
Expected: ALL pass

**Step 8: Commit**

```bash
git add crates/ferrite-session/src/types.rs crates/ferrite-session/src/torrent.rs crates/ferrite-session/src/error.rs
git commit -m "feat(session): add set_file_priority/file_priorities API on TorrentHandle (M12)"
```

---

### Task 5: Storage — Skip Allocation for Skip-Priority Files

**Files:**
- Modify: `crates/ferrite-storage/src/filesystem.rs:30-60` — add `file_priorities` parameter

**Step 1: Write the failing test**

Add to the `mod tests` block in `crates/ferrite-storage/src/filesystem.rs`:

```rust
    #[test]
    fn skip_priority_file_not_created() {
        use ferrite_core::FilePriority;

        let dir = temp_dir("skip_alloc");
        let lengths = Lengths::new(200, 100, 50);
        let s = FilesystemStorage::new(
            &dir,
            vec![PathBuf::from("wanted.bin"), PathBuf::from("skipped.bin")],
            vec![100, 100],
            lengths,
            Some(&[FilePriority::Normal, FilePriority::Skip]),
        )
        .unwrap();

        // wanted.bin should exist
        assert!(dir.join("wanted.bin").exists());
        // skipped.bin should NOT exist
        assert!(!dir.join("skipped.bin").exists());

        // Writing to wanted.bin should still work
        let data = vec![42u8; 50];
        s.write_chunk(0, 0, &data).unwrap();
        let read = s.read_chunk(0, 0, 50).unwrap();
        assert_eq!(read, data);

        fs::remove_dir_all(&dir).unwrap();
    }
```

**Step 2: Run to verify failure**

Run: `cargo test -p ferrite-storage -- skip_priority`
Expected: FAIL — `new()` doesn't accept 5th argument

**Step 3: Update `FilesystemStorage::new()` signature**

In `crates/ferrite-storage/src/filesystem.rs`, change `new()` (lines 30-60):

```rust
    pub fn new(
        base_dir: &Path,
        file_paths: Vec<PathBuf>,
        file_lengths: Vec<u64>,
        lengths: Lengths,
        file_priorities: Option<&[ferrite_core::FilePriority]>,
    ) -> Result<Self> {
        let file_map = FileMap::new(file_lengths.clone(), lengths.clone());

        // Pre-create directories and sparse files.
        for (i, path) in file_paths.iter().enumerate() {
            // Skip file creation for Skip-priority files
            if let Some(priorities) = file_priorities {
                if priorities.get(i).copied() == Some(ferrite_core::FilePriority::Skip) {
                    continue;
                }
            }

            let full = base_dir.join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent)?;
            }
            // Create sparse file with correct length.
            let f = File::create(&full)?;
            f.set_len(file_lengths[i])?;
        }

        let files = (0..file_paths.len())
            .map(|_| Mutex::new(None))
            .collect();

        Ok(FilesystemStorage {
            base_dir: base_dir.to_owned(),
            file_paths,
            files,
            file_map,
            lengths,
        })
    }
```

**Step 4: Fix existing call sites**

All existing tests in `filesystem.rs` call `FilesystemStorage::new(dir, paths, lens, lengths)` — they need to add `None` as the 5th argument:

Search for `FilesystemStorage::new(` in all files and add `, None` for the `file_priorities` parameter:

- `crates/ferrite-storage/src/filesystem.rs` — all test calls (~9 calls)
- Any other callers in `crates/ferrite-session/src/torrent.rs` or `crates/ferrite-session/src/session.rs` — check for `FilesystemStorage::new` calls

**Step 5: Run all tests**

Run: `cargo test -p ferrite-storage`
Expected: ALL pass (existing + 1 new)

Also: `cargo test --workspace`
Expected: ALL pass

**Step 6: Commit**

```bash
git add crates/ferrite-storage/src/filesystem.rs
git commit -m "feat(storage): skip file allocation for Skip-priority files (M12)"
```

---

### Task 6: Resume Data Integration — file_priority Round-Trip

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs` — populate `rd.file_priority` in `build_resume_data()`

**Step 1: Write the failing test**

Add to `crates/ferrite-session/src/torrent.rs` tests:

```rust
    #[tokio::test]
    async fn resume_data_preserves_file_priorities() {
        let info_bytes = b"d5:filesld6:lengthi100e4:pathl5:a.bineed6:lengthi100e4:pathl5:b.bineee4:name4:test12:piece lengthi100e6:pieces40:AAAAAAAAAAAAAAAAAAAABBBBBBBBBBBBBBBBBBBBe";
        let mut torrent_bytes = b"d4:info".to_vec();
        torrent_bytes.extend_from_slice(info_bytes);
        torrent_bytes.push(b'e');

        let meta = ferrite_core::torrent_from_bytes(&torrent_bytes).unwrap();
        let lengths = Lengths::new(200, 100, DEFAULT_CHUNK_SIZE);
        let storage: Arc<dyn TorrentStorage> = Arc::new(MemoryStorage::new(lengths));
        let config = TorrentConfig {
            listen_port: 0,
            ..Default::default()
        };

        let handle = TorrentHandle::from_torrent(meta, storage, config, None)
            .await
            .unwrap();

        // Set file priorities
        handle.set_file_priority(0, FilePriority::High).await.unwrap();
        handle.set_file_priority(1, FilePriority::Skip).await.unwrap();

        // Save resume data
        let rd = handle.save_resume_data().await.unwrap();
        assert_eq!(rd.file_priority, vec![7, 0]); // High=7, Skip=0

        // Verify bencode round-trip
        let encoded = ferrite_bencode::to_bytes(&rd).unwrap();
        let decoded: ferrite_core::FastResumeData = ferrite_bencode::from_bytes(&encoded).unwrap();
        assert_eq!(decoded.file_priority, vec![7, 0]);

        handle.shutdown().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
```

**Step 2: Run to verify failure**

Run: `cargo test -p ferrite-session -- resume_data_preserves_file_priorities`
Expected: FAIL — `rd.file_priority` is empty

**Step 3: Populate file_priority in build_resume_data()**

In `crates/ferrite-session/src/torrent.rs`, in `build_resume_data()` (around line 819, before `Ok(rd)`), add:

```rust
        // Per-file priorities
        rd.file_priority = self
            .file_priorities
            .iter()
            .map(|&p| p as u8 as i64)
            .collect();
```

**Step 4: Run tests**

Run: `cargo test -p ferrite-session -- resume_data_preserves_file_priorities`
Expected: PASS

Also: `cargo test -p ferrite-session`
Expected: ALL pass

**Step 5: Commit**

```bash
git add crates/ferrite-session/src/torrent.rs
git commit -m "feat(session): persist file_priorities in resume data (M12)"
```

---

### Task 7: Facade Re-exports, Prelude, Version Bump

**Files:**
- Modify: `crates/ferrite/src/core.rs` — add `FilePriority` re-export
- Modify: `crates/ferrite/src/prelude.rs` — add `FilePriority` to prelude
- Modify: `crates/ferrite/src/lib.rs` — add facade test
- Modify: `Cargo.toml` — bump version 0.12.0 → 0.13.0
- Modify: `CHANGELOG.md` — add M12 entry
- Modify: `README.md` — update test counts and milestone status

**Step 1: Add re-export in ferrite::core**

In `crates/ferrite/src/core.rs`, add `FilePriority` to the main `pub use ferrite_core::{...}` block (line 5-24):

```rust
pub use ferrite_core::{
    // Hash types
    Id20,
    Id32,
    // Peer identity
    PeerId,
    // Magnet links (BEP 9)
    Magnet,
    // Torrent metainfo (BEP 3)
    TorrentMetaV1,
    torrent_from_bytes,
    // Piece/chunk arithmetic
    Lengths,
    DEFAULT_CHUNK_SIZE,
    // SHA1 utility
    sha1,
    // File priority (M12)
    FilePriority,
    // Error types
    Error,
    Result,
};
```

**Step 2: Add to prelude**

In `crates/ferrite/src/prelude.rs`, add:

```rust
// File priority (M12)
pub use crate::core::FilePriority;
```

**Step 3: Add facade test**

In `crates/ferrite/src/lib.rs`, add inside `mod tests`:

```rust
    #[test]
    fn file_priority_accessible_through_facade() {
        use crate::prelude::*;

        // Default is Normal
        assert_eq!(FilePriority::default(), FilePriority::Normal);

        // Ordering works
        assert!(FilePriority::Skip < FilePriority::High);

        // From<u8> conversion
        assert_eq!(FilePriority::from(0u8), FilePriority::Skip);
        assert_eq!(FilePriority::from(7u8), FilePriority::High);
    }
```

**Step 4: Run all tests**

Run: `cargo test --workspace`
Expected: ALL pass

Run: `cargo clippy --workspace -- -D warnings`
Expected: No warnings

**Step 5: Version bump**

In root `Cargo.toml`, change `version = "0.12.0"` to `version = "0.13.0"`.

**Step 6: Update CHANGELOG.md**

Add M12 entry at the top of the changelog (under the existing header).

**Step 7: Update README.md**

Update test count and milestone status for M12.

**Step 8: Run final verification**

Run: `cargo test --workspace`
Run: `cargo clippy --workspace -- -D warnings`
Expected: All green

**Step 9: Commit**

```bash
git add crates/ferrite/src/core.rs crates/ferrite/src/prelude.rs crates/ferrite/src/lib.rs Cargo.toml CHANGELOG.md README.md
git commit -m "feat: M12 selective file download — facade, version bump, docs"
```

**Step 10: Push to both remotes**

```bash
git push origin main && git push github main
```
