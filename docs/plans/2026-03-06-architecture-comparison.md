# BitTorrent Library Architecture Comparison

**torrent (Rust) vs rqbit (Rust) vs libtorrent-rasterbar (C++)**

*Date: 2026-03-06*

---

## 1. Overview

| Dimension | torrent | rqbit | libtorrent |
|-----------|---------|-------|------------|
| **Language** | Rust | Rust | C++14 |
| **Started** | 2025 | 2022 | 2003 |
| **License** | GPL-3.0 | Custom/Other | BSD |
| **Codebase** | ~31.5k LOC, 12 crates | ~14 crates + Tauri app | ~102k LOC, monolithic lib |
| **Tests** | 1,386 | Not documented | 108 unit + 32 simulation + fuzzers |
| **BEPs** | 27 | ~16 | 30+ |
| **v2 Torrents** | Full (BEP 52) | No | Full (BEP 52, first implementer) |
| **Encryption** | MSE/PE (RC4) | None | MSE/PE (RC4) |
| **uTP** | LEDBAT (BEP 29) | CUBIC (not LEDBAT) | LEDBAT (BEP 29) |
| **Max Observed Speed** | 116 MB/s | Claims 20 Gbps link saturation | ~150 MB/s (tuned Deluge) |
| **Primary Users** | Alan's MagneTor client | Standalone CLI/desktop | qBittorrent, Deluge, many others |

---

## 2. Architecture & Modularity

### 2.1 Crate/Module Organization

**torrent** uses a **12-crate Rust workspace** with strict layered dependencies:

```
bencode -> core -> wire/tracker/dht/storage/utp/nat -> session -> facade + sim
```

Each crate is independently usable. You can pull in `torrent-dht` without the session layer, or `torrent-bencode` as a standalone codec. This enables downstream projects (MagneTor) to depend on specific layers without dragging in the full stack.

**rqbit** uses a **14-crate workspace** with a similar philosophy but less rigorous layering. The main `librqbit` crate is a monolith containing session, storage, HTTP API, peer logic, chunk tracking, and streaming — roughly equivalent to torrent's session + storage + facade combined. Protocol crates (`peer_binary_protocol`, `dht`, `tracker_comms`, `bencode`) are properly separated.

**libtorrent** is a **single C++ library** (~102k lines) with logical module separation via directory structure but no independent compilation units. Everything links into one `libtorrent.so`. Internal headers (`aux_/`) provide some encapsulation, but the two largest classes — `session_impl` (7.4k lines) and `torrent` (12.4k lines) — are monolithic god-objects.

**Verdict**: torrent's layered workspace is the cleanest separation. rqbit is middle ground. libtorrent's monolithic structure is a known weakness that 20+ years of organic growth have compounded.

### 2.2 Pluggability & Extension Points

| Extension Point | torrent | rqbit | libtorrent |
|----------------|---------|-------|------------|
| Disk I/O backend | `DiskIoBackend` trait (Posix, Mmap, Disabled) | `TorrentStorage` + `StorageFactory` traits (FS, Mmap) + middleware stack | `disk_interface` (mmap, posix, disabled) |
| Choking algorithm | `ChokerStrategy` trait (FixedSlots, RateBased, seed variants) | No pluggable choker | `choking_algorithm` setting (fixed_slots, rate_based) + seed variants |
| Extension protocol | `ExtensionPlugin` trait | Hardcoded (ut_metadata, ut_pex) | `plugin` base class with full lifecycle hooks |
| Transport layer | `NetworkFactory` trait (TCP, SimNetwork) | Hardcoded TCP + uTP | Socket type abstraction (TCP, uTP, SSL, I2P, SOCKS5) |
| Piece picking | Configurable via `PickContext` (sequential, streaming, rarest-first, stealing) | File-priority ordered, no trait | 7 modes built into monolithic picker |
| Storage middleware | No middleware layer | `write_through_cache`, `timing`, `slow` composable wrappers | Part files, store buffer (built-in) |

torrent and libtorrent offer comparable extensibility through different mechanisms (Rust traits vs C++ virtual classes). rqbit's middleware pattern for storage is elegant but the rest of the system is less pluggable.

---

## 3. Concurrency Model

This is the most consequential architectural divergence between the three implementations.

### 3.1 torrent: Actor Model (tokio)

```
SessionActor
  |-- TorrentActor x N (one per torrent)
  |     |-- PeerTask x M (one per peer connection)
  |     |-- TrackerManager
  |-- DiskActor (shared, one per session)
  |-- DhtHandle (external actor)
  |-- LsdActor
```

- **Communication**: `mpsc` channels with typed command/event enums
- **Shared state**: `Arc<RwLock>` for ban manager, IP filter; `Arc<AtomicU32>` for stats
- **No deadlocks**: Actors use `tokio::select!` with no nested lock acquisition
- **Parallelism**: Multiple torrent actors run truly concurrently across tokio's thread pool

**Key property**: Work is distributed across all CPU cores. Each actor processes its own event loop, so a slow torrent doesn't block others.

### 3.2 rqbit: Shared-State + Task Spawning (tokio)

```
Arc<Session>  (central shared state behind RwLock)
  |-- TCP/uTP listeners
  |-- Per-torrent: speed_estimator, peer_adder, upload_scheduler
  |-- Per-peer: manage_peer task (network I/O + chunk requesting)
```

- **Communication**: Shared state mutations + `Notify` signals, not message passing
- **Synchronization**: `DashMap` (sharded concurrent hashmap) for peer states, `RwLock` for torrent state, `Semaphore` for connection limits
- **Shutdown**: `CancellationToken` hierarchy

**Key property**: Simpler mental model than actors, but `RwLock` contention can become a bottleneck under high peer counts. The session-level `RwLock` is acquired frequently.

### 3.3 libtorrent: Single Network Thread + Worker Pools (Asio)

```
Network Thread (single, runs Asio io_context)
  |-- ALL protocol logic, piece picking, choking, peer management
  |-- Posts disk jobs to:
  |     |-- Disk I/O thread pool (default 10 threads)
  |     |-- Hashing thread pool (default 1 thread)
  |-- Receives callbacks from disk threads
User Thread (application)
  |-- Communicates via message posting to network thread
```

- **Communication**: Asio post() for cross-thread calls, double-buffered alerts for events
- **Locks**: Almost none on hot path — single-threaded network eliminates contention
- **Batch operations**: Corking (write merging), job batching (`submit_jobs()`)

**Key property**: Zero lock contention on the network thread. But all protocol logic is serialized on one core — this becomes the ceiling at extreme scale.

### 3.4 Concurrency Comparison

| Aspect | torrent | rqbit | libtorrent |
|--------|---------|-------|------------|
| **Model** | Actor (message passing) | Shared state + tasks | Single-threaded + worker pools |
| **Hot-path locks** | None (actor-isolated) | RwLock, DashMap | None (single-threaded) |
| **CPU utilization** | Multi-core (actors distribute) | Multi-core (tasks distribute) | Single-core network + multi-core disk |
| **Scalability ceiling** | Channel backpressure | Lock contention | Single-thread CPU saturation |
| **Complexity** | Medium (typed channels) | Low (shared references) | Low (no concurrency in protocol) |
| **Deadlock risk** | None (no nested locks) | Low (documented ordering) | None (single-threaded) |

**Analysis**: libtorrent's single-threaded model is brilliant for eliminating concurrency bugs but caps protocol throughput to one core. torrent's actor model achieves the same lock-free property while distributing work across cores — the best of both worlds, at the cost of channel management complexity. rqbit's shared-state model is the simplest to reason about but the least scalable.

---

## 4. Piece Selection

The piece picker is arguably the most important algorithm in a BitTorrent client. It directly determines download speed, swarm health, and completion time.

### 4.1 Algorithm Comparison

| Feature | torrent | rqbit | libtorrent |
|---------|---------|-------|------------|
| **Rarest-first** | Yes (availability vector) | **No** | Yes (swap-based bucket sort) |
| **Sequential mode** | Yes | No (file-order only) | Yes |
| **Streaming priority** | Yes (FileStream integration) | Yes (HTTP streaming) | Yes (time-critical deadlines) |
| **End-game mode** | Yes (4 req/peer, reactive cancels) | **No** | Yes (single redundant block) |
| **Extent affinity** | Yes (contiguous piece preference) | No | Yes (4 MiB extents) |
| **Piece stealing** | Yes (10x threshold, M56) | Yes (10x + 3x adaptive) | No (but anti-snub reallocation) |
| **Suggested pieces** | Yes (BEP 6) | No (no Fast Extension) | Yes |
| **Speed-class affinity** | Yes (PeerSpeedClassifier) | No | Yes (snubbed → reverse order) |
| **Partial piece preference** | Yes (in-flight tracking) | N/A (one peer per piece) | Yes (priority = avail*2 + !partial) |
| **Initial random mode** | No | No | Yes (first 4 pieces random) |
| **Auto-sequential** | Yes (hysteresis threshold) | No | No |

### 4.2 Data Structure

**libtorrent** uses a carefully optimized **contiguous vector with swap-based bucket boundaries**:
- O(1) availability increment/decrement (swap + boundary adjust)
- Lazy rebuilds on peer churn (dirty flag, batch rebuild at pick time)
- Zero heap allocation during normal operation
- Cache-friendly linear memory layout

**torrent** uses a **per-piece availability vector** with sorting at pick time:
- O(n) scan filtered by PickContext constraints
- Rich context (peer speed, bitfield, in-flight state, streaming, wanted mask)
- More features per pick operation but less optimized data structure

**rqbit** has no availability tracking at all — just a queue of pieces ordered by file priority.

### 4.3 Swarm Health Impact

Rarest-first is not just a performance optimization — it's critical for **swarm health**. Without it:
- Common pieces propagate while rare pieces die when their sole seeder leaves
- The swarm becomes fragile: one seeder departure can make a torrent incompletable
- Download speed suffers because peers can't offer pieces the swarm already has

rqbit's lack of rarest-first makes it a **less effective swarm participant**. It downloads common pieces first, contributing less diversity to the swarm. This is the single largest algorithmic gap in rqbit's design.

torrent and libtorrent both implement rarest-first correctly. libtorrent's O(1) bucket structure is more efficient at scale, but torrent's richer PickContext enables more sophisticated multi-factor decisions (speed class, extent affinity, stealing thresholds).

---

## 5. Disk I/O Architecture

### 5.1 Design Philosophy

| Aspect | torrent | rqbit | libtorrent |
|--------|---------|-------|------------|
| **I/O model** | Async actor with write buffer | Sync positioned I/O (deliberate) | Async via worker thread pool |
| **Write strategy** | Buffer blocks, flush on piece completion | Direct pwrite/pwritev | Store buffer + mmap (2.0) or ARC cache (1.x) |
| **Read cache** | ARC (Adaptive Replacement Cache) | LRU write-through middleware | OS page cache (2.0) or ARC (1.x) |
| **Hashing** | Parallel via tokio::spawn | SHA-1 (OpenSSL backend, identified as bottleneck) | Dedicated hashing thread pool |
| **Zero-copy** | Network → buffer → disk | Direct pwrite (minimal copy) | Network → page-aligned buffer → disk (2.0) |
| **Backends** | Posix, Mmap, Disabled | Filesystem, Mmap | Mmap (default 64-bit), Posix, Disabled |
| **Part files** | No | No | Yes (consolidates low-priority pieces) |

### 5.2 Notable Design Choices

**torrent's write buffer** accumulates blocks keyed by `(torrent, piece)` in a `BTreeMap`. When a piece completes, all blocks are flushed in a single sorted write. This minimizes disk seeks and ensures pieces hit disk as contiguous writes. The ARC cache handles reads with frequency+recency awareness.

**rqbit's sync I/O decision** is deliberate and well-reasoned: tokio's async file I/O uses a thread pool with 2 MB overhead per file. By using POSIX `pread`/`pwrite`/`pwritev` directly, rqbit avoids this overhead and enables true parallel positioned writes without file-level locks. The trade-off is that disk operations can block a tokio worker thread.

**libtorrent's mmap transition** (1.x → 2.0) is the most documented disk I/O design decision in BitTorrent history. The 1.x ARC cache was brilliant — O(1) replacement, elevator-ordered reads, hash cursor awareness, upload-from-cache. The 2.0 mmap approach delegated all caching to the OS kernel, which lost BitTorrent-specific intelligence. This caused measurable regressions: UI freezes on Windows (page faults blocking the thread), poor HDD performance, and ~50% speed loss in some scenarios. Arvid acknowledged this was a bet on SSD ubiquity that didn't fully pay off.

### 5.3 Performance Implications

torrent's measured 116 MB/s with the store buffer + fire-and-forget pattern (v0.65.0) suggests its disk I/O is not the bottleneck. The combination of write buffering and ARC read caching provides application-level intelligence without the mmap pitfalls libtorrent 2.0 encountered.

rqbit's approach is the simplest and works well for SSD-backed storage. On HDDs, the lack of elevator ordering or write coalescing would hurt.

---

## 6. Request Pipelining

Pipelining depth directly controls throughput — too shallow and the connection stalls waiting for responses; too deep and you waste bandwidth on cancelled requests.

| Aspect | torrent | rqbit | libtorrent |
|--------|---------|-------|------------|
| **Default depth** | 128 fixed permits | 128 via Semaphore | 5 initial, dynamic |
| **Sizing strategy** | Fixed (no slow-start) | Fixed (burst on unchoke) | `request_queue_time` (3s) * rate / block_size |
| **Max depth** | 128 | 128 | 500 (`max_out_request_queue`) |
| **Adaptation** | None (fixed at max) | None | Dynamic based on measured throughput |
| **End-game** | 4 requests/peer, reactive cancels | None | 1 redundant block/peer |
| **Per-peer tracking** | RTT measurement, EWMA rate | Steal time tracking | Download rate, request time |

**libtorrent's dynamic approach** is the most sophisticated: it starts conservative (5 requests), doubles every RTT (application-level slow-start matching TCP), and targets 3 seconds of outstanding data based on measured throughput. This adapts to both fast and slow connections optimally.

**torrent and rqbit** both use fixed 128-request depth, which is aggressive. This works well on fast connections (128 * 16 KiB = 2 MiB in flight) but can over-request on slow connections, leading to wasted bandwidth if the peer disconnects. The advantage is simplicity and immediate saturation of fast links.

torrent's end-game mode (4 requests/peer with reactive cancellation) is more aggressive than libtorrent's (1 redundant block/peer), which helps finish the last few pieces faster but increases duplicate traffic.

---

## 7. Protocol & Feature Coverage

### 7.1 Feature Matrix

| Feature | torrent | rqbit | libtorrent |
|---------|:-------:|:-----:|:----------:|
| Core protocol (BEP 3) | Y | Y | Y |
| Fast Extension (BEP 6) | Y | **N** | Y |
| DHT (BEP 5) | Y | Y | Y |
| DHT Security (BEP 42) | Y | **N** | Y |
| DHT Storage (BEP 44) | Y | **N** | Y |
| DHT Infohash Index (BEP 51) | Y | **N** | Y |
| Metadata Exchange (BEP 9) | Y | Y | Y |
| Extension Protocol (BEP 10) | Y | Y | Y |
| Peer Exchange (BEP 11) | Y | Y | Y |
| Multitracker (BEP 12) | Y | Y | Y |
| Local Discovery (BEP 14) | Y | Y | Y |
| UDP Tracker (BEP 15) | Y | Y | Y |
| Super Seeding (BEP 16) | Y | **N** | Y |
| HTTP Seeding (BEP 17/19) | Y | **N** | Y |
| Upload Only (BEP 21) | Y | **N** | Y |
| Private Torrents (BEP 27) | Y | Y | Y |
| uTP (BEP 29) | Y (LEDBAT) | Partial (CUBIC) | Y (LEDBAT) |
| SSL Torrents (BEP 35) | Y | **N** | Y |
| Canonical Peer Priority (BEP 40) | Y | **N** | Y |
| Pad Files (BEP 47) | Y | Y | Y |
| Scrape (BEP 48) | Y | **N** | Y |
| BitTorrent v2 (BEP 52) | Y | **N** | Y |
| Magnet so= (BEP 53) | Y | Y | Y |
| Holepunch (BEP 55) | Y | **N** | Y |
| MSE/PE Encryption | Y | **N** | Y |
| I2P Anonymous Network | Y (SAM v3.1) | **N** | Partial (no DHT over I2P) |
| IP Filtering | Y (.dat + .p2p) | **N** | Y |
| Smart Banning | Y (parole system) | **N** | Y |
| Torrent Creation | Y | **N** | Y |
| Fast Resume | Y | Probabilistic | Y |

### 7.2 Coverage Summary

- **libtorrent**: 30+ BEPs. The gold standard. First to implement BEP 52 (v2 torrents).
- **torrent**: 27 BEPs. Near-parity with libtorrent, including every major BEP. Only gaps are minor (BEP 38 finding local data, BEP 43 read-only DHT, BEP 45 multi-address DHT, BEP 46 DHT torrent updates).
- **rqbit**: ~16 BEPs. Missing critical features: no encryption, no v2 torrents, no Fast Extension, no DHT security, no holepunch, no super seeding.

---

## 8. Choking & Incentive Mechanism

The choking algorithm is how BitTorrent implements **tit-for-tat reciprocity** — the game theory that makes the protocol work.

| Aspect | torrent | rqbit | libtorrent |
|--------|---------|-------|------------|
| **Tit-for-tat** | Yes (download-rate sorted) | **None** (always unchokes) | Yes (download-rate sorted) |
| **Optimistic unchoke** | Yes (random rotation) | **None** | Yes (30s rotation) |
| **Pluggable strategies** | `ChokerStrategy` trait | No | Setting-based (fixed/rate-based) |
| **Seed strategies** | FastestUpload, RoundRobin, AntiLeech | N/A | round_robin, fastest_upload, anti_leech |
| **Anti-snub** | Yes (snub detection) | No | Yes (60s timeout, reverse-order grouping) |
| **Peer turnover** | Yes (periodic replacement) | No | Yes (4% every 300s) |

**rqbit always sends Unchoke to every peer.** This makes it a **freeloader** in game-theory terms — it receives without reciprocating proportionally. While this maximizes its own short-term download speed (every peer uploads to it), it harms swarm economics. If every client behaved this way, there would be no incentive to upload, and the protocol would collapse.

torrent and libtorrent both implement proper tit-for-tat with comparable sophistication. torrent's `ChokerStrategy` trait allows custom algorithms, while libtorrent uses configuration settings.

---

## 9. Network Protocol Quality

### 9.1 uTP Implementation

| Aspect | torrent | rqbit | libtorrent |
|--------|---------|-------|------------|
| **Congestion control** | LEDBAT (delay-based) | CUBIC (loss-based) | LEDBAT (delay-based) |
| **Correct per BEP 29?** | Yes | **No** | Yes |
| **Network friendliness** | Yields to TCP within 1 RTT | Competes with TCP (defeats purpose) | Yields to TCP within 1 RTT |
| **Target delay** | Standard | N/A | 75ms (configurable) |
| **Clock drift compensation** | Standard | Unknown | Bidirectional baseline tracking |
| **Path MTU discovery** | Unknown | Unknown | Binary search with DF bit |
| **Status** | Production | Experimental (disabled by default) | Production (20+ years) |

The entire point of uTP is **LEDBAT congestion control** — low-priority traffic that yields to regular TCP. rqbit uses CUBIC instead, which means its uTP is functionally just "TCP over UDP" without the network-friendliness benefit. The author acknowledges LEDBAT is "notably missing."

### 9.2 Encryption

Both torrent and libtorrent implement MSE/PE correctly (Diffie-Hellman key exchange + RC4 stream cipher). This is critical for:
- Bypassing ISP traffic shaping
- Connecting to peers that require encryption
- Private tracker compatibility

rqbit has **no encryption at all**, which means:
- ISPs can trivially identify and throttle the traffic
- Cannot connect to peers that require encryption
- Reduced compatibility with the broader swarm

---

## 10. Performance Architecture

### 10.1 Key Performance Metrics

| Metric | torrent | rqbit | libtorrent |
|--------|---------|-------|------------|
| **Measured throughput** | 116 MB/s (Arch ISO) | "Saturates 20 Gbps" (unverified) | ~150 MB/s (tuned Deluge) |
| **Memory usage** | Not measured | "Tens of megabytes" | ~128 KB/torrent (peer list) |
| **Ramp-up time** | Immediate (128 depth) | Immediate (128 burst) | <1s (app-level slow start) |
| **DHT saturation** | Fast (persistent table) | Documented issues | ~28 min (NICE maintenance) |
| **Scalability target** | Single client use | Single client use | 1 million torrents (ghost mode) |

### 10.2 Memory & Allocation Efficiency

**libtorrent** is the most aggressively optimized:
- Zero-allocation alert queue (heterogeneous, double-buffered, stack-allocated strings)
- 8-byte packed bdecode tokens (77x faster parser than original)
- Struct packing with custom access profilers for cache-line optimization
- Buffer pools for disk and receive buffers
- Ghost torrents for million-torrent scaling

**torrent** uses Rust's zero-cost abstractions:
- `Bytes` (reference-counted, zero-copy slicing) for wire protocol data
- Atomic counters for stats (no lock overhead)
- `thiserror` for zero-overhead typed errors
- Serde + bencode with sorted key serialization

**rqbit** focuses on low baseline memory:
- Pre-allocated write buffers (`Box<[u8; MAX_MSG_LEN]>`)
- DashMap for lock-free peer state access
- Dual-buffer zero-copy message parsing
- SHA-1 identified as the biggest CPU bottleneck

### 10.3 Write Path Comparison

```
torrent:      recv -> WriteBuffer (BTreeMap by piece+offset) -> sorted flush on completion -> disk
rqbit:        recv -> pwrite/pwritev directly per block -> disk
libtorrent:   recv -> page-aligned buffer -> store buffer -> writev on completion -> disk (or mmap)
```

torrent and libtorrent both batch writes until piece completion, minimizing disk seeks. rqbit writes each block immediately, relying on the OS and SSD firmware for write coalescing.

---

## 11. Testing & Simulation

| Aspect | torrent | rqbit | libtorrent |
|--------|---------|-------|------------|
| **Unit tests** | 1,386 | Not documented | 108 files |
| **Simulation framework** | `torrent-sim` (virtual clock, SimNetwork, SimSwarm) | None | 32 simulation test files |
| **Fuzzing** | Not documented | Not documented | Fuzzing harnesses |
| **Network simulation** | `NetworkFactory` pluggable transport | None | libsimulator (in-process network) |
| **Deterministic testing** | Yes (virtual clock) | No | Yes (libsimulator) |
| **Disabled disk backend** | `DisabledDiskIo` (benchmark isolation) | Not documented | `disabled_disk_io` |

Both torrent and libtorrent have invested heavily in simulation-based testing. This is essential for verifying complex protocol interactions (multi-peer swarms, DHT routing, NAT traversal) that can't be reliably tested with real networks. rqbit lacks this infrastructure entirely.

---

## 12. Architectural Strengths Summary

### torrent

1. **Best modularity** — 12-crate workspace with clean dependency DAG; any crate reusable independently
2. **Actor model without locks** — distributed across cores like libtorrent's single-thread eliminates locks, but without the single-core ceiling
3. **Near-complete BEP coverage** — 27 BEPs including v2, hybrid, holepunch, I2P, SSL
4. **Pluggable everything** — disk I/O, choking, transport, extensions via Rust traits
5. **Rust memory safety** — no use-after-free, data races, or buffer overflows by construction
6. **Sophisticated piece picker** — rarest-first with extent affinity, speed classes, stealing, end-game, auto-sequential
7. **Simulation framework** — deterministic testing with virtual clock and pluggable network

### rqbit

1. **Simplest architecture** — easy to understand and contribute to
2. **Low memory footprint** — runs well on Raspberry Pi and embedded devices
3. **Deliberate sync I/O** — well-reasoned positioned I/O avoids tokio overhead; enables pwritev
4. **Storage middleware** — composable cache/timing/slow wrappers
5. **Piece stealing** — adaptive dual-threshold mechanism compensates somewhat for lack of rarest-first
6. **Desktop app** — Tauri-based GUI and web UI included
7. **HTTP streaming** — built-in DLNA/UPnP media serving

### libtorrent

1. **Single-threaded network model** — the defining choice; zero lock contention, proven at scale for 20+ years
2. **Most optimized data structures** — piece picker buckets, bdecode tokens, alert queue, struct packing
3. **Broadest BEP coverage** — 30+ BEPs, first BEP 52 implementation
4. **Performance engineering culture** — custom profilers, documented optimizations, systematic tuning
5. **Battle-tested** — powers qBittorrent, Deluge, and dozens of other clients
6. **Massive scalability** — ghost torrents enable 1 million concurrent torrents
7. **Most complete uTP** — LEDBAT with clock drift compensation, path MTU discovery

---

## 13. Architectural Weaknesses Summary

### torrent

1. **Young codebase** — less battle-tested than libtorrent (months vs 20+ years of real-world use)
2. **Fixed pipeline depth** — 128 permits regardless of connection speed; could over-request on slow links
3. **No part files** — low-priority pieces still occupy full file space
4. **No ghost torrent mode** — not designed for million-torrent scale (not a current requirement)
5. **Channel overhead** — actor message passing has inherent serialization cost vs direct function calls

### rqbit

1. **No encryption** — ISP throttling, reduced swarm compatibility
2. **No rarest-first** — harms swarm health, slower downloads in sparse swarms
3. **No end-game** — last-piece stalls on slow peers
4. **No tit-for-tat** — freeloads from the swarm without reciprocity enforcement
5. **No v2 torrents** — can't participate in modern torrent ecosystem
6. **No DHT security (BEP 42)** — vulnerable to Sybil attacks on routing table
7. **uTP uses CUBIC not LEDBAT** — defeats the purpose of uTP
8. **DHT issues** — documented bootstrap problems, memory leaks, stale nodes
9. **No simulation testing** — complex protocol interactions untested

### libtorrent

1. **Mmap regression** — 2.0 lost application-level cache intelligence; ~50% speed loss in some scenarios
2. **Single-core ceiling** — network thread bottlenecks at extreme connection counts
3. **Monolithic classes** — `session_impl` (7.4k lines) and `torrent` (12.4k lines) are hard to test
4. **Boost dependency** — adds build complexity, requires full source tree
5. **C++ memory safety** — no compile-time guarantees against use-after-free, buffer overflows
6. **ABI fragility** — mismatched build configuration causes silent corruption
7. **Limited I2P** — no DHT over I2P, only works with I2P-specific torrents

---

## 14. Verdict: Which Architecture is Superior?

### For a high-performance torrent client, the answer depends on the time horizon.

**Today, libtorrent wins on proven performance and ecosystem.** 20+ years of systematic optimization, the broadest protocol coverage, battle-tested at scale powering qBittorrent and Deluge. Arvid Norberg's performance engineering — zero-allocation alerts, O(1) piece picker operations, application-level slow start, corking, struct packing — represents decades of iterative refinement. No other implementation matches this depth of optimization.

**However, libtorrent carries architectural debt.** The mmap regression in 2.0 was a significant step backward. The monolithic `session_impl` and `torrent` classes are increasingly difficult to extend. The single-threaded network model, while brilliant for eliminating concurrency bugs, creates a hard ceiling that multi-core systems can't exploit. C++ provides no compile-time memory safety guarantees, meaning entire classes of bugs (use-after-free, data races) must be caught by testing rather than prevented by the type system.

**torrent has the most forward-looking architecture.** The actor model distributes protocol processing across cores while maintaining the lock-free property that makes libtorrent fast. Rust's ownership system eliminates entire categories of bugs at compile time. The 12-crate workspace enables clean reuse. 27 BEPs — including v2 torrents, holepunch, I2P, SSL, and pluggable choking — demonstrate that feature completeness doesn't require monolithic design.

The main risk for torrent is **maturity**. libtorrent's optimizations were discovered through years of production profiling. torrent hasn't yet faced the edge cases that millions of users will surface. The fixed 128-request pipeline, while effective, lacks libtorrent's sophisticated dynamic sizing. The piece picker, while feature-rich, doesn't match libtorrent's O(1) bucket operations.

**rqbit occupies a different niche.** It's optimized for simplicity and low resource usage, not protocol completeness. The architectural gaps (no encryption, no rarest-first, no tit-for-tat, no v2) make it unsuitable as the engine for a qBittorrent replacement. Its strengths — low memory, simple codebase, good streaming — serve the "download one thing quickly on a Raspberry Pi" use case well.

### Ranking by Architecture Quality

1. **torrent** — Best modularity, best concurrency model for modern hardware, near-complete protocols, Rust safety guarantees. Needs maturity and performance tuning.

2. **libtorrent** — Most optimized, most battle-tested, broadest protocol support. Held back by monolithic design, mmap regression, single-threaded ceiling, and C++ safety gaps.

3. **rqbit** — Simplest and lightest, but missing too many fundamental protocols and algorithms to compete as a general-purpose high-performance engine.

### The Path Forward

torrent's architecture is **designed to surpass libtorrent** once it reaches comparable maturity. The key areas where torrent should focus to close the gap:

1. **Dynamic request queue sizing** — adopt libtorrent's `request_queue_time * rate / block_size` formula instead of fixed 128
2. **O(1) piece picker operations** — implement swap-based bucket boundaries like libtorrent for large torrents (10k+ pieces)
3. **Application-level slow start** — match TCP slow start for cleaner ramp-up
4. **Struct/cache optimization** — profile hot paths, ensure frequently accessed fields share cache lines
5. **Production hardening** — run against diverse real-world swarms, profile, iterate

The actor model + Rust safety + modular crates give torrent the strongest *foundation* for a high-performance client. libtorrent has the strongest *execution* after 20 years of refinement. The question is whether torrent can close that execution gap before libtorrent's architectural limitations become insurmountable.
