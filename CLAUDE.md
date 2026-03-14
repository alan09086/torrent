# Torrent

Rust BitTorrent library — libtorrent-rasterbar + rqbit streaming parity.

## Build & Test

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

## Architecture

12-crate workspace: `torrent-bencode` → `torrent-core` → `torrent-wire`/`torrent-tracker`/`torrent-dht`/`torrent-storage`/`torrent-utp`/`torrent-nat` → `torrent-session` → `torrent` (facade) + `torrent-sim` (simulation)

- **torrent-session**: Actor model — `SessionActor`/`TorrentActor` with `tokio::select!` loops and command channels
- **torrent** (facade): `ClientBuilder` fluent API, `AddTorrentParams`, unified `Error`, `prelude` module

## Conventions

- Edition 2024, workspace resolver = "2"
- `thiserror` typed errors per crate, no `anyhow`
- `bytes::Bytes` throughout (not custom wrappers)
- Manual big-endian serialization for wire protocol
- Random bytes: thread-local xorshift64 seeded from SystemTime (avoids `rand` dep)
- Sorted map serializer for BEP 3 key ordering
- KRPC: `BTreeMap<Vec<u8>, BencodeValue>` for encoding, pattern-match on "y" field for decoding

## Milestones

- 51-milestone roadmap: `docs/plans/2026-03-01-torrent-roadmap-v3-full-parity.md`
- M1-M53 complete. All 51 parity milestones done + M52 API documentation + TorrentStats full parity + M53 full torrent operations API parity.
- Current: v0.93.0, 1488 tests. M93: Lock-free piece dispatch — replaced Arc<RwLock<PieceReservationState>> with atomic CAS-based dispatch (AtomicPieceStates, AvailabilitySnapshot bucket-sorted rarest-first, PeerDispatchState per-peer cursor, PeerSlab arena allocation), 500ms snapshot rebuild timer, zero locks on hot path. 25 new / 28 removed tests (net flat). M92: Peer event batching — replaced per-block ChunkWritten with batched PieceBlocksBatch, PendingBatch accumulator with 25ms flush timer, process_block_completion() extraction. 8 new tests. M91: SimTransport integration tests — 4 end-to-end transfer tests (basic, multi-peer, partition recovery, link config), removed stale dead_code annotations. M90: I2P session integration — outbound SAM connects with timeout, I2P destination tracking via synthetic 240.0.0.0/4 addresses, BEP 7 HTTP tracker announces with I2P destination, mixed-mode PEX filtering for I2P/clearnet segregation. M88: BEP 44 session API — SessionHandle exposes dht_put/get_immutable() and dht_put/get_mutable() with alert firing and Error::DhtDisabled gating. M87: BEP 52 hash serving — v2/hybrid seeders serve piece-layer Merkle hashes via HashRequest (msg ID 21), extracted testable serve_hashes() function, subtree proof layout, HashPicker placeholder cleanup, merkle.rs closure cleanup. M85: DHT routing table overhaul — iterative bootstrap (FindNodeLookup), node liveness tracking (Good/Questionable/Bad), background pinger, query rate limiter (250/s), periodic state persistence (60s atomic writes), proper mark_failed via PendingQuery node_id, BEP 42 node ID persistence across sessions, re-bootstrap after BEP 42 regeneration, V6 DHT give-up after 30 empty-table retries. M86: removed dead DiskIoBackend::move_storage plumbing. M89: NAT cleanup debug logging. M84: aws-lc-rs crypto backend (replaces ring) — pluggable crypto feature flags. M83: connection ramp-up elimination — default encryption Disabled, AIMD initial depth 128. 56.8 MB/s (+47%).
- Audit remediation plans (M86-M91): `docs/plans/2026-03-13-torrent-m86-m91-audit-remediation-design.md`
- Implementation plans exist for all remaining milestones (M40-M51) in `docs/plans/`
- Commit format: `feat: description (M24)` — milestone tag in parentheses
- Version bumps: workspace version in root `Cargo.toml`, bump with each milestone
- Plan files: `docs/plans/YYYY-MM-DD-torrent-<milestone>-<topic>.md`
- **After every milestone**: update `README.md` (badges, test counts, BEP table, roadmap, version table) and `CHANGELOG.md` (new version entry with Added/Changed sections). Commit separately as `docs: update README and CHANGELOG for <milestone>`

## Remotes

- `origin` → Codeberg, `github` → GitHub
- Push to BOTH on every push: `git push origin main && git push github main`

## Key Types & Patterns Reference

### Bencode Serialization (`torrent-bencode`)
- `to_bytes<T: Serialize>(value) -> Vec<u8>` / `from_bytes<T: Deserialize>(bytes) -> T`
- `SortedMapSerializer`: buffers all key-value pairs, sorts by key bytes, writes in order — ensures BEP 3 dict key ordering automatically
- `find_dict_key_span(data, "info") -> Range<usize>`: finds raw byte span of a dict key's value (used for info-hash computation)
- `BencodeValue` enum: `Integer(i64)`, `Bytes(Vec<u8>)`, `List(Vec<_>)`, `Dict(BTreeMap<Vec<u8>, _>)`
- **Bencode has no null** — `serialize_none()` returns error. All `Option` fields on serializable structs must use `#[serde(skip_serializing_if = "Option::is_none")]`

### Core Types (`torrent-core`)
- `Id20([u8; 20])` — SHA1 hash (info hash, piece hash). Methods: `from_hex()`, `to_hex()`, `as_bytes()`
- `Id32([u8; 32])` — SHA-256 (BEP 52). Methods: `from_hex()`, `to_hex()`, `from_base32()`, `to_base32()`, `from_multihash_hex()`, `to_multihash_hex()`
- `InfoHashes { v1: Option<Id20>, v2: Option<Id32> }` — unified hash container. Constructors: `v1_only()`, `v2_only()`, `hybrid()`. `best_v1()` for tracker/DHT compat
- `sha1(data: &[u8]) -> Id20` — uses `ring` (BoringSSL assembly)
- `sha256(data: &[u8]) -> Id32` — uses `ring` (BoringSSL assembly)
- `MerkleTree` — binary heap layout. `from_leaves()`, `root()`, `layer()`, `piece_layer()`, `proof_path()`, `verify_proof()`
- `Lengths { total_length, piece_length, chunk_size }` — piece arithmetic: `num_pieces()`, `piece_size(idx)`, `piece_offset(idx)`
- `DEFAULT_CHUNK_SIZE = 16384`

### Torrent Metadata (`torrent-core/src/metainfo.rs`, `metainfo_v2.rs`, `detect.rs`)
- `TorrentMetaV1` — NOT a serde struct (manually constructed). Fields: info_hash, announce, announce_list, comment, created_by, creation_date, info, url_list, httpseeds
- `InfoDict` — `Deserialize + Serialize`. Fields: name, piece_length (renamed), pieces (serde_bytes), length (Option), files (Option), private (Option<i64>), source (Option<String>)
- `FileEntry` — `Deserialize + Serialize`. Fields: length, path (Vec<String>), attr (Option<String>, BEP 47), mtime (Option<i64>), symlink_path (Option<Vec<String>>)
- `torrent_from_bytes(data) -> Result<TorrentMetaV1>` — parses v1 .torrent from raw bytes
- `TorrentMetaV2` — v2 torrent (BEP 52). Fields: info_hashes, info_bytes, info (InfoDictV2), piece_layers
- `InfoDictV2` — v2 info dict with nested `FileTreeNode` file tree. `files()`, `num_pieces()`, `file_piece_ranges()`
- `torrent_v2_from_bytes(data) -> Result<TorrentMetaV2>` — parses v2 .torrent from raw bytes
- `TorrentMeta` enum (`V1`/`V2`/`Hybrid`) — `torrent_from_bytes_any(data)` auto-detects format. `as_v1()`/`as_v2()` accessors work for both pure and hybrid variants
- `TorrentVersion` enum (`V1Only`/`V2Only`/`Hybrid`) — version-aware dispatch throughout session and creation
- `Magnet` — `info_hashes: InfoHashes` (v1 + v2), `selected_files: Option<Vec<FileSelection>>` (BEP 53 `so=`), `info_hash()` method for backward compat. Parses `urn:btih:`, `urn:btmh:`, and `so=`
- `FileSelection` enum (`Single(usize)` / `Range(usize, usize)`) — BEP 53 file selection. `parse(value) -> Result<Vec<FileSelection>>`, `to_priorities(sels, num_files) -> Vec<FilePriority>`, `to_so_value(sels) -> String`
- Info-hash = SHA1 (v1) or SHA-256 (v2) of **raw bencode bytes** of info dict (not re-serialized)

### BEP 52 Hash Coordination (`torrent-core`, M34a)
- `HashRequest` — Merkle tree hash range request: `file_root`, `base` (layer), `index`, `count`, `proof_layers`
- `validate_hash_request(req, file_num_blocks, file_num_pieces) -> bool` — tree geometry bounds check
- `MerkleTreeState` — per-file verification state: stores piece-layer + block-layer hashes, tracks verified blocks
- `SetBlockResult` enum: `Ok` (piece sub-tree verified), `Unknown` (awaiting hashes/siblings), `HashFailed` (bad data)
- `HashPicker` — coordinates hash requests to peers. Priority: block requests > piece-layer requests (512-piece batches)
- `FileHashInfo` — `{ root: Id32, num_blocks: u32, num_pieces: u32 }` for picker initialization
- `AddHashesResult` — `{ valid: bool, hash_passed: Vec<u32>, hash_failed: Vec<u32> }`
- Wire messages: `Message::HashRequest` (ID 21), `Message::Hashes` (ID 22), `Message::HashReject` (ID 23) — 49-byte fixed layout + variable hash array

### Storage (`torrent-storage`)
- `FileMap::new(file_lengths, lengths)` — O(log n) piece-to-file segment mapping
- `TorrentStorage` trait: `write_chunk()`, `read_chunk()`, `read_piece()`, `verify_piece()`, `verify_piece_v2()` (SHA-256), `hash_block()` (per-block SHA-256)
- `ChunkTracker`: v1 chunk tracking + optional v2 block-level Merkle verification (`enable_v2_tracking()`, `mark_block_verified()`, `all_blocks_verified()`)
- `DiskHandle`: `verify_piece_v2()`, `hash_block()` async methods for v2 disk I/O

### BEP 52 Session Integration (`torrent-session/src/torrent.rs`, M34c/M35)
- `TorrentActor` fields: `hash_picker: Option<HashPicker>`, `version: TorrentVersion`, `meta_v2: Option<TorrentMetaV2>`
- `verify_and_mark_piece()` dispatches to v1 (SHA-1), v2 (SHA-256 Merkle), or hybrid (both) path
- `verify_and_mark_piece_hybrid()` — dual verification with `HashResult` tribool decision matrix
- `on_inconsistent_hashes()` — fatal handler when v1/v2 disagree (destroys hash picker, pauses torrent)
- `on_piece_verified()` / `on_piece_hash_failed()` — shared post-verification logic (Have broadcast, completion check, smart banning)
- `handle_hashes_received()` — validates Merkle proof via `HashPicker::add_hashes()`, resolves deferred pieces
- `PeerEvent`/`PeerCommand` v2 variants for hash message exchange (wire IDs 21-23)
- `FastResumeData` v2 fields: `info_hash2` (SHA-256), `trees` (piece-layer hash cache)

### Session Configuration (`torrent-session/src/settings.rs`)
- `Settings` — unified 102-field session configuration (replaces former `SessionConfig`)
- Presets: `Settings::min_memory()`, `Settings::high_performance()`
- `Settings::validate()` — 7 checks (piece size power-of-2, thread counts, proxy config)
- JSON serialization with `#[serde(default = "...")]` for forward-compatible config files
- `From<&Settings>` for `DiskConfig`, `BanConfig`, `TorrentConfig`
- Helper methods: `to_dht_config()`, `to_nat_config()`, `to_utp_config(port)`
- `SessionHandle::settings()` / `apply_settings()` — runtime query and mutation
- Runtime `apply_settings()` updates rate limiters + alert mask immediately; sub-actor reconfig on restart

### Facade (`torrent`)
- `ClientBuilder` — fluent builder → `SessionHandle`. Takes owned `self` for chaining. `into_settings()` returns `Settings`.
- `AddTorrentParams` — `from_torrent()`, `from_magnet()`, `from_file()`, `from_bytes()`
- Re-exports: `torrent/src/core.rs` (core types), `torrent/src/session.rs` (session types), `torrent/src/prelude.rs` (convenience)

### Torrent Creation (`torrent-core/src/create.rs`)
- `CreateTorrent` — owned-self builder: `new()` → `add_file/dir()` → `set_*()` → `generate()`
- `CreateTorrentResult` — `meta: TorrentMeta` + `bytes: Vec<u8>` (raw .torrent file)
- `set_version(TorrentVersion)` — create v1, hybrid, or v2 .torrent files (v2-only not yet supported)
- `auto_piece_size(total) -> u64` — libtorrent-style piece size selection (32 KiB–4 MiB)
- `TorrentOutput` (private) — serializable wrapper for outer .torrent dict (bencode key renames)
- Info hash: serialize `InfoDict` → SHA1 (deterministic via SortedMapSerializer)
- Pad files (BEP 47): `set_pad_file_limit(Option<u64>)` — inserts zero-fill pad entries between files
- Pre-computed hashes: `set_hash(piece, Id20)` skips disk reads during generation

### Error Pattern
- Per-crate `thiserror` enums with `#[from]` for upstream errors
- Facade `torrent::Error` wraps all crate errors: `Core(#[from])`, `Session(#[from])`, `Io(#[from])`, etc.

### Session Sharing Patterns
- `SharedBanManager = Arc<std::sync::RwLock<BanManager>>` — created in `SessionHandle::start()`, cloned to each `TorrentActor`
- `SharedIpFilter = Arc<std::sync::RwLock<IpFilter>>` — same pattern
- Ban/filter checks at 3 connection points: `handle_add_peers()`, `try_connect_peers()`, `spawn_peer_from_stream()`

## License

GPL-3.0-or-later
