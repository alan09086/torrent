# Torrent Audit Remediation — M86–M91 Design

**Date:** 2026-03-13
**Status:** Approved
**Scope:** Six milestones addressing dead code, stubs, and incomplete implementations found during a full 12-crate audit of v0.85.0.

## Background

A comprehensive audit of all 12 torrent crates at v0.85.0 (1460 tests) identified dead code, stub implementations, and unwired features. M85 (DHT routing overhaul) resolved the DHT-specific findings (`pending_node_id` stub, stale `#[allow(dead_code)]` annotations). The remaining findings are organized into six sequential milestones (M86–M91), ordered by user-facing severity.

## Milestone Summary

| Milestone | Title | Severity | Size | Crates Touched |
|-----------|-------|----------|------|----------------|
| M86 | Dead disk backend cleanup | HIGH | Small | torrent-session |
| M87 | BEP 52 hash serving | HIGH | Medium | torrent-session, torrent-core |
| M88 | BEP 44 session API | MEDIUM | Medium | torrent-session |
| M89 | NAT cleanup logging | MEDIUM | Small | torrent-nat |
| M90 | I2P session integration | MEDIUM | Large | torrent-session |
| M91 | SimTransport integration | MEDIUM | Large | torrent-sim, torrent-session |

---

## M86: Dead Disk Backend Cleanup

### Problem

`DiskIoBackend::move_storage()` is a trait method with three stub implementations (DisabledDiskIo, PosixDiskIo, MmapDiskIo) that all return `Ok(())` without doing anything. A `DiskJob::MoveStorage` variant and `DiskHandle::move_storage()` method dispatch to these stubs. None of this code is ever called — `TorrentActor::handle_move_storage()` (torrent.rs:2342-2429) handles the entire operation directly via `relocate_files()`, including rename-with-copy-fallback, storage unregister/re-register, and `StorageMoved` alert firing.

### Scope

- Remove `move_storage()` from the `DiskIoBackend` trait (disk_backend.rs:92)
- Remove all three impl stubs (disk_backend.rs:171, 394, 538)
- Remove `DiskJob::MoveStorage` variant (disk.rs:123-127)
- Remove `DiskHandle::move_storage()` method (disk.rs:478-493)
- Remove the `DiskJob::MoveStorage` match arm in the disk actor dispatch loop (disk.rs:893-909)
- Verify no callers reference any of these

### Not In Scope

Refactoring `TorrentActor::handle_move_storage()` to go through the disk backend. The current direct approach works correctly — the move operation involves session-level orchestration (unregister/re-register, alert firing) that doesn't belong in a disk backend.

### Tests

Existing move_storage tests continue to pass since they exercise `TorrentActor::handle_move_storage()`, not the dead backend path.

---

## M87: BEP 52 Hash Serving

### Problem

`handle_incoming_hash_request()` (torrent.rs:4666-4679) always sends `HashReject` to peers, even when seeding a v2/hybrid torrent with full Merkle tree data available. V2 leechers can download pieces from us but can't get the hash proofs they need for Merkle verification.

### Current State

- `MerkleTree` in torrent-core has `proof_path(leaf_index)` — returns uncle hashes for verification
- `MerkleTreeState` tracks piece/block hashes during download but isn't populated for seeding
- `HashRequest` specifies `file_root`, `base` (layer), `index`, `count`, `proof_layers`
- V2 torrent metadata (`meta_v2`) contains `piece_layers` — piece-layer hashes per file
- `TorrentActor` stores `meta_v2: Option<TorrentMetaV2>` and `version: TorrentVersion`
- `validate_hash_request()` already exists for bounds checking

### Design

1. When a completed v2/hybrid torrent is seeding, build a read-only hash serving state from the torrent's `piece_layers` data (already in memory via `meta_v2`).
2. In `handle_incoming_hash_request()`:
   - If v1-only torrent or no metadata → reject (current behaviour, correct)
   - If v2/hybrid with piece layers available → validate the request geometry via `validate_hash_request()`, extract requested hashes from the piece layer, compute proof hashes, send `Message::Hashes` response
   - If request is invalid (out of bounds, bad layer) → reject
3. Hash proof generation uses `MerkleTree::proof_path()` which is already implemented and tested.

### Cleanup Bundled In

- Remove `_num_requests: u32` and `_last_request: Option<Instant>` placeholder fields from `PieceHashRequest` and `BlockHashRequest` in hash_picker.rs
- Fix 6 redundant closures in merkle.rs tests (`|i| leaf(i)` → `leaf`)

### Not In Scope

Serving block-level hashes (layer 0). That would require reading raw data from disk and hashing on the fly. Piece-layer serving covers the common case — peers need piece-layer hashes to verify downloaded pieces.

### Tests

- Unit test: build a v2 torrent, verify hash request produces correct `Hashes` response
- Unit test: reject hash request for v1-only torrent
- Unit test: reject out-of-bounds hash request

---

## M88: BEP 44 Session API

### Problem

The DHT crate fully implements BEP 44 — `DhtHandle` exposes `put_immutable()`, `get_immutable()`, `put_mutable()`, `get_mutable()` with complete KRPC handling, iterative lookups, token acquisition, and ed25519 verification. But the session layer has no public API to reach these methods. The alert variants (`DhtPutComplete`, `DhtMutablePutComplete`, `DhtGetResult`, `DhtMutableGetResult`, `DhtItemError`) are defined in alert.rs:398-436 but can never fire.

### Current State

- `SessionActor` holds `dht_handle: Option<DhtHandle>` (created at startup)
- `DhtHandle` has all four BEP 44 methods ready to call
- Alert variants exist with full field definitions and category mappings
- No `SessionCommand` variants exist for DHT item operations
- No `SessionHandle` public methods exist for DHT item operations

### Design

1. Add four `SessionCommand` variants: `DhtPutImmutable`, `DhtGetImmutable`, `DhtPutMutable`, `DhtGetMutable`
2. Add four `SessionHandle` public methods mirroring `DhtHandle`'s API:
   - `dht_put_immutable(value: Vec<u8>) -> Result<Id20>`
   - `dht_get_immutable(target: Id20) -> Result<Option<Vec<u8>>>`
   - `dht_put_mutable(keypair: [u8; 32], value: Vec<u8>, seq: i64, salt: Vec<u8>) -> Result<Id20>`
   - `dht_get_mutable(public_key: [u8; 32], salt: Vec<u8>) -> Result<Option<(Vec<u8>, i64)>>`
3. `SessionActor` handlers: forward to `dht_handle`, fire the corresponding alert on completion, return result via oneshot.
4. If DHT is disabled (`dht_handle` is `None`), return `Err(Error::DhtDisabled)` — new error variant.
5. Remove the `// TODO: Wire DHT item alerts` comment from alert.rs.

### Not In Scope

Higher-level abstractions like "publish a torrent to the DHT." This is raw BEP 44 plumbing — put/get arbitrary data.

### Tests

- Integration test: put immutable item → get it back by target hash
- Integration test: put mutable item → get it back by public key + salt
- Test: verify alerts fire with correct fields
- Test: operations return `DhtDisabled` error when DHT is off

---

## M89: NAT Cleanup Logging

### Problem

`do_unmap()` in torrent-nat's actor (actor.rs:499-536) silently discards errors from 4 cleanup operations using `let _ =`. If the gateway is unreachable during shutdown, port mappings leak on the router with no log trail.

### Current State

The four `let _ =` patterns are inside nested conditionals that must be preserved:

```rust
if self.config.enable_natpmp && let Some(gw) = self.gateway {
    let _ = crate::natpmp::delete_tcp_mapping(gw, tcp_port).await;       // (1)
    if let Some(udp_port) = self.active_udp_port {
        let _ = crate::natpmp::delete_udp_mapping(gw, udp_port).await;   // (2)
    }
}
if let Some((ref control_url, ref service_type)) = self.upnp_control {
    let _ = crate::upnp::soap::soap_request(...).await;                  // (3) TCP
    if let Some(udp_port) = self.active_udp_port {
        let _ = crate::upnp::soap::soap_request(...).await;              // (4) UDP
    }
}
```

### Design

Replace each `let _ =` with protocol-specific debug logging, inside its existing conditional block:
```rust
if let Err(e) = crate::natpmp::delete_tcp_mapping(gw, tcp_port).await {
    debug!("failed to delete NAT-PMP TCP mapping on port {tcp_port}: {e}");
}
```

Keep best-effort semantics — errors don't block shutdown, don't propagate, don't become warnings. `debug!` level is appropriate since this is only useful for diagnosing "why is my port still mapped" issues.

### Not In Scope

Retry logic, timeout adjustments, or making cleanup errors visible via alerts. Shutdown should be fast and best-effort.

### Tests

Existing NAT tests continue to pass. No new tests needed — logging-only change.

---

## M90: I2P Session Integration

### Problem

The I2P SAM protocol client is fully implemented (695 lines), the destination type is complete (325 lines), settings are fully wired with validation, and the TorrentActor has an I2P accept branch in its `select!` loop with synthetic address assignment. But the pieces aren't connected — no accept loop is spawned at session startup, no outbound I2P connects, no mixed-mode enforcement.

### Current State — What's Built

- `SamSession::create()` — complete handshake + session creation (sam.rs:300-379)
- `SamSession::connect()` — outbound STREAM CONNECT (sam.rs:398-449)
- `SamSession::accept()` — inbound STREAM ACCEPT with destination extraction (sam.rs:458-512)
- `I2pDestination` — full type with Base64/Base32 encoding (destination.rs)
- Settings: `enable_i2p`, `i2p_hostname`, `i2p_port`, tunnel quantity/length, `allow_i2p_mixed` (settings.rs:560-790)
- SessionActor: creates `SamSession` at startup (session.rs:539-574), stores as `Option<Arc<SamSession>>`
- TorrentActor: `handle_i2p_incoming()` assigns synthetic 240.0.0.0/4 addresses, spawns peer via `spawn_peer_from_stream()` (torrent.rs:6326-6367)
- TorrentActor: `select!` branch reads from `i2p_accept_rx` channel (torrent.rs:1977-1981)

### What's Missing

1. **Accept loop** — No task spawned to call `sam_session.accept()` in a loop and feed `SamStream`s into each TorrentActor's `i2p_accept_rx` channel
2. **Outbound connects** — When peer discovery yields an I2P destination, `TorrentActor` should call `sam_session.connect()` instead of TCP connect
3. **I2P peer source** — Tracker announces need to include the I2P destination so peers can discover us via I2P
4. **Mixed-mode enforcement** — When `allow_i2p_mixed = false`, I2P and clearnet peers must be segregated (no I2P peer addresses leaked to clearnet peers via PEX, and vice versa)

### Design

1. **Accept loop task**: At session startup (after SAM session creation), spawn a `tokio::spawn` task that loops `sam_session.accept()` and sends accepted `SamStream`s to a broadcast/fanout channel. Each TorrentActor subscribes via its `i2p_accept_rx`.
2. **Outbound I2P connects**: Extend the peer connection path to recognize I2P destinations. When `try_connect_peers()` encounters an I2P peer, route through `sam_session.connect()` instead of `TcpStream::connect()`.
3. **Tracker I2P announce**: When I2P is enabled, include the I2P destination in tracker announce parameters so the swarm can discover us.
4. **Mixed-mode gating**: In PEX message construction, filter peers by transport type when `allow_i2p_mixed = false`. In peer acceptance, respect the flag.

### Not In Scope

DHT over I2P (requires a separate I2P-only DHT instance — that's its own milestone). I2P UDP/uTP transport (SAM v3.1 is TCP-only).

### Tests

- Unit test: accept loop delivers SamStream to TorrentActor channel
- Unit test: mixed-mode filtering in PEX messages
- Integration test (requires local I2P router, so `#[ignore]` by default): full SAM session → connect → transfer

---

## M91: SimTransport Integration

### Problem

The simulation crate has all the infrastructure built — `SimNetwork` with latency/bandwidth/loss/partitions, `SimListener`, `sim_transport_factory()`, `SimSwarm` with session creation — but the Task 5 items (`SimConnection`, `NetworkInner`, `register_listener`, `unregister_listener`, `connect_tcp`) are marked `#[allow(dead_code)]` because no end-to-end test validates real peer-to-peer data transfer through the simulated network.

### Current State — What's Built

- `SimNetwork`: node allocation (10.0.0.N), duplex channel creation, partition logic, per-link config
- `sim_transport_factory()`: creates `NetworkFactory` with bind/connect closures routing through `SimNetwork`
- `SimSwarm::build()`: creates N nodes with factories, calls `SessionHandle::start_with_transport(factory)`
- `introduce_peers()`: announces peer addresses across all nodes
- `SimClock`: virtual time with manual advance
- All unit tests pass (16 network, 3 transport, 2 swarm)

### What's Missing

- No test verifies that a seeder + leecher connected through `SimNetwork` can complete a torrent download
- Zero of the ~20 planned swarm scenarios are implemented
- `#[allow(dead_code)]` annotations remain on infrastructure that would be exercised by tests

### Design

1. **Basic transfer test**: Seed a small test torrent on node A, add it by infohash on node B, verify node B completes the download through SimNetwork.
2. **Multi-peer swarm test**: 1 seeder + 3 leechers, verify all complete. Tests piece distribution and peer-to-peer sharing.
3. **Network partition test**: Start transfer, partition the network, verify stall, heal partition, verify completion resumes.
4. **Latency/bandwidth test**: Configure link with latency and bandwidth cap, verify transfer completes (slower) without errors.
5. **Remove `#[allow(dead_code)]`**: Once tests exercise the full path, remove the annotations.
6. **SimClock integration**: Wire virtual time into at least one test to verify deterministic behaviour.

### Not In Scope

The full 20-scenario test suite from the original M51 plan. We start with 4-6 scenarios that validate the infrastructure works. Additional scenarios can be added incrementally.

### Tests

This milestone IS tests. The deliverable is a working end-to-end simulation test suite that proves SimTransport carries real BitTorrent traffic.

---

## Dependencies

```
M86 ─┐
M87 ─┤
M88 ─┼─ all independent, can be done in any order
M89 ─┤
M90 ─┤
M91 ─┘
```

All six milestones are independent — none depends on another. The recommended execution order (M86→M91) is by severity, not dependency.

## Audit Findings Cross-Reference

| Audit Finding | Resolution |
|---------------|------------|
| `move_storage()` no-op (3 backends) | M86: remove dead code |
| BEP 52 hash serving always rejects | M87: implement hash serving |
| DHT item alerts unwired at session level | M88: wire session API |
| `pending_node_id()` stub | Resolved by M85 |
| SimTransport Task 5 dead code | M91: end-to-end tests |
| I2P integration depth | M90: wire accept loop + outbound + mixed-mode |
| NAT cleanup silent failures | M89: debug logging |
| HashPicker placeholder fields | M87: cleanup bundled in |
| Redundant closures in merkle.rs | M87: cleanup bundled in |
| Stale `#[allow(dead_code)]` in DHT | Resolved by M85 |
| `factories` field dead in sim | M91: cleanup bundled in |
