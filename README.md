# 🧲 Torrent

A from-scratch Rust BitTorrent library targeting full **libtorrent-rasterbar** feature parity.

Torrent is a modular workspace of focused crates, each handling one layer of the BitTorrent stack. The goal is a clean, well-tested engine that powers [MagneTor](https://codeberg.org/alan090/magnetor) -- a qBittorrent replacement built entirely in Rust.

[![Tests](https://img.shields.io/badge/tests-1476-brightgreen)](#testing)
[![Clippy](https://img.shields.io/badge/clippy-zero%20warnings-brightgreen)](#testing)
[![Version](https://img.shields.io/badge/version-0.88.0-blue)](#versioning)
[![License](https://img.shields.io/badge/license-GPL--3.0--or--later-orange)](#license)
[![Rust](https://img.shields.io/badge/rust-edition%202024-red)](#building)

---

## ✨ Highlights

- 🏗️ **12-crate modular workspace** -- each layer independently testable and reusable
- 🔐 **Full BEP 52 support** -- BitTorrent v2 metadata, wire protocol, storage, and hybrid v1+v2 torrents
- ⚡ **Async everything** -- tokio-based actor model with async disk I/O, ARC cache, and parallel hashing
- 🌐 **Complete networking** -- MSE/PE encryption, uTP (LEDBAT), UPnP/NAT-PMP/PCP, dual-stack IPv6
- 📡 **27 BEPs implemented** -- from base protocol (BEP 3) through BitTorrent v2 (BEP 52/53)
- 🚀 **56.8 MB/s average throughput** -- 1.3x gap to rqbit, AIMD pipeline, peer-integrated dispatch
- 🎛️ **106-field runtime config** -- unified `Settings` struct with presets, JSON serialization, and live updates
- 🧪 **In-process simulation** -- pluggable transport + SimNetwork for deterministic swarm integration tests
- 🧩 **Extension plugin system** -- trait-based BEP 10 extension interface for custom protocol extensions
- 📊 **1476 tests, zero clippy warnings**

---

## 🚀 Usage

```bash
# Download a torrent from a magnet link
torrent download "magnet:?xt=urn:btih:..." -o /tmp/downloads

# Download from a .torrent file
torrent download ./ubuntu.torrent -o /tmp/downloads

# Create a .torrent file
torrent create ./my-file.tar.gz -t http://tracker.example.com/announce -o my-file.torrent

# Display torrent metadata
torrent info my-file.torrent
```

CLI options: `--sequential`, `--no-dht`, `--seed`, `--port`, `--quiet`, `--config` (JSON settings file), and `--log-level`.

### 📚 Library

To use torrent as a library in your own project, add it to your `Cargo.toml`:

```toml
[dependencies]
torrent = "0.85.0"
tokio = { version = "1", features = ["full"] }
```

See [`examples/`](crates/torrent/examples/) for usage patterns including downloading, torrent creation, file streaming, and DHT lookups.

---

## 🏛️ Architecture

```
torrent-bencode      🔤 Serde bencode codec (leaf, no deps)
     │
torrent-core         🔑 Hashes, metainfo (v1+v2), magnets, piece arithmetic
     │
     ├──▶ torrent-wire       🔌 Peer wire protocol, handshake, BEP 6/9/10/21/52, MSE/PE
     ├──▶ torrent-tracker    📡 HTTP + UDP tracker announce/scrape
     ├──▶ torrent-dht        🌐 Kademlia DHT (BEP 5/24), actor model
     │
torrent-storage      💾 Piece verification (SHA-1 + SHA-256), chunk tracking, mmap, ARC cache
     │
torrent-session      🎯 Session manager, torrent orchestration, disk I/O actor
     │
torrent-utp          🚀 uTP (BEP 29) micro transport protocol, LEDBAT congestion
     │
torrent-nat          🔓 PCP / NAT-PMP / UPnP IGD automatic port mapping
     │
torrent              📦 Public facade: ClientBuilder + prelude + unified error
     │
torrent-sim          🧪 In-process network simulation: SimNetwork, SimSwarm, virtual clock
```

---

## 📦 Crates

| Crate | Description | Tests |
|-------|-------------|:-----:|
| `torrent-bencode` | Serde-based bencode serialization with sorted map key ordering | 64 |
| `torrent-core` | Id20/Id32, TorrentMeta (v1/v2/hybrid), InfoHashes, MerkleTree, Magnet (v1+v2), CreateTorrent, FastResumeData, FilePriority, FileSelection (BEP 53) | 183 |
| `torrent-wire` | Handshake, Message codec, BEP 6/9/10/21/52 extensions, MSE/PE encryption (RC4 + DH), SSL/TLS transport | 91 |
| `torrent-tracker` | HTTP (reqwest) + UDP (BEP 15) tracker client, BEP 48 scrape, IPv6 compact peers, SSRF-safe HTTP client | 40 |
| `torrent-dht` | Kademlia DHT with actor model, KRPC, routing table, BEP 24 IPv6 dual-stack, BEP 42 security, BEP 44 data storage, BEP 51 infohash indexing | 141 |
| `torrent-storage` | Bitfield, FileMap (O(log n) lookup), ChunkTracker (v1+v2), MmapStorage, ARC disk cache | 66 |
| `torrent-session` | Full session orchestration -- see [Session Features](#session-features) below | 672 |
| `torrent-utp` | uTP (BEP 29) with LEDBAT congestion control, SACK, retransmission | 24 |
| `torrent-nat` | PCP (RFC 6887) / NAT-PMP (RFC 6886) / UPnP IGD with auto-renewal | 20 |
| `torrent` | Public facade: `ClientBuilder` fluent API, `AddTorrentParams`, unified `Error`, `prelude` | 54 |
| `torrent-sim` | In-process network simulation: SimClock, SimNetwork, SimTransport, SimSwarm harness | 26 |
| `torrent-cli` | CLI binary: download, create, info subcommands | 61 |

### 🎯 Session Features

The `torrent-session` crate (733 tests) includes:

| Category | Features |
|----------|----------|
| **Protocol** | BEP 6 Fast Extension, BEP 9 metadata exchange (bidirectional), BEP 10 extension protocol, BEP 11 PEX, BEP 14 LSD, BEP 16 super seeding, BEP 21 upload-only, BEP 40 canonical peer priority, BEP 52 v2 Merkle verification + hash exchange, BEP 53 magnet `so=` file selection |
| **Transfer** | Rarest-first piece picker with extent affinity, end-game mode with batch refill, AIMD pipeline with per-peer congestion control, file streaming (`AsyncRead` + `AsyncSeek`), sequential download, auto-sequential hysteresis, block-level picking, SuggestPiece, predictive announce |
| **Bandwidth** | Global + per-torrent token bucket rate limiting, per-class limits (TCP/uTP), mixed-mode TCP/uTP algorithm, automatic upload slot optimization |
| **Storage** | Pluggable DiskIoBackend trait (Posix/Mmap/Disabled), async DiskActor, ARC read cache, write buffering, parallel hashing, move storage |
| **Networking** | MSE/PE encryption, uTP integration, UPnP/NAT-PMP/PCP, dual-stack IPv6, HTTP/web seeding (BEP 17/19), SOCKS5/HTTP proxy, SSL/TLS transport, pluggable transport (NetworkFactory) |
| **Security** | SSRF mitigation (URL guard + redirect policy), IDNA/homograph rejection, HTTPS tracker validation, IP filtering (.dat parser), smart banning + parole |
| **Management** | Unified Settings (106 fields, runtime updates), alerts/events system, queue management (auto-manage), peer turnover |
| **Persistence** | FastResumeData (bencode), session state, DHT node cache |
| **Extensibility** | Extension plugin trait, share mode, hybrid v1+v2 dual verification, dual-swarm announces, pure v2 torrent support |

---

## 📋 BEP Coverage

| BEP | Description | Status |
|:---:|-------------|:------:|
| 3 | The BitTorrent Protocol | ✅ |
| 5| DHT (Kademlia) | ✅ |
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
| 35 | Torrent Signing / SSL Torrents | ✅ |
| 40 | Canonical Peer Priority | ✅ |
| 42 | DHT Security Extension | ✅ |
| 44 | Storing Arbitrary Data in the DHT | ✅ |
| 47 | Pad Files and File Attributes | ✅ |
| 48 | Tracker Protocol Extension: Scrape | ✅ |
| 51 | DHT Infohash Indexing | ✅ |
| 52 | BitTorrent v2 (SHA-256 Merkle) | ✅ |
| 53 | Magnet URI Extension (select-only) | ✅ |
| 55 | Holepunch Extension | ✅ |

**27 BEPs implemented** -- targeting full libtorrent-rasterbar parity.

---

## ⚡ Performance

Benchmarked against the Arch Linux ISO (~1.45 GiB, well-seeded), 3 trials per version:

| Version | Avg Speed | Peak | CPU Time | RSS | Notes |
|---------|-----------|------|----------|-----|-------|
| **0.88.0** | — | — | — | — | I2P session integration: outbound SAM connects, BEP 7 tracker announces, PEX filtering |
| 0.87.0 | — | — | — | — | BEP 44 session API (DHT storage put/get through SessionHandle) |
| 0.86.0 | — | — | — | — | BEP 52 hash serving for v2/hybrid seeders |
| 0.85.0 | — | — | — | — | DHT routing table overhaul: iterative bootstrap, node liveness, background pinger, periodic persistence |
| 0.84.0 | 55.7 MB/s | 67.3 MB/s | 12.5s | 107 MiB | AWS-LC crypto backend (-28% CPU, -23% RSS vs ring) |
| 0.83.0 | 56.8 MB/s | 83.5 MB/s | 13.1s | 68.8 MiB | Encryption disabled, AIMD depth 128, SIGTERM handling |
| 0.82.0 | 38.6 MB/s | 62.1 MB/s | 18.4s | 88.9 MiB | Adaptive max_in_flight, cached dispatch, AIMD pipeline |
| 0.77.0 | 54.2 MB/s | -- | -- | -- | Wake storm elimination, batch drain 512 |
| 0.75.0 | 56.7 MB/s | -- | -- | -- | Peer-integrated dispatch (4.6x over M74) |

Reference: rqbit achieves ~74.8 MB/s on the same workload.

---

## 🏗️ Building

### Prerequisites

- **Rust 1.85+** (edition 2024) -- install via [rustup](https://rustup.rs/)
- **C compiler + cmake** -- required by the `aws-lc-sys` cryptography crate (AWS-LC)

#### Linux

```bash
# Debian / Ubuntu
sudo apt install build-essential cmake

# Fedora / RHEL
sudo dnf install gcc cmake

# Arch Linux
sudo pacman -S base-devel cmake
```

#### macOS

```bash
# Xcode command line tools (includes clang) + cmake
xcode-select --install
brew install cmake
```

#### Windows

Install [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) with the "C++ build tools" workload, and install [CMake](https://cmake.org/download/). The MSVC toolchain (`x86_64-pc-windows-msvc`) is recommended.

### Build

```bash
# Clone and build
git clone https://codeberg.org/alan090/torrent.git
cd torrent
cargo build --release

# Run the CLI
cargo run --release -p torrent-cli -- download "magnet:?xt=urn:btih:..." -o /tmp/downloads

# Run tests and lints
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

The release binary is at `target/release/torrent-cli`.

---

## 🎨 Design Decisions

- **Modular crates** -- each layer is independently testable and reusable
- **`thiserror` typed errors** per crate, no `anyhow` -- explicit error propagation
- **Rust edition 2024** with workspace resolver 2
- **`bytes::Bytes`** for zero-copy buffer sharing across the wire protocol
- **Actor model** -- `SessionActor`/`TorrentActor`/`DhtActor` with `tokio::select!` loops and command channels
- **Async disk I/O** -- central `DiskActor` with write buffering, ARC read cache, and semaphore-limited `spawn_blocking`
- **Wire-compatible coordinates** -- `(piece, begin, length)` throughout, matching BEP 3 directly
- **Binary search file lookup** -- O(log n) piece-to-file mapping via sorted `FileMap`
- **`aws-lc-rs` cryptography** -- AWS-LC (BoringSSL fork) for SHA-1/SHA-256, with `ring` and `openssl` as optional backends
- **No `rand` dependency** -- thread-local xorshift64 seeded from `SystemTime`
- **Deterministic serialization** -- `SortedMapSerializer` ensures BEP 3 dict key ordering
- **AIMD pipeline** -- per-peer congestion control with slow-start, additive increase, multiplicative decrease

---

## 🗺️ Roadmap

See [docs/plans/](docs/plans/) for the full milestone roadmap and per-milestone implementation plans.

All 51 parity milestones are complete. Post-parity work focuses on performance optimization:

| Phase | Milestones | Focus | Status |
|-------|-----------|-------|:------:|
| Foundation | M1-M10 | Bencode, core types, wire, tracker, DHT, storage, session, facade | ✅ |
| Desktop Essentials | M11-M16 | Resume data, selective download, end-game, bandwidth, alerts, queue | ✅ |
| Transport & Security | M17-M20 | MSE/PE encryption, uTP, NAT traversal | ✅ |
| Protocol Extensions | M21-M24 | IPv6, web seeding, super seeding, tracker scrape | ✅ |
| Performance | M25-M28 | Smart banning, async disk + ARC cache, parallel hashing, piece picker | ✅ |
| Network & Tools | M29-M32d | IP filter, torrent creation, settings, metadata serving, plugins | ✅ |
| BitTorrent v2 | M33-M35 | BEP 52 metadata, wire + storage, hybrid v1+v2 | ✅ |
| v2 Completion & DHT | M36-M39 | BEP 53, BEP 42/44/51 DHT hardening | ✅ |
| Connectivity & Privacy | M40-M42 | BEP 55 holepunch, I2P (SAM), SSL torrents | ✅ |
| Swarm Intelligence | M43-M46 | Choking algorithms, piece picker, mixed-mode, peer turnover | ✅ |
| Security & Hardening | M47-M48 | SSRF mitigation, DSCP, anonymous mode | ✅ |
| Pluggable Interfaces | M49-M50 | Pluggable disk I/O, session statistics (~100 counters) | ✅ |
| Simulation | M51 | In-process network simulation framework | ✅ |
| API Parity | M52-M53 | API documentation, full torrent operations API parity | ✅ |
| Speed Optimization | M55-M83 | DHT persistence, dispatch architecture, pipeline tuning, CPU efficiency | ✅ |
| Crypto Backend | M84 | AWS-LC default, pluggable crypto (ring/openssl/aws-lc feature flags) | ✅ |
| DHT Overhaul | M85 | Iterative bootstrap, node liveness, background pinger, rate limiting, periodic persistence | ✅ |
| Audit Remediation | M86-M87 | Dead code removal, BEP 52 hash serving for v2/hybrid seeders | ✅ |
| BEP 44 Session API | M88 | DHT storage put/get through SessionHandle with alert firing | ✅ |
| I2P Integration | M90 | Outbound SAM connects, BEP 7 tracker announces, PEX mixed-mode filtering | ✅ |

---

## 📄 License

GPL-3.0-or-later -- see [LICENSE](LICENSE).
