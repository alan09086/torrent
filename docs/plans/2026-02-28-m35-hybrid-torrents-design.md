# M35: Hybrid v1+v2 Torrents — Design Document

## Context

M33-M34 implemented BEP 52 v2 support: metadata types, wire messages, Merkle hash picker,
per-block SHA-256 storage verification, and session integration. However, all v2 support
is for **pure v2 torrents only**. Real-world adoption requires **hybrid torrents** — a
single .torrent file containing both v1 and v2 info dicts so clients of either version
can participate.

BEP 53 (magnet file selection via `&so=` parameter) was originally bundled with M35 in the
roadmap but has been split to a separate milestone — it's independent of hybrid torrents.

## BEP 52 Hybrid Torrent Format

A hybrid .torrent file has a **single info dict** containing keys from both v1 and v2:

**v1 keys:** `pieces` (SHA-1 concatenated hashes), `files`/`length` (flat file list)
**v2 keys:** `meta version` (= 2), `file tree` (nested file tree with Merkle roots)
**Shared:** `name`, `piece length`

The **same raw info dict bytes** are hashed with SHA-1 (producing the v1 info hash)
and SHA-256 (producing the v2 info hash). These are fundamentally different hashes of
identical data — unlike pure v2 where the v1 hash is just a truncation.

**Dual swarms:** The two info hashes create two separate tracker/DHT swarms. v1-only
clients join the v1 swarm; v2-capable clients can join both. All peers download the
same piece data.

### Reference: libtorrent-rasterbar 2.0.11

Source code analysis of libtorrent's hybrid torrent implementation:

1. **Dual verification always** — `on_piece_hashed()` computes both SHA-1 and SHA-256
   block hashes in a single disk job, then checks both.
2. **Tribool logic** — `boost::tribool` with `true/false/indeterminate`. A hash type
   that isn't available yet is `indeterminate`, not `false`.
3. **Conflict = hard error** — If one hash passes and the other fails, the torrent is
   paused, piece picker and hash picker are destroyed, and all progress is discarded.
   Error: `torrent_inconsistent_hashes`.
4. **Accept condition** — `v1_passed OR v2_passed` (with `indeterminate` acting as
   neutral — doesn't cause acceptance or rejection alone).
5. **Peer swarm tagging** — Peers from v2 tracker announce get `pex_lt_v2` flag (bit 7).
   Used for PEX relay and to know which peers support hash messages.

## Design

### 1. Hybrid Torrent Detection & Parsing (`ferrite-core`)

**Current state:** `detect.rs` has `is_v2()` checking for `meta version = 2`.
`TorrentMeta` enum is `V1 | V2`.

**Changes:**

1. Replace `is_v2()` with `detect_version()` returning a new internal enum:
   ```rust
   enum DetectedVersion { V1Only, V2Only, Hybrid }
   ```
   Detection logic:
   - Has `meta version = 2` AND has `pieces` key → `Hybrid`
   - Has `meta version = 2` only → `V2Only`
   - Neither → `V1Only`

2. Add `TorrentMeta::Hybrid(TorrentMetaV1, TorrentMetaV2)` variant.

3. `torrent_from_bytes_any()` for `Hybrid`:
   - Parse with `torrent_from_bytes()` → `TorrentMetaV1` (extracts v1 fields, ignores v2)
   - Parse with `torrent_v2_from_bytes()` → `TorrentMetaV2` (extracts v2 fields, ignores v1)
   - Override `TorrentMetaV2.info_hashes` — set `v1` to the REAL SHA-1 hash (from the
     v1 parse), not the truncated SHA-256.
   - Return `TorrentMeta::Hybrid(v1, v2)`

4. `TorrentMeta` gains:
   - `is_hybrid() -> bool`
   - `info_hashes()` for `Hybrid` returns `InfoHashes::hybrid(v1_hash, v2_hash)`

**Key correctness property:** The v1 info hash in a hybrid torrent is SHA-1 of the raw
info dict bytes. The v2 info hash is SHA-256 of those same bytes. Both parsers see the
same info dict span.

### 2. TorrentVersion Enum (`ferrite-core` or `ferrite-session`)

New enum replacing `is_v2: bool` in `TorrentActor`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorrentVersion {
    V1Only,
    V2Only,
    Hybrid,
}

impl TorrentVersion {
    pub fn has_v1(&self) -> bool { matches!(self, Self::V1Only | Self::Hybrid) }
    pub fn has_v2(&self) -> bool { matches!(self, Self::V2Only | Self::Hybrid) }
    pub fn is_hybrid(&self) -> bool { matches!(self, Self::Hybrid) }
}
```

This replaces the `is_v2: bool` field in `TorrentActor`. Constructed from `TorrentMeta`
or `InfoHashes` when the torrent is added.

### 3. Dual Verification (`ferrite-session`)

**Hash result type:**

```rust
enum HashResult {
    Passed,
    Failed,
    NotApplicable,  // hash type not available (e.g., Merkle hashes not received yet)
}
```

**Verification dispatch in `TorrentActor`:**

```
verify_and_mark_piece(index):
    match self.version:
        V1Only  → verify_and_mark_piece_v1(index)
        V2Only  → verify_and_mark_piece_v2(index)
        Hybrid  → verify_and_mark_piece_hybrid(index)
```

**`verify_and_mark_piece_hybrid(index)`:**

1. Run v1 verification (SHA-1 of entire piece vs stored piece hash):
   - Match → `v1_result = Passed`
   - Mismatch → `v1_result = Failed`

2. Run v2 verification (per-block SHA-256 → HashPicker Merkle tree):
   - All blocks verified → `v2_result = Passed`
   - Any block hash failed → `v2_result = Failed`
   - Merkle hashes not yet available → `v2_result = NotApplicable`

3. Decision matrix:
   | v1 | v2 | Action |
   |----|----|--------|
   | Passed | Passed | `on_piece_verified()` |
   | Passed | NotApplicable | `on_piece_verified()` |
   | NotApplicable | Passed | `on_piece_verified()` (shouldn't happen in practice) |
   | Failed | Failed | `on_piece_hash_failed()` |
   | Passed | Failed | **`on_inconsistent_hashes()`** |
   | Failed | Passed | **`on_inconsistent_hashes()`** |
   | Failed | NotApplicable | `on_piece_hash_failed()` |
   | NotApplicable | NotApplicable | Error (impossible — at least v1 hashes exist) |

**`on_inconsistent_hashes(piece)`** (new method, matching libtorrent):
- Post `AlertKind::InconsistentHashes { piece }` alert
- Destroy hash picker (`self.hash_picker = None`)
- Mark torrent as errored
- Pause the torrent
- Log error

### 4. Dual-Swarm Participation (`ferrite-session`, `ferrite-tracker`)

**Tracker announces:**
For hybrid torrents, the session announces to trackers with BOTH info hashes.
Currently tracker announces use a single `info_hash: Id20`. Changes:

- `TorrentActor` sends two announce requests per tracker for hybrid torrents:
  one with `info_hashes.v1`, one with `info_hashes.best_v1()` (from v2 hash).
- Actually: BEP 52 says for hybrid, the v1 swarm uses the v1 info hash and the
  v2 swarm uses the **truncated SHA-256** as the 20-byte hash. But `best_v1()`
  already handles this — it returns v1 if present, otherwise truncates v2. For
  hybrid, we need to announce BOTH the real v1 hash AND the v2-truncated hash.
- Peers from v1 announce don't get a v2 flag; peers from v2 announce get `supports_v2`.

**DHT:**
Same pattern — two `get_peers` requests, one per info hash.

**PEX:**
Add `pex_lt_v2` flag bit (bit 7) to peer exchange messages for v2-capable peers.

### 5. Hybrid Torrent Creation (`ferrite-core`)

**Builder changes:**

```rust
pub enum TorrentVersion { V1, V2, Hybrid }

impl CreateTorrent {
    pub fn set_version(mut self, version: TorrentVersion) -> Self;
}
```

Default remains `V1` for backward compatibility.

**`generate()` for Hybrid mode:**

Single pass over file data computes both:
- SHA-1 hash of each piece (concatenated into `pieces` field)
- SHA-256 hash of each 16 KiB block (accumulated into per-file Merkle trees)

The output .torrent file contains a single info dict with both v1 and v2 keys.

**V2/Hybrid constraints:**
- Piece size must be ≥ 16384 and power of 2 (already enforced)
- For multi-file hybrid torrents, files are padded to piece boundaries (v2 requirement)
  → `set_pad_file_limit(Some(0))` is implicitly enabled for v2/hybrid

**Result type changes:**

```rust
pub struct CreateTorrentResult {
    pub meta: TorrentMeta,        // V1, V2, or Hybrid
    pub bytes: Vec<u8>,           // raw .torrent file
    pub info_hashes: InfoHashes,  // convenience accessor
}
```

**V2-only creation:**
For completeness, `set_version(V2)` produces a pure v2 torrent (no `pieces` key, no
flat file list). This is less common but supported.

### 6. Resume Data (`ferrite-session`)

**Already supported:** `FastResumeData` has `info_hash2: Option<Vec<u8>>` and
`trees: HashMap<String, Vec<u8>>` from M34.

**Hybrid additions:**
- Both `info_hash` (v1 SHA-1) and `info_hash2` (v2 SHA-256) are populated.
- `version: TorrentVersion` field in resume data (or inferred from presence of both hashes).
- On resume, verify both hashes match the torrent before restoring state.

### 7. Error Types

New error variant in `ferrite-session`:

```rust
// In AlertKind:
InconsistentHashes { piece: u32 },
```

New error in `ferrite-core` (for parsing):

```rust
// In Error:
HybridInconsistency(String),
```

## Test Plan (~10 tests)

| # | Test | Location |
|---|------|----------|
| 1 | Hybrid detection: info dict with both `pieces` and `meta version = 2` | `detect.rs` |
| 2 | Hybrid parsing: correct `InfoHashes` with real SHA-1 (not truncated) | `detect.rs` |
| 3 | `TorrentMeta::Hybrid` accessor methods (`is_hybrid`, `info_hashes`) | `detect.rs` |
| 4 | `TorrentVersion` enum methods (`has_v1`, `has_v2`, `is_hybrid`) | `ferrite-core` |
| 5 | Hybrid `CreateTorrent` round-trip: create → parse → verify both hashes | `create.rs` |
| 6 | V2-only `CreateTorrent` round-trip | `create.rs` |
| 7 | Dual verification: both pass → accept | `torrent.rs` |
| 8 | Dual verification: v1 pass + v2 fail → inconsistent error | `torrent.rs` |
| 9 | Dual verification: v1 fail + v2 pass → inconsistent error | `torrent.rs` |
| 10 | Resume data round-trip with both hashes | `session` |

## Scope Exclusions

- **BEP 53 (`&so=` magnet file selection)** — separate milestone
- **Multi-file piece-to-file mapping edge cases** — v1 and v2 file lists are guaranteed
  to describe the same data in the same order by BEP 52, so no special mapping needed
- **v2-only peer handling** — peers that only speak v2 connect via the v2 swarm; the
  `TorrentActor` handles them identically to current v2-only behaviour

## Milestone Deliverables

- Version bump: 0.40.0 → 0.41.0
- ~10 new tests (total: ~893)
- Zero clippy warnings
- Commit format: `feat: description (M35)`
- Push to both remotes
