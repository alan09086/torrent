# M12: Selective File Download — Design

**Date:** 2026-02-26
**Milestone:** M12 (Phase 1: Essential Desktop Client Features)
**Goal:** Download only specific files from a multi-file torrent via per-file priorities

## Architecture

Three crates touched:

| Crate | Changes |
|-------|---------|
| ferrite-core | New `FilePriority` enum |
| ferrite-storage | Skip allocation for `Skip`-priority files |
| ferrite-session | `wanted` param on `PieceSelector::pick()`, `file_priorities` on TorrentActor, `set_file_priority`/`file_priorities` API on TorrentHandle |
| ferrite | Facade re-exports + prelude |

## 1. FilePriority Enum (`ferrite-core`)

```rust
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
    fn default() -> Self { Self::Normal }
}

impl From<u8> for FilePriority {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Skip,
            1 => Self::Low,
            4 => Self::Normal,
            7 => Self::High,
            _ => Self::Normal, // unknown → Normal
        }
    }
}
```

Matches libtorrent's priority scale. `PartialOrd`/`Ord` means Higher priority > Lower
(Skip < Low < Normal < High) which is useful for sorting.

## 2. PieceSelector::pick() — Add `wanted` Parameter

Current:
```rust
fn pick(&self, peer_has: &Bitfield, we_have: &Bitfield, in_flight: &HashSet<u32>) -> Option<u32>
```

New:
```rust
fn pick(&self, peer_has: &Bitfield, we_have: &Bitfield, in_flight: &HashSet<u32>, wanted: &Bitfield) -> Option<u32>
```

Filter logic adds: `wanted.get(i)` must be true. All existing callers pass an
all-ones bitfield for backwards compatibility.

## 3. Wanted Pieces Bitfield (`TorrentActor`)

New fields:
```rust
struct TorrentActor {
    // ... existing ...
    file_priorities: Vec<FilePriority>,   // one per file, default Normal
    wanted_pieces: Bitfield,              // recomputed when priorities change
}
```

`rebuild_wanted_pieces()`:
1. Start with all-zeros bitfield of `num_pieces` length
2. For each file where `priority > Skip`:
   - Compute piece range via `Lengths::file_pieces(file_offset, file_length)`
   - Set those bits to 1
3. Shared pieces (spanning file boundaries): wanted if **any** overlapping file
   is non-Skip — handled naturally since we OR bits per file

Called on:
- Torrent creation (initial priorities all Normal → all-ones)
- `set_file_priority()` command

## 4. TorrentHandle API

```rust
impl TorrentHandle {
    pub async fn set_file_priority(&self, index: usize, priority: FilePriority) -> Result<()>;
    pub async fn file_priorities(&self) -> Result<Vec<FilePriority>>;
}
```

`SetFilePriority` command in TorrentActor:
1. Validate index < file count
2. Update `file_priorities[index]`
3. Call `rebuild_wanted_pieces()`
4. Cancel in-flight pieces that are no longer wanted (send Cancel messages)
5. If new priority > Skip and old was Skip, try requesting new pieces

`FileFilePriorities` command: return clone of `file_priorities` vec.

## 5. Storage Skip Allocation

`FilesystemStorage::new()` gains optional parameter:
```rust
pub fn new(
    base_dir: &Path,
    file_paths: Vec<PathBuf>,
    file_lengths: Vec<u64>,
    lengths: Lengths,
    file_priorities: Option<&[FilePriority]>,
) -> Result<Self>
```

If priority is `Skip`, skip `File::create` + `set_len` for that file.
Files with `Skip` remain unopened; lazy-open on first write handles the case
where priority changes from Skip → non-Skip at runtime.

## 6. Resume Data Integration

`FastResumeData.file_priority` already exists as `Vec<i64>`.
- `build_resume_data()`: populate from `self.file_priorities` (cast to i64)
- `add_torrent_from_resume()`: restore from resume data (cast back to FilePriority)

## 7. Test Plan (~8 tests)

| # | Test | Crate |
|---|------|-------|
| 1 | `FilePriority` ordering, Default, From<u8> | ferrite-core |
| 2 | `PieceSelector::pick()` skips unwanted pieces | ferrite-session |
| 3 | `rebuild_wanted_pieces` with shared boundary pieces | ferrite-session |
| 4 | `set_file_priority` at runtime changes wanted | ferrite-session |
| 5 | Skip-priority file not allocated on disk | ferrite-storage |
| 6 | Resume data round-trip preserves file priorities | ferrite-session |
| 7 | `file_priorities()` returns current state | ferrite-session |
| 8 | Facade re-exports FilePriority | ferrite |
