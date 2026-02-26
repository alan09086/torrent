# M10e: Prelude + Unified Error + Integration Tests — Design

**Date:** 2026-02-26
**Status:** Approved
**Crate:** `crates/ferrite/`

## Goal

Final M10 milestone. Add `ferrite::prelude`, unified `ferrite::Error` enum
wrapping all per-crate errors, and integration tests verifying the full
facade works end-to-end.

## Changes

### 1. `src/error.rs` — Unified error enum

```rust
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("bencode: {0}")]
    Bencode(#[from] ferrite_bencode::Error),
    #[error("core: {0}")]
    Core(#[from] ferrite_core::Error),
    #[error("wire: {0}")]
    Wire(#[from] ferrite_wire::Error),
    #[error("tracker: {0}")]
    Tracker(#[from] ferrite_tracker::Error),
    #[error("dht: {0}")]
    Dht(#[from] ferrite_dht::Error),
    #[error("storage: {0}")]
    Storage(#[from] ferrite_storage::Error),
    #[error("session: {0}")]
    Session(#[from] ferrite_session::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
```

### 2. `src/prelude.rs` — Convenience re-exports

Key types a typical user needs:
- `ClientBuilder`, `AddTorrentParams`
- `SessionHandle`, `TorrentHandle`, `TorrentState`, `TorrentStats`, `TorrentInfo`
- `Id20`, `Magnet`, `TorrentMetaV1`
- `TorrentStorage`, `FilesystemStorage`
- `Error`, `Result` (unified)

### 3. Tests (~3)

- Unified error `From` conversions (bencode → Error, session → Error, etc.)
- Prelude imports compile and types accessible
- Full type chain: parse magnet → build AddTorrentParams → verify types compose

### 4. Release

- CHANGELOG, README (mark M10e Done, M10 complete), bump to 0.11.0
- Push both remotes
