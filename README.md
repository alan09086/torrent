# Torrent

A from-scratch Rust BitTorrent engine targeting full **libtorrent-rasterbar** feature parity.

[![Tests](https://img.shields.io/badge/tests-1663-brightgreen)](#testing)
[![Clippy](https://img.shields.io/badge/clippy-zero%20warnings-brightgreen)](#testing)
[![Version](https://img.shields.io/badge/version-0.114.0-blue)](#versioning)
[![License](https://img.shields.io/badge/license-GPL--3.0--or--later-orange)](#license)
[![Rust](https://img.shields.io/badge/rust-edition%202024-red)](#building)

12-crate modular workspace. 26 BEPs. ~78K lines of Rust. 1,663 tests. Zero clippy warnings.

---

## Highlights

- **Full BitTorrent v1 + v2** -- BEP 52 metadata, Merkle verification, v2-only + hybrid torrent creation, BEP 53 file selection
- **26 BEPs implemented** -- from base protocol (BEP 3) through holepunch (BEP 55)
- **Async actor architecture** -- tokio-based `SessionActor`/`TorrentActor`/`DhtActor` with `select!` loops and command channels
- **Pluggable everything** -- crypto backends (AWS-LC/ring/OpenSSL), disk I/O backends (POSIX/mmap/disabled), transport (TCP/uTP/SimTransport)
- **106-field runtime config** -- unified `Settings` struct with presets, JSON serialization, and live hot-reload
- **In-process simulation** -- deterministic swarm testing via `SimNetwork` with virtual clock and configurable link properties
- **Extension plugin system** -- trait-based BEP 10 extension interface

---

## Quick Start

### CLI

```bash
# Download from magnet link
torrent download "magnet:?xt=urn:btih:..." -o /tmp/downloads

# Download from .torrent file
torrent download ./ubuntu.torrent -o /tmp/downloads

# Create a .torrent file
torrent create ./my-file.tar.gz -t http://tracker.example.com/announce -o my-file.torrent

# Display torrent metadata
torrent info my-file.torrent

# List contents without downloading
torrent download ./ubuntu.torrent --list
```

**CLI flags:** `--seed`, `--no-dht`, `--quiet`, `--port`, `--workers`, `--no-pin-cores`, `--overwrite`, `--config` (JSON settings), `--initial-peers`, `--disable-trackers`, `--log-level`.

### Library

```toml
[dependencies]
torrent = "0.110.0"
tokio = { version = "1", features = ["full"] }
```

See [`examples/`](crates/torrent/examples/) for usage patterns: downloading, torrent creation, file streaming, and DHT lookups.

---

## Architecture

```
torrent-bencode       Serde bencode codec (leaf crate, no deps)
    |
torrent-core          Hashes, metainfo (v1/v2/hybrid), magnets, piece arithmetic, torrent creation
    |
    |--- torrent-wire       Peer wire protocol, handshake, BEP 6/9/10/21/52, MSE/PE encryption
    |--- torrent-tracker    HTTP + UDP tracker client, BEP 48 scrape, SSRF-safe
    |--- torrent-dht        Kademlia DHT, KRPC, BEP 5/24/42/44/51, actor model
    |
torrent-storage       Piece verification (SHA-1 + SHA-256), FileMap, chunk tracking, mmap, ARC cache
    |
torrent-session       Session orchestration, disk I/O actor, hash pool, peer management
    |
torrent-utp           uTP (BEP 29), LEDBAT congestion control, SACK
    |
torrent-nat           PCP / NAT-PMP / UPnP IGD with auto-renewal
    |
torrent               Public facade: ClientBuilder + prelude + unified Error
    |
torrent-sim           In-process network simulation: SimNetwork, SimSwarm, virtual clock
    |
torrent-cli           CLI binary: download, create, info subcommands
```

### Crate Details

| Crate | Description | Tests |
|-------|-------------|------:|
| `torrent-bencode` | Serde-based bencode serialization with sorted map key ordering (BEP 3) | 66 |
| `torrent-core` | Id20/Id32, TorrentMeta (v1/v2/hybrid), InfoHashes, MerkleTree, Magnet, CreateTorrent, Sha1Hasher, FastResumeData, FilePriority, FileSelection | 175 |
| `torrent-wire` | Handshake, Message codec, BEP 6/9/10/21/52, MSE/PE (RC4 + DH), SSL/TLS | 93 |
| `torrent-tracker` | HTTP (reqwest) + UDP (BEP 15), BEP 48 scrape, IPv6 compact peers, SSRF guard | 42 |
| `torrent-dht` | Kademlia with KRPC, routing table, BEP 24 IPv6, BEP 42 security, BEP 44 storage, BEP 51 indexing | 160 |
| `torrent-storage` | Bitfield, FileMap (O(log n)), ChunkTracker (v1+v2), SmallVec segments, MmapStorage, ARC cache | 69 |
| `torrent-session` | Session management, disk I/O, hash pool, peer orchestration -- see below | 856 |
| `torrent-utp` | uTP (BEP 29) with LEDBAT congestion, SACK, retransmission | 24 |
| `torrent-nat` | PCP (RFC 6887) / NAT-PMP (RFC 6886) / UPnP IGD auto port mapping | 20 |
| `torrent` | `ClientBuilder` fluent API, `AddTorrentParams`, unified `Error`, `prelude` module | 59 |
| `torrent-sim` | SimClock, SimNetwork, SimTransport, SimSwarm for deterministic testing | 30 |
| `torrent-cli` | CLI binary with progress display, SIGTERM handling, DHT persistence | 10 |

### Session Features (`torrent-session`)

| Category | Features |
|----------|----------|
| **Protocol** | BEP 6 Fast Extension, BEP 9 metadata exchange, BEP 10 extension protocol, BEP 11 PEX, BEP 14 LSD, BEP 16 super seeding, BEP 21 upload-only, BEP 52 v2 Merkle verification + hash exchange + v2-only creation, BEP 53 `so=` file selection |
| **Transfer** | Rarest-first piece picker with extent affinity, end-game mode, fixed-depth pipeline (Semaphore(128) per peer), lock-free piece dispatch (atomic CAS), file streaming (`AsyncRead` + `AsyncSeek`), sequential download with auto-hysteresis, SuggestPiece, predictive announce |
| **Disk I/O** | Pluggable `DiskIoBackend` (POSIX/mmap/disabled), async DiskActor, deferred write queue with batch `spawn_blocking` (64 jobs/call), ARC read cache, streaming piece verification (64 KiB buffer, `Sha1Hasher`), parallel hashing (`HashPool` with `Data`/`Streaming` variants) |
| **Bandwidth** | Global + per-torrent token bucket rate limiting, per-class limits (TCP/uTP), mixed-mode algorithm, automatic upload slot optimization |
| **Networking** | MSE/PE encryption, uTP integration, UPnP/NAT-PMP/PCP, dual-stack IPv6, HTTP/web seeding (BEP 17/19), SOCKS5/HTTP proxy, SSL/TLS transport, I2P (SAM), pluggable transport (`NetworkFactory`) |
| **Security** | SSRF mitigation (URL guard + redirect policy), IDNA/homograph rejection, HTTPS tracker validation, IP filtering (.dat parser), smart banning + parole, BEP 42 DHT security |
| **Management** | 106-field `Settings` with runtime hot-reload, alerts/events system, queue management (auto-manage), peer turnover |
| **Persistence** | FastResumeData (bencode), DHT node cache, session state |

---

## BEP Coverage

| BEP | Description | Status |
|:---:|-------------|:------:|
| 3 | The BitTorrent Protocol | Done |
| 5 | DHT (Kademlia) | Done |
| 6 | Fast Extension | Done |
| 7 | IPv6 Tracker Extension | Done |
| 9 | Extension for Peers to Send Metadata Files | Done |
| 10 | Extension Protocol | Done |
| 11 | Peer Exchange (PEX) | Done |
| 12 | Multitracker Metadata Extension | Done |
| 14 | Local Service Discovery | Done |
| 15 | UDP Tracker Protocol | Done |
| 16 | Super Seeding | Done |
| 17 | HTTP Seeding (Hoffman-style) | Done |
| 19 | WebSeed (GetRight-style) | Done |
| 21 | Extension for Partial Seeds | Done |
| 24 | Tracker Returns External IP / IPv6 DHT | Done |
| 27 | Private Torrents | Done |
| 29 | uTP (Micro Transport Protocol) | Done |
| 35 | Torrent Signing / SSL Torrents | Done |
| ~~40~~ | ~~Canonical Peer Priority~~ | Removed (superseded by peer scoring M106) |
| 42 | DHT Security Extension | Done |
| 44 | Storing Arbitrary Data in the DHT | Done |
| 47 | Pad Files and File Attributes | Done |
| 48 | Tracker Protocol Extension: Scrape | Done |
| 51 | DHT Infohash Indexing | Done |
| 52 | BitTorrent v2 (SHA-256 Merkle) | Done |
| 53 | Magnet URI Extension (select-only) | Done |
| 55 | Holepunch Extension | Done |

---

## Performance

Benchmarked against the Arch Linux ISO (~1.45 GiB, well-seeded). All trials are cold-start (DHT bootstrap from scratch).

### v0.101.0 (10 trials, cold-start, 2026-03-16)

```
Speed:  42.4 +/- 8.1 MB/s  (min 33.7, max 53.6)
CPU:     8.7 +/- 1.9s
RSS:      51 +/- 12 MiB
Ctx sw: 131K avg
Page f:  44K avg
```

| Metric | v0.100.0 | v0.101.0 | rqbit 8.1.1 | Notes |
|--------|:--------:|:--------:|:-----------:|-------|
| **CPU time** | 10.5s | **8.7s** | 8.9s | Parity achieved |
| **Ctx switches** | 355K | **131K** | 118K | 1.1x (was 3.0x) |
| **RSS** | 52 MiB | **51 MiB** | 38 MiB | 1.3x (was 1.4x) |
| **Speed** | 51 MB/s | **42.4 MB/s** | 82.5 MB/s | Swarm-dependent variance |

CPU parity with rqbit. Context switches reduced 63% (from 3x gap to 1.1x). Speed variance is DHT bootstrap latency, not a regression -- the 53.6 MB/s peak exceeds v0.100.0's average.

### Optimization Stack (M85--M109)

The performance work spans 24 milestones of profiler-driven optimization:

| Version | Optimization | Impact |
|---------|-------------|--------|
| 0.114.0 | Listener task extraction -- TCP/uTP accept loops moved from SessionActor select! to dedicated ListenerTask, FuturesUnordered concurrent identification, 5s preamble timeout, DashMap info_hash_registry, removed per-connection HashMap clone | +9 tests (1663 total) |
| 0.113.0 | BEP 52 V2-only torrent creation -- `build_v2_output()` helper extraction, V2Only skips SHA-1 entirely, `HashPicker::load_piece_layers()` for session-layer V2 verification, sim transfer test | +6 tests (1654 total) |
| 0.112.0 | BEP 55 holepunch initiation + cold-start optimization -- holepunch wired on NAT connect failures (sync buffer pattern, 120s cooldown), DHT re-query delay 60s→5s for magnets, adaptive cap during metadata fetch | +8 tests (1648 total) |
| 0.111.0 | BEP compliance sweep -- BEP 27 private torrents now disable LSD (4 session guard sites), BEP 40 dead code removed, BEP 51 client-side `sample_infohashes` wired with session timer | -7 tests net (1640 total), ~200 lines net deleted |
| 0.110.0 | Zero-copy piece pipeline -- `Message<B>` generic over buffer type, three-phase borrowed decode (`fill_message`/`try_decode`/`advance`), direct synchronous pwrite from ring slices, vectored write for ring-wrap blocks | +16 tests (1647 total), target: page faults <15K, heap allocs <5K, ≥65 MB/s |
| 0.109.0 | Ring buffer codec -- fixed 32 KiB ReadBuf replaces FramedRead, pre-allocated PeerWriter replaces FramedWrite, zero-copy DoubleBufHelper for wrap-boundary parsing | +21 tests, eliminates page faults from BytesMut growth/shrink |
| 0.108.0 | Full PEX + page fault reduction -- bidirectional PEX send-side (BEP 11), Have batching default 100ms, hot-path pre-allocation, connection stats logging | +9 tests, target: peers >500, page faults <15K |
| 0.107.0 | Aggressive peer pipeline -- semaphore-paced admission (`peer_adder_task`), TCP+uTP parallel race, parallel metadata fetch, adaptive DHT re-query, max_peers 200, unconditional Unchoke | +20 tests, ~570 lines deleted |
| 0.106.0 | Peer scoring system -- composite score (bandwidth/RTT/reliability/availability), phase-aware turnover, score-based admission, hybrid snub eviction | 36.9 MB/s mean (10 trials), +20 tests |
| 0.105.0 | DHT reliability & simplification -- routing table node cap, two-phase ping, background DNS backoff, unified iterative lookup, JSON persistence | ~90 lines removed, 12x steady-state traffic reduction |
| 0.104.0 | Fixed-depth pipeline & connection overhaul -- AIMD→Semaphore(128), fixed 500ms connect, per-peer backoff | Eliminates ~430 lines pipeline complexity |
| 0.103.0 | Per-block stealing & reactive dispatch -- BlockMaps, StealCandidates, 3-phase dispatch | Eliminates legacy steal code, 50ms reactive snapshots |
| 0.102.0 | Unified buffer pool (libtorrent 1.x) -- hash-from-cache, full-piece prefetch, T2 suggest | Eliminates write->read->hash round-trip, 64 MiB unified cache |
| 0.101.0 | Allocation hotspots -- SmallVec segments, batch writer, streaming verify | 111K allocs eliminated, 93K->~1.5K spawns, 262 KiB/piece alloc eliminated |
| 0.100.0 | Direct per-block pwrite -- deferred write queue | RSS 46-73 MiB, ~1100 lines deleted |
| 0.98.0 | Write coalescing -- per-peer block buffering | ~32x fewer write syscalls |
| 0.97.0 | DHT cold-start hardening | 10/10 cold-start reliability |
| 0.96.0 | Parallel piece verification -- HashPool thread pool | SHA1 off main thread |
| 0.95.0 | Core affinity -- tokio workers pinned to CPU cores | CPU migrations 140K->645 |
| 0.94.0 | Memory footprint -- bounded StoreBuffer, cache 64->16 MiB | RSS reduction |
| 0.93.0 | Lock-free piece dispatch -- atomic CAS, zero locks on hot path | Dispatch latency reduction |
| 0.92.0 | Peer event batching -- 25ms flush timer | Ctx switches 462K->114K |
| 0.85.0 | DHT routing table overhaul -- iterative bootstrap, node liveness | Reliability foundation |

### Version History

| Version | Avg Speed | CPU Time | RSS | Ctx Switches | Page Faults | Notes |
|---------|-----------|----------|-----|:------------:|:-----------:|-------|
| **0.101.0** | 42.4 MB/s | 8.7s | 51 MiB | 131K | 44K | SmallVec, batch writer, streaming verify |
| 0.100.0 | 36.4 MB/s | 17.9s | 46-73 MiB | 633K | 118K | Direct pwrite, deferred queue |
| 0.98.1 | 31.8 MB/s | 9.8s | 256 MiB | 114K | 226K | Write coalescer fix |
| 0.95.0 | 66.9 MB/s | — | ~133 MiB | — | — | Core affinity (warm-state) |
| 0.84.0 | 55.7 MB/s | 12.5s | 107 MiB | — | — | AWS-LC crypto (warm-state) |
| 0.83.0 | 56.8 MB/s | 13.1s | 68.8 MiB | — | — | AIMD depth 128 (warm-state) |

---

## Building

### Prerequisites

- **Rust 1.85+** (edition 2024) -- install via [rustup](https://rustup.rs/)
- **C compiler + cmake** -- required by `aws-lc-sys` (AWS-LC cryptography)

```bash
# Debian / Ubuntu
sudo apt install build-essential cmake

# Fedora / RHEL
sudo dnf install gcc cmake

# Arch Linux
sudo pacman -S base-devel cmake

# macOS
xcode-select --install && brew install cmake
```

Windows: install [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) with C++ workload + [CMake](https://cmake.org/download/).

### Build and Run

```bash
git clone https://codeberg.org/alan090/torrent.git
cd torrent
cargo build --release

# Run
./target/release/torrent download "magnet:?xt=urn:btih:..." -o /tmp/downloads

# Test
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

### Crypto Backends

The default crypto backend is **AWS-LC** (`aws-lc-rs`). Alternative backends can be selected via Cargo features:

| Feature | Backend | Notes |
|---------|---------|-------|
| `crypto-aws-lc` (default) | AWS-LC (BoringSSL fork) | Best performance, requires cmake |
| `crypto-ring` | ring | Pure Rust + asm, no cmake needed |
| `crypto-openssl` | OpenSSL | System OpenSSL, requires pkg-config |

---

## Design Decisions

- **Modular crates** -- each layer independently testable and reusable; `torrent-bencode` has zero dependencies
- **`thiserror` typed errors** per crate, no `anyhow` -- explicit error propagation with `#[from]` chaining
- **`bytes::Bytes`** for zero-copy buffer sharing across wire protocol and disk I/O
- **Actor model** -- each subsystem runs its own `tokio::select!` loop with typed command channels
- **Wire-compatible coordinates** -- `(piece, begin, length)` throughout, matching BEP 3 directly
- **Binary search file lookup** -- O(log n) piece-to-file mapping via sorted `FileMap` with SmallVec segments
- **No `rand` dependency** -- thread-local xorshift64 seeded from `SystemTime`
- **Deterministic serialization** -- `SortedMapSerializer` ensures BEP 3 dict key ordering for correct info hashes
- **Fixed-depth pipeline** -- per-peer Semaphore(128) with EWMA throughput tracking for snub detection
- **Lock-free hot path** -- atomic CAS piece dispatch, batched writes, streaming verification
- **Streaming verification** -- `Sha1Hasher` incremental API avoids full-piece allocation; hash pool supports both pre-read (`Data`) and streaming (`Streaming`) verification modes

---

## Roadmap

All 51 libtorrent-rasterbar parity milestones are complete. Post-parity work (M55--M114) focuses on performance optimization, DHT reliability, wire-level efficiency, and session architecture. See [docs/plans/](docs/plans/) for the full roadmap and per-milestone implementation plans.

| Phase | Milestones | Focus | Status |
|-------|-----------|-------|:------:|
| Foundation | M1-M10 | Bencode, core types, wire, tracker, DHT, storage, session, facade | Done |
| Desktop Essentials | M11-M16 | Resume data, selective download, end-game, bandwidth, alerts, queue | Done |
| Transport and Security | M17-M20 | MSE/PE encryption, uTP, NAT traversal | Done |
| Protocol Extensions | M21-M24 | IPv6, web seeding, super seeding, tracker scrape | Done |
| Performance | M25-M28 | Smart banning, async disk + ARC cache, parallel hashing, piece picker | Done |
| Network and Tools | M29-M32d | IP filter, torrent creation, settings, metadata serving, plugins | Done |
| BitTorrent v2 | M33-M35 | BEP 52 metadata, wire + storage, hybrid v1+v2 | Done |
| v2 Completion and DHT | M36-M39 | BEP 53, BEP 42/44/51 DHT hardening | Done |
| Connectivity and Privacy | M40-M42 | BEP 55 holepunch, I2P (SAM), SSL torrents | Done |
| Swarm Intelligence | M43-M46 | Choking algorithms, piece picker, mixed-mode, peer turnover | Done |
| Security and Hardening | M47-M48 | SSRF mitigation, DSCP, anonymous mode | Done |
| Pluggable Interfaces | M49-M50 | Pluggable disk I/O, session statistics (~100 counters) | Done |
| Simulation | M51 | In-process network simulation framework | Done |
| API Parity | M52-M53 | API documentation, full torrent operations API | Done |
| Speed Optimization | M55-M104 | Dispatch architecture, pipeline tuning, CPU efficiency, unified buffer pool, block stealing | Done |
| DHT Reliability | M105 | Routing table cap, two-phase ping, background DNS backoff, unified lookup, JSON persistence | Done |
| Peer Pipeline | M106-M107 | Peer scoring system, semaphore-paced admission, TCP+uTP race, parallel metadata fetch | Done |
| Wire Optimization | M108-M110 | Full PEX send-side, Have batching, hot-path pre-allocation, ring buffer codec, zero-copy piece pipeline | Done |
| Protocol Compliance | M111-M113 | BEP compliance sweep, holepunch initiation, V2-only torrent creation | Done |
| Session Architecture | M114 | Listener task extraction -- TCP/uTP accept loops in dedicated spawned task | Done |

**Versioning:** `0.X.0` = milestone MX. Non-milestone patches use `0.X.1`.

---

## Testing

```bash
cargo test --workspace                      # 1,631 tests
cargo clippy --workspace -- -D warnings     # Zero warnings
```

The test suite covers unit tests, integration tests, and simulation-based end-to-end transfer tests. The `torrent-sim` crate provides deterministic swarm testing with `SimNetwork` -- configurable latency, bandwidth, and partition scenarios without touching real networks.

---

## License

GPL-3.0-or-later -- see [LICENSE](LICENSE).
