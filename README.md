# 🧲 Ferrite

A from-scratch Rust BitTorrent library targeting full **libtorrent-rasterbar** feature parity.

Ferrite is a modular workspace of focused crates, each handling one layer of the BitTorrent stack. The goal is a clean, well-tested engine that powers [magnetor](https://codeberg.org/alan090/magnetor) — a qBittorrent replacement built entirely in Rust.

[![Tests](https://img.shields.io/badge/tests-1312-brightgreen)](#-testing)
[![Clippy](https://img.shields.io/badge/clippy-zero%20warnings-brightgreen)](#-testing)
[![Version](https://img.shields.io/badge/version-0.58.0-blue)](#-versioning)
[![License](https://img.shields.io/badge/license-GPL--3.0--or--later-orange)](#-license)
[![Rust](https://img.shields.io/badge/rust-edition%202024-red)](#-building)

---

## ✨ Highlights

- 🏗️ **12-crate modular workspace** — each layer independently testable and reusable
- 🔐 **Full BEP 52 support** — BitTorrent v2 metadata, wire protocol, storage, and hybrid v1+v2 torrents
- ⚡ **Async everything** — tokio-based actor model with async disk I/O, ARC cache, and parallel hashing
- 🌐 **Complete networking** — MSE/PE encryption, uTP (LEDBAT), UPnP/NAT-PMP/PCP, dual-stack IPv6
- 📡 **27 BEPs implemented** — from base protocol (BEP 3) through BitTorrent v2 (BEP 52/53)
- 🎛️ **95-field runtime config** — unified `Settings` struct with presets, JSON serialization, and live updates
- 🧪 **In-process simulation** — pluggable transport + SimNetwork for deterministic swarm integration tests
- 🧩 **Extension plugin system** — trait-based BEP 10 extension interface for custom protocol extensions
- 📊 **1312 tests, zero clippy warnings**

---

## 🚀 Getting Started

Add ferrite to your `Cargo.toml`:

```toml
[dependencies]
ferrite = "0.58.0"
tokio = { version = "1", features = ["full"] }
```

Download a torrent from a magnet link:

```rust,no_run
use ferrite::prelude::*;

#[tokio::main]
async fn main() -> ferrite::Result<()> {
    let session = ClientBuilder::new()
        .download_dir("/tmp/downloads")
        .start()
        .await?;

    let magnet = Magnet::parse("magnet:?xt=urn:btih:...").unwrap();
    let info_hash = session.add_magnet(magnet).await?;

    loop {
        let stats = session.torrent_stats(info_hash).await?;
        if matches!(stats.state, TorrentState::Seeding) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    session.shutdown().await
}
```

See [`examples/`](crates/ferrite/examples/) for more usage patterns including torrent creation, file streaming, and DHT lookups.

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
     │
ferrite-sim          🧪 In-process network simulation: SimNetwork, SimSwarm, virtual clock
```

---

## 📦 Crates

| Crate | Description | Tests |
|-------|-------------|:-----:|
| `ferrite-bencode` | Serde-based bencode serialization with sorted map key ordering | 64 |
| `ferrite-core` | Id20/Id32, TorrentMeta (v1/v2/hybrid), InfoHashes, MerkleTree, Magnet (v1+v2), CreateTorrent, FastResumeData, FilePriority, FileSelection (BEP 53) | 177 |
| `ferrite-wire` | Handshake, Message codec, BEP 6/9/10/21/52 extensions, MSE/PE encryption (RC4 + DH), SSL/TLS transport | 75 |
| `ferrite-tracker` | HTTP (reqwest) + UDP (BEP 15) tracker client, BEP 48 scrape, IPv6 compact peers, SSRF-safe HTTP client | 37 |
| `ferrite-dht` | Kademlia DHT with actor model, KRPC, routing table, BEP 24 IPv6 dual-stack, BEP 42 security, BEP 44 data storage, BEP 51 infohash indexing | 148 |
| `ferrite-storage` | Bitfield, FileMap (O(log n) lookup), ChunkTracker (v1+v2), MmapStorage, ARC disk cache | 65 |
| `ferrite-session` | Full session orchestration — see [Session Features](#-session-features) below | 610 |
| `ferrite-utp` | uTP (BEP 29) with LEDBAT congestion control, SACK, retransmission | 21 |
| `ferrite-nat` | PCP (RFC 6887) / NAT-PMP (RFC 6886) / UPnP IGD with auto-renewal | 20 |
| `ferrite` | Public facade: `ClientBuilder` fluent API, `AddTorrentParams`, unified `Error`, `prelude` | 49 |
| `ferrite-sim` | In-process network simulation: SimClock, SimNetwork, SimTransport, SimSwarm harness | 26 |

### 🎯 Session Features

The `ferrite-session` crate (610 tests) includes:

| Category | Features |
|----------|----------|
| **Protocol** | BEP 6 Fast Extension, BEP 9 metadata exchange (bidirectional), BEP 10 extension protocol, BEP 11 PEX, BEP 14 LSD, BEP 16 super seeding, BEP 21 upload-only, BEP 40 canonical peer priority, BEP 52 v2 Merkle verification + hash exchange, BEP 53 magnet `so=` file selection |
| **Transfer** | Rarest-first piece picker with extent affinity, end-game mode, dynamic request queue, file streaming (`AsyncRead` + `AsyncSeek`), sequential download, auto-sequential hysteresis, block-level picking, SuggestPiece, predictive announce |
| **Bandwidth** | Global + per-torrent token bucket rate limiting, per-class limits (TCP/uTP), mixed-mode TCP/uTP algorithm, automatic upload slot optimization |
| **Storage** | Pluggable DiskIoBackend trait (Posix/Mmap/Disabled), async DiskActor, ARC read cache, write buffering, parallel hashing, move storage |
| **Networking** | MSE/PE encryption, uTP integration, UPnP/NAT-PMP/PCP, dual-stack IPv6, HTTP/web seeding (BEP 17/19), SOCKS5/HTTP proxy, SSL/TLS transport, pluggable transport (NetworkFactory) |
| **Security** | SSRF mitigation (URL guard + redirect policy), IDNA/homograph rejection, HTTPS tracker validation, IP filtering (.dat parser), smart banning + parole |
| **Management** | Unified Settings (95 fields, runtime updates), alerts/events system, queue management (auto-manage), peer turnover |
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
| 55 | Holepunch Extension | ✅ |
| 35 | Torrent Signing / SSL Torrents | ✅ |

**27 BEPs implemented** — targeting full libtorrent-rasterbar parity.

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
| 8: Connectivity & Privacy | M40–M42 | BEP 55 holepunch, I2P (SAM), SSL torrents | ✅ Done |
| 9: Swarm Intelligence | M43–M46 | Choking algorithms, piece picker, mixed-mode, peer turnover | ✅ Done |
| 10: Security & Hardening | M47–M48 | SSRF mitigation, DSCP, anonymous mode | ✅ Done |
| 11: Pluggable Interfaces | M49–M50 | Pluggable disk I/O, session statistics (~100 counters) | ✅ Done |
| 12: Simulation | M51 | In-process network simulation framework | ✅ Done |

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
| 0.58.0 | M52 | API documentation: `#![warn(missing_docs)]` on all 12 crates, example programs, `SessionHandle::open_file()` |
| 0.57.0 | M51 | Network simulation: ferrite-sim crate, NetworkFactory pluggable transport, SimNetwork/SimSwarm, 5 integration tests |
| 0.56.0 | M50 | Session statistics: 70 atomic counters, periodic reporting, SessionStatsAlert, rate computation |
| 0.55.0 | M49 | Pluggable disk I/O: DiskIoBackend trait, PosixDiskIo, MmapDiskIo, DisabledDiskIo, backend factory |
| 0.54.0 | M48 | DSCP/ToS socket marking, anonymous mode hardening, ext handshake suppression |
| 0.53.0 | M47 | SSRF mitigation, IDNA/homograph rejection, HTTPS tracker validation, URL guard module |
| 0.52.0 | M46 | Peer turnover: periodic replacement of underperforming peers, peak rate cutoff, 4 exemptions |
| 0.51.0 | M45 | Mixed-mode TCP/uTP bandwidth algorithm, auto-sequential hysteresis, peer transport tracking |
| 0.50.0 | M44 | Piece picker: extent affinity, SuggestPiece mode, predictive announce, PeerSpeedClassifier |
| 0.49.0 | M43 | Pluggable choking algorithms (FixedSlots/RateBased), seed-mode strategies (FastestUpload/RoundRobin/AntiLeech) |
| 0.48.0 | M42 | BEP 35 SSL torrents, TLS transport, SNI-based routing, self-signed certs |
| 0.47.0 | M41 | I2P anonymous network support via SAM v3.1 protocol |
| 0.46.0 | M40 | BEP 55 holepunch extension, NAT traversal, relay logic |
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
