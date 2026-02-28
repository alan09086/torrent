# Ferrite

Rust BitTorrent library targeting libtorrent-rasterbar + rqbit streaming parity.

## Build & Test

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

## Architecture

11-crate workspace: `ferrite-bencode` → `ferrite-core` → `ferrite-wire`/`ferrite-tracker`/`ferrite-dht`/`ferrite-storage`/`ferrite-utp`/`ferrite-nat` → `ferrite-session` → `ferrite` (facade)

- **ferrite-session**: Actor model — `SessionActor`/`TorrentActor` with `tokio::select!` loops and command channels
- **ferrite** (facade): `ClientBuilder` fluent API, `AddTorrentParams`, unified `Error`, `prelude` module

## Conventions

- Edition 2024, workspace resolver = "2"
- `thiserror` typed errors per crate, no `anyhow`
- `bytes::Bytes` throughout (not custom wrappers)
- Manual big-endian serialization for wire protocol
- Random bytes: thread-local xorshift64 seeded from SystemTime (avoids `rand` dep)
- Sorted map serializer for BEP 3 key ordering
- KRPC: `BTreeMap<Vec<u8>, BencodeValue>` for encoding, pattern-match on "y" field for decoding

## Milestones

- 35-milestone roadmap: `docs/plans/2026-02-26-ferrite-roadmap-v2.md`
- Commit format: `feat: description (M24)` — milestone tag in parentheses
- Version bumps: workspace version in root `Cargo.toml`, bump with each milestone
- Plan files: `docs/plans/YYYY-MM-DD-ferrite-<milestone>-<topic>.md`

## Remotes

- `origin` → Codeberg, `github` → GitHub
- Push to BOTH on every push: `git push origin main && git push github main`

## Key Types & Patterns Reference

### Bencode Serialization (`ferrite-bencode`)
- `to_bytes<T: Serialize>(value) -> Vec<u8>` / `from_bytes<T: Deserialize>(bytes) -> T`
- `SortedMapSerializer`: buffers all key-value pairs, sorts by key bytes, writes in order — ensures BEP 3 dict key ordering automatically
- `find_dict_key_span(data, "info") -> Range<usize>`: finds raw byte span of a dict key's value (used for info-hash computation)
- `BencodeValue` enum: `Integer(i64)`, `Bytes(Vec<u8>)`, `List(Vec<_>)`, `Dict(BTreeMap<Vec<u8>, _>)`
- **Bencode has no null** — `serialize_none()` returns error. All `Option` fields on serializable structs must use `#[serde(skip_serializing_if = "Option::is_none")]`

### Core Types (`ferrite-core`)
- `Id20([u8; 20])` — SHA1 hash (info hash, piece hash). Methods: `from_hex()`, `to_hex()`, `as_bytes()`
- `Id32([u8; 32])` — SHA-256 (for future BEP 52)
- `sha1(data: &[u8]) -> Id20` — uses `sha1` crate (v0.10)
- `Lengths { total_length, piece_length, chunk_size }` — piece arithmetic: `num_pieces()`, `piece_size(idx)`, `piece_offset(idx)`
- `DEFAULT_CHUNK_SIZE = 16384`

### Torrent Metadata (`ferrite-core/src/metainfo.rs`)
- `TorrentMetaV1` — NOT a serde struct (manually constructed). Fields: info_hash, announce, announce_list, comment, created_by, creation_date, info, url_list, httpseeds
- `InfoDict` — `Deserialize + Serialize`. Fields: name, piece_length (renamed), pieces (serde_bytes), length (Option), files (Option), private (Option<i64>), source (Option<String>)
- `FileEntry` — `Deserialize + Serialize`. Fields: length, path (Vec<String>), attr (Option<String>, BEP 47), mtime (Option<i64>), symlink_path (Option<Vec<String>>)
- `torrent_from_bytes(data) -> Result<TorrentMetaV1>` — parses .torrent from raw bytes, computes info_hash via `find_dict_key_span`
- Info-hash = SHA1 of **raw bencode bytes** of info dict (not re-serialized). For creation: serialize `InfoDict` → hash the output (deterministic via SortedMapSerializer)

### Storage (`ferrite-storage`)
- `FileMap::new(file_lengths, lengths)` — O(log n) piece-to-file segment mapping
- `TorrentStorage` trait: `write_chunk()`, `read_chunk()`, `read_piece()`, `verify_piece()`

### Facade (`ferrite`)
- `ClientBuilder` — fluent builder → `SessionHandle`. Takes owned `self` for chaining.
- `AddTorrentParams` — `from_torrent()`, `from_magnet()`, `from_file()`, `from_bytes()`
- Re-exports: `ferrite/src/core.rs` (core types), `ferrite/src/session.rs` (session types), `ferrite/src/prelude.rs` (convenience)

### Torrent Creation (`ferrite-core/src/create.rs`)
- `CreateTorrent` — owned-self builder: `new()` → `add_file/dir()` → `set_*()` → `generate()`
- `CreateTorrentResult` — `meta: TorrentMetaV1` + `bytes: Vec<u8>` (raw .torrent file)
- `auto_piece_size(total) -> u64` — libtorrent-style piece size selection (32 KiB–4 MiB)
- `TorrentOutput` (private) — serializable wrapper for outer .torrent dict (bencode key renames)
- Info hash: serialize `InfoDict` → SHA1 (deterministic via SortedMapSerializer)
- Pad files (BEP 47): `set_pad_file_limit(Option<u64>)` — inserts zero-fill pad entries between files
- Pre-computed hashes: `set_hash(piece, Id20)` skips disk reads during generation

### Error Pattern
- Per-crate `thiserror` enums with `#[from]` for upstream errors
- Facade `ferrite::Error` wraps all crate errors: `Core(#[from])`, `Session(#[from])`, `Io(#[from])`, etc.

### Session Sharing Patterns
- `SharedBanManager = Arc<std::sync::RwLock<BanManager>>` — created in `SessionHandle::start()`, cloned to each `TorrentActor`
- `SharedIpFilter = Arc<std::sync::RwLock<IpFilter>>` — same pattern
- Ban/filter checks at 3 connection points: `handle_add_peers()`, `try_connect_peers()`, `spawn_peer_from_stream()`

## License

GPL-3.0-or-later
