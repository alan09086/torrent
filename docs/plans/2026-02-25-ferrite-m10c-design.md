# M10c: DHT + Storage Re-exports — Design

**Date:** 2026-02-25
**Status:** Approved
**Crate:** `crates/ferrite/`

## Goal

Add `ferrite::dht` and `ferrite::storage` modules to the facade crate,
re-exporting all public types from `ferrite-dht` and `ferrite-storage`.
Same `pub use` pattern as M10a/M10b.

## Changes

### 1. `Cargo.toml` — Add dependencies

```toml
ferrite-dht = { path = "../ferrite-dht" }
ferrite-storage = { path = "../ferrite-storage" }
```

### 2. `src/dht.rs` — Re-export public items

From crate root (`pub use` in ferrite-dht/src/lib.rs):
- `DhtHandle`, `DhtConfig`, `DhtStats`
- `CompactNodeInfo`
- `RoutingTable`
- `KrpcMessage`, `KrpcBody`, `KrpcQuery`, `KrpcResponse`, `GetPeersResponse`, `TransactionId`
- `Error`, `Result`

From `pub mod` sub-modules (accessible via ferrite_dht::module::item):
- `compact::parse_compact_nodes`, `compact::encode_compact_nodes`
- `peer_store::PeerStore`
- `routing_table::K`

### 3. `src/storage.rs` — Re-export 9 public items

- `TorrentStorage`, `Bitfield`, `ChunkTracker`
- `FileMap`, `FileSegment`
- `MemoryStorage`, `FilesystemStorage`
- `Error`, `Result`

### 4. `src/lib.rs` — Add module declarations

```rust
pub mod dht;
pub mod storage;
```

### 5. Tests (~2)

- DHT compact node round-trip through facade
- Storage bitfield operations through facade

### 6. Release

- CHANGELOG.md: add v0.9.0 entry
- README.md: mark M10c as Done, update test count
- Bump workspace version to 0.9.0
- Push to both remotes
