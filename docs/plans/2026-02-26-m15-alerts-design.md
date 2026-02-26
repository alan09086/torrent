# M15: Alerts / Events System — Design Document

## Summary

Async alert/event system for ferrite using tokio broadcast channels with bitflags-based category filtering. Consumers (like MagneTor) observe torrent lifecycle events, peer activity, tracker replies, errors, and more via typed, category-filtered push channels.

## Architecture

### Channel Design

- `SessionHandle` holds `broadcast::Sender<Alert>` + `Arc<AtomicU32>` alert mask
- `subscribe()` returns raw `broadcast::Receiver<Alert>` (all alerts passing session mask)
- `subscribe_filtered(filter)` returns `AlertStream` (per-subscriber filtering wrapper)
- `set_alert_mask()` does atomic store — no command roundtrip needed
- Both `SessionActor` and each `TorrentActor` get clones of `alert_tx` + `alert_mask`
- Before posting, actors check `alert.category() & mask != 0` (cheap atomic load)
- Lagged consumers get `RecvError::Lagged(n)` — alerts are fire-and-forget

### Two-Level Filtering

1. **Session-level** (`Arc<AtomicU32>` mask): coarse global filter — prevents high-volume categories from flooding the broadcast channel when no subscriber wants them
2. **Per-subscriber** (`AlertStream` wrapper): drops non-matching alerts on recv — lets different subscribers see different categories without interference

### Alert Structure

```rust
pub struct Alert {
    pub timestamp: SystemTime,  // wall-clock for API serializability
    pub kind: AlertKind,
}
```

### AlertCategory (bitflags)

```rust
bitflags! {
    pub struct AlertCategory: u32 {
        const STATUS       = 0x001;
        const ERROR        = 0x002;
        const PEER         = 0x004;
        const TRACKER      = 0x008;
        const STORAGE      = 0x010;
        const DHT          = 0x020;
        const STATS        = 0x040;
        const PIECE        = 0x080;
        const BLOCK        = 0x100;
        const PERFORMANCE  = 0x200;
        const PORT_MAPPING = 0x400;
        const ALL          = 0x7FF;
    }
}
```

### AlertKind (~30 variants)

| Category | Variants |
|----------|----------|
| STATUS | TorrentAdded, TorrentRemoved, TorrentPaused, TorrentResumed, TorrentFinished, StateChanged, MetadataReceived, ListenSucceeded, ListenFailed, SessionStatsUpdate, ResumeDataSaved |
| ERROR | TorrentError |
| PEER | PeerConnected, PeerDisconnected, PeerBanned |
| TRACKER | TrackerReply, TrackerWarning, TrackerError (also ERROR) |
| STORAGE | FileRenamed, StorageMoved, FileError |
| DHT | DhtBootstrapComplete, DhtGetPeers |
| PIECE | PieceFinished, HashFailed |
| BLOCK | BlockFinished |
| PERFORMANCE | PerformanceWarning |
| PORT_MAPPING | PortMappingSucceeded, PortMappingFailed |

### post_alert() — Free Function

```rust
fn post_alert(tx: &broadcast::Sender<Alert>, mask: &AtomicU32, kind: AlertKind) {
    let alert = Alert::new(kind);
    let m = AlertCategory::from_bits_truncate(mask.load(Ordering::Relaxed));
    if alert.category().intersects(m) {
        let _ = tx.send(alert);
    }
}
```

Shared by `SessionActor` and `TorrentActor` — no code duplication.

### TrackerManager AnnounceResult

Changed `announce()` return type from `Vec<SocketAddr>` to:

```rust
pub struct AnnounceResult {
    pub peers: Vec<SocketAddr>,
    pub outcomes: Vec<TrackerOutcome>,
}

pub struct TrackerOutcome {
    pub url: String,
    pub result: Result<usize, String>,
}
```

Enables per-tracker TrackerReply/TrackerError alerts.

### transition_state() Helper

All TorrentActor state transitions go through a single method that fires `StateChanged` alerts automatically — no state change can silently skip the alert.

## Design Decisions

1. **broadcast not mpsc** — MagneTor needs multiple subscribers (GUI + API)
2. **Alert mask as Arc<AtomicU32>** — lockless, no command roundtrip for set_alert_mask()
3. **Two-level filtering** — session mask as coarse filter, AlertStream for per-subscriber
4. **SystemTime not Instant** — serializable for MagneTor's qBittorrent WebUI API
5. **bitflags crate** — industry-standard, derives Debug/Clone/Copy, set operations
6. **Free function post_alert()** — eliminates duplication between actors
7. **Full ~30 variants upfront** — enum stable from day one; unimplemented features never fire

## Files Changed

| File | Change |
|------|--------|
| `crates/ferrite-session/src/alert.rs` | New — all alert types, AlertStream, post_alert() |
| `crates/ferrite-session/src/types.rs` | alert_mask + alert_channel_size on SessionConfig |
| `crates/ferrite-session/src/session.rs` | broadcast channel, SessionHandle subscribe methods, session-level alerts |
| `crates/ferrite-session/src/torrent.rs` | alert fields, transition_state(), alerts at event sites |
| `crates/ferrite-session/src/tracker_manager.rs` | AnnounceResult return type, per-tracker outcomes |
| `crates/ferrite-session/src/lib.rs` | pub mod alert + re-exports |
| `crates/ferrite/src/session.rs` | Facade re-exports |
| `crates/ferrite/src/prelude.rs` | Prelude additions |
| `crates/ferrite/src/client.rs` | alert_mask() + alert_channel_size() on ClientBuilder |
| `Cargo.toml` | bitflags workspace dep, version bump |
