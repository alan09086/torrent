# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

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
