# Ferrite: Full libtorrent-Parity BitTorrent Engine

## Context

Ferrite (`/mnt/TempNVME/projects/ferrite/`) is a from-scratch Rust BitTorrent library. M1-M8c are complete (337 tests, zero clippy warnings). The goal is full libtorrent-rasterbar parity, using librqbit as a research model to improve upon. Ferrite will eventually replace librqbit as the engine in `rqbit-slint`.

**librqbit gaps ferrite must fix:** no tracker management, no peer client ID, no torrent metadata exposure, no queue/priority, no sequential download, no per-torrent speed limits, no seeding limits, no relocation, no rename, no recheck/reannounce.

## Final Architecture

```
ferrite-bencode   (M1 ✓)
     |
ferrite-core      (M2 ✓)
     |
     +---> ferrite-wire      (M3 ✓)
     +---> ferrite-tracker   (M4 ✓)
     +---> ferrite-dht       (M5 ✓)
     |
ferrite-storage   (M6 ✓)
     |
ferrite-session   (M7-M9)
     |
ferrite           (M10 - public facade)
```

## Milestone Roadmap

| MS | Crate | BEPs | Tests | Status |
|----|-------|------|-------|--------|
| M5 | ferrite-dht | 5 | 42 | **Done** |
| M6 | ferrite-storage | (infra) | 41 | **Done** |
| M7 | ferrite-session (peer+torrent) | 3, 9, 10, 11, 27 | 52 | **Done** |
| M8a | ferrite-session (tracker+DHT integration) | 12, 15 | 16 | **Done** |
| M8b | ferrite-session (session mgr) | 14, 6 | 32 | **Done** |
| M8c | ferrite-session (magnet/LSD/AllowedFast/RejectRequest) | 6, 14 | 6 | **Done** |
| M9 | ferrite-session (seeding/queue/rename) | 16, 19 | ~25 | Needs M8c |
| M10 | ferrite (facade) | — | ~10 | Needs M9 |
| M11+ | BEP 52, uTP, DHT ext, RSS, persistence | 52, 29, 42, 44, 33 | TBD | Post-MVP |

---

## M5: ferrite-dht — Detailed Plan

**Crate**: `crates/ferrite-dht`
**Deps**: ferrite-bencode, ferrite-core, bytes, tokio, thiserror, tracing
**Pattern**: Actor model — `DhtHandle::start()` spawns background task, returns handle

### Module Structure

```
ferrite-dht/src/
    lib.rs              -- pub exports, DhtHandle
    error.rs            -- Error enum (thiserror)
    krpc.rs             -- KRPC message types + bencode ser/de
    routing_table.rs    -- RoutingTable, KBucket, RoutingNode
    actor.rs            -- DhtActor event loop (internal)
    peer_store.rs       -- token gen/validation, peer storage per info_hash
    compact.rs          -- CompactNodeInfo 26-byte encode/decode
```

### Key Types

**KRPC Messages** (`krpc.rs`):
- `TransactionId(pub u16)` — wraps 2-byte transaction IDs
- `KrpcMessage` — Query / Response / Error variants
- `KrpcQuery` — Ping, FindNode, GetPeers, AnnouncePeer
- `KrpcResponse` — Ping, FindNode, GetPeers, AnnouncePeer

**Routing Table** (`routing_table.rs`):
- `K = 8` bucket size
- `RoutingTable { own_id, buckets }` with insert/closest/remove/mark_seen/stale

**DHT Actor + Handle** (`lib.rs` / `actor.rs`):
- `DhtConfig` — bind_addr, bootstrap_nodes, own_id, queries_per_second
- `DhtHandle` — clone-friendly handle with async methods
- `DhtActor` — single-owner event loop (internal)

**Compact Node Info** (`compact.rs`):
- 26 bytes: 20-byte ID + 4-byte IP + 2-byte port (big-endian)

**Peer Store** (`peer_store.rs`):
- Token generation (SHA1 of IP + secret + time bucket)
- Token validation (accept current + previous time bucket)
- Per-info_hash peer sets with expiry

### Improvements Over librqbit DHT

1. **Actor model** — single-owner event loop vs `DashMap` + `RwLock` shared state
2. **Configurable rate limits** — first-class config field
3. **Clean public API** — `DhtHandle` with async methods vs `Arc<DhtState>` exposing internals
4. **PeerStream** — async stream of discovered peers vs collecting all into a vec

### Implementation Order

1. `error.rs` — Error enum
2. `compact.rs` — CompactNodeInfo encode/decode
3. `krpc.rs` — KRPC message bencode ser/de
4. `routing_table.rs` — RoutingTable + KBucket
5. `peer_store.rs` — Token gen/validation + peer sets
6. `actor.rs` — DhtActor event loop
7. `lib.rs` — DhtHandle public API

### Test Plan (~35 tests)

- KRPC encode/decode round-trips: ~10
- CompactNodeInfo encode/decode: ~3
- RoutingTable insert/evict/closest/stale/split: ~12
- PeerStore token gen/validation/expiry: ~4
- DhtHandle integration: ~6

---

## M6: ferrite-storage — Detailed Plan

**Crate**: `crates/ferrite-storage`
**Deps**: ferrite-core, thiserror, tracing
**Pattern**: Fully synchronous — no bytes, tokio, or serde

### Module Structure

```
ferrite-storage/src/
    lib.rs              -- pub exports
    error.rs            -- Error enum
    bitfield.rs         -- Compact bit-vector (wire-format compatible)
    file_map.rs         -- Piece/chunk coords → file segments
    chunk_tracker.rs    -- Per-piece chunk state + have-bitfield
    storage.rs          -- TorrentStorage trait
    memory.rs           -- In-memory backend (tests + magnet pre-metadata)
    filesystem.rs       -- Disk-backed backend (sync I/O)
```

### Key Types

- `Bitfield` — MSB-first bit ordering matching BEP 3 wire format, from_bytes/as_bytes round-trip
- `FileMap` — pre-computed cumulative offsets, binary search O(log n) file lookup
- `ChunkTracker` — piece-level `have` Bitfield + per-piece chunk Bitfield (only active pieces)
- `TorrentStorage` trait — `&self` (Arc-shared), `(piece, begin, length)` wire-compatible API, `verify_piece` default impl
- `MemoryStorage` — `RwLock<Vec<u8>>` flat buffer for tests and magnet-link metadata
- `FilesystemStorage` — `Vec<Mutex<Option<File>>>` lazy open, per-file locking, sparse files

### Improvements Over librqbit

1. **`from_bitfield` resume** — persist bitfield, skip re-verification on restart
2. **Binary search file lookup** — O(log n) vs linear scan
3. **Clean trait** — `verify_piece` default method, wire-compatible API
4. **Lazy file handles** — reduced startup overhead
5. **Reusable Bitfield** — same type for internal tracking and wire protocol

### Test Results: 41 tests

- bitfield.rs: 12 tests
- file_map.rs: 8 tests
- chunk_tracker.rs: 8 tests
- memory.rs: 5 tests
- filesystem.rs: 8 tests

---

## Verification

```bash
cd /mnt/TempNVME/projects/ferrite
cargo test --workspace           # 337 tests pass
cargo clippy --workspace -- -D warnings  # Zero warnings
```
