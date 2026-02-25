# M10: Public Facade — Design Document

**Date:** 2026-02-25
**Status:** Planned
**Crate:** `crates/ferrite/`
**Tests:** ~12 estimated across 5 parts

## Goal

Create the `ferrite` crate — the public facade that re-exports all internal
crates through a clean, ergonomic API. This is the crate users `cargo add` to
build BitTorrent applications. Designed for general-purpose use, targeting
libtorrent-rasterbar parity as a Rust-native replacement.

## Architecture

The facade follows a layer-by-layer approach matching the dependency graph:

```
ferrite/src/
    lib.rs          -- crate docs, feature flags, top-level re-exports
    bencode.rs      -- ferrite::bencode (M10a)
    core.rs         -- ferrite::core (M10a)
    wire.rs         -- ferrite::wire (M10b)
    tracker.rs      -- ferrite::tracker (M10b)
    dht.rs          -- ferrite::dht (M10c)
    storage.rs      -- ferrite::storage (M10c)
    session.rs      -- ferrite::session (M10d)
    client.rs       -- ferrite::ClientBuilder + AddTorrentParams (M10d)
    prelude.rs      -- ferrite::prelude (M10e)
    error.rs        -- ferrite::Error unified enum (M10e)
```

## Split into 5 Parts

### M10a: Crate Scaffold + Core Re-exports

Creates the `ferrite` crate with workspace wiring and re-exports the
foundation types from `ferrite-bencode` and `ferrite-core`.

**Modules:**
- `bencode.rs` — `to_bytes`, `from_bytes`, `BencodeValue`, `find_dict_key_span`, `Error`
- `core.rs` — `Id20`, `Id32`, `PeerId`, `Magnet`, `TorrentMetaV1`, `InfoDict`,
  `FileEntry`, `FileInfo`, `Lengths`, `torrent_from_bytes`, `sha1`, `DEFAULT_CHUNK_SIZE`, `Error`

**Tests (~3):**
- Bencode round-trip through facade
- Parse .torrent through facade
- Magnet link parse through facade

### M10b: Wire + Tracker Re-exports

Adds the protocol layer for custom client builders and low-level tools.

**Modules:**
- `wire.rs` — `Handshake`, `Message`, `MessageCodec`, `ExtHandshake`, `ExtMessage`,
  `MetadataMessage`, `MetadataMessageType`, `allowed_fast_set`, `HANDSHAKE_SIZE`, `Error`
- `tracker.rs` — `HttpTracker`, `UdpTracker`, `AnnounceRequest`, `AnnounceResponse`,
  `AnnounceEvent`, `HttpAnnounceResponse`, `UdpAnnounceResponse`, `parse_compact_peers`, `Error`

**Tests (~2):**
- Handshake round-trip through facade
- Announce request construction through facade

### M10c: DHT + Storage Re-exports

Infrastructure layer — peer discovery and piece storage.

**Modules:**
- `dht.rs` — `DhtHandle`, `DhtConfig`, `DhtStats`, `CompactNodeInfo`, `KrpcMessage`,
  `KrpcBody`, `KrpcQuery`, `KrpcResponse`, `RoutingTable`, `PeerStore`,
  `parse_compact_nodes`, `encode_compact_nodes`, `K`, `Error`
- `storage.rs` — `TorrentStorage`, `Bitfield`, `ChunkTracker`, `FileMap`, `FileSegment`,
  `MemoryStorage`, `FilesystemStorage`, `Error`

**Tests (~2):**
- DHT config defaults through facade
- Storage write/read/verify through facade

### M10d: Session Re-exports + Client Builder

The main user-facing entry point. Wraps `SessionHandle` with an ergonomic
builder pattern inspired by libtorrent's `session_params` + `add_torrent_params`.

**Modules:**
- `session.rs` — `SessionHandle`, `SessionConfig`, `SessionStats`, `TorrentHandle`,
  `TorrentConfig`, `TorrentState`, `TorrentStats`, `TorrentInfo`, `FileInfo`,
  `StorageFactory`, `Error`
- `client.rs` — `ClientBuilder`, `AddTorrentParams`

**Key new types:**

```rust
/// Ergonomic builder for creating a ferrite session.
///
/// # Example
/// ```no_run
/// let session = ferrite::ClientBuilder::new()
///     .download_dir("/tmp/downloads")
///     .listen_port(6881)
///     .enable_dht(true)
///     .start()
///     .await?;
/// ```
pub struct ClientBuilder {
    config: SessionConfig,
}

impl ClientBuilder {
    pub fn new() -> Self;
    pub fn listen_port(self, port: u16) -> Self;
    pub fn download_dir(self, path: impl Into<PathBuf>) -> Self;
    pub fn max_torrents(self, n: usize) -> Self;
    pub fn enable_dht(self, v: bool) -> Self;
    pub fn enable_lsd(self, v: bool) -> Self;
    pub fn enable_pex(self, v: bool) -> Self;
    pub fn enable_fast_extension(self, v: bool) -> Self;
    pub fn seed_ratio_limit(self, ratio: f64) -> Self;
    pub async fn start(self) -> Result<SessionHandle>;
}

/// Unified parameters for adding a torrent to a session.
pub struct AddTorrentParams {
    source: TorrentSource,
    download_dir: Option<PathBuf>,
    storage: Option<Arc<dyn TorrentStorage>>,
}

enum TorrentSource {
    Meta(TorrentMetaV1),
    Magnet(Magnet),
    File(PathBuf),
    Bytes(Vec<u8>),
}

impl AddTorrentParams {
    pub fn from_torrent(meta: TorrentMetaV1) -> Self;
    pub fn from_magnet(magnet: Magnet) -> Self;
    pub fn from_file(path: impl Into<PathBuf>) -> Self;
    pub fn from_bytes(data: Vec<u8>) -> Self;
    pub fn download_dir(self, path: impl Into<PathBuf>) -> Self;
    pub fn storage(self, s: Arc<dyn TorrentStorage>) -> Self;
}
```

**Tests (~2):**
- ClientBuilder defaults and chaining
- AddTorrentParams from magnet

### M10e: Prelude, Unified Error, Integration Tests, Docs

Polish pass — makes `ferrite` feel like a complete, professional library.

**Modules:**
- `prelude.rs` — `ClientBuilder`, `AddTorrentParams`, `SessionHandle`, `TorrentHandle`,
  `TorrentState`, `TorrentStats`, `TorrentInfo`, `Id20`, `Magnet`, `TorrentMetaV1`,
  `TorrentStorage`, `FilesystemStorage`, `Result`
- `error.rs` — unified `ferrite::Error` enum with `From` impls for each crate error

**Unified error:**

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

**Integration tests (~3):**
- End-to-end: parse torrent file → create client → add torrent → check state
- Magnet link flow: parse magnet → add to session → verify FetchingMetadata state
- Resume/seeding flow: pre-seeded storage → verify_existing_pieces → Seeding state

**Docs:**
- Crate-level rustdoc on `lib.rs` with usage examples
- Module-level docs on each re-export module

## Key Design Decisions

### Re-export vs wrap

Most types are re-exported directly (`pub use`). Only `ClientBuilder` and
`AddTorrentParams` are new wrapper types. This avoids maintenance burden of
thin wrappers while adding ergonomics where they matter most (session creation
and torrent addition).

### Per-crate errors preserved

Each module re-exports its crate's `Error` type (e.g., `ferrite::wire::Error`).
The unified `ferrite::Error` in `error.rs` wraps all of them with `From` impls.
Users can match on specific crate errors or use the unified type.

### Feature flags (future)

The facade can add optional feature flags in the future:
- `default = ["dht", "tracker", "storage-fs"]`
- `dht` — include ferrite-dht
- `tracker` — include ferrite-tracker
- `storage-fs` — include FilesystemStorage (pulls in std::fs)

Not implemented in M10 — all features always on for now.

### Comparison with libtorrent-rasterbar

| libtorrent concept | ferrite equivalent |
|---|---|
| `session` / `session_handle` | `SessionHandle` (via `ClientBuilder::start()`) |
| `add_torrent_params` | `AddTorrentParams` |
| `torrent_handle` | `TorrentHandle` |
| `torrent_info` | `TorrentInfo` |
| `torrent_status` | `TorrentStats` + `TorrentState` |
| `settings_pack` | `SessionConfig` / `TorrentConfig` (via `ClientBuilder`) |
| `alert` system | Direct async return values (no alert queue needed) |

## Deferred

- Feature flags for optional crate inclusion → M11+
- `#[no_std]` support for ferrite-bencode/core → M11+
- C FFI bindings → M11+
- Python bindings (PyO3) → M11+
