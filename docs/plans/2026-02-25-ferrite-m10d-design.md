# M10d: Session Re-exports + ClientBuilder — Design

**Date:** 2026-02-25
**Status:** Approved
**Crate:** `crates/ferrite/`

## Goal

Add `ferrite::session` (re-exports) and `ferrite::client` (new `ClientBuilder`
+ `AddTorrentParams` wrapper types) to the facade crate.

## Changes

### 1. `Cargo.toml` — Add dependency

```toml
ferrite-session = { path = "../ferrite-session" }
```

### 2. `src/session.rs` — Re-export 12 public items

```rust
pub use ferrite_session::{
    SessionHandle, SessionConfig, SessionStats,
    TorrentHandle, TorrentConfig, TorrentState, TorrentStats, TorrentInfo,
    FileInfo, StorageFactory,
    Error, Result,
};
```

### 3. `src/client.rs` — New wrapper types

**`ClientBuilder`** — Fluent builder wrapping `SessionConfig`, calls
`SessionHandle::start()` on `.start()`.

**`AddTorrentParams`** — Unified torrent source enum with optional
download_dir and storage overrides. Constructed via `from_torrent()`,
`from_magnet()`, `from_file()`, `from_bytes()`.

### 4. Tests (~2)

- ClientBuilder defaults and method chaining (sync, no runtime needed)
- AddTorrentParams construction from magnet

### 5. Release

- CHANGELOG, README, bump to 0.10.0, push both remotes
