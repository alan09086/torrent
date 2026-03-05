# M15: Alerts / Events System — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add an async alert/event system to ferrite so consumers (like MagneTor) can observe torrent lifecycle events, peer activity, tracker replies, errors, and more via typed, category-filtered broadcast channels.

**Tech Stack:** Rust, tokio (broadcast channel), bitflags crate

---

## Context

Currently ferrite's entire public API is pull-based (request-reply via mpsc + oneshot). There is no way for consumers to receive push notifications when events occur — torrent added, piece finished, peer disconnected, etc. These events are logged via `tracing` but not surfaced to the API. M15 adds the first push-based channel: a `broadcast::Sender<Alert>` that both `SessionActor` and `TorrentActor` post to, with bitmask filtering matching libtorrent's alert_category system.

## Architecture

### Channel Design: broadcast (multi-consumer)

- `SessionHandle` holds `broadcast::Sender<Alert>` + `Arc<AtomicU32>` alert mask
- `subscribe()` calls `self.alert_tx.subscribe()` — no command roundtrip needed
- `set_alert_mask()` does an atomic store — no command roundtrip needed
- Both `SessionActor` and each `TorrentActor` get clones of `alert_tx` + `alert_mask`
- Before posting, actors check `alert.category() & mask != 0` (cheap atomic load)
- Lagged consumers get `RecvError::Lagged(n)` — alerts are fire-and-forget, not queued

### Alert Structure

```rust
/// A timestamped alert from the session.
#[derive(Debug, Clone)]
pub struct Alert {
    pub timestamp: std::time::Instant,
    pub kind: AlertKind,
}

/// ~30 alert variants, grouped by category.
#[derive(Debug, Clone)]
pub enum AlertKind { ... }
```

### AlertCategory (bitflags)

```rust
bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AlertCategory: u32 {
        const STATUS      = 0x001;
        const ERROR       = 0x002;
        const PEER        = 0x004;
        const TRACKER     = 0x008;
        const STORAGE     = 0x010;
        const DHT         = 0x020;
        const STATS       = 0x040;
        const PIECE       = 0x080;
        const BLOCK       = 0x100;
        const PERFORMANCE = 0x200;
        const PORT_MAPPING = 0x400;
        const ALL         = 0x7FF;
    }
}
```

### Wiring Flow

```
SessionHandle::start(config)
  ├── creates broadcast::channel(config.alert_channel_size)
  ├── stores Sender + Arc<AtomicU32> mask in SessionHandle
  ├── passes Sender clone + mask clone to SessionActor
  └── SessionActor passes Sender clone + mask clone to each TorrentActor

Consumer: session.subscribe() → broadcast::Receiver<Alert>
          session.set_alert_mask(AlertCategory::STATUS | AlertCategory::ERROR)
```

### AlertKind Variants (~30)

**Torrent lifecycle (STATUS):**
- `TorrentAdded { info_hash, name }`
- `TorrentRemoved { info_hash }`
- `TorrentPaused { info_hash }`
- `TorrentResumed { info_hash }`
- `TorrentFinished { info_hash }`
- `StateChanged { info_hash, prev_state, new_state }`
- `MetadataReceived { info_hash, name }`

**Transfer (PIECE / BLOCK):**
- `PieceFinished { info_hash, piece }`
- `BlockFinished { info_hash, piece, offset }`
- `HashFailed { info_hash, piece }`

**Peers (PEER):**
- `PeerConnected { info_hash, addr }`
- `PeerDisconnected { info_hash, addr, reason }`
- `PeerBanned { info_hash, addr }`

**Tracker (TRACKER):**
- `TrackerReply { info_hash, url, num_peers }`
- `TrackerWarning { info_hash, url, message }`
- `TrackerError { info_hash, url, message }`

**DHT (DHT):**
- `DhtBootstrapComplete`
- `DhtGetPeers { info_hash, num_peers }`

**Session (STATUS):**
- `ListenSucceeded { port }`
- `ListenFailed { port, message }`
- `SessionStatsUpdate(SessionStats)`

**Storage (STORAGE):**
- `FileRenamed { info_hash, index, new_path }`
- `StorageMoved { info_hash, new_path }`
- `FileError { info_hash, path, message }`

**Resume (STATUS):**
- `ResumeDataSaved { info_hash }`

**Error (ERROR):**
- `TorrentError { info_hash, message }`

**Performance (PERFORMANCE):**
- `PerformanceWarning { info_hash, message }`

**Port mapping (PORT_MAPPING):**
- `PortMappingSucceeded { port, protocol }`
- `PortMappingFailed { port, message }`

Variants for unimplemented features (storage move, port mapping, etc.) are defined but won't fire until those milestones land.

## Design Decisions

1. **broadcast not mpsc** — MagneTor needs multiple subscribers (GUI + API). broadcast::Sender is cheap to clone.
2. **Alert mask as Arc<AtomicU32>** — lockless, shared between actors and handle. No command roundtrip for set_alert_mask().
3. **subscribe() on SessionHandle directly** — no actor command needed, just `sender.subscribe()`.
4. **Timestamp is Instant, not SystemTime** — monotonic, useful for intervals. Consumers can map to wall-clock if needed.
5. **bitflags crate** — industry-standard, derives Debug/Clone/Copy, provides set operations.
6. **Fire-and-forget posting** — `broadcast::send()` returns Err only if no receivers; we ignore it. Lagged consumers handle their own recovery.
7. **Full ~30 variants upfront** — enum is stable from day one; unimplemented features simply never fire their variants.

## Critical Files

| File | Role |
|------|------|
| `crates/ferrite-session/src/alert.rs` | **New** — `Alert`, `AlertKind`, `AlertCategory`, `Alert::category()` |
| `crates/ferrite-session/src/types.rs` | Add `alert_mask`, `alert_channel_size` to `SessionConfig` |
| `crates/ferrite-session/src/session.rs` | Create broadcast channel, store in handle/actor, pass to torrents, post session-level alerts |
| `crates/ferrite-session/src/torrent.rs` | Accept `alert_tx` + `alert_mask`, `post_alert()` helper, fire alerts at event sites |
| `crates/ferrite-session/src/lib.rs` | `pub mod alert;` + re-exports |
| `crates/ferrite-session/Cargo.toml` | Add `bitflags` dependency |
| `Cargo.toml` (workspace) | Add `bitflags` to workspace deps |
| `crates/ferrite/src/lib.rs` | Re-export alert types via `ferrite::session` |
| `crates/ferrite/src/client.rs` | Add `.alert_mask()`, `.alert_channel_size()` to ClientBuilder |

---

### Task 1: Add bitflags dependency + alert_mask/channel_size config fields

**Files:**
- Modify: `Cargo.toml` (workspace)
- Modify: `crates/ferrite-session/Cargo.toml`
- Modify: `crates/ferrite-session/src/types.rs`

**Step 1: Write failing tests**
```rust
#[test]
fn session_config_alert_defaults() {
    let config = SessionConfig::default();
    // Default mask = all categories enabled
    assert_eq!(config.alert_mask, 0x7FF);
    assert_eq!(config.alert_channel_size, 1024);
}
```

**Step 2: Run test — verify fails**

**Step 3: Add bitflags to workspace deps**

In root `Cargo.toml`:
```toml
bitflags = "2"
```

In `crates/ferrite-session/Cargo.toml`:
```toml
bitflags = { workspace = true }
```

**Step 4: Add config fields to SessionConfig**

```rust
/// Bitmask of alert categories to deliver (default: all).
pub alert_mask: u32,
/// Capacity of the alert broadcast channel (default: 1024).
pub alert_channel_size: usize,
```

Defaults: `alert_mask: 0x7FF` (ALL), `alert_channel_size: 1024`.

Use raw `u32` in config to avoid circular dependency (AlertCategory is in alert.rs which we haven't created yet). The alert module will provide conversion.

**Step 5: Run test — verify passes**
**Step 6: Clippy**
**Step 7: Commit**
```
feat(session): add alert config fields to SessionConfig (M15)
```

---

### Task 2: Implement Alert, AlertKind, AlertCategory

**Files:**
- Create: `crates/ferrite-session/src/alert.rs`
- Modify: `crates/ferrite-session/src/lib.rs`

**Step 1: Write failing tests**
```rust
#[test]
fn alert_category_all_includes_every_flag() {
    let all = AlertCategory::ALL;
    assert!(all.contains(AlertCategory::STATUS));
    assert!(all.contains(AlertCategory::ERROR));
    assert!(all.contains(AlertCategory::PEER));
    assert!(all.contains(AlertCategory::TRACKER));
    assert!(all.contains(AlertCategory::STORAGE));
    assert!(all.contains(AlertCategory::DHT));
    assert!(all.contains(AlertCategory::STATS));
    assert!(all.contains(AlertCategory::PIECE));
    assert!(all.contains(AlertCategory::BLOCK));
    assert!(all.contains(AlertCategory::PERFORMANCE));
    assert!(all.contains(AlertCategory::PORT_MAPPING));
}

#[test]
fn alert_category_mapping() {
    use AlertKind::*;
    let info_hash = ferrite_core::Id20::default();
    // Spot-check a few variants
    let a = Alert::new(TorrentAdded { info_hash, name: String::new() });
    assert!(a.category().contains(AlertCategory::STATUS));

    let a = Alert::new(PieceFinished { info_hash, piece: 0 });
    assert!(a.category().contains(AlertCategory::PIECE));

    let a = Alert::new(PeerConnected { info_hash, addr: "127.0.0.1:6881".parse().unwrap() });
    assert!(a.category().contains(AlertCategory::PEER));

    let a = Alert::new(TrackerError { info_hash, url: String::new(), message: String::new() });
    assert!(a.category().contains(AlertCategory::TRACKER));
    assert!(a.category().contains(AlertCategory::ERROR));
}

#[test]
fn alert_has_timestamp() {
    let before = std::time::Instant::now();
    let alert = Alert::new(AlertKind::DhtBootstrapComplete);
    assert!(alert.timestamp >= before);
}
```

**Step 2: Run test — verify fails**

**Step 3: Implement alert.rs**

Full `AlertCategory` bitflags, `AlertKind` enum with ~30 variants, `Alert` struct with `timestamp: Instant` and `kind: AlertKind`, `Alert::new(kind)` constructor, `Alert::category()` method that maps each variant to its category bitmask.

Note: Some variants belong to multiple categories (e.g., `TrackerError` is both `TRACKER` and `ERROR`). The `category()` method returns `AlertCategory` with multiple bits set.

Add `pub mod alert;` to lib.rs and add public re-exports: `pub use alert::{Alert, AlertKind, AlertCategory};`

**Step 4: Run test — verify passes**
**Step 5: Clippy**
**Step 6: Commit**
```
feat(session): implement Alert, AlertKind, AlertCategory (M15)
```

---

### Task 3: Wire broadcast channel into SessionHandle + SessionActor

**Files:**
- Modify: `crates/ferrite-session/src/session.rs`

**Step 1: Write failing test**
```rust
#[tokio::test]
async fn subscribe_receives_torrent_added_alert() {
    let config = test_session_config();
    let session = SessionHandle::start(config).await.unwrap();

    let mut alerts = session.subscribe();

    // Add a torrent — should produce TorrentAdded alert
    let meta = test_meta();
    let storage = test_storage(&meta);
    let _info_hash = session.add_torrent(meta.clone(), Some(storage)).await.unwrap();

    // Receive the alert
    let alert = tokio::time::timeout(Duration::from_secs(2), alerts.recv())
        .await.unwrap().unwrap();
    assert!(matches!(alert.kind, AlertKind::TorrentAdded { .. }));

    session.shutdown().await.unwrap();
}
```

**Step 2: Run test — verify fails**

**Step 3: Add broadcast channel to SessionHandle**

```rust
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::sync::broadcast;

pub struct SessionHandle {
    cmd_tx: mpsc::Sender<SessionCommand>,
    alert_tx: broadcast::Sender<Alert>,
    alert_mask: Arc<AtomicU32>,
}

impl SessionHandle {
    /// Subscribe to alerts. Returns a receiver that gets all alerts
    /// matching the current alert mask.
    pub fn subscribe(&self) -> broadcast::Receiver<Alert> {
        self.alert_tx.subscribe()
    }

    /// Update the alert category mask at runtime.
    pub fn set_alert_mask(&self, mask: AlertCategory) {
        self.alert_mask.store(mask.bits(), Ordering::Relaxed);
    }

    /// Get the current alert mask.
    pub fn alert_mask(&self) -> AlertCategory {
        AlertCategory::from_bits_truncate(self.alert_mask.load(Ordering::Relaxed))
    }
}
```

**Step 4: Update SessionHandle::start()**

```rust
pub async fn start(config: SessionConfig) -> crate::Result<Self> {
    let (cmd_tx, cmd_rx) = mpsc::channel(256);
    let (alert_tx, _) = broadcast::channel(config.alert_channel_size);
    let alert_mask = Arc::new(AtomicU32::new(config.alert_mask));

    // ... existing LSD + rate limiter setup ...

    let actor = SessionActor {
        // ... existing fields ...
        alert_tx: alert_tx.clone(),
        alert_mask: Arc::clone(&alert_mask),
    };

    tokio::spawn(actor.run());
    Ok(SessionHandle { cmd_tx, alert_tx, alert_mask })
}
```

**Step 5: Add alert_tx + alert_mask to SessionActor, add post_alert() helper**

```rust
struct SessionActor {
    // ... existing fields ...
    alert_tx: broadcast::Sender<Alert>,
    alert_mask: Arc<AtomicU32>,
}

impl SessionActor {
    fn post_alert(&self, kind: AlertKind) {
        let alert = Alert::new(kind);
        let mask = AlertCategory::from_bits_truncate(
            self.alert_mask.load(Ordering::Relaxed)
        );
        if alert.category().intersects(mask) {
            let _ = self.alert_tx.send(alert);
        }
    }
}
```

**Step 6: Fire session-level alerts**

Add `self.post_alert(AlertKind::TorrentAdded { ... })` in `handle_add_torrent()` after the info! log.
Add `self.post_alert(AlertKind::TorrentRemoved { ... })` in `handle_remove_torrent()`.
Add `self.post_alert(AlertKind::TorrentAdded { ... })` in `handle_add_magnet()`.

**Step 7: Update SessionHandle Clone impl**

SessionHandle derives Clone. broadcast::Sender is Clone, Arc is Clone, so this should work automatically. Verify.

**Step 8: Update test_session_config() if needed**

**Step 9: Run test — verify passes**
**Step 10: Clippy**
**Step 11: Commit**
```
feat(session): wire alert broadcast into SessionHandle/SessionActor (M15)
```

---

### Task 4: Wire alerts into TorrentActor

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs`
- Modify: `crates/ferrite-session/src/session.rs` (pass alert_tx/mask to torrent constructors)

**Step 1: Write failing tests**
```rust
#[tokio::test]
async fn torrent_fires_piece_finished_alert() {
    // Uses existing torrent test harness
    // Connect a fake peer, send a complete piece, verify PieceFinished alert
}

#[tokio::test]
async fn alert_mask_filters_categories() {
    // Set mask to STATUS only
    // Add torrent (STATUS) — should arrive
    // Complete a piece (PIECE) — should NOT arrive
}
```

**Step 2: Run test — verify fails**

**Step 3: Add alert fields to TorrentActor + constructors**

```rust
struct TorrentActor {
    // ... existing fields ...
    alert_tx: broadcast::Sender<Alert>,
    alert_mask: Arc<AtomicU32>,
}
```

Extend `from_torrent()` and `from_magnet()` signatures:
```rust
pub(crate) async fn from_torrent(
    ...,
    alert_tx: broadcast::Sender<Alert>,
    alert_mask: Arc<AtomicU32>,
) -> crate::Result<Self> { ... }
```

Add `post_alert()` helper to TorrentActor (same pattern as SessionActor).

**Step 4: Update session.rs call sites**

Pass `self.alert_tx.clone()` and `Arc::clone(&self.alert_mask)` to `from_torrent()` and `from_magnet()` in `handle_add_torrent()` and `handle_add_magnet()`.

**Step 5: Fire torrent-level alerts at existing event sites**

Key injection points in torrent.rs (alongside existing tracing calls):

| Location | Alert |
|----------|-------|
| `verify_and_mark_piece()` success | `PieceFinished { info_hash, piece }` |
| `verify_and_mark_piece()` hash fail | `HashFailed { info_hash, piece }` |
| `verify_and_mark_piece()` download complete | `TorrentFinished { info_hash }` + `StateChanged` |
| `try_assemble_metadata()` success | `MetadataReceived { info_hash, name }` + `StateChanged` |
| `handle_peer_event(Disconnected)` | `PeerDisconnected { info_hash, addr, reason }` |
| `handle_pause()` | `TorrentPaused { info_hash }` + `StateChanged` |
| `handle_resume()` | `TorrentResumed { info_hash }` + `StateChanged` |
| Peer connected (after handshake) | `PeerConnected { info_hash, addr }` |
| Tracker announce reply | `TrackerReply { info_hash, url, num_peers }` |

StateChanged alerts require tracking `prev_state` before each transition. Add a helper:
```rust
fn transition_state(&mut self, new_state: TorrentState) {
    let prev = self.state;
    if prev != new_state {
        self.state = new_state;
        self.post_alert(AlertKind::StateChanged {
            info_hash: self.info_hash,
            prev_state: prev,
            new_state,
        });
    }
}
```

Replace all direct `self.state = X` assignments with `self.transition_state(X)`.

**Step 6: Update ALL test call sites for from_torrent/from_magnet**

Same pattern as M14: update every test that constructs a TorrentHandle to pass the new alert params. Use a test helper:
```rust
fn test_alert_channel() -> (broadcast::Sender<Alert>, Arc<AtomicU32>) {
    let (tx, _) = broadcast::channel(64);
    let mask = Arc::new(AtomicU32::new(AlertCategory::ALL.bits()));
    (tx, mask)
}
```

**Step 7: Run tests — verify passes**
**Step 8: Clippy**
**Step 9: Commit**
```
feat(session): wire alerts into TorrentActor (M15)
```

---

### Task 5: Facade re-exports + ClientBuilder methods

**Files:**
- Modify: `crates/ferrite/src/lib.rs`
- Modify: `crates/ferrite/src/client.rs`

**Step 1: Add re-exports**

In `crates/ferrite/src/lib.rs`, the `session` module re-exports — add `Alert`, `AlertKind`, `AlertCategory`.
In the prelude, add `Alert`, `AlertKind`, `AlertCategory`.

**Step 2: Add ClientBuilder methods**

```rust
/// Set the alert category mask (default: all categories).
pub fn alert_mask(mut self, mask: ferrite_session::AlertCategory) -> Self {
    self.config.alert_mask = mask.bits();
    self
}

/// Set the alert broadcast channel capacity (default: 1024).
pub fn alert_channel_size(mut self, size: usize) -> Self {
    self.config.alert_channel_size = size;
    self
}
```

**Step 3: Run full suite + clippy**
**Step 4: Commit**
```
feat: expose alerts in facade and ClientBuilder (M15)
```

---

### Task 6: Integration tests

**Files:**
- Modify: `crates/ferrite-session/src/alert.rs` (test module)
- Modify: `crates/ferrite-session/src/torrent.rs` (test module)

Add integration-level tests:

```rust
#[tokio::test]
async fn multiple_subscribers_each_receive_alerts() {
    // Two subscribers, add torrent, both get TorrentAdded
}

#[tokio::test]
async fn set_alert_mask_filters_at_runtime() {
    // Start with ALL, receive alert
    // Change to NONE (0), verify no alerts arrive
    // Change back to STATUS, verify alerts arrive again
}

#[tokio::test]
async fn state_changed_tracks_transitions() {
    // Add torrent, pause it, resume it
    // Verify StateChanged alerts with correct prev/new states
}
```

**Step 1: Write and run tests**
**Step 2: Clippy**
**Step 3: Commit**
```
test(session): integration tests for alert system (M15)
```

---

### Task 7: Version bump, design doc, memory update

**Files:**
- Modify: `Cargo.toml` (workspace version `0.15.0` → `0.16.0`)
- Create: `docs/plans/2026-02-26-m15-alerts-design.md`

**Step 1: Bump version**
**Step 2: Write design doc**
**Step 3: Update memory** (ferrite.md — add M15 to completed, update test count, version)
**Step 4: Run full suite + clippy**
**Step 5: Commit**
```
feat: M15 alerts system — version bump to 0.16.0, docs
```

---

## Verification

After all tasks:

1. `cargo test --workspace` — all tests pass (~435+ tests)
2. `cargo clippy --workspace -- -D warnings` — zero warnings
3. `cargo test -p ferrite-session alert` — alert module tests
4. `cargo test -p ferrite-session -- subscribe` — integration tests
5. Manual review: alerts fire at correct sites, mask filtering works, multiple subscribers work

## Test Summary

| Location | Tests | Coverage |
|----------|-------|----------|
| `alert.rs` | ~4 | category mapping, ALL flag, timestamp, alert construction |
| `session.rs` | ~2 | subscribe receives alerts, torrent added/removed alerts |
| `torrent.rs` | ~3 | piece finished, alert mask filtering, state transitions |
| `alert.rs` (integration) | ~3 | multiple subscribers, runtime mask change, state tracking |
| `types.rs` | ~1 | config defaults |
| **Total new** | **~13** | |
