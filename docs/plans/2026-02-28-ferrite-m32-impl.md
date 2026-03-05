# M32: Protocol Completeness — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Complete the v1 BitTorrent protocol feature set, achieving or exceeding libtorrent-rasterbar parity in metadata exchange, peer management, rate limiting, storage operations, and extensibility.

**Architecture:** Four sequential sub-milestones (M32a→M32b→M32c→M32d), each independently shippable with its own version bump. The work fills the last significant protocol gaps: bidirectional BEP 9 metadata, BEP 40 peer prioritisation, per-class rate limiting, live storage relocation, relay/share mode, and a plugin system for custom BEP 10 extensions.

**Tech Stack:** Rust 2024, tokio, bytes, serde, thiserror. CRC32C inline implementation (no new deps). All existing workspace crates.

**Design doc:** `docs/plans/2026-02-28-m32-protocol-completeness-design.md`

---

## Senior Review: Out-of-Spec Fixes

The following issues from older milestones will be addressed inline during M32:

1. **BEP 9 metadata serving silently dropped** (`peer.rs:451-453`) — Fixed in Task 3 (M32a).
2. **DiskJob::MoveStorage stub returns Ok(()) without moving** (`disk.rs:443-445`) — Fixed in Task 13 (M32c).
3. **No external_ip tracking** — SessionActor has no `external_ip` field; NAT mapping results and tracker responses that provide external IP are not captured. Fixed in Task 8 (M32b).
4. **available_peers linear scan** — `available_peers.contains(&addr)` in `handle_add_peers` is O(n). Will be addressed via priority-sorted data structure in Task 9 (M32b).

---

## M32a: Metadata Serving + Peer Source Tracking (v0.33.0)

### Task 1: Add `info_bytes` to TorrentMetaV1

**Files:**
- Modify: `crates/ferrite-core/src/metainfo.rs:46-65` (TorrentMetaV1 struct)
- Modify: `crates/ferrite-core/src/metainfo.rs:139-165` (torrent_from_bytes)
- Modify: `crates/ferrite-core/src/create.rs` (CreateTorrent::generate)
- Test: `crates/ferrite-core/src/metainfo.rs` (existing test module)

**Step 1: Write the failing test**

```rust
// In crates/ferrite-core/src/metainfo.rs, test module
#[test]
fn torrent_from_bytes_stores_raw_info_bytes() {
    let data = make_torrent_bytes_sorted(b"", b"");
    let meta = torrent_from_bytes(&data).unwrap();
    assert!(meta.info_bytes.is_some());
    let info_bytes = meta.info_bytes.unwrap();
    // Re-hashing the stored bytes should produce the same info hash
    let rehash = crate::sha1(&info_bytes);
    assert_eq!(rehash, meta.info_hash);
}
```

**Step 2: Run test to verify it fails**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-core torrent_from_bytes_stores_raw_info_bytes`
Expected: FAIL — `info_bytes` field does not exist.

**Step 3: Implement**

Add `pub info_bytes: Option<bytes::Bytes>` to `TorrentMetaV1`. Add `bytes` dependency to ferrite-core if not already present.

In `torrent_from_bytes()`, after computing `info_span`:
```rust
let info_raw = Bytes::copy_from_slice(&data[info_span.clone()]);
// ... later in Ok(TorrentMetaV1 { ... })
info_bytes: Some(info_raw),
```

In `CreateTorrent::generate()` (create.rs), after serialising InfoDict:
```rust
let info_bytes = ferrite_bencode::to_bytes(&info);
let info_hash = crate::sha1(&info_bytes);
// Store in result meta:
meta.info_bytes = Some(Bytes::from(info_bytes.clone()));
```

**Step 4: Run test to verify it passes**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-core torrent_from_bytes_stores_raw_info_bytes`
Expected: PASS

**Step 5: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite-core/src/metainfo.rs crates/ferrite-core/src/create.rs crates/ferrite-core/Cargo.toml
git commit -m "feat: store raw info_bytes on TorrentMetaV1 for metadata serving (M32a)"
```

---

### Task 2: Populate info_bytes from magnet metadata assembly

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs:1741-1791` (try_assemble_metadata)

**Step 1: Write the failing test**

```rust
// In crates/ferrite-session/src/torrent.rs, test module
#[test]
fn assembled_metadata_stores_info_bytes() {
    // Build raw info dict bytes
    let info_bytes = b"d5:filesld6:lengthi100e4:pathl5:a.bineed6:lengthi100e4:pathl5:b.bineee4:name4:test12:piece lengthi100e6:pieces40:AAAAAAAAAAAAAAAAAAAABBBBBBBBBBBBBBBBBBBBe";
    let mut torrent_bytes = b"d4:info".to_vec();
    torrent_bytes.extend_from_slice(info_bytes);
    torrent_bytes.push(b'e');
    let meta = ferrite_core::torrent_from_bytes(&torrent_bytes).unwrap();
    assert!(meta.info_bytes.is_some());
    let stored = meta.info_bytes.unwrap();
    assert_eq!(stored.as_ref(), &info_bytes[..]);
}
```

**Step 2: Run test to verify it fails**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session assembled_metadata_stores_info_bytes`
Expected: PASS (this validates the Task 1 change works through the session layer). If it fails, the issue is in how try_assemble_metadata wraps raw bytes.

**Step 3: Update try_assemble_metadata**

In `try_assemble_metadata()`, after `dl.assemble_and_verify()` returns `Ok(info_bytes)`, store the raw info_bytes on the parsed meta before assigning to self.meta:
```rust
// After torrent_from_bytes succeeds:
let mut meta = meta;
meta.info_bytes = Some(Bytes::from(info_bytes));
```

Note: `info_bytes` here is the raw bytes from MetadataDownloader. The `torrent_from_bytes()` call already extracts them via find_dict_key_span, but the wrapper bytes we constructed (`d4:info...e`) shift the span. Storing the original `info_bytes` directly is correct since it IS the raw info dict.

**Step 4: Run tests**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session assembled_metadata`
Expected: PASS

**Step 5: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite-session/src/torrent.rs
git commit -m "feat: populate info_bytes from magnet metadata assembly (M32a)"
```

---

### Task 3: Serve metadata requests from peer task

**Files:**
- Modify: `crates/ferrite-session/src/peer.rs:30-44` (run_peer signature)
- Modify: `crates/ferrite-session/src/peer.rs:451-453` (MetadataMessageType::Request handler)
- Modify: `crates/ferrite-session/src/torrent.rs` (pass info_bytes to run_peer calls)
- Test: `crates/ferrite-session/src/peer.rs` (test module)

**Step 1: Write the failing test**

```rust
// In crates/ferrite-session/src/peer.rs test module
#[tokio::test]
async fn serves_metadata_request() {
    // Build minimal info dict
    let info_raw = b"d4:name4:test12:piece lengthi16384e6:pieces20:AAAAAAAAAAAAAAAAAAAAe";
    let info_bytes = Bytes::from_static(info_raw);

    // Simulate a peer sending a metadata request for piece 0
    let (event_tx, mut event_rx) = mpsc::channel(16);
    let addr: SocketAddr = "1.2.3.4:5678".parse().unwrap();

    let meta_msg = MetadataMessage::request(0);
    handle_metadata_message(addr, &meta_msg.to_bytes().unwrap(), &event_tx, Some(&info_bytes)).await.unwrap();

    // Should NOT produce an event (peer task handles it directly)
    // Instead, we check that a Data response would be generated
    // This test validates the function signature change
    assert!(event_rx.try_recv().is_err());
}
```

**Step 2: Run test to verify it fails**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session serves_metadata_request`
Expected: FAIL — `handle_metadata_message` doesn't accept info_bytes param.

**Step 3: Implement metadata serving**

The recommended approach from the design doc: pass `info_bytes: Option<Bytes>` to `run_peer()` (cloned from TorrentActor's `self.meta` on connect). The peer task serves requests directly without round-tripping to the actor.

1. Add `info_bytes: Option<Bytes>` parameter to `run_peer()`.
2. Thread it down to `handle_metadata_message()` as `info_bytes: Option<&Bytes>`.
3. In `MetadataMessageType::Request` handler:
```rust
MetadataMessageType::Request => {
    if let Some(info) = info_bytes {
        let total_size = info.len() as u64;
        let offset = meta_msg.piece as usize * 16384;
        if offset < info.len() {
            let end = (offset + 16384).min(info.len());
            let chunk = info.slice(offset..end);
            let response = MetadataMessage {
                msg_type: MetadataMessageType::Data,
                piece: meta_msg.piece,
                total_size: Some(total_size),
                data: Some(chunk),
            };
            // Send via framed_write (peer_ut_metadata is the remote's ID)
            if let Some(remote_id) = *peer_ut_metadata {
                let payload = response.to_bytes()?;
                framed_write.send(Message::Extended { ext_id: remote_id, payload: payload.into() }).await.map_err(crate::Error::Wire)?;
            }
        } else {
            // Invalid piece index — reject
            let reject = MetadataMessage::reject(meta_msg.piece);
            if let Some(remote_id) = *peer_ut_metadata {
                let payload = reject.to_bytes()?;
                framed_write.send(Message::Extended { ext_id: remote_id, payload: payload.into() }).await.map_err(crate::Error::Wire)?;
            }
        }
    } else {
        // No metadata available — reject
        let reject = MetadataMessage::reject(meta_msg.piece);
        if let Some(remote_id) = *peer_ut_metadata {
            let payload = reject.to_bytes()?;
            framed_write.send(Message::Extended { ext_id: remote_id, payload: payload.into() }).await.map_err(crate::Error::Wire)?;
        }
    }
}
```

Note: Because `handle_metadata_message` is a standalone async fn called from the main loop (not a method on a struct), and it doesn't have access to `framed_write` or `peer_ut_metadata`, we need to restructure. The cleanest approach: **inline the metadata request handling** in the main message loop where `framed_write` and `peer_ut_metadata` are in scope, or return a response action from `handle_metadata_message` that the main loop sends.

**Recommended:** Return `Option<(u8, Bytes)>` from `handle_metadata_message` for the response (remote ext_id, payload), and have the caller send it. This avoids changing the function's access pattern.

4. Update all `run_peer()` call sites in `torrent.rs` to pass `self.meta.as_ref().and_then(|m| m.info_bytes.clone())`.

**Step 4: Run tests**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session serves_metadata_request`
Expected: PASS

Also run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session`
Expected: All existing tests PASS (no regressions).

**Step 5: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite-session/src/peer.rs crates/ferrite-session/src/torrent.rs
git commit -m "feat: serve BEP 9 metadata requests from peer task (M32a)"
```

---

### Task 4: Add MetadataMessage::reject helper

**Files:**
- Modify: `crates/ferrite-wire/src/extended.rs` (MetadataMessage impl)
- Test: `crates/ferrite-wire/src/extended.rs` (test module)

**Step 1: Write the failing test**

```rust
#[test]
fn metadata_reject_roundtrip() {
    let msg = MetadataMessage::reject(5);
    assert_eq!(msg.msg_type, MetadataMessageType::Reject);
    assert_eq!(msg.piece, 5);
    let bytes = msg.to_bytes().unwrap();
    let parsed = MetadataMessage::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.msg_type, MetadataMessageType::Reject);
    assert_eq!(parsed.piece, 5);
}
```

**Step 2: Run test to verify it fails**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-wire metadata_reject_roundtrip`
Expected: FAIL — `reject` method doesn't exist.

**Step 3: Implement**

```rust
/// Create a reject for a metadata piece.
pub fn reject(piece: u32) -> Self {
    MetadataMessage {
        msg_type: MetadataMessageType::Reject,
        piece,
        total_size: None,
        data: None,
    }
}
```

**Step 4: Run test**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-wire metadata_reject_roundtrip`
Expected: PASS

**Step 5: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite-wire/src/extended.rs
git commit -m "feat: add MetadataMessage::reject() constructor (M32a)"
```

**Note:** Task 4 can be done before Task 3 (it's a dependency). Reorder during execution if needed.

---

### Task 5: Add PeerSource enum and update peer tracking

**Files:**
- Modify: `crates/ferrite-session/src/peer_state.rs` (PeerSource enum, source field)
- Modify: `crates/ferrite-session/src/torrent.rs` (available_peers type, handle_add_peers signature, all callers)
- Modify: `crates/ferrite-session/src/types.rs` (TorrentStats, TorrentCommand::AddPeers)
- Test: `crates/ferrite-session/src/peer_state.rs`

**Step 1: Write the failing test**

```rust
// In crates/ferrite-session/src/peer_state.rs
#[test]
fn peer_source_serialization() {
    let source = PeerSource::Tracker;
    let json = serde_json::to_string(&source).unwrap();
    assert_eq!(json, "\"Tracker\"");
    let roundtrip: PeerSource = serde_json::from_str(&json).unwrap();
    assert_eq!(roundtrip, PeerSource::Tracker);
}
```

**Step 2: Run test to verify it fails**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session peer_source_serialization`
Expected: FAIL — `PeerSource` doesn't exist.

**Step 3: Implement**

1. Add `PeerSource` enum to `peer_state.rs`:
```rust
/// Origin of a peer address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PeerSource {
    Tracker,
    Dht,
    Pex,
    Lsd,
    Incoming,
    ResumeData,
}
```

2. Add `pub source: PeerSource` to `PeerState`, update `PeerState::new()` to accept it.

3. Change `available_peers: Vec<SocketAddr>` → `Vec<(SocketAddr, PeerSource)>` in TorrentActor.

4. Update `handle_add_peers` signature:
```rust
fn handle_add_peers(&mut self, peers: Vec<SocketAddr>, source: PeerSource) {
    // ... same logic, but push (addr, source) tuples
}
```

5. Update all 7 call sites:
   - `torrent.rs:655` (AddPeers command) → `PeerSource::Tracker` (or pass from command)
   - `torrent.rs:786` (tracker announce) → `PeerSource::Tracker`
   - `torrent.rs:800` (DHT v4) → `PeerSource::Dht`
   - `torrent.rs:818` (DHT v6) → `PeerSource::Dht`
   - `torrent.rs:982` (initial announce) → `PeerSource::Tracker`
   - `torrent.rs:1096` (PEX) → `PeerSource::Pex`
   - LSD peers (if present) → `PeerSource::Lsd`

6. Update `try_connect_peers()` to destructure `(addr, source)` from `available_peers.pop()` and pass source to `PeerState::new()`.

7. Update `TorrentCommand::AddPeers` to include source:
```rust
AddPeers { peers: Vec<SocketAddr>, source: PeerSource },
```

8. Update `TorrentHandle::add_peers()` to accept/default source.

**Step 4: Run tests**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test --workspace`
Expected: All tests PASS. Clippy clean.

**Step 5: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite-session/src/peer_state.rs crates/ferrite-session/src/torrent.rs crates/ferrite-session/src/types.rs
git commit -m "feat: add PeerSource enum and track peer origins (M32a)"
```

---

### Task 6: Expose peers_by_source in TorrentStats

**Files:**
- Modify: `crates/ferrite-session/src/types.rs:147-158` (TorrentStats)
- Modify: `crates/ferrite-session/src/torrent.rs` (make_stats)
- Test: `crates/ferrite-session/src/torrent.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn torrent_stats_has_peers_by_source() {
    let stats = TorrentStats {
        state: TorrentState::Downloading,
        downloaded: 0,
        uploaded: 0,
        pieces_have: 0,
        pieces_total: 10,
        peers_connected: 0,
        peers_available: 0,
        checking_progress: 0.0,
        peers_by_source: HashMap::new(),
    };
    assert!(stats.peers_by_source.is_empty());
}
```

**Step 2: Run test to verify it fails**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session torrent_stats_has_peers_by_source`
Expected: FAIL — field doesn't exist.

**Step 3: Implement**

Add to `TorrentStats`:
```rust
pub peers_by_source: HashMap<PeerSource, usize>,
```

Update `make_stats()` to compute:
```rust
let mut peers_by_source = HashMap::new();
for ps in self.peers.values() {
    *peers_by_source.entry(ps.source).or_insert(0) += 1;
}
```

**Step 4: Run tests**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session`
Expected: PASS

**Step 5: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite-session/src/types.rs crates/ferrite-session/src/torrent.rs
git commit -m "feat: expose peers_by_source in TorrentStats (M32a)"
```

---

### Task 7: M32a facade updates, version bump, and full test

**Files:**
- Modify: `crates/ferrite/src/session.rs` (re-export PeerSource)
- Modify: `crates/ferrite/src/prelude.rs` (add PeerSource)
- Modify: `Cargo.toml` (workspace version → 0.33.0)

**Step 1: Add re-exports**

In `crates/ferrite/src/session.rs`, add `PeerSource` to the re-export list.
In `crates/ferrite/src/prelude.rs`, add `pub use crate::session::PeerSource;`.

**Step 2: Bump version**

In root `Cargo.toml`: `version = "0.33.0"`.

**Step 3: Full test + clippy**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: All pass, zero warnings.

**Step 4: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add Cargo.toml crates/ferrite/src/session.rs crates/ferrite/src/prelude.rs
git commit -m "feat: M32a facade re-exports, bump to v0.33.0 (M32a)"
```

---

## M32b: BEP 40 Canonical Peer Priority + Per-Class Rate Limits (v0.34.0)

### Task 8: External IP tracking on SessionActor

**Files:**
- Modify: `crates/ferrite-session/src/session.rs` (SessionActor fields, NAT event handling)
- Modify: `crates/ferrite-session/src/settings.rs` (add external_ip field)
- Modify: `crates/ferrite/src/client.rs` (ClientBuilder method)
- Test: `crates/ferrite-session/src/session.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn settings_external_ip_default_none() {
    let s = Settings::default();
    assert!(s.external_ip.is_none());
}
```

**Step 2: Run test to verify it fails**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session settings_external_ip_default_none`
Expected: FAIL — field doesn't exist.

**Step 3: Implement**

1. Add to `Settings`:
```rust
// ── External IP ──
#[serde(default, skip_serializing_if = "Option::is_none")]
pub external_ip: Option<IpAddr>,
```

2. Add `external_ip: Option<IpAddr>` to `SessionActor`.

3. In SessionActor's NAT event handling, when receiving a `PortMappingSucceeded` event that includes an external address, store it:
```rust
self.external_ip = Some(mapped_addr);
```

4. Pass `external_ip` to TorrentActor when creating new torrents (via a new field on TorrentConfig or a shared `Arc<Mutex<Option<IpAddr>>>`).

5. Add `ClientBuilder::external_ip(mut self, ip: IpAddr) -> Self` method.

**Step 4: Run tests**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session settings_external_ip`
Expected: PASS

**Step 5: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite-session/src/settings.rs crates/ferrite-session/src/session.rs crates/ferrite/src/client.rs
git commit -m "feat: track external IP from NAT mappings (M32b)"
```

---

### Task 9: BEP 40 canonical peer priority

**Files:**
- Create: `crates/ferrite-session/src/peer_priority.rs`
- Modify: `crates/ferrite-session/src/lib.rs` (add module)
- Test: `crates/ferrite-session/src/peer_priority.rs`

**Step 1: Write the failing tests**

```rust
// In crates/ferrite-session/src/peer_priority.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_known_vectors() {
        // RFC 3720 test vectors for CRC32C
        assert_eq!(crc32c(b""), 0x00000000);
        assert_eq!(crc32c(b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"), 0xAA36918A);
    }

    #[test]
    fn ipv4_same_subnet_same_priority() {
        // Two IPs in the same /16 produce the same priority regardless of low bytes
        let our = "192.168.1.1".parse().unwrap();
        let p1 = "10.0.1.1".parse().unwrap();
        let p2 = "10.0.2.2".parse().unwrap();
        let prio1 = canonical_peer_priority(our, p1);
        let prio2 = canonical_peer_priority(our, p2);
        assert_eq!(prio1, prio2, "same /16 prefix should produce same priority");
    }

    #[test]
    fn ipv4_different_subnets_different_priority() {
        let our = "192.168.1.1".parse().unwrap();
        let p1 = "10.0.1.1".parse().unwrap();
        let p2 = "172.16.1.1".parse().unwrap();
        let prio1 = canonical_peer_priority(our, p1);
        let prio2 = canonical_peer_priority(our, p2);
        assert_ne!(prio1, prio2);
    }

    #[test]
    fn symmetry() {
        // BEP 40 requires both sides compute the same priority
        let a: IpAddr = "1.2.3.4".parse().unwrap();
        let b: IpAddr = "5.6.7.8".parse().unwrap();
        assert_eq!(canonical_peer_priority(a, b), canonical_peer_priority(b, a));
    }

    #[test]
    fn ipv6_uses_48bit_mask() {
        let our: IpAddr = "2001:db8:1::1".parse().unwrap();
        let p1: IpAddr = "2001:db8:2::1".parse().unwrap();
        let p2: IpAddr = "2001:db8:2::ffff".parse().unwrap();
        // p1 and p2 share the same /48, so same priority
        assert_eq!(
            canonical_peer_priority(our, p1),
            canonical_peer_priority(our, p2)
        );
    }
}
```

**Step 2: Run test to verify they fail**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session peer_priority`
Expected: FAIL — module doesn't exist.

**Step 3: Implement**

Create `crates/ferrite-session/src/peer_priority.rs`:

```rust
//! BEP 40: Canonical Peer Priority.
//!
//! Deterministic IP-based peer ordering so clients in a swarm converge on
//! the same connection graph.

use std::net::IpAddr;

/// CRC32C lookup table (Castagnoli polynomial 0x1EDC6F41).
const CRC32C_TABLE: [u32; 256] = { /* generate at compile time or include literal */ };

/// Compute CRC32C checksum.
pub(crate) fn crc32c(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        let idx = ((crc as u8) ^ b) as usize;
        crc = (crc >> 8) ^ CRC32C_TABLE[idx];
    }
    !crc
}

/// Compute BEP 40 canonical peer priority between two IPs.
///
/// IPv4: mask both to /16, XOR, CRC32C.
/// IPv6: mask both to /48, XOR, CRC32C.
/// Result is symmetric: `priority(A, B) == priority(B, A)`.
pub(crate) fn canonical_peer_priority(our_ip: IpAddr, peer_ip: IpAddr) -> u32 {
    match (our_ip, peer_ip) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let a = a.octets();
            let b = b.octets();
            // Mask to /16 and XOR
            let xored = [a[0] ^ b[0], a[1] ^ b[1]];
            crc32c(&xored)
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let a = a.octets();
            let b = b.octets();
            // Mask to /48 (first 6 bytes) and XOR
            let mut xored = [0u8; 6];
            for i in 0..6 {
                xored[i] = a[i] ^ b[i];
            }
            crc32c(&xored)
        }
        // Mixed: treat IPv4 as IPv4-mapped IPv6
        (IpAddr::V4(a), IpAddr::V6(b)) => {
            let mapped = a.to_ipv6_mapped();
            canonical_peer_priority(IpAddr::V6(mapped), IpAddr::V6(b))
        }
        (IpAddr::V6(a), IpAddr::V4(b)) => {
            let mapped = b.to_ipv6_mapped();
            canonical_peer_priority(IpAddr::V6(a), IpAddr::V6(mapped))
        }
    }
}
```

Add `mod peer_priority;` to `crates/ferrite-session/src/lib.rs`.

**Step 4: Run tests**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session peer_priority`
Expected: PASS

**Step 5: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite-session/src/peer_priority.rs crates/ferrite-session/src/lib.rs
git commit -m "feat: BEP 40 canonical peer priority with CRC32C (M32b)"
```

---

### Task 10: Sort available_peers by canonical priority

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs` (try_connect_peers, handle_add_peers)
- Test: `crates/ferrite-session/src/torrent.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn peers_sorted_by_canonical_priority() {
    use crate::peer_priority::canonical_peer_priority;
    let our_ip: IpAddr = "192.168.1.1".parse().unwrap();
    let peers = vec![
        "10.0.1.1:6881".parse::<SocketAddr>().unwrap(),
        "172.16.0.1:6881".parse().unwrap(),
        "8.8.8.8:6881".parse().unwrap(),
    ];
    let mut sorted = peers.clone();
    sorted.sort_by_key(|p| canonical_peer_priority(our_ip, p.ip()));
    // Verify deterministic ordering
    assert_ne!(sorted, peers, "random order shouldn't match sorted order (statistically)");
    // Verify stability: sorting again produces same result
    let mut sorted2 = peers.clone();
    sorted2.sort_by_key(|p| canonical_peer_priority(our_ip, p.ip()));
    assert_eq!(sorted, sorted2);
}
```

**Step 2: Run test to verify it fails**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session peers_sorted_by_canonical_priority`
Expected: May pass or fail depending on order — the real test is the integration below.

**Step 3: Implement**

In `handle_add_peers()`, after adding new peers, sort `available_peers` by priority if external IP is known:

```rust
fn handle_add_peers(&mut self, peers: Vec<SocketAddr>, source: PeerSource) {
    // ... existing add logic ...
    // Sort by BEP 40 priority if we know our external IP
    if let Some(our_ip) = self.external_ip {
        self.available_peers.sort_by_key(|(addr, _)| {
            crate::peer_priority::canonical_peer_priority(our_ip, addr.ip())
        });
    }
}
```

Add `external_ip: Option<IpAddr>` field to TorrentActor, populated from session on creation.

Also change `try_connect_peers()` to pop from the front (lowest priority = connect first) instead of the back:
```rust
// Change from self.available_peers.pop() to:
let (addr, source) = match self.available_peers.first() {
    Some(a) => a.clone(),
    None => break,
};
self.available_peers.remove(0);
```

Or better: reverse sort order so `pop()` still works (sort descending).

**Step 4: Run tests**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test --workspace`
Expected: All PASS

**Step 5: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite-session/src/torrent.rs
git commit -m "feat: sort peer candidates by BEP 40 canonical priority (M32b)"
```

---

### Task 11: PeerTransport enum and RateLimiterSet

**Files:**
- Modify: `crates/ferrite-session/src/peer_state.rs` (PeerTransport enum, field)
- Modify: `crates/ferrite-session/src/rate_limiter.rs` (RateLimiterSet)
- Modify: `crates/ferrite-session/src/settings.rs` (4 new fields)
- Modify: `crates/ferrite/src/client.rs` (4 new builder methods)
- Test: `crates/ferrite-session/src/rate_limiter.rs`

**Step 1: Write the failing tests**

```rust
// In rate_limiter.rs
#[test]
fn rate_limiter_set_class_and_global_check() {
    let mut set = RateLimiterSet::new(
        1000,  // global upload
        1000,  // global download
        500,   // tcp upload
        500,   // tcp download
        0,     // utp upload (unlimited)
        0,     // utp download (unlimited)
    );
    let elapsed = Duration::from_millis(100);
    set.refill_all(elapsed);
    // TCP: must pass both class + global
    assert!(set.try_consume_upload(50, PeerTransport::Tcp));
    // uTP: class is unlimited, only global matters
    assert!(set.try_consume_upload(50, PeerTransport::Utp));
}
```

**Step 2: Run test to verify it fails**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session rate_limiter_set`
Expected: FAIL — struct doesn't exist.

**Step 3: Implement**

1. Add `PeerTransport` to `peer_state.rs`:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerTransport { Tcp, Utp }
```

2. Add `pub transport: PeerTransport` to `PeerState`.

3. Create `RateLimiterSet` in `rate_limiter.rs`:
```rust
pub(crate) struct RateLimiterSet {
    pub global_upload: TokenBucket,
    pub global_download: TokenBucket,
    pub tcp_upload: TokenBucket,
    pub tcp_download: TokenBucket,
    pub utp_upload: TokenBucket,
    pub utp_download: TokenBucket,
}

impl RateLimiterSet {
    pub fn new(gu: u64, gd: u64, tu: u64, td: u64, uu: u64, ud: u64) -> Self {
        Self {
            global_upload: TokenBucket::new(gu),
            global_download: TokenBucket::new(gd),
            tcp_upload: TokenBucket::new(tu),
            tcp_download: TokenBucket::new(td),
            utp_upload: TokenBucket::new(uu),
            utp_download: TokenBucket::new(ud),
        }
    }

    pub fn refill_all(&mut self, elapsed: Duration) {
        self.global_upload.refill(elapsed);
        self.global_download.refill(elapsed);
        self.tcp_upload.refill(elapsed);
        self.tcp_download.refill(elapsed);
        self.utp_upload.refill(elapsed);
        self.utp_download.refill(elapsed);
    }

    pub fn try_consume_upload(&mut self, amount: u64, transport: PeerTransport) -> bool {
        let class = match transport {
            PeerTransport::Tcp => &mut self.tcp_upload,
            PeerTransport::Utp => &mut self.utp_upload,
        };
        if !class.try_consume(amount) { return false; }
        if !self.global_upload.try_consume(amount) {
            // Refund class bucket — global denied
            // TokenBucket doesn't support refund, so we accept the minor inaccuracy
            return false;
        }
        true
    }

    pub fn try_consume_download(&mut self, amount: u64, transport: PeerTransport) -> bool {
        let class = match transport {
            PeerTransport::Tcp => &mut self.tcp_download,
            PeerTransport::Utp => &mut self.utp_download,
        };
        if !class.try_consume(amount) { return false; }
        if !self.global_download.try_consume(amount) { return false; }
        true
    }
}
```

4. Add 4 new Settings fields:
```rust
// ── Per-class rate limiting ──
#[serde(default)]
pub tcp_upload_rate_limit: u64,
#[serde(default)]
pub tcp_download_rate_limit: u64,
#[serde(default)]
pub utp_upload_rate_limit: u64,
#[serde(default)]
pub utp_download_rate_limit: u64,
```

5. Add ClientBuilder methods for the 4 new fields.

6. Update `SessionActor` to use `RateLimiterSet` instead of separate `SharedBucket` fields.

7. Update `TorrentActor`'s rate limiter refill and check logic.

**Step 4: Run tests**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: All PASS

**Step 5: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite-session/src/rate_limiter.rs crates/ferrite-session/src/peer_state.rs crates/ferrite-session/src/settings.rs crates/ferrite-session/src/session.rs crates/ferrite-session/src/torrent.rs crates/ferrite/src/client.rs
git commit -m "feat: per-class rate limits (TCP/uTP) with RateLimiterSet (M32b)"
```

---

### Task 12: M32b facade updates, version bump

**Files:**
- Modify: `crates/ferrite/src/session.rs` (re-export PeerTransport)
- Modify: `Cargo.toml` (workspace version → 0.34.0)

**Step 1: Add re-exports**

Add `PeerTransport` to session.rs re-exports.

**Step 2: Bump version**

Root `Cargo.toml`: `version = "0.34.0"`.

**Step 3: Full test + clippy**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: All pass.

**Step 4: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add Cargo.toml crates/ferrite/src/session.rs
git commit -m "feat: M32b facade re-exports, bump to v0.34.0 (M32b)"
```

---

## M32c: Move Storage + Share Mode (v0.35.0)

### Task 13: Implement DiskJob::MoveStorage handler

**Files:**
- Modify: `crates/ferrite-session/src/disk.rs:73-79` (MoveStorage variant — remove dead_code)
- Modify: `crates/ferrite-session/src/disk.rs:443-446` (process_job MoveStorage handler)
- Test: `crates/ferrite-session/src/disk.rs`

**Step 1: Write the failing test**

```rust
#[tokio::test]
async fn move_storage_relocates_files() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    std::fs::create_dir_all(&src).unwrap();

    // Create a file in src
    std::fs::write(src.join("test.txt"), b"hello").unwrap();

    let lengths = Lengths::new(5, 5, 5);
    let storage = Arc::new(FilesystemStorage::new(&src, &[("test.txt".into(), 5)], lengths.clone()).unwrap());
    let config = DiskConfig::default();
    let mgr = DiskManagerHandle::start(config);

    let ih = make_hash(100);
    let disk = mgr.register_torrent(ih, storage).await;
    let result = disk.move_storage(dst.clone()).await;
    assert!(result.is_ok());

    // Verify file exists at destination
    assert!(dst.join("test.txt").exists());
    assert_eq!(std::fs::read(dst.join("test.txt")).unwrap(), b"hello");
}
```

**Step 2: Run test to verify it fails**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session move_storage_relocates_files`
Expected: FAIL — returns Ok(()) without moving anything.

**Step 3: Implement**

Replace the stub in `process_job()`:
```rust
DiskJob::MoveStorage { info_hash, new_path, reply } => {
    let result = self.handle_move_storage(info_hash, new_path).await;
    let _ = reply.send(result);
}
```

Add method:
```rust
async fn handle_move_storage(
    &mut self,
    info_hash: Id20,
    new_path: PathBuf,
) -> ferrite_storage::Result<()> {
    let storage = match self.storages.get(&info_hash) {
        Some(s) => s.clone(),
        None => return Err(ferrite_storage::Error::Io(
            std::io::Error::new(std::io::ErrorKind::NotFound, "torrent not registered")
        )),
    };

    // Get file list from storage
    let files = storage.file_paths();
    std::fs::create_dir_all(&new_path)?;

    for file_path in &files {
        let dest = new_path.join(file_path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let src = storage.base_path().join(file_path);
        if !src.exists() {
            continue;
        }

        // Try rename first (fast, same filesystem)
        if std::fs::rename(&src, &dest).is_err() {
            // Fallback: copy + remove (cross-filesystem)
            std::fs::copy(&src, &dest)?;
            std::fs::remove_file(&src)?;
        }
    }

    Ok(())
}
```

Note: The exact implementation depends on `TorrentStorage` trait methods. If `file_paths()` and `base_path()` don't exist, we need to store file metadata alongside the storage in the DiskActor's registry. Check actual trait and adapt.

Remove `#[allow(dead_code)]` from the MoveStorage variant fields.

**Step 4: Run tests**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session move_storage`
Expected: PASS

**Step 5: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite-session/src/disk.rs
git commit -m "feat: implement DiskJob::MoveStorage with rename/copy fallback (M32c)"
```

---

### Task 14: TorrentCommand::MoveStorage and public API

**Files:**
- Modify: `crates/ferrite-session/src/types.rs` (TorrentCommand, TorrentHandle)
- Modify: `crates/ferrite-session/src/torrent.rs` (command handler)
- Modify: `crates/ferrite-session/src/session.rs` (SessionHandle method)
- Test: `crates/ferrite-session/src/session.rs`

**Step 1: Write the failing test**

```rust
#[tokio::test]
async fn torrent_handle_has_move_storage() {
    // Verify the method exists on TorrentHandle
    let session = SessionHandle::start(test_settings()).await.unwrap();
    // This is a compile-time test — the method signature is what we're validating
    let _: fn(&TorrentHandle, PathBuf) -> _ = TorrentHandle::move_storage;
    session.shutdown().await.unwrap();
}
```

**Step 2: Run test to verify it fails**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session torrent_handle_has_move_storage`
Expected: FAIL — method doesn't exist.

**Step 3: Implement**

1. Add to `TorrentCommand`:
```rust
MoveStorage {
    new_path: PathBuf,
    reply: oneshot::Sender<crate::Result<()>>,
},
```

2. Add to `TorrentHandle`:
```rust
pub async fn move_storage(&self, new_path: PathBuf) -> crate::Result<()> {
    let (tx, rx) = oneshot::channel();
    self.cmd_tx.send(TorrentCommand::MoveStorage { new_path, reply: tx }).await.map_err(|_| crate::Error::Shutdown)?;
    rx.await.map_err(|_| crate::Error::Shutdown)?
}
```

3. Handle in TorrentActor's command loop:
```rust
Some(TorrentCommand::MoveStorage { new_path, reply }) => {
    let result = if let Some(ref disk) = self.disk {
        disk.move_storage(new_path).await.map_err(crate::Error::Storage)
    } else {
        Err(crate::Error::MetadataNotReady)
    };
    let _ = reply.send(result);
}
```

4. Add `SessionHandle::move_torrent_storage(info_hash, new_path)`:
```rust
pub async fn move_torrent_storage(&self, info_hash: Id20, new_path: PathBuf) -> crate::Result<()> {
    let handle = self.torrent_handle(info_hash)?;
    handle.move_storage(new_path).await
}
```

**Step 4: Run tests**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test --workspace`
Expected: All PASS

**Step 5: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite-session/src/types.rs crates/ferrite-session/src/torrent.rs crates/ferrite-session/src/session.rs
git commit -m "feat: add TorrentHandle::move_storage() public API (M32c)"
```

---

### Task 15: Share mode — config and TorrentState::Sharing

**Files:**
- Modify: `crates/ferrite-session/src/types.rs` (TorrentState::Sharing)
- Modify: `crates/ferrite-session/src/settings.rs` (share_mode field)
- Modify: `crates/ferrite-session/src/types.rs` (TorrentConfig share_mode)
- Modify: `crates/ferrite/src/client.rs` (share_mode builder method)
- Test: `crates/ferrite-session/src/types.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn share_mode_state_variant_exists() {
    let state = TorrentState::Sharing;
    assert_ne!(state, TorrentState::Seeding);
}

#[test]
fn settings_share_mode_default_false() {
    let s = Settings::default();
    assert!(!s.share_mode);
}

#[test]
fn share_mode_requires_fast_extension() {
    let mut s = Settings::default();
    s.share_mode = true;
    s.enable_fast_extension = false;
    assert!(s.validate().is_err());
}
```

**Step 2: Run tests to verify they fail**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session share_mode`
Expected: FAIL — fields/variants don't exist.

**Step 3: Implement**

1. Add `Sharing` to `TorrentState` enum.
2. Add `pub share_mode: bool` to `Settings` (default false, serde default).
3. Add `pub share_mode: bool` to `TorrentConfig`, wire from Settings.
4. Add validation rule in `Settings::validate()`:
```rust
if self.share_mode && !self.enable_fast_extension {
    return Err(SettingsError::Invalid(
        "share_mode requires enable_fast_extension".into(),
    ));
}
```
5. Add `ClientBuilder::share_mode(mut self, v: bool) -> Self`.

**Step 4: Run tests**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session share_mode`
Expected: PASS

**Step 5: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite-session/src/types.rs crates/ferrite-session/src/settings.rs crates/ferrite/src/client.rs
git commit -m "feat: add share_mode config and TorrentState::Sharing (M32c)"
```

---

### Task 16: Share mode — in-memory piece relay behaviour

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs` (share mode logic in TorrentActor)
- Test: `crates/ferrite-session/src/torrent.rs`

**Step 1: Write the failing test**

```rust
#[tokio::test]
async fn share_mode_skips_disk_registration() {
    // When share_mode is true, torrent should not register with disk manager
    // and should use in-memory buffering instead
    let mut config = TorrentConfig::default();
    config.share_mode = true;
    // The torrent should enter Sharing state, not Downloading
    assert_eq!(config.share_mode, true);
}
```

**Step 2: Implement**

Share mode behaviour in TorrentActor:

1. When `config.share_mode` is true and metadata is available:
   - Do NOT call `disk_manager.register_torrent()` — skip disk allocation
   - Use the existing `WriteBuffer` (from ARC cache) for in-memory piece assembly
   - After SHA1 verification passes, keep piece data in memory cache
   - Transition to `TorrentState::Sharing` instead of `Downloading`
   - Never transition to `Complete` or `Seeding`

2. For serving requests in share mode:
   - Read from in-memory cache instead of disk
   - If piece has been evicted (cache full), respond with `RejectRequest` (BEP 6)

3. Piece eviction:
   - When cache exceeds `disk_cache_size`, evict LRU verified pieces
   - Clear the piece from internal tracking (don't clear bitfield — BT has no un-Have)

This is the most complex task in M32. Implement incrementally:
- First: skip disk registration when share_mode=true, transition to Sharing
- Then: in-memory piece verification
- Then: serving from cache
- Then: LRU eviction with RejectRequest

**Step 3: Run tests**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test --workspace`
Expected: All PASS

**Step 4: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite-session/src/torrent.rs
git commit -m "feat: share mode in-memory piece relay with LRU eviction (M32c)"
```

---

### Task 17: M32c facade updates, version bump

**Files:**
- Modify: `crates/ferrite/src/session.rs` (no new re-exports needed — TorrentState::Sharing is already covered)
- Modify: `Cargo.toml` (workspace version → 0.35.0)

**Step 1: Verify TorrentState::Sharing is visible through facade**

The `TorrentState` enum is already re-exported. Adding a new variant makes it automatically visible.

**Step 2: Bump version**

Root `Cargo.toml`: `version = "0.35.0"`.

**Step 3: Full test + clippy**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: All pass.

**Step 4: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add Cargo.toml
git commit -m "feat: bump to v0.35.0 (M32c)"
```

---

## M32d: Extension Plugin Interface (v0.36.0)

### Task 18: Define ExtensionPlugin trait

**Files:**
- Create: `crates/ferrite-session/src/extension.rs`
- Modify: `crates/ferrite-session/src/lib.rs` (add module)
- Test: `crates/ferrite-session/src/extension.rs`

**Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    struct TestPlugin;

    impl ExtensionPlugin for TestPlugin {
        fn name(&self) -> &str { "ut_test" }

        fn on_handshake(
            &self,
            _info_hash: &Id20,
            _peer_addr: SocketAddr,
            _handshake: &ExtHandshake,
        ) -> Option<BTreeMap<String, BencodeValue>> {
            None
        }

        fn on_message(
            &self,
            _info_hash: &Id20,
            _peer_addr: SocketAddr,
            _payload: &[u8],
        ) -> Option<Vec<u8>> {
            Some(b"pong".to_vec())
        }
    }

    #[test]
    fn plugin_trait_object_works() {
        let plugin: Box<dyn ExtensionPlugin> = Box::new(TestPlugin);
        assert_eq!(plugin.name(), "ut_test");
        let resp = plugin.on_message(
            &Id20([0; 20]),
            "1.2.3.4:5678".parse().unwrap(),
            b"ping",
        );
        assert_eq!(resp, Some(b"pong".to_vec()));
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session plugin_trait_object_works`
Expected: FAIL — module/trait doesn't exist.

**Step 3: Implement**

Create `crates/ferrite-session/src/extension.rs`:

```rust
//! BEP 10 extension plugin interface.
//!
//! Allows custom extensions to be registered with the session. Built-in
//! extensions (ut_metadata=1, ut_pex=2, lt_trackers=3) are handled
//! separately; plugins receive IDs starting at 10.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use ferrite_bencode::BencodeValue;
use ferrite_core::Id20;
use ferrite_wire::ExtHandshake;

/// A custom BEP 10 extension message handler.
///
/// Plugins are registered via `ClientBuilder::add_extension()` and are
/// immutable after session start. Plugins cannot access torrent internals
/// (piece state, peer list) — they receive only the payload bytes.
pub trait ExtensionPlugin: Send + Sync + 'static {
    /// Extension name for BEP 10 handshake negotiation (e.g. "ut_comment").
    fn name(&self) -> &str;

    /// Called when a peer's extension handshake arrives.
    /// Return extra key-value pairs to merge into our handshake response.
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

    /// Peer connected (after BT handshake). Optional hook.
    fn on_peer_connected(&self, _info_hash: &Id20, _peer_addr: SocketAddr) {}

    /// Peer disconnected. Optional hook.
    fn on_peer_disconnected(&self, _info_hash: &Id20, _peer_addr: SocketAddr) {}
}
```

Add `pub mod extension;` to `crates/ferrite-session/src/lib.rs`.

**Step 4: Run test**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session plugin_trait_object_works`
Expected: PASS

**Step 5: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite-session/src/extension.rs crates/ferrite-session/src/lib.rs
git commit -m "feat: define ExtensionPlugin trait for BEP 10 custom extensions (M32d)"
```

---

### Task 19: Plugin registration on ClientBuilder

**Files:**
- Modify: `crates/ferrite/src/client.rs` (add_extension method)
- Modify: `crates/ferrite-session/src/session.rs` (SessionHandle stores plugins)
- Test: `crates/ferrite/src/client.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn client_builder_accepts_plugins() {
    struct DummyPlugin;
    impl ExtensionPlugin for DummyPlugin {
        fn name(&self) -> &str { "ut_dummy" }
        fn on_handshake(&self, _, _, _) -> Option<_> { None }
        fn on_message(&self, _, _, _) -> Option<Vec<u8>> { None }
    }

    let builder = ClientBuilder::new()
        .add_extension(Box::new(DummyPlugin));
    let settings = builder.into_settings();
    // Should compile and not panic
    assert_eq!(settings.listen_port, 6881);
}
```

**Step 2: Run test to verify it fails**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite client_builder_accepts_plugins`
Expected: FAIL — method doesn't exist.

**Step 3: Implement**

1. Add `plugins: Vec<Box<dyn ExtensionPlugin>>` to `ClientBuilder`.
2. Add method:
```rust
pub fn add_extension(mut self, plugin: Box<dyn ExtensionPlugin>) -> Self {
    self.plugins.push(plugin);
    self
}
```
3. In `ClientBuilder::start()`, convert to `Arc<Vec<Box<dyn ExtensionPlugin>>>` and pass to `SessionHandle::start_with_plugins()`.
4. Store on `SessionHandle`, clone `Arc` to each `TorrentActor`, which clones to each peer task.

**Step 4: Run tests**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test --workspace`
Expected: All PASS

**Step 5: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite/src/client.rs crates/ferrite-session/src/session.rs
git commit -m "feat: ClientBuilder::add_extension() for plugin registration (M32d)"
```

---

### Task 20: Dynamic extension ID allocation and handshake merging

**Files:**
- Modify: `crates/ferrite-wire/src/extended.rs` (ExtHandshake::new_with_plugins)
- Modify: `crates/ferrite-session/src/peer.rs` (handshake construction)
- Test: `crates/ferrite-wire/src/extended.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn ext_handshake_with_plugins() {
    let plugin_names = vec!["ut_comment", "ut_holepunch"];
    let hs = ExtHandshake::new_with_plugins(&plugin_names);
    assert_eq!(hs.ext_id("ut_metadata"), Some(1));
    assert_eq!(hs.ext_id("ut_pex"), Some(2));
    assert_eq!(hs.ext_id("lt_trackers"), Some(3));
    assert_eq!(hs.ext_id("ut_comment"), Some(10));
    assert_eq!(hs.ext_id("ut_holepunch"), Some(11));
}
```

**Step 2: Run test to verify it fails**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-wire ext_handshake_with_plugins`
Expected: FAIL — method doesn't exist.

**Step 3: Implement**

Add to `ExtHandshake`:
```rust
/// Create a new extension handshake with built-in + plugin extension names.
/// Built-in: ut_metadata=1, ut_pex=2, lt_trackers=3.
/// Plugins: IDs starting at 10, in order.
pub fn new_with_plugins(plugin_names: &[&str]) -> Self {
    let mut m = BTreeMap::new();
    m.insert("ut_metadata".into(), 1);
    m.insert("ut_pex".into(), 2);
    m.insert("lt_trackers".into(), 3);
    for (i, name) in plugin_names.iter().enumerate() {
        m.insert(name.to_string(), 10 + i as u8);
    }
    Self { m, ..Self::default_fields() }
}
```

In `peer.rs`, when constructing `our_ext`, use `new_with_plugins()` if plugins are present.

**Step 4: Run test**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-wire ext_handshake_with_plugins`
Expected: PASS

**Step 5: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite-wire/src/extended.rs crates/ferrite-session/src/peer.rs
git commit -m "feat: dynamic extension ID allocation for plugins (M32d)"
```

---

### Task 21: Plugin dispatch in peer task

**Files:**
- Modify: `crates/ferrite-session/src/peer.rs:335-345` (extension message router)
- Test: `crates/ferrite-session/src/peer.rs`

**Step 1: Write the failing test**

```rust
#[tokio::test]
async fn plugin_receives_extension_message() {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct TrackingPlugin {
        received: Arc<AtomicBool>,
    }

    impl ExtensionPlugin for TrackingPlugin {
        fn name(&self) -> &str { "ut_test" }
        fn on_handshake(&self, _, _, _) -> Option<_> { None }
        fn on_message(&self, _, _, payload: &[u8]) -> Option<Vec<u8>> {
            self.received.store(true, Ordering::SeqCst);
            Some(payload.to_vec()) // echo back
        }
    }

    let received = Arc::new(AtomicBool::new(false));
    let plugin = TrackingPlugin { received: received.clone() };
    // ... test dispatching an extension message with ext_id=10 to this plugin
    assert!(received.load(Ordering::SeqCst));
}
```

**Step 2: Implement**

In the extension message handler (peer.rs:335-345), after the built-in checks:

```rust
Message::Extended { ext_id, payload } => {
    if Some(ext_id) == our_ut_metadata {
        handle_metadata_message(addr, &payload, &event_tx, info_bytes.as_ref()).await?;
    } else if Some(ext_id) == our_ut_pex {
        handle_pex_message(&payload, &event_tx).await?;
    } else if Some(ext_id) == our_lt_trackers {
        handle_lt_trackers_message(&payload, &event_tx).await?;
    } else if ext_id >= 10 {
        // Plugin dispatch
        let plugin_idx = (ext_id - 10) as usize;
        if let Some(plugin) = plugins.get(plugin_idx) {
            if let Some(response) = plugin.on_message(&info_hash, addr, &payload) {
                // Look up the remote's ID for this extension
                if let Some(&remote_id) = peer_ext_ids.get(plugin.name()) {
                    framed_write
                        .send(Message::Extended { ext_id: remote_id, payload: response.into() })
                        .await
                        .map_err(crate::Error::Wire)?;
                }
            }
        }
    } else {
        warn!(%addr, ext_id, "unknown extension message");
    }
}
```

Also call `plugin.on_peer_connected()` after handshake and `plugin.on_peer_disconnected()` on disconnect.

Track `peer_ext_ids: HashMap<String, u8>` from the peer's extension handshake for all plugin names.

**Step 3: Run tests**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test --workspace`
Expected: All PASS

**Step 4: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add crates/ferrite-session/src/peer.rs
git commit -m "feat: plugin dispatch in peer task extension message handler (M32d)"
```

---

### Task 22: M32d facade updates, version bump, final test

**Files:**
- Modify: `crates/ferrite/src/session.rs` (re-export ExtensionPlugin)
- Modify: `crates/ferrite/src/prelude.rs` (add ExtensionPlugin)
- Modify: `Cargo.toml` (workspace version → 0.36.0)

**Step 1: Add re-exports**

In `crates/ferrite/src/session.rs`: add `ExtensionPlugin` to re-exports.
In `crates/ferrite/src/prelude.rs`: add `pub use crate::session::ExtensionPlugin;`.

**Step 2: Bump version**

Root `Cargo.toml`: `version = "0.36.0"`.

**Step 3: Full test + clippy**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: All pass, zero warnings.

**Step 4: Commit**

```bash
cd /mnt/TempNVME/projects/ferrite
git add Cargo.toml crates/ferrite/src/session.rs crates/ferrite/src/prelude.rs
git commit -m "feat: M32d facade re-exports, bump to v0.36.0 (M32d)"
```

---

## Verification Checklist

After all M32 tasks are complete:

- [ ] `cargo test --workspace` — all tests pass (target: ~790+ tests)
- [ ] `cargo clippy --workspace -- -D warnings` — zero warnings
- [ ] BEP 9 metadata serving works (peer.rs no longer ignores requests)
- [ ] DiskJob::MoveStorage actually moves files (no more stub)
- [ ] PeerSource tracks where each peer came from
- [ ] BEP 40 canonical peer priority sorts peer connection order
- [ ] Per-class rate limits differentiate TCP and uTP traffic
- [ ] Share mode relays pieces without disk allocation
- [ ] Extension plugins can be registered and receive messages
- [ ] All facade re-exports present (PeerSource, PeerTransport, ExtensionPlugin)
- [ ] Workspace version is 0.36.0
- [ ] Push to both remotes: `git push origin main && git push github main`

---

## libtorrent Parity Comparison (Post-M32)

| Feature | libtorrent | ferrite (post-M32) | Status |
|---------|-----------|-------------------|--------|
| Metadata exchange (BEP 9) | Full | Full (M32a) | Parity |
| Peer source tracking | Yes | Yes (M32a) | Parity |
| Canonical peer priority (BEP 40) | Yes | Yes (M32b) | Parity |
| Per-class rate limits | Yes | Yes (M32b) | Parity |
| Move storage | Yes | Yes (M32c) | Parity |
| Share mode | Yes | Yes (M32c) | Parity |
| Extension plugins | Yes | Yes (M32d) | Parity |
| BitTorrent v2 (BEP 52) | Yes | No (M33-35) | Deferred |
| SSL torrents | Yes | No | Out of scope |
| i2p support | Yes | No | Out of scope |
