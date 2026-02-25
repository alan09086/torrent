# M8b: Session Manager + LSD + Fast Extension — Design

## Goal

Add a multi-torrent session manager, BEP 14 Local Peer Discovery, and BEP 6 Fast Extension to ferrite-session. This completes the session layer's peer discovery and management capabilities.

## Context

M1-M8a complete (299 tests). TorrentActor handles single-torrent lifecycle including tracker announces and DHT peer discovery. Missing: multi-torrent orchestration, LAN peer discovery, and fast extension message handling.

**Approach:** Modular — session.rs, lsd.rs, and BEP 6 changes spread across wire/peer/choker as separate, independently testable components.

---

## Component 1: SessionHandle / SessionActor

### Architecture

Actor model, same pattern as TorrentActor/DhtActor:
- `SessionHandle` — cloneable public API (mpsc sender)
- `SessionActor` — single-owner event loop (internal)

SessionActor owns:
- Shared DHT handle (started once, passed to all torrents)
- LSD announcer (broadcasts for all active torrents)
- `HashMap<Id20, TorrentHandle>` for active torrents
- StorageFactory for creating per-torrent storage
- select! loop: command channel + LSD timer

### Types

```rust
pub type StorageFactory = Box<dyn Fn(&TorrentMetaV1, &Path) -> Arc<dyn TorrentStorage> + Send + Sync>;

pub struct SessionConfig {
    pub listen_port: u16,
    pub download_dir: PathBuf,
    pub max_torrents: usize,        // default: 100
    pub enable_dht: bool,
    pub enable_pex: bool,
    pub enable_lsd: bool,
    pub enable_fast_extension: bool,
    pub storage_factory: Option<StorageFactory>,
}

pub struct TorrentInfo {
    pub info_hash: Id20,
    pub name: String,
    pub total_length: u64,
    pub piece_length: u64,
    pub num_pieces: u32,
    pub files: Vec<FileInfo>,
    pub private: bool,
}

pub struct FileInfo {
    pub path: PathBuf,
    pub length: u64,
}

pub struct SessionStats {
    pub active_torrents: usize,
    pub total_downloaded: u64,
    pub total_uploaded: u64,
    pub dht_nodes: usize,
}
```

### Public API

```rust
impl SessionHandle {
    pub async fn start(config: SessionConfig) -> Result<Self>;
    pub async fn add_torrent(&self, meta: TorrentMetaV1, storage: Option<Arc<dyn TorrentStorage>>) -> Result<Id20>;
    pub async fn add_magnet(&self, magnet: Magnet) -> Result<Id20>;
    pub async fn remove_torrent(&self, info_hash: Id20) -> Result<()>;
    pub async fn pause_torrent(&self, info_hash: Id20) -> Result<()>;
    pub async fn resume_torrent(&self, info_hash: Id20) -> Result<()>;
    pub async fn torrent_stats(&self, info_hash: Id20) -> Result<TorrentStats>;
    pub async fn torrent_info(&self, info_hash: Id20) -> Result<TorrentInfo>;
    pub async fn list_torrents(&self) -> Result<Vec<Id20>>;
    pub async fn session_stats(&self) -> Result<SessionStats>;
    pub async fn shutdown(&self) -> Result<()>;
}
```

### Pause/Resume

- `TorrentState::Paused` — new variant
- `TorrentCommand::Pause` — disconnect all peers, announce Stopped, state → Paused
- `TorrentCommand::Resume` — state → Downloading, announce Started, reconnect
- SessionHandle proxies to TorrentHandle

### StorageFactory

Default factory creates `FilesystemStorage` from `download_dir + torrent_name/`. Callers can override for custom paths, memory-only testing, etc.

---

## Component 2: BEP 14 Local Peer Discovery (LSD)

### Module: `lsd.rs`

UDP multicast announcer + listener for LAN peer discovery.

### Protocol

- Multicast group: `239.192.152.143:6771` (IPv4)
- Message format:
  ```
  BT-SEARCH * HTTP/1.1\r\n
  Host: 239.192.152.143:6771\r\n
  Port: {listen_port}\r\n
  Infohash: {hex_info_hash}\r\n
  \r\n\r\n
  ```
- Multiple Infohash headers per message (batch announces)
- Rate limit: max 1 announce per torrent per 5 minutes
- Packet size ≤ 1400 bytes

### Types

```rust
pub(crate) struct LsdAnnouncer {
    socket: UdpSocket,
    listen_port: u16,
    last_announce: HashMap<Id20, Instant>,
}

pub(crate) struct LsdHandle {
    tx: mpsc::Sender<LsdCommand>,
}
```

### Behavior

- `LsdHandle::announce(info_hashes: &[Id20])` — batch announce, respecting rate limits
- Also listens for incoming LSD messages → parses and returns `(Id20, SocketAddr)` peer discoveries
- SessionActor calls `announce()` every 5 minutes with all active (non-paused) info hashes
- Discovered peers routed to the matching TorrentHandle via `add_peers()`

---

## Component 3: BEP 6 Fast Extension

### Wire Changes (`ferrite-wire`)

New message IDs:
- `0x0D` — Suggest Piece
- `0x0E` — Have All
- `0x0F` — Have None
- `0x10` — Reject Request
- `0x11` — Allowed Fast

New `Message` variants:
```rust
SuggestPiece(u32),
HaveAll,
HaveNone,
RejectRequest { index: u32, begin: u32, length: u32 },
AllowedFast(u32),
```

### Handshake Changes

- `Handshake::new()` sets Fast Extension bit: `reserved[7] |= 0x04`
- `Handshake::supports_fast() -> bool` — checks `reserved[7] & 0x04 != 0`
- BEP 6 messages only used when both peers support it

### Allowed-Fast Set Algorithm

```rust
pub fn allowed_fast_set(
    info_hash: &Id20,
    peer_ip: Ipv4Addr,
    num_pieces: u32,
    count: usize,
) -> Vec<u32>
```

BEP 6 algorithm: mask IP to /24, SHA1(masked_ip + info_hash), iteratively generate unique piece indices from hash bytes, re-hash when exhausted.

### PeerTask Changes

- Handshake: if both support fast, send `HaveAll`/`HaveNone` instead of Bitfield when we have all/no pieces
- Handle `RejectRequest` → clear pending request from in-flight, don't re-request immediately
- Handle `AllowedFast` → track allowed-fast pieces per peer, requestable even while choked
- Handle `SuggestPiece` → forward to TorrentActor as hint

### TorrentActor Changes

- `PeerState` gains `supports_fast: bool` and `allowed_fast: HashSet<u32>`
- When choking a fast-supporting peer: send `RejectRequest` for pending requests
- On peer connect: compute and send `AllowedFast` set

---

## Files Changed

| File | Change |
|------|--------|
| `ferrite-wire/src/message.rs` | Add 5 BEP 6 message variants + encode/decode |
| `ferrite-wire/src/handshake.rs` | Fast Extension flag + `supports_fast()` |
| `ferrite-wire/src/lib.rs` | Export `allowed_fast_set` function |
| `ferrite-session/src/session.rs` | **New** — SessionHandle + SessionActor |
| `ferrite-session/src/lsd.rs` | **New** — LSD announcer + listener |
| `ferrite-session/src/types.rs` | Add Paused state, SessionConfig, TorrentInfo, FileInfo, SessionStats, Pause/Resume commands |
| `ferrite-session/src/torrent.rs` | Handle Pause/Resume commands, expose info, fast extension peer handling |
| `ferrite-session/src/peer.rs` | BEP 6 message handling in peer task |
| `ferrite-session/src/peer_state.rs` | Add `supports_fast`, `allowed_fast` fields |
| `ferrite-session/src/lib.rs` | Export session module + new types |

## Test Plan (~30 tests)

| Area | Tests | Count |
|------|-------|-------|
| BEP 6 message encode/decode | Round-trip all 5 new messages | 5 |
| Allowed-fast set generation | Deterministic output, uniqueness, edge cases | 3 |
| HaveAll/HaveNone handling | Correctly set bitfield | 2 |
| Reject behavior | Clear in-flight on reject | 2 |
| LSD message formatting | Format, batch within MTU | 2 |
| LSD message parsing | Parse incoming announcements | 2 |
| LSD rate limiting | Respect 5-minute interval | 1 |
| Session lifecycle | Start, add, remove, shutdown | 3 |
| Session add_torrent | Adds torrent, returns info_hash | 1 |
| Session add_magnet | Adds magnet, returns info_hash | 1 |
| Session pause/resume | State transitions correctly | 2 |
| Session torrent_info | Returns correct metadata | 1 |
| Session list_torrents | Lists all active | 1 |
| Session stats | Aggregate stats correct | 1 |
| Session duplicate rejection | Can't add same torrent twice | 1 |
| Session max_torrents | Rejects when at capacity | 1 |
| Fast extension handshake flag | Set and detect correctly | 1 |

**Total: ~30 new tests → ~329 workspace total**

## Implementation Order

1. **BEP 6 wire messages** — ferrite-wire changes (message variants + codec + handshake flag + allowed-fast)
2. **Types + Pause/Resume** — ferrite-session types.rs additions, TorrentActor Pause/Resume handling
3. **LSD module** — lsd.rs with announcer + listener
4. **SessionActor** — session.rs with full lifecycle management
5. **BEP 6 peer behavior** — peer task and TorrentActor fast extension handling
6. **Integration tests** — session lifecycle, LSD, fast extension end-to-end
7. **Final verification** — full workspace test + clippy + docs + push
