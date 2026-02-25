# M8c: Complete M8 Gaps — Design Document

## Overview

M8c closes five remaining features from the original M8b design that were deferred
for scope reasons: magnet link support in the session manager, Local Service Discovery
(LSD) as a full actor, AllowedFast messages on peer connect, and precise request
tracking with RejectRequest-on-choke.

## Changes

### 1. add_magnet() on SessionHandle

`TorrentEntry.meta` changed from `TorrentMetaV1` to `Option<TorrentMetaV1>` to
accommodate magnets where metadata is fetched asynchronously via BEP 9. A new
`MetadataNotReady` error variant is returned from `torrent_info()` until the metadata
download completes. `AddMagnet` command variant delegates to `TorrentHandle::from_magnet()`.

### 2. Pending Request Tracking + RejectRequest on Choke

`PeerState.pending_requests` changed from `usize` to `Vec<(u32, u32, u32)>` for
precise matching by (index, begin, length). Added `incoming_requests` field to track
requests from peers to us. `Message::Request` is now forwarded as `IncomingRequest`
PeerEvent instead of being ignored. When choking a fast-extension peer, all pending
incoming requests are rejected via `RejectRequest` before sending the choke message.

### 3. AllowedFast on Peer Connect (BEP 6)

Phase 4b inserted after bitfield exchange: when both peers support fast extension and
the torrent has pieces, 10 AllowedFast messages are sent using the deterministic
`allowed_fast_set()` algorithm from ferrite-wire for IPv4 peers.

### 4. LSD UDP Actor (BEP 14)

`LsdHandle`/`LsdActor` built on top of existing `format_announce`, `parse_announce`,
and `LsdRateLimiter` utilities. Actor binds UDP port 6771, joins multicast group
239.192.152.143, and runs a `tokio::select!` loop handling commands (announce/shutdown)
and incoming datagrams. Discovered peers are sent via channel.

### 5. LSD Wired into SessionActor

SessionActor optionally starts LSD on `enable_lsd`. The `run()` loop converted to
`tokio::select!` to concurrently process commands and LSD peer discoveries. LSD peers
are routed to matching torrents via `add_peers()`. Announces triggered on
`add_torrent()`/`add_magnet()`. Graceful LSD shutdown in `shutdown_all()`.

## Test Summary

- 337 total tests (up from 331)
- 6 new tests: add_magnet_and_list, add_magnet_duplicate_rejected,
  fast_extension_sends_allowed_fast_on_connect, no_allowed_fast_without_fast_extension,
  lsd_actor_start_and_shutdown, session_with_lsd_enabled

## Files Changed

| File | Changes |
|------|---------|
| `ferrite-session/src/error.rs` | +MetadataNotReady variant |
| `ferrite-session/src/session.rs` | add_magnet(), LSD fields, select! loop, LSD peer routing |
| `ferrite-session/src/peer_state.rs` | pending_requests -> Vec, +incoming_requests |
| `ferrite-session/src/torrent.rs` | 5 pending_requests sites, IncomingRequest handler, RejectRequest on choke |
| `ferrite-session/src/types.rs` | +IncomingRequest PeerEvent variant |
| `ferrite-session/src/peer.rs` | Forward Request as event, AllowedFast Phase 4b |
| `ferrite-session/src/lsd.rs` | LsdActor, LsdHandle, LsdCommand, run() select! loop |
