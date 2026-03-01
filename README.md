# 🧲 Ferrite

A from-scratch Rust BitTorrent library targeting full **libtorrent-rasterbar** feature parity.

Ferrite is a modular workspace of focused crates, each handling one layer of the BitTorrent stack. The goal is a clean, well-tested engine that powers [magnetor](https://codeberg.org/alan090/magnetor) — a qBittorrent replacement built entirely in Rust.

[![Tests](https://img.shields.io/badge/tests-1007-brightgreen)](#-testing)
[![Clippy](https://img.shields.io/badge/clippy-zero%20warnings-brightgreen)](#-testing)
[![Version](https://img.shields.io/badge/version-0.45.0-blue)](#-versioning)
[![License](https://img.shields.io/badge/license-GPL--3.0--or--later-orange)](#-license)
[![Rust](https://img.shields.io/badge/rust-edition%202024-red)](#-building)

---

## ✨ Highlights

- 🏗️ **11-crate modular workspace** — each layer independently testable and reusable
- 🔐 **Full BEP 52 support** — BitTorrent v2 metadata, wire protocol, storage, and hybrid v1+v2 torrents
- ⚡ **Async everything** — tokio-based actor model with async disk I/O, ARC cache, and parallel hashing
- 🌐 **Complete networking** — MSE/PE encryption, uTP (LEDBAT), UPnP/NAT-PMP/PCP, dual-stack IPv6
- 📡 **25 BEPs implemented** — from base protocol (BEP 3) through BitTorrent v2 (BEP 52/53)
- 🎛️ **58-field runtime config** — unified `Settings` struct with presets, JSON serialization, and live updates
- 🧩 **Extension plugin system** — trait-based BEP 10 extension interface for custom protocol extensions
- 📊 **1007 tests, zero clippy warnings**

---

## 🏛️ Architecture

```
ferrite-bencode      🔤 Serde bencode codec (leaf, no deps)
     │
ferrite-core         🔑 Hashes, metainfo (v1+v2), magnets, piece arithmetic
     │
     ├──▶ ferrite-wire       🔌 Peer wire protocol, handshake, BEP 6/9/10/21/52, MSE/PE
     ├──▶ ferrite-tracker    📡 HTTP + UDP tracker announce/scrape
     ├──▶ ferrite-dht        🌐 Kademlia DHT (BEP 5/24), actor model
     │
ferrite-storage      💾 Piece verification (SHA-1 + SHA-256), chunk tracking, mmap, ARC cache
     │
ferrite-session      🎯 Session manager, torrent orchestration, disk I/O actor
     │
ferrite-utp          🚀 uTP (BEP 29) micro transport protocol, LEDBAT congestion
     │
ferrite-nat          🔓 PCP / NAT-PMP / UPnP IGD automatic port mapping
     │
ferrite              📦 Public facade: ClientBuilder + prelude + unified error
```

---

## 📦 Crates

| Crate | Description | Tests |
|-------|-------------|:-----:|
| `ferrite-bencode` | Serde-based bencode serialization with sorted map key ordering | 64 |
| `ferrite-core` | Id20/Id32, TorrentMeta (v1/v2/hybrid), InfoHashes, MerkleTree, Magnet (v1+v2), CreateTorrent, FastResumeData, FilePriority, FileSelection (BEP 53) | 177 |
| `ferrite-wire` | Handshake, Message codec, BEP 6/9/10/21/52 extensions, MSE/PE encryption (RC4 + DH) | 68 |
| `ferrite-tracker` | HTTP (reqwest) + UDP (BEP 15) tracker client, BEP 48 scrape, IPv6 compact peers | 35 |
| `ferrite-dht` | Kademlia DHT with actor model, KRPC, routing table, BEP 24 IPv6 dual-stack, BEP 42 security, BEP 44 data storage, BEP 51 infohash indexing | 148 |
| `ferrite-storage` | Bitfield, FileMap (O(log n) lookup), ChunkTracker (v1+v2), MmapStorage, ARC disk cache | 63 |
| `ferrite-session` | Full session orchestration — see [Session Features](#-session-features) below | 376 |
| `ferrite-utp` | uTP (BEP 29) with LEDBAT congestion control, SACK, retransmission | 21 |
| `ferrite-nat` | PCP (RFC 6887) / NAT-PMP (RFC 6886) / UPnP IGD with auto-renewal | 20 |
| `ferrite` | Public facade: `ClientBuilder` fluent API, `AddTorrentParams`, unified `Error`, `prelude` | 35 |

### 🎯 Session Features

The `ferrite-session` crate (376 tests) includes:

| Category | Features |
|----------|----------|
| **Protocol** | BEP 6 Fast Extension, BEP 9 metadata exchange (bidirectional), BEP 10 extension protocol, BEP 11 PEX, BEP 14 LSD, BEP 16 super seeding, BEP 21 upload-only, BEP 40 canonical peer priority, BEP 52 v2 Merkle verification + hash exchange, BEP 53 magnet `so=` file selection |
| **Transfer** | Rarest-first piece picker, end-game mode, dynamic request queue, file streaming (`AsyncRead` + `AsyncSeek`), sequential download, block-level picking |
| **Bandwidth** | Global + per-torrent token bucket rate limiting, per-class limits (TCP/uTP), automatic upload slot optimization |
| **Storage** | Async DiskActor with write buffering, ARC read cache, mmap backend, parallel hashing, move storage |
| **Networking** | MSE/PE encryption, uTP integration, UPnP/NAT-PMP/PCP, dual-stack IPv6, HTTP/web seeding (BEP 17/19), SOCKS5/HTTP proxy |
| **Management** | Unified Settings (56 fields, runtime updates), alerts/events system, queue management (auto-manage), smart banning + parole, IP filtering (.dat parser) |
| **Persistence** | FastResumeData (bencode), session state, DHT node cache |
| **Extensibility** | Extension plugin trait, share mode, hybrid v1+v2 dual verification, dual-swarm announces, pure v2 torrent support |

---

## 📋 BEP Coverage

| BEP | Description | Status |
|:---:|-------------|:------:|
| 3 | The BitTorrent Protocol | ✅ |
| 5 | DHT (Kademlia) | ✅ |
| 6 | Fast Extension | ✅ |
| 7 | IPv6 Tracker Extension | ✅ |
| 9 | Extension for Peers to Send Metadata Files | ✅ |
| 10 | Extension Protocol | ✅ |
| 11 | Peer Exchange (PEX) | ✅ |
| 12 | Multitracker Metadata Extension | ✅ |
| 14 | Local Service Discovery | ✅ |
| 15 | UDP Tracker Protocol | ✅ |
| 16 | Super Seeding | ✅ |
| 17 | HTTP Seeding (Hoffman-style) | ✅ |
| 19 | WebSeed (GetRight-style) | ✅ |
| 21 | Extension for Partial Seeds | ✅ |
| 24 | Tracker Returns External IP / IPv6 DHT | ✅ |
| 27 | Private Torrents | ✅ |
| 29 | uTP (Micro Transport Protocol) | ✅ |
| 40 | Canonical Peer Priority | ✅ |
| 47 | Pad Files and File Attributes | ✅ |
| 48 | Tracker Protocol Extension: Scrape | ✅ |
| 52 | BitTorrent v2 (SHA-256 Merkle) | ✅ |
| 53 | Magnet URI Extension (select-only) | ✅ |
| 42 | DHT Security Extension | ✅ |
| 44 | Storing Arbitrary Data in the DHT | ✅ |
| 51 | DHT Infohash Indexing | ✅ |
| 55 | Holepunch Extension | 🔜 M40 |

**25 BEPs implemented, 1 planned** — targeting 26 total for full libtorrent-rasterbar parity.

---

## 🏗️ Building

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

Requires Rust edition 2024 support (**rustc 1.85+**).

---

## 🗺️ Roadmap

See [docs/plans/2026-03-01-ferrite-roadmap-v3-full-parity.md](docs/plans/2026-03-01-ferrite-roadmap-v3-full-parity.md) for the full 51-milestone roadmap to complete libtorrent-rasterbar parity. Implementation plans exist for every milestone.

| Phase | Milestones | Focus | Status |
|-------|-----------|-------|:------:|
| Foundation | M1–M10 | Bencode, core types, wire, tracker, DHT, storage, session, facade | ✅ Done |
| 1: Desktop Essentials | M11–M16 | Resume data, selective download, end-game, bandwidth, alerts, queue | ✅ Done |
| 2: Transport & Security | M17–M20 | MSE/PE encryption, uTP, NAT traversal | ✅ Done |
| 3: Protocol Extensions | M21–M24 | IPv6, web seeding, super seeding, tracker scrape | ✅ Done |
| 4: Performance | M25–M28 | Smart banning, async disk + ARC cache, parallel hashing, piece picker | ✅ Done |
| 5: Network & Tools | M29–M32d | IP filter, torrent creation, settings, metadata serving, plugins | ✅ Done |
| 6: BitTorrent v2 | M33–M35 | BEP 52 metadata, wire + storage, hybrid v1+v2 | ✅ Done |
| 7: v2 Completion & DHT | M36–M39 | BEP 53, BEP 42/44/51 DHT hardening | ✅ Done |
| 8: Connectivity & Privacy | M40–M42 | BEP 55 holepunch, I2P (SAM), SSL torrents | 📝 Planned |
| 9: Swarm Intelligence | M43–M46 | Choking algorithms, piece picker, mixed-mode, peer turnover | 📝 Planned |
| 10: Security & Hardening | M47–M48 | SSRF mitigation, DSCP, anonymous mode | 📝 Planned |
| 11: Pluggable Interfaces | M49–M50 | Pluggable disk I/O, session statistics (~100 counters) | 📝 Planned |
| 12: Simulation | M51 | In-process network simulation framework | 📝 Planned |

---

## 🎨 Design Decisions

- **Modular crates** — each layer is independently testable and reusable
- **`thiserror` typed errors** per crate, no `anyhow` — explicit error propagation
- **Rust edition 2024** with workspace resolver 2
- **`bytes::Bytes`** for zero-copy buffer sharing across the wire protocol
- **Actor model** — `SessionActor`/`TorrentActor`/`DhtActor` with `tokio::select!` loops and command channels
- **Async disk I/O** — central `DiskActor` with write buffering, ARC read cache, and semaphore-limited `spawn_blocking`
- **Wire-compatible coordinates** — `(piece, begin, length)` throughout, matching BEP 3 directly
- **Binary search file lookup** — O(log n) piece-to-file mapping via sorted `FileMap`
- **No `rand` dependency** — thread-local xorshift64 seeded from `SystemTime`
- **Deterministic serialization** — `SortedMapSerializer` ensures BEP 3 dict key ordering

---

## 🔖 Versioning

Ferrite uses workspace-level versioning in the root `Cargo.toml`. Each milestone bumps the version:

| Version | Milestone | Highlights |
|---------|-----------|------------|
| 0.45.0 | M39 | BEP 51 DHT infohash indexing, `sample_infohashes` query/response |
| 0.44.0 | M38 | BEP 44 DHT arbitrary data storage, immutable/mutable items, ed25519 |
| 0.43.0 | M37 | BEP 42 DHT security extension, node ID verification, IP voting |
| 0.42.0 | M36 | BEP 53 `so=`, dual-swarm announces, pure v2 support |
| 0.41.0 | M35 | Hybrid v1+v2 torrents, `TorrentVersion` enum |
| 0.40.0 | M34c | BEP 52 session integration, Merkle hash exchange |
| 0.39.0 | M34b | v2 storage, per-block SHA-256 |
| 0.38.0 | M34a | v2 wire protocol, `HashPicker` |
| 0.37.0 | M33 | BEP 52 core, `Id32`, `TorrentMetaV2`, `MerkleTree` |
| 0.36.0 | M32d | Extension plugin interface |

---

## 📄 License

GPL-3.0-or-later — see [LICENSE](LICENSE).
