# M32: Protocol Completeness — Design Document

**Date:** 2026-02-28
**Goal:** Complete the v1 BitTorrent protocol feature set before BitTorrent v2 work (M33-M35).

M32 is split into 4 sub-milestones to keep each session-sized and independently shippable.

---

## Sub-milestone Overview

| Sub-milestone | Scope | Version | Est. Tests |
|---|---|---|---|
| **M32a** | Metadata serving (BEP 9 complete) + peer source tracking | 0.33.0 | ~8 |
| **M32b** | BEP 40 canonical peer priority + per-class rate limits | 0.34.0 | ~10 |
| **M32c** | Move storage + share mode | 0.35.0 | ~8 |
| **M32d** | Extension plugin interface | 0.36.0 | ~8 |

---

## M32a: Metadata Serving + Peer Source Tracking

### Metadata Serving

**Problem:** ut_metadata (BEP 9) is implemented for downloading metadata (magnet links), but incoming metadata requests are silently dropped at peer.rs:452. Peers can't fetch metadata from us.

**Current state:**
- `MetadataDownloader` in metadata.rs handles receiving metadata pieces
- `MetadataMessageType::Request` is ignored in peer.rs
- `TorrentMetaV1` doesn't store raw info dict bytes (needed for serving)

**Design:**

1. **Store raw info dict bytes.** Add `info_bytes: Option<Bytes>` to `TorrentMetaV1`. Populated from:
   - `torrent_from_bytes()`: extract raw span via `find_dict_key_span("info")`, store as `Bytes::copy_from_slice`
   - `CreateTorrent::generate()`: store serialized InfoDict bytes before hashing
   - Magnet metadata assembly in MetadataDownloader: store assembled bytes after SHA1 verification

2. **Serve metadata from peer task.** On `MetadataMessageType::Request { piece }`:
   - Peer task asks TorrentActor for metadata slice (new command/event roundtrip)
   - TorrentActor responds with the 16 KiB chunk from `info_bytes`, or signals rejection
   - Peer task sends `MetadataMessageType::Data { piece, total_size, data }` or `Reject`

3. **New types:**
   - `PeerCommand::MetadataResponse { piece: u32, data: Option<Bytes> }` — actor → peer
   - `PeerEvent::MetadataRequest { peer_addr, piece: u32 }` — peer → actor
   - Or simpler: store `info_bytes: Option<Bytes>` on peer task state (cloned from TorrentActor on handshake)

4. **Simpler alternative (recommended):** Clone `info_bytes` to each peer task when the peer connects (if metadata is available). Peer task can serve requests directly without roundtripping to the actor. Memory cost: N_peers × metadata_size (typically <1 MB total).

### Peer Source Tracking

**Problem:** `available_peers` is `Vec<SocketAddr>` — no origin info. Can't distinguish tracker/DHT/PEX/LSD/incoming peers.

**Current state:**
- Peers added via `handle_add_peers(Vec<SocketAddr>)` — same signature for all sources
- `PeerState` has no source field
- No source info in stats or API

**Design:**

1. **`PeerSource` enum** in peer_state.rs:
   ```rust
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
   pub enum PeerSource {
       Tracker,
       Dht,
       Pex,
       Lsd,
       Incoming,
       ResumeData,
   }
   ```

2. **Change `available_peers: Vec<SocketAddr>` → `Vec<(SocketAddr, PeerSource)>`** in TorrentActor. Tag at each insertion point:
   - Tracker announce handler → `Tracker`
   - DHT results → `Dht`
   - PEX messages → `Pex`
   - LSD discoveries → `Lsd`
   - Inbound connections → `Incoming`
   - Resume data peer list → `ResumeData`

3. **Add `pub source: PeerSource` to PeerState.** Set when peer connects in `try_connect_peers()`.

4. **Expose in stats.** Add `peers_by_source: HashMap<PeerSource, usize>` to `TorrentStats`, or a dedicated peer list query.

---

## M32b: BEP 40 Canonical Peer Priority + Per-Class Rate Limits

### BEP 40 Canonical Peer Priority

**Problem:** Peers connect in FIFO order (`Vec::pop()`). BEP 40 defines deterministic IP-based ordering so clients in a swarm converge on the same connection graph — fewer redundant connections, better piece propagation.

**Algorithm (from BEP 40):**
- For IPv4: mask both IPs to /16, XOR the masked IPs, CRC32C the result → priority
- For IPv6: mask both IPs to /48, XOR the masked IPs, CRC32C the result → priority
- Sort candidate peers by priority (lower = connect first)
- Two clients with the same IP pair always compute the same priority

**Design:**

1. **New module `peer_priority.rs`** in ferrite-session:
   - `canonical_peer_priority(our_ip: IpAddr, peer_ip: IpAddr) -> u32`
   - Uses CRC32C — inline implementation (~30 lines, no new dependency) or `crc32c` crate

2. **Our external IP.** Add `external_ip: Option<IpAddr>` to SessionActor:
   - Set from tracker `external ip` response key
   - Set from NAT mapping result (already available)
   - Explicit override via `Settings::external_ip`
   - Fallback: local bind address

3. **Sort `available_peers` by priority** in `try_connect_peers()`:
   - When external_ip is known: sort by `canonical_peer_priority(external_ip, peer_ip)`
   - When unknown: fall back to current FIFO behavior

### Per-Class Rate Limits

**Problem:** Global + per-torrent rate limiters exist, but no differentiation between TCP, uTP, and LAN peers. uTP has its own LEDBAT congestion control and shouldn't compete for the same budget as TCP.

**Current state:**
- `TokenBucket` in rate_limiter.rs with `new()`, `refill()`, `try_consume()`, `set_rate()`
- `SharedBucket = Arc<Mutex<TokenBucket>>` shared across TorrentActors
- `is_local_network()` exempts LAN peers from global limiter

**Design:**

1. **4 new Settings fields:**
   - `tcp_upload_rate_limit: u64` (default: 0 = unlimited)
   - `tcp_download_rate_limit: u64`
   - `utp_upload_rate_limit: u64`
   - `utp_download_rate_limit: u64`

2. **`RateLimiterSet` struct** in rate_limiter.rs:
   ```rust
   pub struct RateLimiterSet {
       pub global_upload: TokenBucket,
       pub global_download: TokenBucket,
       pub tcp_upload: TokenBucket,
       pub tcp_download: TokenBucket,
       pub utp_upload: TokenBucket,
       pub utp_download: TokenBucket,
   }
   ```
   Replaces the two separate `SharedBucket` fields. Wrapped in `Arc<Mutex<RateLimiterSet>>`.

3. **`PeerTransport` enum** on PeerState:
   ```rust
   pub enum PeerTransport { Tcp, Utp }
   ```
   Set during connection establishment (TCP vs uTP path).

4. **Rate limit check chain:** For each transfer:
   - If `is_local_network(ip)` → exempt (no check)
   - Else: check class bucket (TCP or uTP) → check global bucket → both must pass

5. **Runtime update:** `apply_settings()` calls `set_rate()` on all 6 buckets.

---

## M32c: Move Storage + Share Mode

### Move Storage

**Problem:** `DiskHandle::move_storage()` and `DiskJob::MoveStorage` exist as stubs (disk.rs:443 returns `Ok(())`). Need actual file movement.

**Design:**

1. **Implement `DiskJob::MoveStorage` handler in DiskActor:**
   - Look up torrent's file list and base path from registry
   - For each file: create destination directories, then `std::fs::rename()` (fast same-filesystem) with `std::fs::copy()` + `std::fs::remove_file()` fallback (cross-filesystem)
   - Update base path in registry
   - Return `Ok(new_path)` or `Err` on first failure

2. **Pause I/O during move.** Set a flag on the disk registry entry to block read/write jobs while the move runs. Resume after completion.

3. **Add `TorrentCommand::MoveStorage`** with public `TorrentHandle::move_storage(new_path)` method.

4. **Expose via `SessionHandle::move_torrent_storage(info_hash, new_path)`.**

5. **Alerts:** `AlertKind::StorageMoved` on success (already exists), `AlertKind::FileError` on failure (already exists).

### Share Mode

**Problem:** Share mode lets a peer join a swarm and relay pieces without possessing full files locally. Pieces arrive, get verified, cached in memory, and served to requesters — never written to persistent disk.

**Use case:** Seed boxes, relay infrastructure, bandwidth donation.

**Design:**

1. **Add `share_mode: bool` to TorrentConfig and Settings** (default: false).

2. **TorrentActor behavior changes when `share_mode = true`:**
   - Skip DiskHandle registration — no file allocation
   - Write received chunks to in-memory buffer only (reuse WriteBuffer / ARC cache)
   - Verify pieces via SHA1 using in-memory data (normal verification path)
   - Serve requests from memory cache instead of disk
   - Never transition to `Complete` or `Seeding`

3. **New `TorrentState::Sharing`** — active relay state, never completes.

4. **Piece lifecycle:**
   ```
   Receive blocks → assemble in memory → SHA1 verify → mark in bitfield
   → advertise via Have → serve from cache → evict LRU when cache full
   ```

5. **Eviction handling:**
   - When cache is full, evict least-recently-served verified pieces
   - Clear bit in internal tracking (stop offering piece)
   - BT protocol has no "un-Have", so peers may still request evicted pieces
   - Respond with `RejectRequest` (BEP 6 Fast Extension) for evicted pieces

6. **Constraint:** Share mode requires BEP 6 Fast Extension (`enable_fast = true`). `Settings::validate()` returns error if `share_mode = true` and `enable_fast_extension = false`.

7. **Cache size:** Governed by `disk_cache_size` setting (repurposed for share mode as in-memory piece cache limit).

---

## M32d: Extension Plugin Interface

**Problem:** Extension message dispatch is hard-coded in peer.rs (ut_metadata=1, ut_pex=2, lt_trackers=3). Adding new extensions requires modifying ferrite internals.

**Design:**

### ExtensionPlugin Trait

```rust
/// A custom BEP 10 extension message handler.
pub trait ExtensionPlugin: Send + Sync + 'static {
    /// Extension name for BEP 10 handshake negotiation (e.g. "ut_comment").
    fn name(&self) -> &str;

    /// Called when a peer's extension handshake arrives.
    /// Return extra key-value pairs to merge into our handshake.
    fn on_handshake(
        &self,
        info_hash: &Id20,
        peer_addr: SocketAddr,
        handshake: &ExtHandshake,
    ) -> Option<BTreeMap<String, BencodeValue>>;

    /// Called when an extension message for this plugin arrives.
    /// Return optional response payload to send back.
    fn on_message(
        &self,
        info_hash: &Id20,
        peer_addr: SocketAddr,
        payload: &[u8],
    ) -> Option<Vec<u8>>;

    /// Peer connected (after BT handshake).
    fn on_peer_connected(&self, _info_hash: &Id20, _peer_addr: SocketAddr) {}

    /// Peer disconnected.
    fn on_peer_disconnected(&self, _info_hash: &Id20, _peer_addr: SocketAddr) {}
}
```

### Registration

- `ClientBuilder::add_extension(plugin: Box<dyn ExtensionPlugin>)` — collects plugins before session start
- Plugins stored as `Arc<Vec<Box<dyn ExtensionPlugin>>>` on SessionHandle, cloned to TorrentActors and peer tasks
- Immutable after session start (no hot-reload)

### Extension ID Allocation

- Built-in: ut_metadata=1, ut_pex=2, lt_trackers=3
- Plugins: IDs starting at 10, assigned in registration order
- `ExtHandshake::new()` merges built-in + plugin names into the `m` map

### Dispatch

In peer task's extension message handler (after existing hard-coded checks):
```
for (i, plugin) in plugins.iter().enumerate() {
    let our_id = 10 + i as u8;
    if ext_id == our_id {
        if let Some(response) = plugin.on_message(info_hash, peer_addr, payload) {
            // Look up peer's ID for this extension name
            if let Some(peer_ext_id) = peer_ext_ids.get(plugin.name()) {
                send Extended { ext_id: peer_ext_id, payload: response }
            }
        }
        break;
    }
}
```

### Constraints

- Plugins cannot access TorrentActor internals (piece state, peer list)
- Plugins cannot initiate unsolicited messages — respond only
- Plugins cannot override built-in extensions
- `on_message()` is synchronous — plugins needing async work spawn their own tasks
- Plugin registration order determines extension IDs (deterministic)

### Facade

- `ExtensionPlugin` trait: defined in ferrite-session, re-exported in `ferrite::session` and `ferrite::prelude`
- `ClientBuilder::add_extension()` method on facade
- `BencodeValue` re-exported for plugin handshake data

---

## Dependency Graph

```
M32a (metadata serving, peer source)
  │
  ├──► M32b (BEP 40, per-class rate limits)
  │      │
  │      ├──► M32c (move storage, share mode)
  │      │      │
  │      │      └──► M32d (extension plugin interface)
```

Each sub-milestone is independently useful but benefits from the prior work. M32d comes last because the concrete extension work in M32a-c informs the plugin API design.

---

## What This Does NOT Cover

- **Application-level hooks** (on_torrent_complete callbacks for tools like plex-pipeline/MagneTor) — these use the existing SessionHandle API + alert system
- **Hot-reload of plugins** — YAGNI, plugins are immutable after session start
- **Async plugin callbacks** — plugins are synchronous; spawn tasks internally if needed
- **BitTorrent v2** — deferred to M33-M35
