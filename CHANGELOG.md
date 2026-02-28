# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

## 0.31.0 — 2026-02-28

Torrent creation from local files/directories with full BEP support. Eleven crates, 748 tests.

### M30: Torrent Creation

### Added
- `CreateTorrent` builder — create `.torrent` files from local files or directories
- `CreateTorrentResult` — generated torrent metadata + raw `.torrent` bytes
- `auto_piece_size()` — libtorrent-style automatic piece size selection (32 KiB–4 MiB based on total size)
- Builder API: `add_file()`, `add_directory()`, `set_name()`, `set_piece_size()`, `set_comment()`, `set_creator()`, `set_creation_date()`, `set_private()`, `set_source()`
- Network metadata: `add_tracker(url, tier)`, `add_web_seed()` (BEP 19), `add_http_seed()` (BEP 17), `add_dht_node()` (BEP 5)
- BEP 47 pad file alignment: `set_pad_file_limit(Option<u64>)` — None=no padding, Some(0)=pad all, Some(n)=threshold
- File metadata: `include_mtime(bool)` for modification times, `include_symlinks(bool)` for symlink targets
- BEP 47 file attributes: auto-detected "x" (executable), "h" (hidden), "p" (pad), "l" (symlink)
- Pre-computed hashes: `set_hash(piece, Id20)` skips disk reads for pieces with known hashes
- Progress callback: `generate_with_progress(|current, total| ...)` for piece hashing progress
- `Serialize` derive added to `InfoDict` and `FileEntry` (backward-compatible, enables bencode output)
- `InfoDict::source` field — private tracker source tag
- `FileEntry::attr`, `FileEntry::mtime`, `FileEntry::symlink_path` fields (BEP 47)
- `Error::Io` and `Error::CreateTorrent` variants in ferrite-core
- Facade re-exports: `CreateTorrent`, `CreateTorrentResult` in `ferrite::core` and `ferrite::prelude`
- 14 new tests: auto piece size, single/multi-file round-trip, private+source, tracker tiers, web/HTTP seeds, pad files, progress callback, empty input error, info hash round-trip, DHT nodes, file mtime, pre-computed hashes

## 0.30.0 — 2026-02-28

IP filtering, proxy support, anonymous mode, and force proxy mode. First milestone in Phase 5 (Network Hardening & Tools). Eleven crates, 734 tests.

### M29: IP Filtering + Proxy Support

### Added
- `IpFilter` — sorted interval map-based IP address filtering with IPv4/IPv6 support
- `PortFilter` — port range filtering using the same interval map structure
- `IntervalMap<K>` — generic sorted interval map with O(log n) lookup via BTreeMap
- `parse_dat()` / `parse_p2p()` — eMule `.dat` and P2P plaintext blocklist parsers
- Local network exemption: RFC 1918, loopback, and link-local addresses always allowed
- `SharedIpFilter` wired through `SessionActor` → `TorrentActor` with checks at all 3 connection points
- `SessionHandle::set_ip_filter()` / `ip_filter()` for dynamic filter updates
- `apply_ip_filter_to_trackers` config option (default: true)
- `PeerBlocked` alert variant (PEER category)
- `ProxyType` enum: None, Socks4, Socks5, Socks5Password, Http, HttpPassword
- `ProxyConfig` with per-connection-type routing (peer, tracker, hostname resolution)
- Hand-rolled SOCKS4 handshake (RFC 1928 predecessor)
- Hand-rolled SOCKS5 handshake with RFC 1929 username/password auth
- HTTP CONNECT tunnel with optional Basic auth
- SOCKS5 UDP ASSOCIATE + `ProxiedUdpSocket` for UDP relay
- `connect_through_proxy()` — TCP tunnel through any supported proxy type
- `HttpTracker::with_proxy()` — proxy-aware HTTP tracker client
- `force_proxy` mode: mandates proxy for all connections, disables listen/UPnP/NAT-PMP/DHT/LSD
- `anonymous_mode`: suppresses client version in BEP 10 ext handshake, disables discovery
- Outbound TCP connections routed through proxy when `proxy_peer_connections` is true
- uTP skipped when proxy is active (not proxied)
- `ClientBuilder::proxy()`, `force_proxy()`, `anonymous_mode()`, `apply_ip_filter_to_trackers()`
- Facade re-exports: `IpFilter`, `IpFilterError`, `PortFilter`, `parse_dat`, `parse_p2p`, `ProxyType`, `ProxyConfig`

## 0.29.0 — 2026-02-28

Advanced piece picker with block-level priority, dynamic request queue sizing, and file streaming. Eleven crates, 689 tests.

### M28: Advanced Piece Picker + Dynamic Request Queue + File Streaming

### Added
- `PieceSelector` rewrite with 5 priority layers: streaming window, time-critical, suggested, partial (speed affinity), new piece (rarest-first/sequential/random)
- Block-level piece picking: `pick_blocks()` returns individual block assignments instead of whole pieces
- `PeerSpeed` classification (Slow/Medium/Fast) with speed-affinity partial piece assignment
- `InFlightPiece` — per-block peer assignment tracking replacing `HashSet<u32>` in-flight set
- `PickContext` — structured per-peer pick cycle input with 17 fields
- `PickResult` — pick output with piece, blocks, and exclusive flag for whole-piece assignment
- Seed counter on `PieceSelector` for effective availability without modifying per-piece arrays
- Auto-sequential mode: switches to sequential when in-flight explosion detected
- Snubbed peer reverse picking (highest availability first)
- `PeerPipelineState` — per-peer dynamic request queue with TCP-like slow-start and EWMA rate tracking
- BDP-based queue depth calculation: `rate * request_queue_time / chunk_size`, clamped to [2, max]
- Slow-start exit detection: plateau in throughput (delta < 10 KB/s between seconds)
- Block-level request timeout tracking with `timed_out_blocks()` and `most_recent_request()`
- `FileStream` — `AsyncRead + AsyncSeek` over individual torrent files for media streaming
- `FileStreamHandle` — internal handle passed from actor to construct FileStream
- `StreamingCursor` — actor-tracked cursor with readahead window for streaming priority
- `TorrentHandle::open_file()` — open a streaming reader for a file within the torrent
- Piece completion broadcasting via `broadcast::channel` for FileStream wake-on-piece
- Have-bitfield watch channel for FileStream piece availability checks
- Concurrent stream reader limiting via `Semaphore`
- `EndGame::activate_with_inflight()` — merges InFlightPiece data with pending requests
- `EndGame::pick_block_streaming()` — streaming-aware block picking in end-game mode
- `PeerState` fields: `pipeline`, `snubbed`, `last_data_received`, `suggested_pieces`
- `TorrentConfig` fields: `sequential_download`, `initial_picker_threshold`, `whole_pieces_threshold`, `snub_timeout_secs`, `readahead_pieces`, `streaming_timeout_escalation`, `max_concurrent_stream_reads`
- `SessionConfig` fields: `max_request_queue_depth`, `request_queue_time`, `block_request_timeout_secs`, `max_concurrent_stream_reads`
- `ClientBuilder` methods: `max_request_queue_depth()`, `request_queue_time()`, `block_request_timeout_secs()`, `max_concurrent_stream_reads()`
- `FileStream` re-exported in `ferrite::session` and `ferrite::prelude`
- `Error::FileSkipped` variant for attempting to stream a skipped file
- 22 new tests: 2 config, 4 pipeline, 11 piece selector, 5 streaming

### Changed
- `TorrentActor::in_flight` replaced with `in_flight_pieces: HashMap<u32, InFlightPiece>` for block-level tracking
- `request_pieces_from_peer()` rewritten to use `pick_blocks()` with pipeline queue depth
- `handle_piece_data()` updates pipeline state, clears snub on data receipt
- `check_end_game_activation()` uses `activate_with_inflight()` for block-level end-game
- `request_end_game_block()` uses `pick_block_streaming()` when streaming active
- SuggestPiece handler records to `peer.suggested_pieces`
- Unchoke tick now updates streaming cursors and time-critical pieces

## 0.28.0 — 2026-02-28

Multi-threaded piece hashing with JoinSet, Checking state with progress reporting, configurable hashing threads. Eleven crates, 665 tests.

### M27: Multi-threaded Hashing

### Added
- `TorrentState::Checking` variant — entered during piece verification on startup
- `TorrentStats.checking_progress` field (0.0–1.0) for tracking verification progress
- `AlertKind::TorrentChecked` — fires after piece verification with `pieces_have`/`pieces_total`
- `AlertKind::CheckingProgress` — fires per-piece with monotonically increasing progress
- `TorrentConfig.hashing_threads` and `SessionConfig.hashing_threads` (default: 2) — controls pipeline depth for concurrent piece verification
- `ClientBuilder::hashing_threads()` facade method
- 5 new tests: concurrent disk verify, config defaults, checking state + progress alerts, stats during check, partial data verification

### Changed
- `verify_existing_pieces()` now uses `tokio::task::JoinSet` for bounded-concurrency parallel hashing instead of sequential loop
- Torrent transitions through `Checking` → `Downloading`/`Seeding` on startup instead of going directly to `Downloading`
- Queue management handles `Checking` state as a download category

## 0.27.0 — 2026-02-28

Async disk I/O with central actor, mmap storage backend, ARC disk cache. Eleven crates, 660 tests.

### M26: Async Disk I/O + mmap Storage + ARC Disk Cache

### Added
- `StorageMode` enum in ferrite-core: Auto, Sparse, Full (fallocate), Mmap
- Full pre-allocation mode (`preallocate: bool`) for `FilesystemStorage` using `fallocate` on Linux
- `MmapStorage` backend using `memmap2` with lazy per-file mmap and multi-file spanning
- `ArcCache<K, V>` — Adaptive Replacement Cache (Megiddo & Modha 2003) with ghost lists and adaptive parameter
- `DiskActor` — central tokio task for all disk I/O, receives jobs via bounded mpsc channel
- `DiskHandle` — per-torrent async API: `write_chunk()`, `read_chunk()`, `verify_piece()`, `flush_piece()`, `move_storage()`
- `DiskManagerHandle` — session-level API: `register_torrent()`, `unregister_torrent()`, `shutdown()`
- `DiskConfig` — configurable I/O threads, storage mode, cache size, write cache ratio
- `DiskJobFlags` bitflags: FORCE_COPY, SEQUENTIAL, VOLATILE_READ, FLUSH_PIECE
- `DiskStats` — read/write bytes, cache hits/misses, write buffer usage, queued jobs
- `WriteBuffer` — per-piece write buffering with pressure-based eviction
- `AlertKind::DiskStatsUpdate(DiskStats)` alert variant
- `SessionConfig` fields: `disk_io_threads`, `storage_mode`, `disk_cache_size`, `disk_write_cache_ratio`
- `ClientBuilder` methods: `disk_io_threads()`, `storage_mode()`, `disk_cache_size()`
- Auto-flush WriteBuffer before piece hash verification
- Session disk lifecycle: register on add, unregister on remove, shutdown on exit
- Magnet torrents register storage with disk manager after metadata assembly
- Facade re-exports: `MmapStorage`, `ArcCache`, `DiskConfig`, `DiskHandle`, `DiskManagerHandle`, `DiskJobFlags`, `DiskStats`, `StorageMode`
- 23 new tests across storage, cache, disk, and session modules

### Changed
- `TorrentActor` uses `DiskHandle` instead of `Arc<dyn TorrentStorage>` — all I/O is async through DiskActor
- `TorrentHandle::from_torrent()` accepts `DiskHandle` + `DiskManagerHandle` instead of `Arc<dyn TorrentStorage>`
- `TorrentHandle::from_magnet()` accepts `DiskManagerHandle` for post-metadata storage registration
- `verify_existing_pieces()` is now async (verifies through DiskHandle)
- Piece reads for upload use async DiskHandle instead of synchronous storage

## 0.26.0 — 2026-02-28

Smart banning — peer attribution for hash failures, parole mode, session-wide bans. Eleven crates, 637 tests.

### M25: Smart Banning

### Added
- `BanConfig` — configurable max failures (default: 3) and parole toggle
- `BanManager` — session-wide ban/strike tracker shared via `Arc<RwLock<_>>`
- `ParoleState` — per-piece parole tracking (re-download from uninvolved peer)
- Piece contributor tracking: `TorrentActor` records which peers send data for each piece
- Parole mode: on hash failure, re-downloads piece from single uninvolved peer to isolate offender
- Parole success → strikes all original contributors; parole failure → strikes parole peer
- Ban checking at all connection points: `handle_add_peers`, `try_connect_peers`, `spawn_peer_from_stream_with_mode`
- `disconnect_banned_ip()` — removes all peers matching banned IP
- `AlertKind::HashFailed` extended with `contributors: Vec<IpAddr>` field
- `SessionHandle::ban_peer()`, `unban_peer()`, `banned_peers()` public API
- `SessionConfig::smart_ban_max_failures`, `smart_ban_parole` configuration fields
- `ClientBuilder::smart_ban_max_failures()`, `smart_ban_parole()` builder methods
- `SessionState::banned_peers`, `peer_strikes` persistence fields (backward compatible via `#[serde(default)]`)
- `PeerStrikeEntry` persistence struct
- `BanManager::restore()` for rebuilding from persisted state
- Facade re-exports: `BanConfig`, `PeerStrikeEntry` through `ferrite::session` and `ferrite::prelude`
- 16 new tests across ban, torrent, persistence, session, and facade modules

## 0.25.0 — 2026-02-28

Tracker scraping, tracker exchange, enhanced tracker management. Eleven crates, 621 tests.

### M24: Tracker Scrape + Enhanced Tracker Management

### Added
- BEP 48 scrape: `ScrapeInfo` type shared between HTTP and UDP, `announce_url_to_scrape()` URL conversion
- HTTP scrape: `HttpScrapeResponse`, `HttpTracker::build_scrape_url()`, `HttpTracker::scrape()` — parses bencode response with raw 20-byte hash keys
- UDP scrape: `UdpScrapeResponse`, `UdpTracker::build_scrape_request()`, `UdpTracker::parse_scrape_response()`, `UdpTracker::scrape()` — BEP 15 action=2 protocol
- `TrackerInfo` and `TrackerStatus` public types for tracker status reporting
- `TrackerManager::tracker_list()` — snapshot of all trackers with status, seeders/leechers, next announce time
- `TrackerManager::force_reannounce()` — reset all announce timers to now
- `TrackerManager::add_tracker_url()` — deduplicated tracker addition (used by lt_trackers exchange)
- `TrackerManager::scrape()` — try each tracker's scrape until one succeeds
- `consecutive_failures` and `scrape_info` tracking on `TrackerEntry`
- Announce response now threads seeders/leechers from tracker into `scrape_info`
- `AlertKind::ScrapeReply` and `AlertKind::ScrapeError` alert variants (categories: TRACKER, TRACKER|ERROR)
- lt_trackers wire protocol: `lt_trackers=3` registered in `ExtHandshake::new()`
- `LtTrackersMessage` — bencode encode/decode for tracker URL exchange between peers
- `PeerEvent::TrackersReceived` — new peer event for received tracker URLs
- Peer dispatch: incoming lt_trackers messages parsed and forwarded to TorrentActor
- TorrentActor: received tracker URLs added via `add_tracker_url()`
- `TorrentCommand::ForceReannounce`, `TrackerList`, `Scrape` command variants
- `TorrentHandle::tracker_list()`, `force_reannounce()`, `scrape()` public API methods
- Facade re-exports: `ScrapeInfo`, `announce_url_to_scrape`, `HttpScrapeResponse`, `UdpScrapeResponse`, `TrackerInfo`, `TrackerStatus`
- 22 new tests across tracker, session, wire, and facade crates

## 0.24.0 — 2026-02-27

Seeding optimizations — BEP 16 super seeding, BEP 21 upload-only, batched Have. Eleven crates, 599 tests.

### M23: Super Seeding, Upload-Only, Batched Have

### Added
- BEP 16 super seeding: `SuperSeedState` tracks per-peer piece assignments, reveals rarest pieces one-per-peer for maximum swarm diversity
- BEP 21 upload-only: `upload_only` field on `ExtHandshake`, `new_upload_only()` constructor, `is_upload_only()` accessor
- BEP 21 choker integration: upload-only peers excluded from optimistic unchoke slot but can earn regular unchoke by upload rate
- BEP 21 automatic broadcast: extension handshake with `upload_only=1` sent to all peers when transitioning to seeding
- Batched Have: `HaveBuffer` accumulates Have messages and flushes on configurable timer; auto-upgrades to full Bitfield when > 50% of pieces pending
- Redundancy elimination: Have messages not sent to peers that already have the piece (both immediate and batched modes)
- `super_seeding`, `upload_only_announce`, `have_send_delay_ms` config fields on `TorrentConfig` and `SessionConfig`
- `PeerCommand::SendExtHandshake` and `PeerCommand::SendBitfield` variants for mid-connection protocol messages
- `PeerState::upload_only` and `PeerState::super_seed_assigned` tracking fields
- `super_seeding` field on `FastResumeData` for resume persistence
- `ClientBuilder::super_seeding()`, `upload_only_announce()`, `have_send_delay_ms()` builder methods
- 14 new tests across wire, session types, choker, super_seed, have_buffer, resume_data, and facade

## 0.23.0 — 2026-02-27

HTTP/web seeding support — BEP 19 (GetRight) and BEP 17 (Hoffman). Eleven crates, 585 tests.

### M22: HTTP/Web Seeding (BEP 19, BEP 17)

### Added
- `url-list` parsing from torrent metadata with custom `UrlList` deserializer (handles both single string and list)
- `httpseeds` parsing from torrent metadata (BEP 17)
- `WebSeedTask` — background async task that downloads pieces from HTTP servers
- `WebSeedUrlBuilder` — constructs HTTP Range requests for single-file and multi-file torrents
- BEP 19 (GetRight): per-file HTTP Range requests against static web servers
- BEP 17 (Hoffman): parameterized URL requests with percent-encoded info hash
- `WebSeedMode` enum: `GetRight` (BEP 19) vs `Hoffman` (BEP 17)
- `WebSeedCommand` / `WebSeedError` types for task communication
- TorrentActor integration: `spawn_web_seeds()`, `assign_pieces_to_web_seeds()`, piece data handling
- Hash-failure banning: permanent URL ban on piece verification failure (per BEP 19 spec)
- `enable_web_seed` config field on `TorrentConfig` and `SessionConfig` (default: `true`)
- `max_web_seeds` config field on `TorrentConfig` (default: `4`)
- `WebSeedBanned` alert variant (category: STATUS)
- Resume data: `url_seeds` and `http_seeds` fields on `FastResumeData`
- `ClientBuilder::enable_web_seed()` facade builder method
- `url_encode_info_hash()` for BEP 17 percent-encoding
- reqwest dependency added to ferrite-session (rustls-tls)
- 19 new tests across metainfo, web_seed, types, resume_data, alert, and facade

## 0.22.0 — 2026-02-27

Full dual-stack IPv6 support — BEP 7, BEP 24, BEP 11 IPv6 extensions. Eleven crates, 566 tests.

### M21: IPv6 Support (BEP 7, 24)

### Added
- IPv6 compact peer format (18-byte: 16 IP + 2 port) — `parse_compact_peers6()` / `encode_compact_peers6()`
- IPv6 compact node format (38-byte: 20 ID + 16 IP + 2 port) — `CompactNodeInfo6` with encode/decode
- `encode_compact_peers()` for IPv4 (previously only parse existed)
- PEX IPv6 fields: `added6`, `added_flags6`, `dropped6` on `PexMessage` (BEP 11 extension)
- HTTP tracker `peers6` key parsing (BEP 7)
- KRPC `nodes6` field support in `GetPeersResponse` and `FindNode` responses (BEP 24)
- `AddressFamily` enum (`V4`, `V6`) in ferrite-core
- `DhtConfig::default_v6()` — IPv6 DHT bootstrap configuration (AAAA records)
- Dual DHT instances: separate `DhtActor` for IPv4 and IPv6, merged results
- DNS result filtering by address family in DHT bootstrap
- Routing table address family enforcement — rejects wrong-family addresses
- `enable_ipv6` config field on `SessionConfig` (default: `true`)
- Dual-stack TCP listeners: `[::]:port` with fallback to separate v4+v6 binds
- Dual-stack uTP sockets: separate v4 and v6 `UtpSocket` instances
- `is_local_network()` extended for IPv6: `::1`, `fe80::/10`, `fc00::/7`
- `allowed_fast_set_for_ip()` — IPv6 support with /48 prefix mask
- `ClientBuilder::enable_ipv6()` builder method
- Resume data `peers6` field codec wiring
- Facade re-exports: `AddressFamily`, `CompactNodeInfo6`, compact peers6/nodes6 codecs
- 32 new tests across tracker, dht, wire, session, and facade crates

### Design Decisions
- Two separate DhtActors (v4 and v6) rather than a single merged actor
- NAT stays IPv4-only (IPv6 doesn't need NAT traversal)
- Same port for both address families
- IPv6 DHT bootstraps from `router.bittorrent.com` (AAAA) and `dht.libtorrent.org:25401`

## 0.21.0 — 2026-02-27

Automatic NAT port mapping — PCP, NAT-PMP, UPnP IGD. Eleven crates, 534 tests.

### M20: UPnP / NAT-PMP / PCP Port Mapping

### Added
- `ferrite-nat` crate — automatic port mapping with three protocol implementations
- PCP (RFC 6887) MAP opcode encode/decode with IPv4-mapped IPv6 addressing
- NAT-PMP (RFC 6886) external address and port mapping request/response codec
- UPnP IGD SSDP M-SEARCH discovery and SOAP XML control (AddPortMapping, DeletePortMapping, GetExternalIPAddress)
- Gateway discovery via `/proc/net/route` parsing and local IP via UDP socket trick
- `NatActor` / `NatHandle` — background port mapping lifecycle with protocol fallback chain (PCP → NAT-PMP → UPnP)
- Automatic mapping renewal at half the granted lifetime
- Best-effort cleanup on shutdown (delete mappings via all protocols)
- `enable_upnp` and `enable_natpmp` config fields on `SessionConfig` (default: `true`)
- NAT event handling in `SessionActor` select loop — fires `PortMappingSucceeded` / `PortMappingFailed` alerts
- `ClientBuilder::enable_upnp()` and `enable_natpmp()` builder methods
- `ferrite::nat` facade module with full re-exports
- TCP listener bind changed from `127.0.0.1` to `0.0.0.0` for external reachability
- 23 new tests across nat, session, and facade crates

### Deferred
- IPv6 NAT traversal (M21)
- UPnP XML namespace-aware parsing (simple string search suffices)
- Per-torrent port mapping (all torrents share session port)

## 0.20.0 — 2026-02-27

uTP session integration — dual-protocol peer connections. Ten crates, 511 tests.

### M19: uTP Session Integration

### Added
- `enable_utp` config field on `TorrentConfig` and `SessionConfig` (default: `true`)
- `TorrentCommand::IncomingPeer` variant for session-routed uTP peers
- `PrefixedStream<S>` — `AsyncRead + AsyncWrite` wrapper replaying consumed preamble bytes
- `identify_plaintext_connection()` — reads 48-byte BT preamble, extracts info_hash for routing
- `spawn_peer_from_stream_with_mode()` — encryption mode override for pre-identified uTP streams
- Session-level `UtpSocket` (shared) and `UtpListener` with inbound accept loop
- Dual-protocol outbound: uTP first (5s timeout), TCP fallback (matches libtorrent behavior)
- Inbound uTP routing: session reads preamble → extracts info_hash → dispatches to correct torrent
- `TorrentHandle::send_incoming_peer()` for session → torrent peer dispatch
- `ClientBuilder::enable_utp()` builder method
- 10 new tests across session, routing, and facade crates

### Deferred
- MSE-encrypted inbound uTP routing (requires multi-skey lookup; dropped with debug log)

## 0.19.0 — 2026-02-27

uTP (BEP 29) micro transport protocol with LEDBAT congestion control. Ten crates, 501 tests.

### M18: uTP Protocol (BEP 29)

### Added
- `ferrite-utp` crate — standalone uTP implementation over UDP
- `SeqNr` — wrapping u16 sequence number arithmetic with correct circular ordering
- BEP 29 packet header encode/decode with SACK extension chain walking
- `LedbatController` — LEDBAT congestion control (100ms target delay, base delay tracking, Jacobson RTT)
- `Connection` — pure-logic state machine (SynSent → Connected → FinSent → Closed) producing `ConnAction` vectors
- `UtpStream` — `AsyncRead + AsyncWrite + Unpin + Send` facade bridging application ↔ socket actor
- `UtpSocket` — actor model (single UDP socket owner, per-connection dispatch, 50ms tick timer)
- `UtpListener` — accept incoming uTP connections
- `UtpConfig` — bind address and connection limits
- Send buffering with congestion window gating and flush-on-ACK
- SACK bitmask generation from receive buffer gaps
- Duplicate ACK counting with loss detection (3 duplicates = loss event)
- Retransmission on timeout with exponential backoff
- Facade re-exports in `ferrite::utp` and `ferrite::error::Error::Utp` variant
- 21 new tests: 4 seq, 4 packet, 4 congestion, 3 conn, 6 integration (1MB loopback transfer with SHA1 verification)

### Changed
- License changed from MIT OR Apache-2.0 to GPL-3.0-or-later

## 0.18.0 — 2026-02-27

MSE/PE connection encryption with DH key exchange and RC4 stream cipher. Nine crates, 480 tests.

### M17: Connection Encryption (MSE/PE)

### Added
- `EncryptionMode` enum: `Disabled`, `Enabled` (default, fallback), `Forced` (RC4 only)
- `MseStream<S>` — transparent `AsyncRead + AsyncWrite` wrapper with optional RC4 encryption
- `negotiate_outbound()` / `negotiate_inbound()` — MSE/PE handshake state machines
- `ferrite-wire::mse` submodule: `cipher` (RC4 with 1024-byte discard), `dh` (768-bit Diffie-Hellman), `crypto` (SHA1 key derivation), `handshake`, `stream`
- `encryption_mode` config field on `SessionConfig`, `TorrentConfig`, and `ClientBuilder`
- `ferrite_core::random_bytes()` — public RNG helper using existing xorshift64
- `EncryptionMode` re-exported in `ferrite::prelude`
- 21 new tests (RC4 roundtrip, DH key agreement, hash functions, encrypted stream, full handshake)

## 0.17.0 — 2026-02-27

Queue management with auto-manage system and position control. Nine crates, 459 tests.

### M16: Queue Management

### Added
- `SessionConfig` queue fields: `active_downloads`, `active_seeds`, `active_limit`, `active_checking`, `dont_count_slow_torrents`, `inactive_down_rate`, `inactive_up_rate`, `auto_manage_interval`, `auto_manage_startup`, `auto_manage_prefer_seeds`
- `SessionHandle` queue position API: `queue_position()`, `set_queue_position()`, `queue_position_up()`, `queue_position_down()`, `queue_position_top()`, `queue_position_bottom()`
- Auto-manage system — periodic queue evaluation that starts/stops auto-managed torrents based on active limits and inactivity detection
- `TorrentQueuePositionChanged` and `TorrentAutoManaged` alert variants
- `FastResumeData::queue_position` field for queue position persistence
- `ClientBuilder` queue config methods: 8 new builder methods for queue configuration
- `ferrite-session::queue` module — pure-logic queue position arithmetic and evaluation algorithm
- 22 new tests across queue unit tests, session integration, and facade tests

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
