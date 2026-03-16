# 🧲 Torrent

A from-scratch Rust BitTorrent library targeting full **libtorrent-rasterbar** feature parity.

Torrent is a modular workspace of focused crates, each handling one layer of the BitTorrent stack. The goal is a clean, well-tested engine that powers [MagneTor](https://codeberg.org/alan090/magnetor) -- a qBittorrent replacement built entirely in Rust.

[![Tests](https://img.shields.io/badge/tests-1505-brightgreen)](#testing)
[![Clippy](https://img.shields.io/badge/clippy-zero%20warnings-brightgreen)](#testing)
[![Version](https://img.shields.io/badge/version-0.100.0-blue)](#versioning)
[![License](https://img.shields.io/badge/license-GPL--3.0--or--later-orange)](#license)
[![Rust](https://img.shields.io/badge/rust-edition%202024-red)](#building)

---

## ✨ Highlights

- 🏗️ **12-crate modular workspace** -- each layer independently testable and reusable
- 🔐 **Full BEP 52 support** -- BitTorrent v2 metadata, wire protocol, storage, and hybrid v1+v2 torrents
- ⚡ **Async everything** -- tokio-based actor model with async disk I/O, ARC cache, and parallel hashing
- 🌐 **Complete networking** -- MSE/PE encryption, uTP (LEDBAT), UPnP/NAT-PMP/PCP, dual-stack IPv6
- 📡 **27 BEPs implemented** -- from base protocol (BEP 3) through BitTorrent v2 (BEP 52/53)
- 🚀 **56.8 MB/s average throughput** -- 1.3x gap to rqbit, AIMD pipeline, write coalescing, lock-free dispatch
- 🎛️ **106-field runtime config** -- unified `Settings` struct with presets, JSON serialization, and live updates
- 🧪 **In-process simulation** -- pluggable transport + SimNetwork for deterministic swarm integration tests
- 🧩 **Extension plugin system** -- trait-based BEP 10 extension interface for custom protocol extensions
- 📊 **1,527 tests, zero clippy warnings**

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

CLI options: `--sequential`, `--no-dht`, `--seed`, `--port`, `--quiet`, `--config` (JSON settings file), `--workers`, `--no-pin-cores`, and `--log-level`.

### 📚 Library

To use torrent as a library in your own project, add it to your `Cargo.toml`:

```toml
[dependencies]
torrent = "0.100.0"
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
| `torrent-bencode` | Serde-based bencode serialization with sorted map key ordering | 66 |
| `torrent-core` | Id20/Id32, TorrentMeta (v1/v2/hybrid), InfoHashes, MerkleTree, Magnet (v1+v2), CreateTorrent, FastResumeData, FilePriority, FileSelection (BEP 53) | 183 |
| `torrent-wire` | Handshake, Message codec, BEP 6/9/10/21/52 extensions, MSE/PE encryption (RC4 + DH), SSL/TLS transport | 93 |
| `torrent-tracker` | HTTP (reqwest) + UDP (BEP 15) tracker client, BEP 48 scrape, IPv6 compact peers, SSRF-safe HTTP client | 42 |
| `torrent-dht` | Kademlia DHT with actor model, KRPC, routing table, BEP 24 IPv6 dual-stack, BEP 42 security, BEP 44 data storage, BEP 51 infohash indexing | 160 |
| `torrent-storage` | Bitfield, FileMap (O(log n) lookup), ChunkTracker (v1+v2), MmapStorage, ARC disk cache | 66 |
| `torrent-session` | Full session orchestration -- see [Session Features](#-session-features) below | 763 |
| `torrent-utp` | uTP (BEP 29) with LEDBAT congestion control, SACK, retransmission | 24 |
| `torrent-nat` | PCP (RFC 6887) / NAT-PMP (RFC 6886) / UPnP IGD with auto-renewal | 20 |
| `torrent` | Public facade: `ClientBuilder` fluent API, `AddTorrentParams`, unified `Error`, `prelude` | 59 |
| `torrent-sim` | In-process network simulation: SimClock, SimNetwork, SimTransport, SimSwarm harness | 30 |
| `torrent-cli` | CLI binary: download, create, info subcommands | 10 |

### 🎯 Session Features

The `torrent-session` crate (763 tests) includes:

| Category | Features |
|----------|----------|
| **Protocol** | BEP 6 Fast Extension, BEP 9 metadata exchange (bidirectional), BEP 10 extension protocol, BEP 11 PEX, BEP 14 LSD, BEP 16 super seeding, BEP 21 upload-only, BEP 40 canonical peer priority, BEP 52 v2 Merkle verification + hash exchange, BEP 53 magnet `so=` file selection |
| **Transfer** | Rarest-first piece picker with extent affinity, end-game mode with batch refill, AIMD pipeline with per-peer congestion control, write coalescing (~32x fewer disk syscalls), lock-free piece dispatch, file streaming (`AsyncRead` + `AsyncSeek`), sequential download, auto-sequential hysteresis, block-level picking, SuggestPiece, predictive announce |
| **Bandwidth** | Global + per-torrent token bucket rate limiting, per-class limits (TCP/uTP), mixed-mode TCP/uTP algorithm, automatic upload slot optimization |
| **Storage** | Pluggable DiskIoBackend trait (Posix/Mmap/Disabled), async DiskActor, ARC read cache, write buffering, write coalescing, parallel hashing (HashPool), bounded store buffer with back-pressure, move storage |
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

Benchmarked against the Arch Linux ISO (~1.45 GiB, well-seeded).

### Head-to-head: torrent v0.98.1 vs rqbit 8.1.1

Same magnet, same machine, back-to-back trials (2026-03-15):

| Metric | torrent v0.98.1 | rqbit 8.1.1 | Ratio | Status |
|--------|:-:|:-:|:-:|:-:|
| **Speed** | 31.8 MB/s | 72.6 MB/s | 0.44x | Gap |
| **CPU time** | 9.8s | 10.6s | 0.92x | Parity |
| **Ctx switches** | 114K | 161K | 0.71x | Winning |
| **RSS** | 256 MiB | 54 MiB | 4.7x | Gap |
| **Page faults** | 226K | 12K | 18.5x | Biggest gap |

**Analysis:** CPU efficiency is at parity — the M92-M98 optimization stack closed that gap entirely. Context switches are now _lower_ than rqbit. The remaining speed gap is driven by **peer discovery latency** (DHT bootstrap overhead) and **memory overhead** (18x more page faults = cache pressure). Next optimizations should target RSS and page fault reduction.

### Version history

| Version | Avg Speed | CPU Time | RSS | Ctx Switches | CPU Migrations | Page Faults | Notes |
|---------|-----------|----------|-----|:------------:|:--------------:|:-----------:|-------|
| **0.100.0** | 36.4 MB/s | 17.9s | 46-73 MiB | 633K | — | 118K | Direct per-block pwrite, deferred write queue, verify from disk |
| 0.99.0 | — | — | ~48 MiB bounded | — | — | — | PieceBufferPool: hard-bounded write pipeline (32 × 512 KiB + 32 MiB store buffer) |
| 0.98.1 | 31.8 MB/s | 9.8s | 256 MiB | 114K | 645 | 226K | Write coalescer buffer reuse fix, store buffer back-pressure |
| 0.98.0 | 31.8 MB/s | 9.8s | 256 MiB | 114K | 645 | 226K | Write coalescing, cold-start |
| 0.95.0 | 66.9 MB/s | -- | ~133 MiB | -- | 141 | -- | Core affinity (warm-state) |
| 0.84.0 | 55.7 MB/s | 12.5s | 107 MiB | -- | -- | -- | AWS-LC crypto (warm-state) |
| 0.83.0 | 56.8 MB/s | 13.1s | 68.8 MiB | -- | -- | -- | AIMD depth 128 (warm-state) |

### Progress vs profiling baseline

Profiling baseline: v0.84.0 cold-start (2026-03-14):

| Metric | Baseline | v0.98.0 | Change |
|--------|----------|---------|--------|
| CPU time | 15.0s | 9.8s | **-35%** |
| Ctx switches | 462K | 114K | **-75%** |
| CPU migrations | 140K | 645 | **-99.5%** |
| Cold-start reliability | ~30% | 100% (10/10) | DHT hardening |

### Optimization stack (M85-M98)

| Version | Optimization | Measured Impact |
|---------|-------------|-----------------|
| 0.100.0 | Direct per-block pwrite — deferred write queue replaces WriteCoalescer/StoreBuffer/PieceBufferPool | RSS 46-73 MiB, ~1100 lines deleted, simpler architecture |
| 0.99.0 | PieceBufferPool — semaphore-gated reusable BytesMut buffers, 32 concurrent pieces | Memory hard-bounded ~48 MiB (was unbounded with peer count) |
| 0.98.1 | Write coalescer buffer reuse — `copy_from_slice` + `clear()`, store buffer sync fallback | Eliminates ~3 GiB alloc churn on large torrents |
| 0.98.0 | Write coalescing — per-peer block buffering, single pwrite per piece | ~32x fewer write syscalls |
| 0.97.0 | DHT cold-start hardening — bootstrap gate, V6 backoff, 5s pings | 10/10 cold-start reliability |
| 0.96.0 | Parallel piece verification — HashPool dedicated thread pool | SHA1 off main thread |
| 0.95.0 | Core affinity — tokio workers pinned to CPU cores | CPU migrations 140K→645 |
| 0.94.0 | Memory footprint — bounded StoreBuffer, codec shrinking, cache 64→16 MiB | RSS reduction |
| 0.93.0 | Lock-free piece dispatch — atomic CAS, zero locks on hot path | Dispatch latency reduction |
| 0.92.0 | Peer event batching — PendingBatch, 25ms flush timer | Ctx switches 462K→114K |
| 0.85.0 | DHT routing table overhaul — iterative bootstrap, node liveness | DHT reliability foundation |

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
- **Async disk I/O** -- central `DiskActor` with write coalescing, ARC read cache, and parallel hashing (HashPool)
- **Wire-compatible coordinates** -- `(piece, begin, length)` throughout, matching BEP 3 directly
- **Binary search file lookup** -- O(log n) piece-to-file mapping via sorted `FileMap`
- **`aws-lc-rs` cryptography** -- AWS-LC (BoringSSL fork) for SHA-1/SHA-256, with `ring` and `openssl` as optional backends
- **No `rand` dependency** -- thread-local xorshift64 seeded from `SystemTime`
- **Deterministic serialization** -- `SortedMapSerializer` ensures BEP 3 dict key ordering
- **AIMD pipeline** -- per-peer congestion control with slow-start, additive increase, multiplicative decrease
- **Lock-free hot path** -- atomic CAS piece dispatch, per-peer coalesced writes, bounded store buffer with back-pressure
- **Hard-bounded memory** -- `PieceBufferPool` (semaphore-gated reusable buffers) caps the write pipeline at ~48 MiB regardless of peer count

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
| Sim Integration Tests | M91 | End-to-end SimTransport transfer tests, partition recovery, dead code cleanup | ✅ |
| Peer Event Batching | M92 | PendingBatch block accumulation, 25ms flush timer, ~3x context switch reduction | ✅ |
| Lock-Free Dispatch | M93 | Atomic CAS reservation, AvailabilitySnapshot rarest-first, PeerSlab arena, zero locks on hot path | ✅ |
| Memory Optimization | M94 | Bounded StoreBuffer (32 MiB back-pressure), codec shrinking on idle, disk cache 64→16 MiB | ✅ |
| Core Affinity | M95 | Pin tokio workers to CPU cores, round-robin assignment, `--workers`/`--no-pin-cores` CLI flags | ✅ |
| Parallel Hashing | M96 | HashPool dedicated thread pool for SHA1, per-torrent result channels, generation counter | ✅ |
| DHT Cold-Start | M97 | Bootstrap completion gate, node-count gate (≥8 nodes), V6 exponential backoff, 5s pings | ✅ |
| Write Coalescing | M98 | Per-peer block buffering, ~32x fewer disk syscalls, split store-buffer/coalesced-write path | ✅ |
| Write Coalescer Fix | v0.98.1 | Buffer reuse via `copy_from_slice` + `clear()`, store buffer sync fallback | ✅ |
| Piece Buffer Pool | M99 | `PieceBufferPool` — semaphore-gated reusable `BytesMut` buffers, hard-bounded ~48 MiB write pipeline | ✅ |

**Versioning:** `0.X.0` = milestone MX. Non-milestone patches use `0.X.1`.

---

## 📄 License

GPL-3.0-or-later -- see [LICENSE](LICENSE).
