# Ferrite Roadmap v3 — Full libtorrent-rasterbar Parity

**Date:** 2026-03-01
**Target:** Complete feature parity with libtorrent-rasterbar (every BEP + every non-BEP feature)
**Current State:** M1-M35 complete, v0.41.0, 11 crates, 895 tests
**Supersedes:** `2026-02-26-ferrite-roadmap-v2.md` (M1-M35 only)

## Completed (M1-M35, Phases 1-6)

| Phase | Milestones | Status |
|-------|-----------|--------|
| Foundation | M1-M10 (bencode → facade) | Done |
| Phase 1: Desktop Essentials | M11-M16 (resume, selective, end-game, bandwidth, alerts, queue) | Done |
| Phase 2: Transport & Security | M17-M20 (encryption, uTP, NAT) | Done |
| Phase 3: Protocol Extensions | M21-M24 (IPv6, web seed, super seed, scrape) | Done |
| Phase 4: Performance | M25-M28 (smart ban, async disk, parallel hash, piece picker) | Done |
| Phase 5: Network & Tools | M29-M32d (IP filter, torrent creation, settings, metadata, BEP 40, rate limits, move, share, plugins) | Done |
| Phase 6: BitTorrent v2 | M33-M35 (BEP 52 core, wire+storage, hybrid) | Done |

### BEPs Implemented (M1-M35)
3, 5, 6, 7, 9, 10, 11, 12, 14, 15, 16, 17, 19, 21, 24, 27, 29, 40, 47, 48, 52

---

## Phase 7: v2 Completion & DHT Hardening (M36-M39)

*Finishes the BitTorrent v2 story and hardens the DHT against Sybil attacks.*

### M36: BEP 53 — Magnet URI Select-Only + Dual-Swarm + Pure v2

Complete the v2 ecosystem: magnet file selection, dual-swarm announces, and pure v2-only session support.

**Scope:**
- **BEP 53 magnet `&so=` parameter:** parse `so=0,2,4-6` from magnet URIs → map to `FilePriority` after metadata received
- `Magnet::selected_files() -> Option<Vec<FileSelection>>` with `FileSelection` enum (`Single(usize)`, `Range(usize, usize)`)
- **Dual-swarm tracker/DHT announces:** when `version == Hybrid`, announce both v1 info hash (SHA-1) and v2 info hash (SHA-256) separately to trackers and DHT
- `SessionActor`: second `get_peers()` + tracker announce for v2 hash on hybrid torrents
- Merge peer lists from both swarms (deduplicate by IP)
- **Pure v2-only session path:** remove `Error::Config` guard on `TorrentVersion::V2Only`
- v2 handshake with 32-byte SHA-256 info hash (pad or truncate for protocol compat)
- v2-only torrents use v2 tracker announce and DHT only

**Crates:** ferrite-core (magnet parsing), ferrite-session (dual-swarm, v2 session)
**Tests:** ~8 (so= parsing, dual announce, v2-only session, peer dedup)

### M37: BEP 42 — DHT Security Extension

Harden the DHT against node ID spoofing and Sybil attacks.

**Scope:**
- Node ID verification: CRC32C of external IP → expected node ID prefix (first 21 bits)
- Reject routing table entries with invalid node IDs (configurable strictness)
- External IP detection: aggregate from tracker responses, NAT mapping replies, and KRPC `ip` field
- `ExternalIpVoter` — consensus-based external IP detection from multiple sources
- `DhtConfig` fields: `enforce_node_id` (default: true), `restrict_routing_ips` (default: true), `restrict_search_ips` (default: true), `prefer_verified_node_ids` (default: true)
- Alert: `DhtNodeIdViolation`

**Crates:** ferrite-dht (node ID validation), ferrite-core (CRC32C, external IP voter)
**Tests:** ~6 (CRC32C prefix, valid/invalid node IDs, routing table rejection, IP consensus)

### M38: BEP 44 — DHT Arbitrary Data Storage (Put/Get)

Enable storing and retrieving arbitrary data in the DHT, supporting both immutable and mutable items.

**Scope:**
- **Immutable items:** `dht_put(data: &[u8]) -> Id20` / `dht_get(hash: Id20) -> Option<Vec<u8>>`
- SHA-1 hash of bencoded data as key
- **Mutable items:** ed25519 key pair signing with sequence numbers
- `dht_put_mutable(keypair, data, seq, salt) -> target` / `dht_get_mutable(pubkey, salt) -> (data, seq)`
- Sequence number monotonicity enforcement (reject older seq)
- Optional salt for multiple items under same key
- **`DhtStorageBackend` trait:** pluggable storage interface
  - `get_immutable(hash) -> Option<Vec<u8>>`
  - `put_immutable(hash, data)`
  - `get_mutable(pubkey, salt) -> Option<(data, seq, sig)>`
  - `put_mutable(pubkey, salt, data, seq, sig)`
  - `count_items() -> usize`
- **Default backend:** in-memory with LRU eviction
- Settings: `dht_max_items` (default: 700), `dht_item_lifetime_seconds` (default: 7200)
- KRPC messages: `put` (query), `get` (query) with `token`, `v`, `k`, `sig`, `seq`, `salt`
- Alert: `DhtPutComplete`, `DhtGetResult`, `DhtError`

**Crates:** ferrite-dht (KRPC messages, storage), ferrite-core (types)
**Dependencies:** `ed25519-dalek` for mutable item signing/verification
**Tests:** ~10 (immutable round-trip, mutable round-trip, seq enforcement, salt, eviction, storage trait)

### M39: BEP 51 — DHT Infohash Indexing

Enable sampling random infohashes from DHT nodes for network surveying.

**Scope:**
- `sample_infohashes` KRPC query (new query type)
- Request: `target` (20-byte ID to route toward)
- Response: `interval` (re-query interval), `num` (estimated total infohashes), `samples` (concatenated 20-byte hashes), `nodes` (compact node info)
- `DhtHandle::sample_infohashes(target: Id20) -> SampleResult`
- `SampleResult { interval: u32, estimated_count: u64, samples: Vec<Id20>, nodes: Vec<CompactNodeInfo> }`
- Handler: respond to incoming `sample_infohashes` queries with random sample from local storage
- Settings: `dht_sample_infohashes_interval` (default: 0, disabled)
- Alert: `DhtSampleInfohashes`

**Crates:** ferrite-dht
**Tests:** ~4 (query encoding, response parsing, handler, rate limiting)

---

## Phase 8: Connectivity & Privacy (M40-M42)

*Peer-to-peer NAT traversal and privacy-oriented transport layers.*

### M40: BEP 55 — Holepunch Extension

Enable direct connections between two NATed peers via a shared relay.

**Scope:**
- `ut_holepunch` BEP 10 extension message (extension ID negotiated in handshake)
- Message types: `Rendezvous` (0x00), `Connect` (0x01), `Error` (0x02)
- Fields: `msg_type` (u8), `addr_type` (0=IPv4, 1=IPv6), `addr` (4 or 16 bytes), `port` (u16), `error_code` (u32, Error only)
- **Initiator flow:** detect NATed target → send `Rendezvous` to relay peer → relay forwards `Connect` to target → both peers initiate simultaneous TCP connect
- **Relay flow:** on receiving `Rendezvous`, check target is connected, forward `Connect` to both parties
- **UDP holepunch:** same mechanism for uTP — simultaneous UDP packets through NAT
- Error codes: `NoSuchPeer` (1), `NotConnected` (2), `NoSupport` (3), `NoSelf` (4)
- NAT type detection heuristic: track whether incoming connections succeed to classify NAT type
- Settings: `enable_holepunch` (default: true)
- Alert: `HolepunchSucceeded`, `HolepunchFailed`

**Crates:** ferrite-wire (message types), ferrite-session (relay logic, initiation)
**Tests:** ~8 (message encoding, relay forwarding, initiator flow, error handling, IPv6)

### M41: I2P Support (SAM Bridge)

Enable anonymous peer connections via the I2P network.

**Scope:**
- **SAM v3.1 protocol client:**
  - TCP control socket to I2P router (default: 127.0.0.1:7656)
  - `HELLO VERSION` handshake
  - `SESSION CREATE STYLE=STREAM DESTINATION=TRANSIENT` — create ephemeral I2P destination
  - `STREAM CONNECT` — outbound connections to I2P destinations
  - `STREAM ACCEPT` — inbound connections from I2P peers
  - `NAMING LOOKUP` — resolve .b32.i2p addresses
- **Peer address extension:** `PeerAddr` enum gains `I2p(I2pDestination)` variant
- `I2pDestination` — base64-encoded ~516-byte I2P address
- **Session integration:**
  - When I2P enabled, create SAM session on startup
  - Announce I2P destination to trackers (via I2P HTTP tracker)
  - DHT: I2P peers stored separately (no mixing with clearnet DHT)
  - PEX: I2P peers exchanged only with other I2P peers
- **Settings:** `i2p_hostname` (default: "127.0.0.1"), `i2p_port` (default: 7656), `i2p_inbound_quantity` (1-16, default: 3), `i2p_outbound_quantity` (1-16, default: 3), `i2p_inbound_length` (0-7 hops, default: 3), `i2p_outbound_length` (0-7 hops, default: 3), `allow_i2p_mixed` (default: false)
- When `allow_i2p_mixed = false`: I2P torrents only connect to I2P peers
- Alert: `I2pSessionCreated`, `I2pError`

**Crates:** ferrite-session (SAM client module, peer addr extension)
**Tests:** ~8 (SAM handshake, session create, stream connect/accept, destination encoding, mixed mode)

### M42: SSL Torrents

Enable peer authentication via X.509 certificates embedded in .torrent files.

**Scope:**
- **Torrent parsing:** detect `ssl-cert` key in info dict → extract PEM certificate
- `TorrentMetaV1.ssl_cert: Option<Vec<u8>>` / `TorrentMeta` accessors
- **TLS handshake wrapper:**
  - After TCP connect, perform TLS handshake before BitTorrent handshake
  - SNI set to hex-encoded info hash (40 chars) for torrent identification
  - Server cert must chain to the CA embedded in the .torrent file
  - Client presents its own cert (generated or configured)
- **Separate SSL listen port:** `ssl_listen_port` setting (default: disabled)
- Listener on SSL port wraps accepted connections with TLS before passing to peer task
- **Certificate management:**
  - Auto-generate self-signed client cert on first use (stored in session data dir)
  - `Settings::ssl_listen_port`, `ssl_cert_path`, `ssl_key_path`
- Reuse existing MSE/PE encryption detection: SSL handshake vs MSE handshake distinguished by first bytes
- Alert: `SslTorrentError`

**Crates:** ferrite-wire (TLS layer), ferrite-core (cert parsing), ferrite-session (SSL listener)
**Dependencies:** `rustls` (TLS), `rcgen` (cert generation), `x509-parser` or `webpki` (cert validation)
**Tests:** ~6 (cert extraction, TLS handshake, SNI verification, chain validation, self-signed gen)

---

## Phase 9: Swarm Intelligence (M43-M46)

*Choking algorithms, piece picker refinements, and peer management strategies.*

### M43: Seed Choking Algorithms + Rate-Based Choker

Multiple strategies for upload slot allocation.

**Scope:**
- **`SeedChokingAlgorithm` enum:**
  - `FastestUpload` — unchoke peers we upload to fastest (current behaviour, becomes explicit)
  - `RoundRobin` — rotate unchoke slots periodically regardless of upload speed
  - `AntiLeech` — prefer peers that also seed other torrents; penalize hit-and-run leechers (check upload ratio via protocol heuristics)
- **`ChokingAlgorithm` enum:**
  - `FixedSlots` — current tit-for-tat + optimistic unchoke with configurable slot count
  - `RateBased` — dynamically determine unchoke count: if `total_upload < upload_capacity`, add a slot; if adding a slot doesn't increase throughput, remove it
- **Refactor:** extract current choking logic into `ChokerStrategy` trait with `select_unchoked(&self, peers, config) -> Vec<PeerId>` method
- Built-in implementations for each algorithm combo
- Settings: `seed_choking_algorithm` (default: `FastestUpload`), `choking_algorithm` (default: `FixedSlots`)

**Crates:** ferrite-session
**Tests:** ~8 (round-robin fairness, anti-leech detection, rate-based slot sizing, strategy switching)

### M44: Piece Picker Enhancements

Fine-grained optimizations for piece selection and announcement.

**Scope:**
- **Piece extent affinity:** group pieces into 4 MiB extents. Peers in the same speed class prefer pieces from the same extent group, improving disk I/O locality by keeping writes sequential.
- **Suggest mode:** `suggest_read_cache` — when a peer connects, suggest pieces currently in the ARC read cache via `Message::SuggestPiece` (BEP 6, wire message already implemented). Reduces disk reads by steering peers toward cached data.
- **Predictive piece announce:** send `Have` messages to peers before disk write IO completes. When a piece passes hash verification, announce immediately rather than waiting for the write callback. Configurable latency threshold.
- **Peer speed categorization:** bucket peers into speed classes (slow/medium/fast) based on download rate. Peers in the same class get affinity for the same piece groups, preventing slow peers from blocking piece completion.
- Settings: `piece_extent_affinity` (default: true), `suggest_mode` (default: `SuggestReadCache`), `max_suggest_pieces` (default: 10), `predictive_piece_announce` (default: 0, disabled; set to ms threshold to enable)

**Crates:** ferrite-session
**Tests:** ~8 (extent grouping, suggest from cache, predictive announce timing, speed classification)

### M45: Mixed-Mode Algorithm + Auto-Sequential

Bandwidth allocation between TCP/uTP and automatic sequential switching.

**Scope:**
- **Mixed-mode TCP/uTP algorithm:**
  - `MixedModeAlgorithm` enum: `PreferTcp`, `PeerProportional`
  - `PreferTcp` — throttle uTP upload bandwidth when TCP peers are present (uTP is background traffic, TCP is foreground)
  - `PeerProportional` — allocate bandwidth proportional to the number of TCP vs uTP peers
  - Integrates with per-class rate limiters from M32b
- **Auto-sequential formalization:**
  - Ferrite already detects high partial-piece explosion and switches to sequential (`piece_selector.rs:354`)
  - Formalize as `Settings::auto_sequential` (default: true)
  - Add hysteresis: switch to sequential when >60% of in-flight pieces are partial, switch back when <30%
  - Prevent flapping between modes
- Settings: `mixed_mode_algorithm` (default: `PeerProportional`)

**Crates:** ferrite-session
**Tests:** ~6 (prefer-tcp throttling, proportional allocation, auto-sequential hysteresis, flap prevention)

### M46: Peer Turnover

Periodically replace underperforming peers with fresh candidates.

**Scope:**
- **Turnover mechanism:** on a timer, disconnect the worst-performing fraction of peers and initiate connections to new candidates from the peer list
- `peer_turnover` — fraction of peers to disconnect per interval (default: 0.04, i.e., 4%)
- `peer_turnover_cutoff` — only trigger turnover if download rate < this fraction of observed peak rate (default: 0.9). When downloading at near-peak speed, don't churn.
- `peer_turnover_interval` — seconds between turnover checks (default: 300)
- **Exemptions:** never disconnect seeds (when leeching), peers with outstanding requests, peers in parole, peers connected < 30 seconds
- Integrates with `try_connect_peers()` for replacement connections
- Alert: `PeerTurnover` (logs disconnected + new peer counts)

**Crates:** ferrite-session
**Tests:** ~4 (turnover fires below cutoff, suppressed at peak, exemptions, interval timing)

---

## Phase 10: Security & Hardening (M47-M48)

*Network safety, anti-abuse, and traffic classification.*

### M47: SSRF Mitigation + IDNA Rejection + HTTPS Validation

Protect against server-side request forgery, homograph attacks, and TLS bypass.

**Scope:**
- **SSRF mitigation (`ssrf_mitigation` setting, default: true):**
  - Localhost tracker URLs (127.0.0.0/8, ::1) must use `/announce` path — reject arbitrary paths
  - Local-network web seed URLs (RFC 1918 + link-local) cannot have query strings
  - HTTP redirects from global URLs to private/local IP ranges are blocked
  - Applied to: tracker HTTP requests, web seed requests, metadata download URLs
  - URL validation at `add_torrent` time and on redirect
- **IDNA rejection (`allow_idna` setting, default: false):**
  - Reject tracker/web seed URLs containing internationalized domain names (non-ASCII)
  - Prevents homograph attacks (e.g., `аnnounce.evil.com` with Cyrillic 'а')
  - Check at URL parse time
- **HTTPS validation (`validate_https_trackers` setting, default: true):**
  - Explicit control over TLS certificate validation for tracker and web seed HTTPS connections
  - When true: reqwest verifies server certificates (default behaviour, made explicit)
  - When false: skip verification (for self-signed trackers, not recommended)

**Crates:** ferrite-session (URL validator module), ferrite-tracker (request filtering)
**Tests:** ~8 (localhost path restriction, private IP redirect block, IDNA reject, HTTPS validation toggle)

### M48: DSCP Marking + Anonymous Mode Hardening

Traffic classification and comprehensive privacy mode.

**Scope:**
- **DSCP/ToS marking:**
  - Set DSCP field on all peer sockets: `setsockopt(IPPROTO_IP, IP_TOS)` and `setsockopt(IPPROTO_IPV6, IPV6_TCLASS)`
  - `peer_dscp` setting (default: 0x04 — CS1/scavenger, low-priority background traffic)
  - Applied to: TCP peer sockets, uTP UDP sockets, tracker sockets
  - Set after socket creation in connection setup
- **Anonymous mode hardening:** audit and extend existing `anonymous_mode`:
  - Strip user-agent header from all HTTP requests (tracker announces, web seed, metadata)
  - Suppress local IP in tracker announce query strings
  - Suppress `reqq`, `upload_only`, and other identifying fields in extension handshake
  - Disable UPnP/NAT-PMP when anonymous (device fingerprinting vector)
  - Disable LSD when anonymous (local network exposure)
  - Use generic peer ID prefix (no client identification)
  - Suppress `v` key in extension handshake (already done, verify)
- Settings: `peer_dscp` (default: 0x04)

**Crates:** ferrite-session (socket options, anonymous audit), ferrite-tracker (header stripping), ferrite-nat (conditional disable)
**Tests:** ~6 (DSCP socket option, anonymous HTTP headers, anonymous extension handshake, UPnP disabled)

---

## Phase 11: Pluggable Interfaces & Observability (M49-M50)

*Extensibility points and deep instrumentation for advanced integrations.*

### M49: Pluggable Disk I/O Interface

Session-level abstraction for custom storage backends.

**Scope:**
- **`DiskIoBackend` trait:**
  - `async fn write_chunk(&self, piece: u32, begin: u32, data: Bytes) -> Result<()>`
  - `async fn read_chunk(&self, piece: u32, begin: u32, len: u32) -> Result<Bytes>`
  - `async fn hash_piece(&self, piece: u32) -> Result<Id20>`
  - `async fn hash_piece_v2(&self, piece: u32) -> Result<Id32>`
  - `async fn move_storage(&self, new_path: &Path) -> Result<()>`
  - `fn name(&self) -> &str`
- **Three built-in implementations:**
  - `MmapDiskIo` — current mmap path (relies on kernel page cache)
  - `PosixDiskIo` — current pread/pwrite path with user-space ARC cache
  - `DisabledDiskIo` — null backend: accepts all writes, returns zeroes on read. For benchmarking network throughput in isolation.
- `ClientBuilder::disk_io_backend(impl DiskIoBackend)` — plug custom storage at session creation
- Refactor current `DiskActor` to delegate to backend trait internally
- Backward-compatible: existing `StorageMode` enum selects between Mmap and Posix; `DisabledDiskIo` is opt-in only

**Crates:** ferrite-storage (trait + impls), ferrite-session (DiskActor refactor)
**Tests:** ~6 (disabled backend round-trip, posix backend, mmap backend, custom backend mock)

### M50: Session Statistics Counters

Deep per-metric instrumentation matching libtorrent's named counter/gauge system.

**Scope:**
- **Metric types:** `Counter` (monotonically increasing) and `Gauge` (current value, can decrease)
- `SessionStatsMetric { name: &'static str, kind: MetricKind }` — static metadata per metric
- `session_stats_metrics() -> &'static [SessionStatsMetric]` — compile-time list of all metrics
- **~100 individual metrics across categories:**
  - *Network:* bytes_sent, bytes_recv, num_connections, num_half_open, num_tcp_peers, num_utp_peers
  - *Disk:* disk_read_count, disk_write_count, disk_read_bytes, disk_write_bytes, cache_hits, cache_misses, disk_queue_depth, disk_job_time_us
  - *DHT:* dht_nodes, dht_lookups, dht_items_stored, dht_bytes_in, dht_bytes_out
  - *Peers:* num_unchoked, num_interested, num_uploading, num_downloading, num_seeding_torrents, num_downloading_torrents
  - *Protocol:* pieces_downloaded, pieces_uploaded, hashfails, waste_bytes, piece_requests, piece_rejects
  - *Bandwidth:* upload_rate, download_rate, upload_rate_tcp, upload_rate_utp, download_rate_tcp, download_rate_utp
- `SessionStatsAlert` — periodic snapshot containing `Vec<i64>` values indexed by metric ID
- Settings: `stats_report_interval` (default: 1000ms, 0 = disabled)
- `SessionHandle::post_session_stats()` — trigger immediate stats snapshot
- Alert: `SessionStatsAlert`

**Crates:** ferrite-session
**Tests:** ~6 (metric registration, counter increment, gauge update, periodic alert, on-demand snapshot)

---

## Phase 12: Simulation Framework (M51)

*In-process network simulator for swarm-level integration testing.*

### M51: Network Simulation Framework

Full network simulator for testing multi-peer scenarios without real network traffic.

**Scope:**
- **New `ferrite-sim` crate**
- **`NetworkTransport` trait:** abstract TCP/UDP at the session level
  - `async fn connect(addr: SocketAddr) -> Result<Stream>`
  - `async fn listen(addr: SocketAddr) -> Result<Listener>`
  - `async fn send_udp(addr: SocketAddr, data: &[u8]) -> Result<()>`
  - `async fn recv_udp() -> Result<(SocketAddr, Vec<u8>)>`
  - Production impl: real tokio TCP/UDP sockets
  - Simulation impl: in-memory channels with configurable behaviour
- **`SimNetwork`:** configurable simulated network
  - Per-link latency (configurable, e.g., 50ms between peers A and B)
  - Per-link bandwidth cap (e.g., 1 MB/s upload, 10 MB/s download)
  - Packet loss rate (0.0-1.0 per link)
  - NAT simulation: `FullCone`, `PortRestricted`, `Symmetric` — controls which packets are forwarded
  - Network partitioning: `sim.partition([group_a], [group_b])` — isolate peer groups
- **Deterministic virtual clock:** replace `tokio::time::Instant` with `SimClock`
  - Tests run at full CPU speed, not wall-clock
  - `SimClock::advance(duration)` — manually advance time
  - All timers, sleeps, and intervals use the virtual clock in simulation mode
- **`SimSwarm` test harness:**
  - `SimSwarm::new(peer_count, network_config) -> SimSwarm`
  - `swarm.seed(torrent_data, seeder_index)` — inject torrent data at a node
  - `swarm.run_until_complete(timeout)` — run simulation until all leechers finish or timeout
  - `swarm.stats()` — per-node download/upload stats
  - `swarm.inject_fault(node, FaultKind)` — inject bad data, disconnect, etc.
- **Session integration:** `SessionHandle` gains `new_with_transport(transport, settings)` constructor
  - Refactor connection setup in `SessionActor` and `TorrentActor` to use `NetworkTransport` trait
  - DHT, tracker, and peer connections all routed through the transport layer
- **Built-in test scenarios (~20 tests):**
  - Basic swarm: 1 seed + 5 leechers, verify all complete
  - Rarest-first: verify piece diversity across leechers
  - Choking fairness: verify tit-for-tat with heterogeneous bandwidth
  - End-game: verify acceleration when <5% remains
  - DHT bootstrap: 20 nodes, verify routing table convergence
  - NAT holepunch: BEP 55 through simulated symmetric NAT
  - Hash failure: inject bad data, verify smart ban + parole
  - Hybrid swarm: mixed v1/v2/hybrid peers
  - Web seed: HTTP server sim + peer swarm
  - I2P: simulated SAM bridge
  - Super seeding: verify piece diversity maximization
  - Queue management: >active_limit torrents, verify auto-manage
  - Bandwidth limiting: verify rate limiter accuracy under load
  - Peer turnover: verify churn improves download rate
  - Share mode: relay-only node participates without persistent storage
  - SSL torrent: TLS handshake through sim network
  - Partition/heal: split network, verify recovery
  - uTP congestion: verify LEDBAT backs off under load
  - Large swarm: 50+ peers, verify stability
  - Streaming: sequential download with seek, verify FileStream blocks correctly

**Crates:** ferrite-sim (new), ferrite-session (transport trait extraction)
**Tests:** ~20 (the scenarios above are the tests)

---

## Phase 13: Documentation, CLI & Benchmarking (M52-M53)

*API documentation, a CLI for real-world testing, and benchmarks against libtorrent and rqbit.*

### M52: API Documentation & Examples

Comprehensive documentation pass now that the API is stable.

**Scope:**
- **`#![warn(missing_docs)]` on all crates:** enable the lint on each inner crate one at a time, fixing gaps as they surface
- **Document high-traffic types:** `TorrentStats`, `TorrentInfo`, `TorrentState` fields; `AlertKind` variants (especially nuanced ones like `InconsistentHashes`, `PeerTurnover`, `DhtSampleInfohashes`)
- **`examples/` directory** with 3-4 working programs:
  - `download.rs` — download a torrent from magnet link, show progress, exit on completion
  - `create.rs` — create a .torrent file from a directory
  - `stream.rs` — stream a file via `FileStream` (AsyncRead + AsyncSeek)
  - `dht_lookup.rs` — standalone DHT peer lookup
- **README "Getting Started" section:** minimal working code snippet, dependency line, feature flags
- **`cargo doc` audit:** verify all public items render correctly, fix broken intra-doc links

**Crates:** all
**Tests:** doc-tests in examples

### M53: CLI Binary & Benchmarking

A thin CLI for downloading torrents and benchmarking against libtorrent/rqbit.

**Scope:**
- **`ferrite-cli` crate** (new workspace member, `[[bin]]`):
  - `ferrite-cli download <magnet|.torrent> -o <dir>` — download with progress bar
  - `ferrite-cli create <path> -o <output.torrent>` — create .torrent
  - `ferrite-cli info <.torrent>` — print torrent metadata
  - Progress display: piece count, download/upload rate, peer count, ETA
  - Graceful shutdown on Ctrl-C (SIGINT)
  - `--sequential`, `--no-dht`, `--proxy`, `--config <json>` flags
- **Benchmark harness:**
  - Download same torrent (e.g., Ubuntu ISO) with ferrite-cli, rqbit, and qBittorrent (libtorrent)
  - Measure: time to completion, peak/average download rate, memory usage (RSS), CPU usage
  - Script to run N trials and compute mean/stddev
  - Results written to `benchmarks/` directory

**Crates:** ferrite-cli (new)
**Dependencies:** clap, indicatif (progress bar), ctrlc
**Tests:** ~4 (CLI arg parsing, graceful shutdown, torrent info display)

---

## Summary

| Phase | Milestones | New Crates | New Dependencies | Est. Tests |
|-------|-----------|------------|------------------|------------|
| 7: v2 Completion & DHT Hardening | M36-M39 | — | ed25519-dalek | ~28 |
| 8: Connectivity & Privacy | M40-M42 | — | rustls, rcgen, x509-parser | ~22 |
| 9: Swarm Intelligence | M43-M46 | — | — | ~26 |
| 10: Security & Hardening | M47-M48 | — | — | ~14 |
| 11: Pluggable Interfaces & Observability | M49-M50 | — | — | ~12 |
| 12: Simulation | M51 | ferrite-sim | — | ~20 |
| 13: Documentation, CLI & Benchmarking | M52-M53 | ferrite-cli | clap, indicatif, ctrlc | ~4 + doc-tests |
| **Total** | **M36-M53 (18 milestones)** | **2 new** | **6 new** | **~126+** |

**Projected final state:** 13 crates, ~1017+ tests, full libtorrent-rasterbar feature parity + CLI + benchmarks.

### BEP Coverage After M51

| BEP | Description | Milestone |
|-----|-------------|-----------|
| 3 | Base protocol | M1-M7 |
| 5 | DHT (Kademlia) | M5 |
| 6 | Fast Extension | M3, M8b |
| 7 | IPv6 tracker | M21 |
| 9 | Metadata (magnet) | M3, M7, M32a |
| 10 | Extension protocol | M3 |
| 11 | Peer Exchange | M7 |
| 12 | Multi-tracker | M4, M8a |
| 14 | Local Service Discovery | M8c |
| 15 | UDP tracker | M4 |
| 16 | Super seeding | M23 |
| 17 | HTTP seeding (Hoffman) | M22 |
| 19 | HTTP seeding (GetRight) | M22 |
| 21 | Upload-only | M23 |
| 24 | IPv6 DHT | M21 |
| 27 | Private torrents | M7 |
| 29 | uTP | M18-M19 |
| 40 | Canonical peer priority | M32b |
| **42** | **DHT security extension** | **M37** |
| **44** | **DHT arbitrary data storage** | **M38** |
| 47 | Pad files | M30 |
| 48 | Tracker scrape | M24 |
| **51** | **DHT infohash indexing** | **M39** |
| 52 | BitTorrent v2 | M33-M35 |
| **53** | **Magnet file selection** | **M36** |
| **55** | **Holepunch extension** | **M40** |

**26 BEPs total** — matches libtorrent-rasterbar's full BEP coverage.

### Non-BEP Features After M51

| Feature | Milestone |
|---------|-----------|
| Resume data / fast resume | M11 |
| Selective file download | M12 |
| End-game mode | M13 |
| Bandwidth limiter + peer classes | M14 |
| Automatic upload slot optimization | M14 |
| Alerts / events system | M15 |
| Queue management + auto-manage | M16 |
| Connection encryption (MSE/PE) | M17 |
| UPnP / NAT-PMP / PCP | M20 |
| Delayed/batched have messages | M23 |
| Smart banning + parole mode | M25 |
| Async disk I/O + mmap | M26 |
| Disk read cache (ARC algorithm) | M26 |
| Multi-threaded hashing | M27 |
| Advanced piece picker (block-level) | M28 |
| Dynamic request queue sizing | M28 |
| File streaming (AsyncRead+AsyncSeek) | M28 |
| IP filtering + proxy (SOCKS5/HTTP) | M29 |
| Torrent creation (v1 + hybrid) | M30 |
| Settings pack + presets | M31 |
| Metadata serving (BEP 9 complete) | M32a |
| Per-class rate limits (TCP/uTP) | M32b |
| Move storage | M32c |
| Share mode | M32c |
| Extension plugin interface | M32d |
| Hybrid v1+v2 torrents | M35 |
| **I2P support (SAM bridge)** | **M41** |
| **SSL torrents (X.509 peer auth)** | **M42** |
| **Seed choking algorithms (3 modes)** | **M43** |
| **Rate-based choker** | **M43** |
| **Piece extent affinity** | **M44** |
| **Suggest mode (read cache)** | **M44** |
| **Predictive piece announce** | **M44** |
| **Peer speed categorization** | **M44** |
| **Mixed-mode TCP/uTP algorithm** | **M45** |
| **Auto-sequential (formalized)** | **M45** |
| **Peer turnover** | **M46** |
| **SSRF mitigation** | **M47** |
| **IDNA rejection** | **M47** |
| **HTTPS certificate validation** | **M47** |
| **DSCP/ToS marking** | **M48** |
| **Anonymous mode (hardened)** | **M48** |
| **Pluggable disk I/O interface** | **M49** |
| **Session statistics counters** | **M50** |
| **Network simulation framework** | **M51** |

### Deliberately Excluded

| Item | Reason |
|------|--------|
| C API / Python bindings | Magnetor's WebUI API provides qBittorrent compatibility |
| BEP 46 (updating torrents via DHT) | libtorrent doesn't implement it; draft BEP, low adoption |
| Windows-specific optimizations | Linux-focused (SetFileValidData, sparse region detection) |
| Multiple crypto backends | Rust SHA-1/SHA-256 crates are sufficient |
| BEP 30 (legacy Merkle torrents) | Deprecated, superseded by BEP 52 |
| BEP 35 (separate from SSL torrents) | Covered by M42's SSL torrent support |
