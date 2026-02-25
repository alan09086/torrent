# M8a Design: Tracker + DHT Integration

## Goal

Wire ferrite-tracker and ferrite-dht into TorrentActor so torrents can discover peers autonomously from tracker announce URLs and DHT, rather than requiring manual `add_peers()` calls.

## Context

M7 delivered a working TorrentActor that can download pieces from peers, but has no peer discovery — `DhtHandle` is accepted but unused (`_dht`), and tracker URLs from `TorrentMetaV1.announce`/`announce_list` are ignored. M8a fixes this.

## New Module: `tracker_manager.rs`

Per-torrent tracker state machine managing the announce lifecycle.

### Types

```rust
pub(crate) struct TrackerManager {
    trackers: Vec<TrackerEntry>,
    our_peer_id: Id20,
    info_hash: Id20,
    port: u16,
    http_client: HttpTracker,
    udp_client: UdpTracker,
}

struct TrackerEntry {
    url: String,
    tier: usize,
    protocol: TrackerProtocol,  // Http or Udp
    state: TrackerState,
    tracker_id: Option<String>,
    next_announce: Instant,
    interval: Duration,         // from last response, default 30min
    backoff: Duration,          // exponential on failure
}

enum TrackerProtocol { Http, Udp }

enum TrackerState {
    NeedsAnnounce,
    Active,
    Failed { error: String },
}
```

### Key Methods

- `TrackerManager::new(meta, peer_id, info_hash, port)` — parses `announce` + `announce_list` (BEP 12 tiers), deduplicates URLs, classifies as HTTP/UDP
- `async fn announce(&mut self, event, uploaded, downloaded, left) -> Vec<SocketAddr>` — announces to all trackers, collects peers, updates intervals
- `fn next_announce_in(&self) -> Duration` — time until soonest tracker needs re-announce (drives select! timer)
- `async fn announce_started(...)` / `announce_completed(...)` / `announce_stopped(...)` — convenience wrappers with correct event

### Error Handling

Tracker failures are non-fatal:
- Exponential backoff: 30s → 60s → 120s → ... → max 30min
- Move to next tracker in tier, then next tier
- Log warning via `tracing::warn!`, never propagate to TorrentActor
- Successful announce resets backoff to zero

### BEP 12 Tier Handling

Trackers within a tier are shuffled (randomized order). Announce to all trackers in tier 0 first, then tier 1, etc. Each tracker runs independently — a failure in one doesn't skip the tier.

## TorrentActor Changes

### DHT Integration

```
from_torrent() / from_magnet():
  if enable_dht && dht_handle.is_some():
    spawn get_peers(info_hash) → feeds peers via mpsc channel
    store dht_handle in actor for later announce_peer()

select! loop:
  dht_peers_rx.recv() → deduplicate, add to available_peers

on state → Downloading:
  dht.announce_peer(info_hash, port)

on connect_interval tick:
  if need_peers && dht_search_exhausted:
    re-trigger get_peers()
```

### Tracker Integration

```
from_torrent():
  create TrackerManager from meta.announce + meta.announce_list
  announce Started event immediately

select! loop:
  tokio::time::sleep(tracker_manager.next_announce_in()) =>
    tracker_manager.announce(None, stats) → add peers to available_peers

on state → Complete:
  tracker_manager.announce_completed()

on shutdown:
  tracker_manager.announce_stopped()
```

### New Select! Arms

Current loop has 6 arms. M8a adds 2:
1. `tracker_announce_timer` — fires when any tracker is due for re-announce
2. `dht_peers_rx.recv()` — receives peer batches from DHT search

### What Stays the Same

- TorrentHandle public API (no new public methods)
- PeerTask, Choker, PieceSelector, PEX, Metadata modules
- No new crate dependencies
- No wire protocol changes

## Test Plan (~20 tests)

### tracker_manager.rs (~12 tests)
1. `parse_single_announce_url` — single `announce` field
2. `parse_announce_list_tiers` — BEP 12 multi-tier
3. `classify_http_url` — http:// → Http protocol
4. `classify_udp_url` — udp:// → Udp protocol
5. `deduplicate_urls` — same URL in announce + announce_list
6. `empty_announce_list` — no trackers gracefully
7. `next_announce_timing` — respects interval from response
8. `backoff_on_failure` — exponential backoff 30s → 60s → 120s
9. `backoff_reset_on_success` — success resets to zero
10. `backoff_max_cap` — caps at 30 minutes
11. `started_event_sent` — first announce uses Started
12. `completed_event_sent` — completed announce uses Completed

### torrent.rs integration (~5 tests)
13. `dht_peers_fed_to_available` — DHT-discovered peers appear in available_peers
14. `tracker_announce_on_start` — Started event sent on construction
15. `tracker_reannounce_loop` — re-announce fires at interval
16. `completed_announces_tracker` — state Complete triggers Completed event
17. `stopped_announces_on_shutdown` — shutdown sends Stopped event

### Edge cases (~3 tests)
18. `private_torrent_skips_dht` — BEP 27 private flag disables DHT
19. `magnet_defers_tracker_until_metadata` — no tracker announce during FetchingMetadata
20. `peer_deduplication` — same peer from tracker + DHT not duplicated

## Verification

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```
