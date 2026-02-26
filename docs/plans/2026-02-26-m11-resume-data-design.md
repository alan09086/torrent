# M11: Resume Data & Session Persistence — Design

**Date:** 2026-02-26
**Milestone:** M11 (Phase 1: Essential Desktop Client Features)
**Goal:** Save and restore torrent/session state across restarts with libtorrent-compatible format

## Format Compatibility

Resume data uses **libtorrent-compatible bencode keys** so magnetor can import
resume data from qBittorrent/Deluge. The struct maps 1:1 to libtorrent's
`write_resume_data()` output via `#[serde(rename)]` annotations.

Reference: [libtorrent write_resume_data.cpp](https://github.com/arvidn/libtorrent/blob/master/src/write_resume_data.cpp)

## Architecture

Two new source files across two crates:

| Crate | File | Contents |
|-------|------|----------|
| ferrite-core | `src/resume_data.rs` | `FastResumeData`, `UnfinishedPiece` structs |
| ferrite-session | `src/persistence.rs` | Save/load logic, auto-save, `SessionState` |

## 1. FastResumeData (`ferrite-core`)

Serde-derived struct with libtorrent-compatible bencode keys:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FastResumeData {
    // Format markers
    #[serde(rename = "file-format")]
    pub file_format: String,              // "libtorrent resume file"
    #[serde(rename = "file-version")]
    pub file_version: i64,                // 1

    // Identity
    #[serde(rename = "info-hash", with = "serde_bytes")]
    pub info_hash: Vec<u8>,              // 20-byte SHA1
    #[serde(rename = "name")]
    pub name: String,

    // Paths
    #[serde(rename = "save_path")]
    pub save_path: String,

    // Progress — piece completion bitfield (MSB-first, matches BEP 3)
    #[serde(rename = "pieces", with = "serde_bytes")]
    pub pieces: Vec<u8>,
    #[serde(rename = "unfinished", skip_serializing_if = "Vec::is_empty", default)]
    pub unfinished: Vec<UnfinishedPiece>,

    // Cumulative stats
    #[serde(rename = "total_uploaded")]
    pub total_uploaded: i64,
    #[serde(rename = "total_downloaded")]
    pub total_downloaded: i64,
    #[serde(rename = "active_time")]
    pub active_time: i64,                // seconds
    #[serde(rename = "seeding_time")]
    pub seeding_time: i64,
    #[serde(rename = "finished_time")]
    pub finished_time: i64,

    // Timestamps (POSIX epoch seconds)
    #[serde(rename = "added_time")]
    pub added_time: i64,
    #[serde(rename = "completed_time", default)]
    pub completed_time: i64,
    #[serde(rename = "last_download", default)]
    pub last_download: i64,
    #[serde(rename = "last_upload", default)]
    pub last_upload: i64,

    // State flags (0 or 1, bencode integers)
    #[serde(rename = "paused", default)]
    pub paused: i64,
    #[serde(rename = "auto_managed", default)]
    pub auto_managed: i64,
    #[serde(rename = "sequential_download", default)]
    pub sequential_download: i64,
    #[serde(rename = "seed_mode", default)]
    pub seed_mode: i64,

    // Trackers (tiered list of lists)
    #[serde(rename = "trackers", skip_serializing_if = "Vec::is_empty", default)]
    pub trackers: Vec<Vec<String>>,
    // Compact peer lists
    #[serde(rename = "peers", with = "serde_bytes", default)]
    pub peers: Vec<u8>,                  // 6 bytes per IPv4 peer
    #[serde(rename = "peers6", with = "serde_bytes", default)]
    pub peers6: Vec<u8>,                 // 18 bytes per IPv6 peer

    // Priorities
    #[serde(rename = "file_priority", skip_serializing_if = "Vec::is_empty", default)]
    pub file_priority: Vec<i64>,
    #[serde(rename = "piece_priority", skip_serializing_if = "Vec::is_empty", default)]
    pub piece_priority: Vec<i64>,

    // Rate limits (-1 = unlimited)
    #[serde(rename = "upload_rate_limit", default)]
    pub upload_rate_limit: i64,
    #[serde(rename = "download_rate_limit", default)]
    pub download_rate_limit: i64,
    #[serde(rename = "max_connections", default)]
    pub max_connections: i64,
    #[serde(rename = "max_uploads", default)]
    pub max_uploads: i64,

    // Embedded torrent info dict (raw bencoded bytes, for magnet-fetched metadata)
    #[serde(rename = "info", skip_serializing_if = "Option::is_none", default)]
    #[serde(with = "serde_bytes")]
    pub info: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnfinishedPiece {
    pub piece: i64,
    #[serde(with = "serde_bytes")]
    pub bitmask: Vec<u8>,               // which 16 KiB blocks we have
}
```

### Why `ferrite-core`?

Same pattern as `TorrentMetaV1`, `Magnet`, `Id20` — a data-only type with Serde
derives, no runtime dependencies on session or storage crates.

## 2. Session Persistence (`ferrite-session`)

### Per-torrent API

```rust
impl TorrentHandle {
    /// Snapshot current torrent state into libtorrent-compatible resume data.
    pub async fn save_resume_data(&self) -> crate::Result<FastResumeData>;
}
```

Collects: bitfield from `ChunkTracker::bitfield()`, stats from `TorrentStats`,
info from `TorrentInfo`, tracker list, connected peer addresses.

### Session-level API

```rust
impl SessionHandle {
    /// Add a torrent from previously saved resume data.
    /// Skips hash verification if bitfield is present and valid.
    pub async fn add_torrent_from_resume(
        &self,
        resume: FastResumeData,
    ) -> crate::Result<Id20>;

    /// Save full session state (all torrents + DHT cache).
    pub async fn save_session_state(&self) -> crate::Result<SessionState>;

    /// Restore session state from a previous save.
    pub async fn load_session_state(
        &self,
        state: SessionState,
    ) -> crate::Result<()>;
}
```

### SessionState

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    #[serde(rename = "dht-nodes")]
    pub dht_nodes: Vec<DhtNodeEntry>,
    #[serde(rename = "torrents")]
    pub torrents: Vec<FastResumeData>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DhtNodeEntry {
    pub host: String,
    pub port: i64,
}
```

### Auto-save triggers

The session actor saves resume data on these state transitions:
- `Downloading → Complete`
- `any → Paused`
- `any → Stopped`
- Periodic interval (configurable, default 5 minutes)

Resume files written to: `<resume_data_dir>/resume/<hex_info_hash>.resume`
Session state written to: `<resume_data_dir>/session_state`

### New SessionConfig field

```rust
pub struct SessionConfig {
    // ... existing fields ...
    /// Directory for resume data files. Defaults to `<download_dir>/.ferrite/`.
    pub resume_data_dir: Option<PathBuf>,
}
```

## 3. Skip Verification Logic

When `add_torrent_from_resume()` loads a torrent:

1. Deserialize `FastResumeData` from bytes
2. Validate: `info_hash` length == 20, `pieces` bitfield length matches piece count
3. If valid: pass bitfield into `ChunkTracker::from_bitfield()` (already exists)
4. If invalid (length mismatch, corrupt format): log warning, fall back to full
   hash check from piece 0
5. `unfinished` pieces: restore partial chunk state into `ChunkTracker`

No file mtime checking in this milestone — that's a future enhancement.

## 4. Facade Updates (`ferrite` crate)

- Re-export `FastResumeData` and `UnfinishedPiece` from `ferrite::core`
- Re-export `SessionState` and `DhtNodeEntry` from `ferrite::session`
- Add to `ferrite::prelude`: `FastResumeData`

## 5. Test Plan (~12 tests)

| # | Test | Location |
|---|------|----------|
| 1 | `FastResumeData` bencode round-trip | ferrite-core |
| 2 | `UnfinishedPiece` bencode round-trip | ferrite-core |
| 3 | Default/empty fields serialize correctly | ferrite-core |
| 4 | Deserialize with unknown keys (forward compat) | ferrite-core |
| 5 | `save_resume_data()` captures bitfield + stats | ferrite-session |
| 6 | `add_torrent_from_resume()` restores bitfield, skips hash check | ferrite-session |
| 7 | Corrupt resume data (bad bitfield length) triggers re-check | ferrite-session |
| 8 | `SessionState` round-trip | ferrite-session |
| 9 | Resume file I/O (write to disk, read back) | ferrite-session |
| 10 | `info` dict embedding round-trip (magnet metadata) | ferrite-core |
| 11 | Auto-save fires on state transition | ferrite-session |
| 12 | Facade re-exports accessible | ferrite |
