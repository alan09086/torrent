# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

## 0.16.0 — 2026-02-26

Alerts / events system with async broadcast channels and category filtering. Nine crates, 437 tests.

### M15: Alerts / Events System

### Added
- `Alert` struct with `SystemTime` timestamp and typed `AlertKind` enum (~30 variants)
- `AlertCategory` bitflags — STATUS, ERROR, PEER, TRACKER, STORAGE, DHT, STATS, PIECE, BLOCK, PERFORMANCE, PORT_MAPPING, ALL
- `AlertStream` — per-subscriber category filter wrapper over `broadcast::Receiver<Alert>`
- `post_alert()` — free function shared by SessionActor and TorrentActor (lockless atomic mask check)
- `SessionHandle::subscribe()` — raw broadcast receiver for all alerts passing session mask
- `SessionHandle::subscribe_filtered(filter)` — convenience wrapper returning `AlertStream`
- `SessionHandle::set_alert_mask()` / `alert_mask()` — runtime category mask control (atomic, no roundtrip)
- `SessionConfig::alert_mask` (default: ALL) and `alert_channel_size` (default: 1024)
- `ClientBuilder::alert_mask()` and `alert_channel_size()` methods
- `transition_state()` on TorrentActor — automatic `StateChanged` alerts on every state transition
- `TrackerOutcome` and `AnnounceResult` — per-tracker announce outcomes for TrackerReply/TrackerError alerts
- Alerts fired at: torrent add/remove/pause/resume/finish, piece complete/hash fail, peer connect/disconnect, tracker reply/error, metadata received, state transitions
- `Serialize` / `Deserialize` derives on `Alert`, `AlertKind`, `AlertCategory`, `TorrentState`, `SessionStats` — ready for MagneTor REST API
- Facade re-exports and prelude additions: `Alert`, `AlertKind`, `AlertCategory`, `AlertStream`
- 16 new tests: 6 alert unit tests, 4 integration tests (multi-subscriber, runtime mask, per-subscriber filter, state tracking), 4 session/torrent alert tests, 2 JSON serialization round-trip tests

## 0.15.0 — 2026-02-26

Bandwidth limiting and automatic upload slot tuning. Nine crates, 422 tests.

### M14: Bandwidth Limiter + Upload Slot Tuning

### Added
- `TokenBucket` — token-bucket rate limiter (bytes/sec, 1-second burst, 100ms refill granularity)
- `is_local_network()` — RFC 1918/loopback/link-local detection for global limiter exemption
- `SlotTuner` — hill-climbing optimizer for automatic unchoke slot count adjustment
- Per-torrent rate limiting: `upload_rate_limit` and `download_rate_limit` on `TorrentConfig` (0 = unlimited)
- Global rate limiting: `upload_rate_limit` and `download_rate_limit` on `SessionConfig` (shared `Arc<Mutex<TokenBucket>>`)
- `auto_upload_slots`, `auto_upload_slots_min`, `auto_upload_slots_max` on `SessionConfig`
- Actor-level gating: download throttled via request pipelining, upload throttled via chunk dispatch
- Local network peers exempt from global limiter (per-torrent still applies)
- `Choker::set_unchoke_slots()` for SlotTuner integration
- 19 new tests: 7 rate_limiter, 6 slot_tuner, 1 choker, 2 types, 3 integration

## 0.14.0 — 2026-02-26

End-game mode for fast piece completion. Nine crates, 403 tests.

### M13: End-Game Mode

### Added
- `EndGame` struct — block-level duplicate-request tracking for fast piece completion
- O(1) activation check: `have + in_flight >= num_pieces`
- Single block per peer in end-game (no pipelining) to maximize coverage
- Cancel messages sent to all other peers on block arrival
- Deactivation on hash failure or download completion
- `strict_end_game` config on `TorrentConfig` (default: true)
- 13 new tests across EndGame unit tests and TorrentActor integration

## 0.13.0 — 2026-02-26

Selective file download with per-file priority control. Nine crates, 390 tests.

### M12: Selective File Download

### Added
- `FilePriority` enum (`Skip`, `Low`, `Normal`, `High`) with `Ord`, `Default` (Normal), serde, `From<u8>` (libtorrent-compatible mapping)
- `PieceSelector::pick()` now accepts a `wanted` bitfield to skip unwanted pieces
- `TorrentHandle::set_file_priority()` — set priority for individual files at runtime
- `TorrentHandle::file_priorities()` — query current per-file priority list
- `build_wanted_pieces()` — compute piece-level wanted bitfield from file priorities and file/piece geometry
- `FilesystemStorage` skips creating files with `Skip` priority (no disk allocation for unwanted files)
- `FastResumeData::file_priority` field — persists per-file priorities across sessions
- Facade re-exports: `ferrite::core::FilePriority`, `ferrite::session::build_wanted_pieces`
- Prelude addition: `FilePriority`
- 16 new tests across ferrite-core (6), ferrite-session (8), ferrite-storage (1), ferrite facade (1)

## 0.12.0 — 2026-02-26

Resume data and session persistence with libtorrent-compatible format. Nine crates, 374 tests.

### M11: Resume Data & Session Persistence

### Added
- `FastResumeData` struct — libtorrent-compatible bencode resume format with all standard fields (pieces, stats, trackers, peers, priorities, rate limits, embedded info dict)
- `UnfinishedPiece` — partial piece state with per-block bitmask
- `TorrentHandle::save_resume_data()` — snapshot torrent state to resume data
- `SessionHandle::save_torrent_resume_data()` — save resume data for a specific torrent
- `SessionHandle::save_session_state()` — save all torrent resume data + DHT node cache
- `SessionState` + `DhtNodeEntry` — session-level persistence types
- `validate_resume_bitfield()` — verify bitfield length matches piece count
- `SessionConfig::resume_data_dir` — configurable resume data directory
- Facade re-exports: `ferrite::core::{FastResumeData, UnfinishedPiece}`, `ferrite::session::{SessionState, DhtNodeEntry, validate_resume_bitfield}`
- Prelude additions: `FastResumeData`, `SessionState`
- 19 new tests across ferrite-core (6), ferrite-session (11), ferrite facade (2)

## 0.11.0 — 2026-02-26

Prelude, unified error, and integration tests complete the M10 facade. Eight crates, 355 tests.

### M10e: ferrite (prelude + unified error + integration tests)

### Added
- `ferrite::Error` — unified error enum wrapping all 7 per-crate error types + `std::io::Error`
- `ferrite::Result<T>` — unified result type alias
- `ferrite::prelude` — convenience re-exports (`ClientBuilder`, `AddTorrentParams`, `SessionHandle`, `TorrentHandle`, `TorrentState`, `TorrentStats`, `TorrentInfo`, `Id20`, `Magnet`, `TorrentMetaV1`, `TorrentStorage`, `FilesystemStorage`, `Error`, `Result`)
- Top-level `ferrite::Error` and `ferrite::Result` re-exports
- 3 facade tests: unified error conversions, prelude imports, full type chain

### Milestone Complete
- **M10 is now complete.** The `ferrite` crate provides a full public facade over all internal crates.

## 0.10.0 — 2026-02-25

Session re-exports and ergonomic client builder in facade crate. Eight crates, 352 tests.

### M10d: ferrite (session + ClientBuilder)

### Added
- `ferrite::session` — re-exports `SessionHandle`, `SessionConfig`, `SessionStats`, `TorrentHandle`, `TorrentConfig`, `TorrentState`, `TorrentStats`, `TorrentInfo`, `FileInfo`, `StorageFactory`, `Error`
- `ferrite::client` — new `ClientBuilder` (fluent builder for `SessionConfig` → `SessionHandle`) and `AddTorrentParams` (unified torrent source with `from_torrent`/`from_magnet`/`from_file`/`from_bytes`)
- Top-level `ferrite::ClientBuilder` and `ferrite::AddTorrentParams` re-exports
- 2 facade tests: client builder defaults + chaining, AddTorrentParams from magnet

## 0.9.0 — 2026-02-25

DHT and storage re-exports in facade crate. Eight crates, 349 tests.

### M10c: ferrite (dht + storage re-exports)

### Added
- `ferrite::dht` — re-exports `DhtHandle`, `DhtConfig`, `DhtStats`, `CompactNodeInfo`, `RoutingTable`, `KrpcMessage`, `KrpcBody`, `KrpcQuery`, `KrpcResponse`, `GetPeersResponse`, `TransactionId`, `parse_compact_nodes`, `encode_compact_nodes`, `PeerStore`, `K`, `Error`
- `ferrite::storage` — re-exports `TorrentStorage`, `Bitfield`, `ChunkTracker`, `FileMap`, `FileSegment`, `MemoryStorage`, `FilesystemStorage`, `Error`
- 2 facade tests: DHT compact node round-trip, storage bitfield operations

## 0.8.0 — 2026-02-25

Wire and tracker re-exports in facade crate. Eight crates, 347 tests.

### M10b: ferrite (wire + tracker re-exports)

### Added
- `ferrite::wire` — re-exports `Handshake`, `Message`, `MessageCodec`, `ExtHandshake`, `ExtMessage`, `MetadataMessage`, `MetadataMessageType`, `allowed_fast_set`, `Error`
- `ferrite::tracker` — re-exports `HttpTracker`, `HttpAnnounceResponse`, `UdpTracker`, `UdpAnnounceResponse`, `AnnounceRequest`, `AnnounceResponse`, `AnnounceEvent`, `parse_compact_peers`, `Error`
- 2 facade tests: handshake round-trip, announce request construction

## 0.7.0 — 2026-02-25

Facade crate scaffold with bencode and core re-exports. Eight crates, 345 tests.

### M10a: ferrite (scaffold + bencode/core re-exports)

### Added
- `ferrite` facade crate with workspace wiring
- `ferrite::bencode` — re-exports `to_bytes`, `from_bytes`, `BencodeValue`, `find_dict_key_span`, `Serializer`, `Deserializer`, `Error`
- `ferrite::core` — re-exports `Id20`, `Id32`, `PeerId`, `Magnet`, `TorrentMetaV1`, `InfoDict`, `FileEntry`, `FileInfo`, `torrent_from_bytes`, `Lengths`, `DEFAULT_CHUNK_SIZE`, `sha1`, `Error`
- 3 facade tests: bencode round-trip, core types, magnet parse

## 0.6.0 — 2026-02-25

Seeding & upload pipeline. Seven crates, 342 tests.

### M9: ferrite-session (seeding & upload pipeline)
- `PeerCommand::SendPiece` — push piece data to peer TCP stream
- `serve_incoming_requests()` — read from storage and dispatch to unchoked peers
- `TorrentState::Seeding` — new state after download completion
- Dual-mode choker: leech mode (download rate tit-for-tat) / seed mode (upload rate)
- Rolling window rate tracking (10s unchoke interval) for download and upload
- Configurable `seed_ratio_limit` on TorrentConfig/SessionConfig — stops torrent when reached
- `verify_existing_pieces()` — scan storage on startup for resume support
- Immediate seeding when all pieces verified on startup

## 0.5.0 — 2026-02-25

Complete M8 gaps: magnet links, LSD actor, AllowedFast, RejectRequest. Seven crates, 337 tests.

### M8c: ferrite-session (magnet/LSD/AllowedFast/RejectRequest)
- `add_magnet()` on `SessionHandle` — magnet link support in session manager
- `MetadataNotReady` error for torrent_info() on magnet torrents pre-BEP 9
- `LsdHandle` + `LsdActor` — full BEP 14 Local Peer Discovery UDP multicast actor
- LSD wired into SessionActor via `tokio::select!` loop with peer routing
- LSD announce triggered on add_torrent/add_magnet
- AllowedFast messages sent on peer connect (Phase 4b, BEP 6)
- Pending requests tracked as `Vec<(u32, u32, u32)>` for precise matching
- Incoming request tracking with `RejectRequest` on choke for fast peers
- `IncomingRequest` PeerEvent variant for forwarding `Message::Request`

## 0.4.0 — 2026-02-25

Session manager, BEP 6 Fast Extension, BEP 14 Local Peer Discovery. Seven crates, 331 tests.

### M8b: ferrite-session (session manager + fast extension + LSD)
- `SessionHandle` + `SessionActor` — multi-torrent session manager (actor model)
- Add/remove/pause/resume torrents, shared DHT, duplicate rejection, capacity limits
- Torrent info/stats queries via SessionHandle
- BEP 6 Fast Extension wire messages: SuggestPiece, HaveAll, HaveNone, RejectRequest, AllowedFast
- BEP 6 `allowed_fast_set()` algorithm (IP /24 mask + SHA1 chain)
- BEP 6 handshake flag (`supports_fast()` / `with_fast()`)
- BEP 6 peer behavior: HaveAll/HaveNone in Phase 4, message handling in peer task
- BEP 14 Local Peer Discovery: announce formatting (batch, MTU-aware), message parsing, rate limiting
- `TorrentState::Paused` with pause/resume on TorrentHandle
- `SessionConfig`, `TorrentInfo`, `FileInfo`, `SessionStats`, `StorageFactory` types

### ferrite-wire
- 5 BEP 6 message variants with encode/decode
- `allowed_fast_set()` public function
- `supports_fast()` / `with_fast()` handshake methods

## 0.3.0 — 2026-02-25

Tracker and DHT integration into TorrentActor. Seven crates, 299 tests.

### M8a: ferrite-session (tracker + DHT integration)
- `TrackerManager` — per-torrent tracker announce lifecycle (BEP 3/12/15)
- BEP 12 announce_list parsing with tier support and URL deduplication
- HTTP and UDP tracker classification with exponential backoff (30s → 30min)
- Tracker re-announce select! arm in TorrentActor (timer-driven, immediate first fire)
- DHT peer receiver select! arm (batched `get_peers` results → `handle_add_peers`)
- DHT re-search on connect timer when peer pool is exhausted
- Completed announce on download finish, Stopped announce on shutdown
- Tracker population on metadata assembly (magnet link flow)
- `from_torrent()` now accepts `dht: Option<DhtHandle>` parameter

## 0.2.0 — 2026-02-25

Session layer with peer management and torrent orchestration. Seven crates, 283 tests.

### M7: ferrite-session (peer + torrent)
- `PieceSelector` — rarest-first piece selection with per-piece availability tracking
- `MetadataDownloader` — BEP 9 metadata download state machine for magnet links (16 KiB pieces, SHA1 verification)
- `PexMessage` — BEP 11 Peer Exchange encode/decode with compact peer format
- `Choker` — tit-for-tat choking: top-N by download rate + optimistic unchoke rotation
- `PeerTask` — async peer connection: handshake, BEP 10 extension negotiation, framed message loop
- `TorrentActor` + `TorrentHandle` — actor model orchestration with select! event loop
- BEP 27 private torrent support (disable DHT/PEX when `info.private = 1`)
- Piece download pipeline: request → write_chunk → verify → mark_verified/mark_failed
- Integration tested with seeder/leecher on loopback TCP

## 0.1.0 — 2026-02-25

Initial development release. Six crates implemented with 231 tests and zero clippy warnings.

### M6: ferrite-storage
- `Bitfield` — compact MSB-first bit-vector matching BEP 3 wire format
- `FileMap` — piece/chunk coordinate to file segment mapping with O(log n) binary search
- `ChunkTracker` — per-piece chunk completion state with `from_bitfield` resume support
- `TorrentStorage` trait — `&self` API (Arc-shared), wire-compatible `(piece, begin, length)` coordinates
- `MemoryStorage` — `RwLock<Vec<u8>>` backend for tests and magnet-link metadata
- `FilesystemStorage` — lazy file handles, sparse file creation, per-file locking for concurrency

### M5: ferrite-dht
- Kademlia DHT implementation (BEP 5)
- Actor model with `DhtHandle` — spawns event loop, returns cloneable async handle
- KRPC message encoding/decoding (ping, find_node, get_peers, announce_peer)
- Routing table with k-buckets (K=8), bucket splitting, stale node detection
- Compact node info (26-byte encode/decode)
- Peer store with SHA1-based token generation/validation and rotating secrets

### M4: ferrite-tracker
- HTTP tracker client (reqwest-based, BEP 3)
- UDP tracker client (BEP 15 connect/announce protocol)
- Compact peer list parsing

### M3: ferrite-wire
- Peer handshake with BEP 5/10 extension flags
- Full BEP 3 message set (choke, unchoke, interested, request, piece, cancel, etc.)
- Extension handshake (BEP 10) and metadata exchange (BEP 9)
- Tokio-util codec for framed message I/O

### M2: ferrite-core
- `Id20`/`Id32` hash types with hex, base32, and XOR distance
- `.torrent` file parsing (`TorrentMetaV1`) with info-hash computation
- Magnet link parsing (BEP 9)
- `Lengths` — piece and chunk arithmetic
- `PeerId` generation (`-FE0100-` prefix)

### M1: ferrite-bencode
- Serde-compatible bencode serializer and deserializer
- `BencodeValue` type for dynamic bencode manipulation
- Zero-copy deserialization
- `find_dict_key_span` for raw byte-range extraction (used for info-hash)
- Sorted key serialization (BEP 3 requirement)
