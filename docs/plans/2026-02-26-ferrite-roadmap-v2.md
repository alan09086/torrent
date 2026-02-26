# Ferrite Roadmap v2 — Full libtorrent-rasterbar Parity

**Date:** 2026-02-26
**Target:** Complete libtorrent-rasterbar feature parity for magnetor (qBittorrent replacement)
**Current State:** M1-M10 complete, v0.11.0, 9 crates, 355 tests

## Completed (M1-M10)

| # | Milestone | Status |
|---|-----------|--------|
| M1 | ferrite-bencode — Serde bencode codec | Done |
| M2 | ferrite-core — Id20/Id32, TorrentMetaV1, Magnet, Lengths, PeerId | Done |
| M3 | ferrite-wire — Handshake, Message codec, BEP 6/9/10 extensions | Done |
| M4 | ferrite-tracker — HTTP + UDP tracker (BEP 3, 15) | Done |
| M5 | ferrite-dht — Kademlia DHT (BEP 5), actor model | Done |
| M6 | ferrite-storage — Bitfield, FileMap, ChunkTracker, TorrentStorage | Done |
| M7 | ferrite-session — Peer + torrent management, actor model | Done |
| M8a-c | ferrite-session — Tracker/DHT integration, session manager, BEP 6/14 | Done |
| M9 | ferrite-session — Seeding & upload pipeline | Done |
| M10a-e | ferrite — Public facade, ClientBuilder, prelude, unified error | Done |

### BEPs Implemented
BEP 3, 5, 6, 9, 10, 11, 12, 14, 15, 27

---

## Phase 1: Essential Desktop Client Features (M11-M16)

*Things you can't ship magnetor without. All session-layer work on existing
crates — no new external dependencies.*

### M11: Resume Data & Session Persistence

Save and restore torrent/session state across restarts.

**Scope:**
- `FastResumeData` struct: bitfield, download/upload stats, active time,
  seeding time, connected peers, tracker list, file priorities, piece
  priorities, mapped file paths
- Bencode serialization format (libtorrent-compatible where sensible)
- `TorrentHandle::save_resume_data()` → serialized bytes
- `SessionHandle::add_torrent_from_resume()` — restore from resume data
- Session state persistence: DHT node cache, torrent list, settings
- Auto-save on torrent state transitions (completed, paused, etc.)
- Skip hash verification when resume data present (fast startup)
- Re-verify if resume data appears corrupt

**Crates:** ferrite-core (FastResumeData type), ferrite-session (save/load)
**Tests:** ~10 (serialize round-trip, restore from resume, skip verification,
corrupt resume triggers re-check)

### M12: Selective File Download

Download only specific files from a multi-file torrent.

**Scope:**
- File priority enum: `Skip = 0`, `Low = 1`, `Normal = 4`, `High = 7`
  (matches libtorrent scale)
- `TorrentHandle::set_file_priority(index, priority)` and
  `file_priorities() -> Vec<FilePriority>`
- Piece picker respects file priorities — only request pieces that overlap
  non-skipped files
- Pieces shared across files: download if any overlapping file is wanted
- Storage skips allocation for fully-skipped files
- Priority changes at runtime (re-evaluate piece interest)
- Integration with resume data (file priorities persisted)

**Crates:** ferrite-session, ferrite-storage
**Tests:** ~8 (priority filtering, shared pieces, runtime change, skip
allocation, resume round-trip)

### M13: End-Game Mode

Accelerate completion of the last few pieces.

**Scope:**
- Activate when `pieces_remaining < threshold` (configurable, default: 20
  or when <5% remains, whichever is smaller)
- Request all remaining blocks from ALL connected peers
- Cancel duplicate requests when a block arrives (`Message::Cancel`)
- Track duplicate requests to avoid re-requesting already-in-flight blocks
- Deactivate if pieces_remaining increases (rare, e.g., hash failure)
- Per-torrent end-game state flag

**Crates:** ferrite-session
**Tests:** ~5 (activation threshold, duplicate cancel, hash failure recovery,
deactivation)

### M14: Bandwidth Limiter

Global and per-torrent rate control.

**Scope:**
- Token bucket rate limiter (upload and download, separate buckets)
- Global limiter: caps total session bandwidth
- Per-torrent limiter: caps individual torrent bandwidth
- Peer class system: global, TCP, local_peer (local network exempt from
  global limit by default)
- `SessionConfig` fields: `upload_rate_limit`, `download_rate_limit`
- `TorrentConfig` fields: `upload_rate_limit`, `download_rate_limit`
- Rate limiter checked before reading/writing socket data
- Smooth distribution across peers (no single peer starves)
- Limiter runs in session select! loop tick (e.g., every 100ms refill)

**Crates:** ferrite-session
**Tests:** ~6 (token bucket logic, global limit, per-torrent limit, peer
class exemption, refill timing)

### M15: Alerts / Events System

Typed, filterable event stream for client applications.

**Scope:**
- `Alert` enum with ~30 variants covering all major events:
  - Torrent lifecycle: Added, Removed, Paused, Resumed, Completed,
    StateChanged, Error
  - Transfer: PieceFinished, BlockFinished, HashFailed
  - Peers: PeerConnected, PeerDisconnected, PeerBanned
  - Tracker: TrackerReply, TrackerWarning, TrackerError, ScrapeReply
  - DHT: DhtBootstrapComplete, DhtGetPeers
  - Session: SessionStats, ListenSucceeded, ListenFailed
  - Storage: FileRenamed, StorageMoved, FileError
  - FastResume: ResumeDataSaved
- `AlertCategory` bitmask for filtering (status, error, peer, tracker,
  storage, dht, stats, etc.)
- `SessionHandle::alerts() -> mpsc::Receiver<Alert>` — async channel
- `SessionHandle::set_alert_mask(AlertCategory)` — filter categories
- Timestamp on every alert

**Crates:** ferrite-session
**Tests:** ~8 (alert delivery, category filtering, mask changes, all major
alert types fire)

### M16: Torrent Queue Management

Limit concurrent active torrents with automatic management.

**Scope:**
- `SessionConfig` fields: `active_downloads`, `active_seeds`,
  `active_limit`, `active_checking_limit`
- `dont_count_slow_torrents`: exempt torrents below a rate threshold
- Auto-manage: start/stop torrents based on queue position and limits
- `TorrentHandle` methods: `queue_position()`,
  `queue_position_up/down/top/bottom()`
- `auto_manage_interval` — how often to re-evaluate queue
- `auto_manage_prefer_seeds` — when constrained, prefer seeding over
  downloading
- Alerts: QueuePositionChanged, TorrentPaused (by auto-manage),
  TorrentResumed (by auto-manage)

**Crates:** ferrite-session
**Tests:** ~6 (queue ordering, auto-pause at limit, slow torrent exemption,
position manipulation, prefer seeds)

---

## Phase 2: Transport & Security (M17-M20)

*Network-level capabilities for real-world deployment. Introduces new crates
(ferrite-utp, ferrite-nat).*

### M17: Connection Encryption (MSE/PE)

Protocol obfuscation for ISP-hostile networks.

**Scope:**
- Message Stream Encryption (MSE) / Protocol Encryption (PE)
- Diffie-Hellman key exchange (768-bit)
- RC4 stream cipher (discard first 1024 bytes)
- Encryption modes: `Disabled`, `Enabled` (prefer encrypted, accept plain),
  `Forced` (encrypted only)
- Transparent wrapper: encrypted stream implements same traits as raw TCP
- Handshake negotiation: initiate with crypto header, fall back if peer
  doesn't support
- `SessionConfig::encryption_mode`
- Works with existing `Handshake` + `MessageCodec` — encryption layer sits
  below

**Crates:** ferrite-wire (or new ferrite-crypto)
**Tests:** ~8 (DH exchange, RC4 encrypt/decrypt, handshake negotiation,
forced mode rejection, round-trip through encrypted stream)

### M18: uTP Part 1 — Protocol Implementation

Micro Transport Protocol with delay-based congestion control.

**Scope:**
- New `ferrite-utp` crate
- LEDBAT (Low Extra Delay Background Transport) congestion control
- Packet types: ST_SYN, ST_DATA, ST_STATE, ST_FIN, ST_RESET
- Connection state machine: SynSent → Connected → FinSent → Closed
- Sliding window with dynamic sizing
- Selective acknowledgment (SACK)
- Timeout and retransmission
- Configurable target delay (default: 100ms)
- `UtpSocket` async read/write interface (tokio AsyncRead/AsyncWrite)
- `UtpListener` for accepting incoming connections
- Unit tests with loopback UDP (no simulated network needed initially)

**Crates:** ferrite-utp (new)
**Tests:** ~15 (connection lifecycle, data transfer, congestion window,
SACK, retransmission, FIN handshake, RST handling, concurrent connections)

### M19: uTP Part 2 — Session Integration

Wire uTP into the session layer alongside TCP.

**Scope:**
- Dual listener: TCP + uTP on same port (UDP socket shared with DHT or
  separate)
- `ConnectionType` enum: TCP, uTP
- Peer connection logic: prefer uTP, fall back to TCP
- `SessionConfig::enable_utp` (default: true)
- Rate limiter integration: uTP peers in separate peer class
  (`utp_peer_class_id`)
- Connection rate limiting for uTP (avoid UDP flood)
- Handshake over uTP stream (same wire protocol, different transport)
- Stats: track TCP vs uTP peer counts

**Crates:** ferrite-session, ferrite-utp
**Tests:** ~6 (dual listener, uTP peer connection, TCP fallback, peer class
assignment, stats tracking)

### M20: UPnP / NAT-PMP / PCP Port Mapping

Automatic port forwarding for incoming connections.

**Scope:**
- New `ferrite-nat` crate
- UPnP IGD (Internet Gateway Device) client:
  - SSDP discovery (M-SEARCH)
  - SOAP AddPortMapping / DeletePortMapping
  - Lease renewal timer
- NAT-PMP client (RFC 6886):
  - UDP request/response for port mapping
  - Gateway discovery
- PCP client (RFC 6887, successor to NAT-PMP):
  - MAP opcode for port mapping
- `NatHandle` — async handle, spawns background actor
- Auto-map on session start, unmap on shutdown
- `SessionConfig::enable_upnp`, `enable_natpmp`
- Alert: PortMappingSuccess, PortMappingError

**Crates:** ferrite-nat (new)
**Tests:** ~8 (SSDP discovery mock, SOAP request format, NAT-PMP packet
format, PCP packet format, lease renewal, error handling)

---

## Phase 3: Protocol Extensions (M21-M24)

*BEP coverage for full protocol compliance. Mostly independent of Phase 2.*

### M21: IPv6 Support (BEP 7, 24)

Dual-stack networking.

**Scope:**
- Dual-stack socket binding (listen on both IPv4 and IPv6)
- `listen_interfaces` config: `"0.0.0.0:6881,[::]:6881"`
- IPv6 compact peer format (18 bytes: 16-byte IP + 2-byte port)
- Tracker: IPv6 compact peers in announce response
- DHT: IPv6 routing table (BEP 24), separate k-buckets for v4/v6
- DHT: `nodes6` in find_node/get_peers responses
- Peer exchange: IPv6 peers in PEX messages
- Resume data: `peers6` list
- CompactNodeInfo6 (38 bytes: 20-byte ID + 16-byte IP + 2-byte port)

**Crates:** ferrite-dht, ferrite-tracker, ferrite-session, ferrite-core
**Tests:** ~8 (dual-stack binding, compact format, DHT v6 routing, PEX v6,
resume data v6)

### M22: HTTP/Web Seeding (BEP 17, 19)

Download pieces from HTTP servers.

**Scope:**
- BEP 19 (GetRight-style): multi-file aware, URL per torrent
  - HTTP GET with Range header for piece/block data
  - File boundary handling (pieces spanning files)
  - Retry with exponential backoff
- BEP 17 (Hoffman-style): URL per file
  - Simpler: one URL per file, standard HTTP Range
- Web seed URLs from .torrent file (`url-list`, `httpseeds` keys)
- Web seed URLs from resume data
- Integration with piece picker: web seeds treated as peers with all pieces
- `AddTorrentParams::url_seeds`, `http_seeds`
- Configurable max concurrent web seed connections

**Crates:** ferrite-session
**Tests:** ~8 (BEP 19 range calculation, BEP 17 file URL, multi-file
boundary, retry backoff, piece picker integration)

### M23: Super Seeding (BEP 16) + Upload-Only (BEP 21)

Optimized initial seeding and upload-only mode.

**Scope:**
- **Super seeding (BEP 16):**
  - Only advertise pieces that no connected peer has
  - Track which peers received which pieces
  - Send `Have` messages selectively (one piece per peer)
  - Maximize piece diversity in the swarm
  - `TorrentConfig::super_seeding` flag
- **Upload-only (BEP 21):**
  - Extension message: `upload_only = 1` in ext handshake
  - Don't request pieces from upload-only peers
  - Advertise upload-only when in seeding state
  - Peer deprioritization for choking decisions

**Crates:** ferrite-session, ferrite-wire
**Tests:** ~6 (super seed piece tracking, selective have, upload-only
message, choker behavior)

### M24: Tracker Scraping + Enhanced Tracker

Complete tracker protocol support.

**Scope:**
- HTTP scrape: `?info_hash=` parameter, parse scrape response
  (complete/incomplete/downloaded counts)
- UDP scrape: BEP 15 scrape action (2), multi-hash scrape
- Tracker exchange: `lt_trackers` extension (share tracker URLs with peers)
- Announce to specific tracker tiers
- `TrackerInfo` struct: seeders, leechers, completed, next_announce
- `TorrentHandle::tracker_list()`, `force_reannounce()`
- Alert: ScrapeReply, ScrapeError

**Crates:** ferrite-tracker, ferrite-session
**Tests:** ~6 (HTTP scrape parse, UDP scrape packet, lt_trackers exchange,
tier selection, force reannounce)

---

## Phase 4: Performance & Hardening (M25-M28)

*Scale and robustness for production use.*

### M25: Smart Banning

Detect and ban peers sending corrupt data.

**Scope:**
- Track hash failures per peer IP address
- After N hashfails from same IP (configurable, default: 3), ban the peer
- Parole mode (`use_parole_mode`): after a hashfail, re-request the piece
  only from a single other peer to isolate the bad source
- Persistent ban list (saved in resume data / session state)
- `SessionHandle::ban_peer(ip)`, `unban_peer(ip)`, `banned_peers()`
- Alert: PeerBanned, HashFailed (with peer attribution)

**Crates:** ferrite-session
**Tests:** ~5 (hashfail tracking, ban threshold, parole mode, persistent ban
list, manual ban/unban)

### M26: Async Disk I/O + mmap Storage

High-performance storage backend.

**Scope:**
- Disk thread pool: configurable number of I/O threads (default: 4)
- Async interface: `DiskRequest` enum (Read, Write, Hash, MoveStorage),
  submit via channel, completion via callback/oneshot
- mmap-based storage backend for 64-bit systems:
  - Memory-mapped file regions
  - Rely on kernel page cache (no user-space cache)
  - Automatic flush on piece completion
- Fallback: traditional pread/pwrite for 32-bit or constrained systems
- Sparse file allocation (default) vs full pre-allocation mode
- Write coalescing: batch adjacent blocks before flush
- Read-ahead: pre-read blocks likely needed soon
- `SessionConfig::disk_io_threads`, `storage_mode` (sparse/full/mmap)

**Crates:** ferrite-storage
**Tests:** ~10 (thread pool lifecycle, async read/write, mmap backend,
sparse allocation, write coalescing, concurrent access)

### M27: Multi-threaded Hashing

Parallel piece verification for fast startup and checking.

**Scope:**
- Parallel piece hashing using thread pool (rayon or manual)
- Configurable thread count (`SessionConfig::hashing_threads`)
- Used for: initial torrent check, resume data verification, piece
  completion verification
- Progress reporting: `checking_progress` float (0.0 → 1.0)
- Alert: TorrentChecked, CheckingProgress
- Integration with async disk I/O (disk threads feed hash threads)
- Piece verification priority: verify recently-completed pieces first

**Crates:** ferrite-storage, ferrite-session
**Tests:** ~5 (parallel hash correctness, progress reporting, priority
ordering, thread count config)

### M28: Advanced Piece Picker

Block-level picking and priority modes.

**Scope:**
- Block-level picking: download different blocks of the same piece from
  different peers simultaneously
- Priority pieces: first + last pieces of each file (for media preview)
- Time-critical mode: request first pieces of highest-priority file ASAP
- Sequential download mode (`TorrentConfig::sequential_download`)
- `initial_picker_threshold`: random pick for first N pieces (avoid
  rarest-first stampede on startup)
- `whole_pieces_threshold`: when a fast peer is detected, request whole
  pieces from them to reduce partial pieces
- Piece request coalescing: batch adjacent blocks for efficient I/O

**Crates:** ferrite-session
**Tests:** ~8 (block-level from multiple peers, priority pieces, sequential
mode, initial threshold, whole piece threshold, time-critical)

---

## Phase 5: Network Hardening & Tools (M29-M32)

*Remaining infrastructure for full parity.*

### M29: IP Filtering + Proxy Support

Network access control and proxy support.

**Scope:**
- **IP filter:**
  - IP range rules with bitmask-based peer class assignment
  - eMule .dat format parser (ipfilter.dat)
  - `SessionHandle::set_ip_filter()`, `ip_filter()`
  - Filter applied to incoming and outgoing connections
  - Local network exemption (192.168.x.x, 10.x.x.x, etc.)
- **Proxy support:**
  - SOCKS5 proxy for TCP connections
  - SOCKS5 UDP relay for DHT and uTP
  - HTTP CONNECT proxy
  - Per-connection proxy or session-wide proxy
  - `SessionConfig::proxy_type`, `proxy_host`, `proxy_port`,
    `proxy_username`, `proxy_password`

**Crates:** ferrite-session
**Tests:** ~8 (IP range filter, .dat parser, SOCKS5 handshake, HTTP CONNECT,
local network exemption, filter + proxy interaction)

### M30: Torrent Creation

Create .torrent files from local content.

**Scope:**
- `CreateTorrent` builder:
  - `add_file(path)`, `add_directory(path)`
  - `set_piece_size(bytes)` (auto-select if not set)
  - `set_comment(str)`, `set_creator(str)`, `set_private(bool)`
  - `add_tracker(url, tier)`, `add_web_seed(url)`, `add_http_seed(url)`
  - `set_source(str)` — source tag for private trackers
  - `generate() -> TorrentMetaV1`
- Piece hashing with progress callback
- Pad files and piece alignment for multi-file torrents
- `.torrent` file writer (bencode serialization)
- `ferrite::create_torrent()` facade function

**Crates:** ferrite-core
**Tests:** ~8 (single file, multi-file, piece size auto-select, private flag,
trackers, web seeds, pad files, round-trip parse)

### M31: Settings Pack + Runtime Config

Centralized typed configuration with runtime changes.

**Scope:**
- `Settings` struct: ~80-100 typed fields covering all configurable knobs
  across session, torrent, disk, network, DHT, tracker
- `SessionHandle::apply_settings(settings)` — change settings at runtime
- `SessionHandle::settings() -> Settings` — query current settings
- Preset profiles:
  - `Settings::default()` — balanced
  - `Settings::min_memory()` — constrained environments
  - `Settings::high_performance()` — desktop/server with plenty of resources
- Settings serialization (bencode or JSON) for persistence
- Settings validation (reject nonsensical combinations)
- Alert: SettingsChanged

**Crates:** ferrite-session
**Tests:** ~6 (default settings, presets, runtime change, validation,
serialization round-trip, alert)

### M32: Share Mode + Remaining Extensions

Final small features for completeness.

**Scope:**
- **Share mode:** join swarm and upload without downloading (seed without
  having the files — relay pieces as they arrive)
- **BEP 40:** canonical peer priority (deterministic peer ordering based
  on IP, reduces connections in large swarms)
- **Per-class rate limits:** rate limits on peer classes (global, TCP, uTP,
  local), not just global/per-torrent
- **Peer source tracking:** identify how each peer was discovered
  (tracker, DHT, PEX, LSD, incoming, resume data)
- **Move storage:** relocate torrent files to new directory while active

**Crates:** ferrite-session
**Tests:** ~6 (share mode relay, BEP 40 ordering, per-class limits, peer
source tracking, move storage)

---

## Phase 6: BitTorrent v2 (M33-M35)

*Next-generation protocol. Touches every layer of the stack.*

### M33: BEP 52 Core — v2 Metadata

v2 torrent format and hashing.

**Scope:**
- SHA-256 hashing (use existing `Id32` type)
- Per-file Merkle hash trees (piece layers)
- `TorrentMetaV2` struct: file tree format (nested dicts, not flat list),
  per-file piece root, pieces aligned to file boundaries
- v2 info-dict parsing and validation
- v2 info-hash computation (SHA-256 of info-dict)
- `Magnet` update: support both v1 (btih) and v2 (btmh) info-hashes
- v2 .torrent file parsing

**Crates:** ferrite-core, ferrite-bencode
**Tests:** ~10 (v2 parsing, Merkle tree construction, info-hash computation,
magnet v2, file tree format)

### M34: BEP 52 — v2 Wire Protocol + Storage

v2 peer protocol and storage changes.

**Scope:**
- v2 handshake: SHA-256 info-hash in handshake (32 bytes)
- Hash request/reject messages for Merkle proof exchange
- Per-file piece boundaries (pieces don't span files in v2)
- Storage layer: v2-aware piece/file mapping
- Merkle proof verification on received blocks (immediate 16 KiB
  verification instead of waiting for full piece)
- Piece layer caching (save verified Merkle nodes for resume)

**Crates:** ferrite-wire, ferrite-storage, ferrite-session
**Tests:** ~10 (v2 handshake, hash request/reject, Merkle verification,
per-file pieces, proof caching)

### M35: Hybrid v1+v2 Torrents + BEP 53

Dual-format support and magnet file selection.

**Scope:**
- **Hybrid torrents:**
  - Parse torrents with both v1 and v2 info-dicts
  - Compute both SHA-1 and SHA-256 info-hashes
  - Join both v1 and v2 swarms simultaneously
  - Handle `torrent_conflict_alert` (v1/v2 hash mismatch)
- **Hybrid torrent creation:** `CreateTorrent` generates both v1 + v2
- **BEP 53:** selective file download via magnet URI
  - `magnet:?...&so=0,2,4-6` (select files 0, 2, 4, 5, 6)
  - Parse `so` parameter, map to file priorities
- Full integration: hybrid torrents work with all features (resume data,
  selective download, web seeding, etc.)

**Crates:** ferrite-core, ferrite-session
**Tests:** ~8 (hybrid parse, dual swarm, BEP 53 magnet, hybrid creation,
conflict detection)

---

## Summary

| Phase | Milestones | New Crates | Estimated Tests |
|-------|-----------|------------|-----------------|
| 1: Desktop Essentials | M11-M16 | — | ~43 |
| 2: Transport & Security | M17-M20 | ferrite-utp, ferrite-nat | ~37 |
| 3: Protocol Extensions | M21-M24 | — | ~28 |
| 4: Performance | M25-M28 | — | ~28 |
| 5: Network & Tools | M29-M32 | — | ~28 |
| 6: BitTorrent v2 | M33-M35 | — | ~28 |
| **Total** | **M11-M35 (25 milestones)** | **2 new** | **~192** |

**Projected final state:** 11 crates, ~547 tests, full libtorrent-rasterbar
feature parity, ready for magnetor.

### BEP Coverage After M35

| BEP | Description | Milestone |
|-----|-------------|-----------|
| 3 | Base protocol | M1-M7 |
| 5 | DHT | M5 |
| 6 | Fast Extension | M3, M8b |
| 7 | IPv6 tracker | M21 |
| 9 | Metadata (magnet) | M3, M7 |
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
| 40 | Canonical peer priority | M32 |
| 52 | BitTorrent v2 | M33-M34 |
| 53 | Magnet file selection | M35 |

### Additional Features (not BEP-numbered)

| Feature | Milestone |
|---------|-----------|
| Resume data / fast resume | M11 |
| Selective file download | M12 |
| End-game mode | M13 |
| Bandwidth limiter + peer classes | M14 |
| Alerts / events system | M15 |
| Queue management + auto-manage | M16 |
| Connection encryption (MSE/PE) | M17 |
| UPnP / NAT-PMP / PCP | M20 |
| Smart banning + parole mode | M25 |
| Async disk I/O + mmap | M26 |
| Multi-threaded hashing | M27 |
| Advanced piece picker (block-level) | M28 |
| IP filtering + proxy (SOCKS5/HTTP) | M29 |
| Torrent creation (.torrent writer) | M30 |
| Settings pack + presets | M31 |
| Share mode | M32 |
