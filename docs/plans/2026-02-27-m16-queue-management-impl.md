# M16: Torrent Queue Management — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add torrent queue management with auto-managed start/stop based on configurable active limits, queue positions, and inactivity detection.

**Architecture:** SessionActor owns all queue state (positions, auto-manage decisions). Queue positions are dense integers in a shared position space. An interval timer evaluates the queue every N seconds, starting/stopping auto-managed torrents to fit within `active_downloads`/`active_seeds`/`active_limit` constraints. Inactive torrents (below rate threshold) are exempt from category limits.

**Tech Stack:** Rust, tokio (Interval, select!), existing actor pattern (mpsc + oneshot)

**Design doc:** `docs/plans/2026-02-27-m16-queue-management-design.md`

---

### Task 1: Add Queue Config Fields to SessionConfig

**Files:**
- Modify: `crates/ferrite-session/src/types.rs:197-246`
- Test: `crates/ferrite-session/src/types.rs` (inline tests)

**Step 1: Write the failing test**

In `crates/ferrite-session/src/types.rs`, add to the existing `#[cfg(test)] mod tests` (or create if absent):

```rust
#[test]
fn session_config_queue_defaults() {
    let cfg = SessionConfig::default();
    assert_eq!(cfg.active_downloads, 3);
    assert_eq!(cfg.active_seeds, 5);
    assert_eq!(cfg.active_limit, 500);
    assert_eq!(cfg.active_checking, 1);
    assert!(cfg.dont_count_slow_torrents);
    assert_eq!(cfg.inactive_down_rate, 2048);
    assert_eq!(cfg.inactive_up_rate, 2048);
    assert_eq!(cfg.auto_manage_interval, 30);
    assert_eq!(cfg.auto_manage_startup, 60);
    assert!(!cfg.auto_manage_prefer_seeds);
}
```

**Step 2: Run test to verify it fails**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session session_config_queue_defaults`
Expected: FAIL — fields don't exist

**Step 3: Add fields to SessionConfig struct and Default impl**

Add these fields to `SessionConfig` (after the `alert_channel_size` field, around line 220):

```rust
    /// Max concurrent auto-managed downloading torrents (-1 = unlimited).
    pub active_downloads: i32,
    /// Max concurrent auto-managed seeding torrents (-1 = unlimited).
    pub active_seeds: i32,
    /// Hard cap on all active auto-managed torrents (-1 = unlimited).
    pub active_limit: i32,
    /// Max concurrent hash-check operations.
    pub active_checking: i32,
    /// Exempt inactive torrents from active_downloads/active_seeds limits.
    pub dont_count_slow_torrents: bool,
    /// Bytes/sec threshold — downloading torrent below this is "inactive".
    pub inactive_down_rate: u64,
    /// Bytes/sec threshold — seeding torrent below this is "inactive".
    pub inactive_up_rate: u64,
    /// Seconds between queue evaluations.
    pub auto_manage_interval: u64,
    /// Grace period: torrent counts as active regardless of speed for this many seconds after start.
    pub auto_manage_startup: u64,
    /// When true, seeding slots are allocated before download slots.
    pub auto_manage_prefer_seeds: bool,
```

Add defaults in the `Default` impl (after `alert_channel_size: 1024,`):

```rust
            active_downloads: 3,
            active_seeds: 5,
            active_limit: 500,
            active_checking: 1,
            dont_count_slow_torrents: true,
            inactive_down_rate: 2048,
            inactive_up_rate: 2048,
            auto_manage_interval: 30,
            auto_manage_startup: 60,
            auto_manage_prefer_seeds: false,
```

**Step 4: Run test to verify it passes**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session session_config_queue_defaults`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/ferrite-session/src/types.rs
git commit -m "feat(session): add queue management config fields to SessionConfig (M16)"
```

---

### Task 2: Extend TorrentEntry with Queue State

**Files:**
- Modify: `crates/ferrite-session/src/session.rs:30-33`
- Test: `crates/ferrite-session/src/session.rs` (inline tests)

**Step 1: Write the failing test**

Add to the test module in `session.rs` (create `#[cfg(test)] mod tests` at the bottom if absent):

```rust
#[cfg(test)]
mod queue_tests {
    use super::*;

    #[test]
    fn torrent_entry_default_queue_state() {
        let entry = TorrentEntry {
            handle: unreachable!(), // won't be called
            meta: None,
            queue_position: -1,
            auto_managed: true,
            started_at: None,
            prev_downloaded: 0,
            prev_uploaded: 0,
        };
        assert_eq!(entry.queue_position, -1);
        assert!(entry.auto_managed);
        assert!(entry.started_at.is_none());
    }
}
```

Note: This test will fail at compile time since fields don't exist yet. That's fine — it proves the struct change is needed.

**Step 2: Run test to verify it fails**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session queue_tests -- --no-run 2>&1 | head -20`
Expected: FAIL — unknown fields

**Step 3: Extend TorrentEntry**

Change `TorrentEntry` at line 30:

```rust
struct TorrentEntry {
    handle: TorrentHandle,
    meta: Option<TorrentMetaV1>,
    /// Queue position (-1 = not queued / not auto-managed).
    queue_position: i32,
    /// Whether the queue system controls this torrent.
    auto_managed: bool,
    /// When the torrent was last started/resumed (for startup grace period).
    started_at: Option<tokio::time::Instant>,
    /// Previous downloaded bytes (for rate calculation between auto-manage ticks).
    prev_downloaded: u64,
    /// Previous uploaded bytes (for rate calculation between auto-manage ticks).
    prev_uploaded: u64,
}
```

Then fix every place that constructs a `TorrentEntry` (in `handle_add_torrent` and `handle_add_magnet`) to include the new fields. Search for `TorrentEntry {` in session.rs and add:

```rust
    queue_position: -1, // assigned below if auto_managed
    auto_managed: true,
    started_at: Some(tokio::time::Instant::now()),
    prev_downloaded: 0,
    prev_uploaded: 0,
```

For auto-managed torrents, after inserting into `self.torrents`, assign queue_position:

```rust
// Assign queue position for auto-managed torrents
let pos = self.next_queue_position();
if let Some(entry) = self.torrents.get_mut(&info_hash) {
    entry.queue_position = pos;
}
```

Add helper on SessionActor:

```rust
/// Returns the next available queue position (one past the max).
fn next_queue_position(&self) -> i32 {
    self.torrents
        .values()
        .filter(|e| e.auto_managed)
        .map(|e| e.queue_position)
        .max()
        .map(|m| m + 1)
        .unwrap_or(0)
}
```

**Step 4: Fix the test** — replace the `unreachable!()` pattern with a simpler struct field check. Since TorrentEntry is private and we can't easily construct a TorrentHandle in a unit test, remove the queue_tests test and instead verify behavior through existing integration paths. The compiler verifying the struct fields is sufficient for this task.

Actually, remove the test from Step 1 entirely. The compile check from `cargo test -p ferrite-session` (which compiles everything) verifies the struct change works.

**Step 4 (revised): Run full compile check**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session --no-run`
Expected: compiles successfully

**Step 5: Commit**

```bash
git add crates/ferrite-session/src/session.rs
git commit -m "feat(session): extend TorrentEntry with queue state fields (M16)"
```

---

### Task 3: Queue Position Arithmetic Helpers

**Files:**
- Create: `crates/ferrite-session/src/queue.rs`
- Modify: `crates/ferrite-session/src/lib.rs` (add `mod queue;`)

**Step 1: Write the failing tests**

Create `crates/ferrite-session/src/queue.rs`:

```rust
//! Queue position arithmetic for auto-managed torrents.

use ferrite_core::Id20;

/// Compact representation of a queued torrent for position operations.
#[derive(Debug, Clone)]
pub(crate) struct QueueEntry {
    pub info_hash: Id20,
    pub position: i32,
}

/// Assigns a position at the end of the queue. Returns the assigned position.
pub(crate) fn append_position(entries: &[QueueEntry]) -> i32 {
    entries.iter().map(|e| e.position).max().map(|m| m + 1).unwrap_or(0)
}

/// Removes a position and shifts all positions above it down by 1.
/// Returns the list of (info_hash, old_pos, new_pos) that changed.
pub(crate) fn remove_position(entries: &mut Vec<QueueEntry>, pos: i32) -> Vec<(Id20, i32, i32)> {
    entries.retain(|e| e.position != pos);
    let mut changed = Vec::new();
    for entry in entries.iter_mut() {
        if entry.position > pos {
            let old = entry.position;
            entry.position -= 1;
            changed.push((entry.info_hash, old, entry.position));
        }
    }
    changed
}

/// Moves a torrent to a new absolute position. Shifts others to maintain density.
/// Returns the list of (info_hash, old_pos, new_pos) for all changed entries.
pub(crate) fn set_position(
    entries: &mut Vec<QueueEntry>,
    info_hash: Id20,
    new_pos: i32,
) -> Vec<(Id20, i32, i32)> {
    let new_pos = new_pos.clamp(0, entries.len().saturating_sub(1) as i32);
    let old_pos = match entries.iter().find(|e| e.info_hash == info_hash) {
        Some(e) => e.position,
        None => return Vec::new(),
    };
    if old_pos == new_pos {
        return Vec::new();
    }

    let mut changed = Vec::new();

    if new_pos < old_pos {
        // Moving up: shift entries in [new_pos, old_pos) down by +1
        for entry in entries.iter_mut() {
            if entry.info_hash == info_hash {
                changed.push((entry.info_hash, old_pos, new_pos));
                entry.position = new_pos;
            } else if entry.position >= new_pos && entry.position < old_pos {
                let old = entry.position;
                entry.position += 1;
                changed.push((entry.info_hash, old, entry.position));
            }
        }
    } else {
        // Moving down: shift entries in (old_pos, new_pos] up by -1
        for entry in entries.iter_mut() {
            if entry.info_hash == info_hash {
                changed.push((entry.info_hash, old_pos, new_pos));
                entry.position = new_pos;
            } else if entry.position > old_pos && entry.position <= new_pos {
                let old = entry.position;
                entry.position -= 1;
                changed.push((entry.info_hash, old, entry.position));
            }
        }
    }

    changed
}

/// Move one position up (lower number = higher priority). Returns changes.
pub(crate) fn move_up(
    entries: &mut Vec<QueueEntry>,
    info_hash: Id20,
) -> Vec<(Id20, i32, i32)> {
    let pos = match entries.iter().find(|e| e.info_hash == info_hash) {
        Some(e) if e.position > 0 => e.position,
        _ => return Vec::new(),
    };
    set_position(entries, info_hash, pos - 1)
}

/// Move one position down. Returns changes.
pub(crate) fn move_down(
    entries: &mut Vec<QueueEntry>,
    info_hash: Id20,
) -> Vec<(Id20, i32, i32)> {
    let max_pos = entries.iter().map(|e| e.position).max().unwrap_or(0);
    let pos = match entries.iter().find(|e| e.info_hash == info_hash) {
        Some(e) if e.position < max_pos => e.position,
        _ => return Vec::new(),
    };
    set_position(entries, info_hash, pos + 1)
}

/// Move to position 0 (highest priority). Returns changes.
pub(crate) fn move_top(
    entries: &mut Vec<QueueEntry>,
    info_hash: Id20,
) -> Vec<(Id20, i32, i32)> {
    set_position(entries, info_hash, 0)
}

/// Move to last position (lowest priority). Returns changes.
pub(crate) fn move_bottom(
    entries: &mut Vec<QueueEntry>,
    info_hash: Id20,
) -> Vec<(Id20, i32, i32)> {
    let max_pos = entries.len().saturating_sub(1) as i32;
    set_position(entries, info_hash, max_pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hash(n: u8) -> Id20 {
        Id20::from([n; 20])
    }

    fn make_entries(n: usize) -> Vec<QueueEntry> {
        (0..n)
            .map(|i| QueueEntry {
                info_hash: make_hash(i as u8),
                position: i as i32,
            })
            .collect()
    }

    fn positions(entries: &[QueueEntry]) -> Vec<(u8, i32)> {
        let mut v: Vec<_> = entries
            .iter()
            .map(|e| (e.info_hash.as_ref()[0], e.position))
            .collect();
        v.sort_by_key(|&(_, pos)| pos);
        v
    }

    #[test]
    fn append_to_empty() {
        assert_eq!(append_position(&[]), 0);
    }

    #[test]
    fn append_to_existing() {
        let entries = make_entries(3);
        assert_eq!(append_position(&entries), 3);
    }

    #[test]
    fn remove_middle_shifts_down() {
        let mut entries = make_entries(4); // 0,1,2,3
        let changed = remove_position(&mut entries, 1);
        assert_eq!(entries.len(), 3);
        assert_eq!(positions(&entries), vec![(0, 0), (2, 1), (3, 2)]);
        assert_eq!(changed.len(), 2); // entries 2 and 3 shifted
    }

    #[test]
    fn remove_last_no_shifts() {
        let mut entries = make_entries(3);
        let changed = remove_position(&mut entries, 2);
        assert_eq!(entries.len(), 2);
        assert_eq!(positions(&entries), vec![(0, 0), (1, 1)]);
        assert!(changed.is_empty());
    }

    #[test]
    fn set_position_move_up() {
        let mut entries = make_entries(4); // 0,1,2,3
        let changed = set_position(&mut entries, make_hash(3), 1);
        // 3 moves to 1, entries at 1,2 shift to 2,3
        assert_eq!(
            positions(&entries),
            vec![(0, 0), (3, 1), (1, 2), (2, 3)]
        );
        assert_eq!(changed.len(), 3);
    }

    #[test]
    fn set_position_move_down() {
        let mut entries = make_entries(4); // 0,1,2,3
        let changed = set_position(&mut entries, make_hash(0), 2);
        // 0 moves to 2, entries at 1,2 shift to 0,1
        assert_eq!(
            positions(&entries),
            vec![(1, 0), (2, 1), (0, 2), (3, 3)]
        );
        assert_eq!(changed.len(), 3);
    }

    #[test]
    fn set_position_same_is_noop() {
        let mut entries = make_entries(3);
        let changed = set_position(&mut entries, make_hash(1), 1);
        assert!(changed.is_empty());
    }

    #[test]
    fn move_up_from_zero_is_noop() {
        let mut entries = make_entries(3);
        let changed = move_up(&mut entries, make_hash(0));
        assert!(changed.is_empty());
    }

    #[test]
    fn move_up_swaps_adjacent() {
        let mut entries = make_entries(3);
        let changed = move_up(&mut entries, make_hash(2));
        assert_eq!(positions(&entries), vec![(0, 0), (2, 1), (1, 2)]);
        assert_eq!(changed.len(), 2);
    }

    #[test]
    fn move_down_from_last_is_noop() {
        let mut entries = make_entries(3);
        let changed = move_down(&mut entries, make_hash(2));
        assert!(changed.is_empty());
    }

    #[test]
    fn move_top() {
        let mut entries = make_entries(4);
        let _changed = move_top(&mut entries, make_hash(3));
        assert_eq!(
            positions(&entries),
            vec![(3, 0), (0, 1), (1, 2), (2, 3)]
        );
    }

    #[test]
    fn move_bottom() {
        let mut entries = make_entries(4);
        let _changed = move_bottom(&mut entries, make_hash(0));
        assert_eq!(
            positions(&entries),
            vec![(1, 0), (2, 1), (3, 2), (0, 3)]
        );
    }
}
```

**Step 2: Add module declaration**

In `crates/ferrite-session/src/lib.rs`, add:

```rust
mod queue;
```

**Step 3: Run tests to verify they pass**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session queue`
Expected: ALL PASS (12 tests)

**Step 4: Commit**

```bash
git add crates/ferrite-session/src/queue.rs crates/ferrite-session/src/lib.rs
git commit -m "feat(session): add queue position arithmetic module (M16)"
```

---

### Task 4: Add AlertKind Variants for Queue Events

**Files:**
- Modify: `crates/ferrite-session/src/alert.rs:52-104`
- Test: `crates/ferrite-session/src/alert.rs` (inline tests)

**Step 1: Write the failing test**

Add to the existing test module in `alert.rs`:

```rust
#[test]
fn queue_position_changed_alert_has_status_category() {
    let alert = Alert::new(AlertKind::TorrentQueuePositionChanged {
        info_hash: Id20::from([0u8; 20]),
        old_pos: 3,
        new_pos: 0,
    });
    assert!(alert.category().contains(AlertCategory::STATUS));
}

#[test]
fn torrent_auto_managed_alert_has_status_category() {
    let alert = Alert::new(AlertKind::TorrentAutoManaged {
        info_hash: Id20::from([0u8; 20]),
        paused: true,
    });
    assert!(alert.category().contains(AlertCategory::STATUS));
}
```

**Step 2: Run to verify failure**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session queue_position_changed`
Expected: FAIL — variants don't exist

**Step 3: Add AlertKind variants and category mapping**

Add to the `AlertKind` enum (after the existing `PerformanceWarning` variant):

```rust
    // ── Queue management (STATUS) ──
    /// Queue position changed (manual move or torrent removal shifted positions).
    TorrentQueuePositionChanged {
        info_hash: Id20,
        old_pos: i32,
        new_pos: i32,
    },
    /// Torrent was paused or resumed by the auto-manage system.
    TorrentAutoManaged {
        info_hash: Id20,
        paused: bool,
    },
```

In the `category()` method on `AlertKind`, add match arms:

```rust
AlertKind::TorrentQueuePositionChanged { .. } => AlertCategory::STATUS,
AlertKind::TorrentAutoManaged { .. } => AlertCategory::STATUS,
```

**Step 4: Run tests to verify they pass**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session queue_position_changed torrent_auto_managed`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/ferrite-session/src/alert.rs
git commit -m "feat(session): add queue management alert variants (M16)"
```

---

### Task 5: Add SessionCommand Variants and SessionHandle Queue API

**Files:**
- Modify: `crates/ferrite-session/src/session.rs:36-80` (SessionCommand enum)
- Modify: `crates/ferrite-session/src/session.rs:90-301` (SessionHandle impl)

**Step 1: Add SessionCommand variants**

Add to the `SessionCommand` enum (before `Shutdown`):

```rust
    QueuePosition {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<i32>>,
    },
    SetQueuePosition {
        info_hash: Id20,
        pos: i32,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    QueuePositionUp {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    QueuePositionDown {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    QueuePositionTop {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    QueuePositionBottom {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<()>>,
    },
```

**Step 2: Add SessionHandle public methods**

Add to `impl SessionHandle` (after the `save_session_state` method):

```rust
    /// Get the queue position of a torrent. Returns -1 if not auto-managed.
    pub async fn queue_position(&self, info_hash: Id20) -> crate::Result<i32> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::QueuePosition { info_hash, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Set the absolute queue position of a torrent. Shifts other torrents.
    pub async fn set_queue_position(&self, info_hash: Id20, pos: i32) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::SetQueuePosition { info_hash, pos, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Move a torrent one position up (lower number = higher priority).
    pub async fn queue_position_up(&self, info_hash: Id20) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::QueuePositionUp { info_hash, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Move a torrent one position down.
    pub async fn queue_position_down(&self, info_hash: Id20) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::QueuePositionDown { info_hash, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Move a torrent to position 0 (highest priority).
    pub async fn queue_position_top(&self, info_hash: Id20) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::QueuePositionTop { info_hash, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Move a torrent to the last position (lowest priority).
    pub async fn queue_position_bottom(&self, info_hash: Id20) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::QueuePositionBottom { info_hash, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }
```

**Step 3: Add command handlers in SessionActor::run()**

In the `select!` match on commands, add arms (before `Shutdown`):

```rust
Some(SessionCommand::QueuePosition { info_hash, reply }) => {
    let result = match self.torrents.get(&info_hash) {
        Some(entry) => Ok(entry.queue_position),
        None => Err(crate::Error::TorrentNotFound(info_hash)),
    };
    let _ = reply.send(result);
}
Some(SessionCommand::SetQueuePosition { info_hash, pos, reply }) => {
    let result = self.handle_set_queue_position(info_hash, pos);
    let _ = reply.send(result);
}
Some(SessionCommand::QueuePositionUp { info_hash, reply }) => {
    let result = self.handle_queue_move(info_hash, crate::queue::move_up);
    let _ = reply.send(result);
}
Some(SessionCommand::QueuePositionDown { info_hash, reply }) => {
    let result = self.handle_queue_move(info_hash, crate::queue::move_down);
    let _ = reply.send(result);
}
Some(SessionCommand::QueuePositionTop { info_hash, reply }) => {
    let result = self.handle_queue_move(info_hash, crate::queue::move_top);
    let _ = reply.send(result);
}
Some(SessionCommand::QueuePositionBottom { info_hash, reply }) => {
    let result = self.handle_queue_move(info_hash, crate::queue::move_bottom);
    let _ = reply.send(result);
}
```

**Step 4: Implement handler methods on SessionActor**

```rust
impl SessionActor {
    /// Build a QueueEntry snapshot from current torrents.
    fn queue_entries(&self) -> Vec<crate::queue::QueueEntry> {
        self.torrents
            .values()
            .filter(|e| e.auto_managed)
            .map(|e| crate::queue::QueueEntry {
                info_hash: *self.torrents.iter()
                    .find(|(_, v)| std::ptr::eq(*v, e))
                    .map(|(k, _)| k)
                    .unwrap(),
                position: e.queue_position,
            })
            .collect()
    }

    fn handle_set_queue_position(&mut self, info_hash: Id20, pos: i32) -> crate::Result<()> {
        if !self.torrents.contains_key(&info_hash) {
            return Err(crate::Error::TorrentNotFound(info_hash));
        }
        let mut entries = self.queue_entries();
        let changed = crate::queue::set_position(&mut entries, info_hash, pos);
        self.apply_queue_changes(&changed);
        Ok(())
    }

    fn handle_queue_move(
        &mut self,
        info_hash: Id20,
        op: fn(&mut Vec<crate::queue::QueueEntry>, Id20) -> Vec<(Id20, i32, i32)>,
    ) -> crate::Result<()> {
        if !self.torrents.contains_key(&info_hash) {
            return Err(crate::Error::TorrentNotFound(info_hash));
        }
        let mut entries = self.queue_entries();
        let changed = op(&mut entries, info_hash);
        self.apply_queue_changes(&changed);
        Ok(())
    }

    /// Apply position changes from queue arithmetic back to TorrentEntry fields and fire alerts.
    fn apply_queue_changes(&mut self, changed: &[(Id20, i32, i32)]) {
        for &(hash, old_pos, new_pos) in changed {
            if let Some(entry) = self.torrents.get_mut(&hash) {
                entry.queue_position = new_pos;
            }
            crate::alert::post_alert(
                &self.alert_tx,
                &self.alert_mask,
                AlertKind::TorrentQueuePositionChanged {
                    info_hash: hash,
                    old_pos,
                    new_pos,
                },
            );
        }
    }
}
```

Note: The `queue_entries()` method above is clunky with the pointer comparison. Simpler approach — iterate `self.torrents` directly since it's `HashMap<Id20, TorrentEntry>`:

```rust
fn queue_entries(&self) -> Vec<crate::queue::QueueEntry> {
    self.torrents
        .iter()
        .filter(|(_, e)| e.auto_managed)
        .map(|(&hash, e)| crate::queue::QueueEntry {
            info_hash: hash,
            position: e.queue_position,
        })
        .collect()
}
```

**Step 5: Also update handle_remove_torrent to shift queue positions**

In the existing `handle_remove_torrent` method, after removing the entry, add:

```rust
// Shift queue positions down for torrents that were after the removed one
if entry.auto_managed && entry.queue_position >= 0 {
    let removed_pos = entry.queue_position;
    for other in self.torrents.values_mut() {
        if other.auto_managed && other.queue_position > removed_pos {
            let old = other.queue_position;
            other.queue_position -= 1;
            // Fire alert for shifted torrents — need info_hash
        }
    }
    // Better approach: use queue arithmetic
    let mut entries = self.queue_entries();
    let changed = crate::queue::remove_position(&mut entries, removed_pos);
    self.apply_queue_changes(&changed);
}
```

**Step 6: Run compile check**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session --no-run`
Expected: compiles

**Step 7: Commit**

```bash
git add crates/ferrite-session/src/session.rs
git commit -m "feat(session): add queue position commands and SessionHandle API (M16)"
```

---

### Task 6: Auto-Manage Timer and evaluate_queue() Algorithm

**Files:**
- Modify: `crates/ferrite-session/src/session.rs` (SessionActor::run and new methods)
- Test: `crates/ferrite-session/src/queue.rs` (add algorithm unit tests)

**Step 1: Write tests for the evaluation algorithm**

Add to `crates/ferrite-session/src/queue.rs`:

```rust
/// Classification of a torrent for queue evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueCategory {
    Downloading,
    Seeding,
}

/// Snapshot of a torrent for queue evaluation.
#[derive(Debug, Clone)]
pub(crate) struct QueueCandidate {
    pub info_hash: Id20,
    pub position: i32,
    pub category: QueueCategory,
    pub is_active: bool,     // currently running (not paused by queue)
    pub is_inactive: bool,   // below rate threshold and past startup grace
}

/// Result of queue evaluation: which torrents to start and stop.
#[derive(Debug, Default)]
pub(crate) struct QueueDecision {
    pub to_resume: Vec<Id20>,
    pub to_pause: Vec<Id20>,
}

/// Evaluate the queue and decide which torrents to start/stop.
pub(crate) fn evaluate(
    candidates: &[QueueCandidate],
    active_downloads: i32,
    active_seeds: i32,
    active_limit: i32,
    dont_count_slow: bool,
) -> QueueDecision {
    let mut decision = QueueDecision::default();

    // Sort by position (lowest = highest priority)
    let mut downloads: Vec<_> = candidates
        .iter()
        .filter(|c| c.category == QueueCategory::Downloading)
        .collect();
    downloads.sort_by_key(|c| c.position);

    let mut seeds: Vec<_> = candidates
        .iter()
        .filter(|c| c.category == QueueCategory::Seeding)
        .collect();
    seeds.sort_by_key(|c| c.position);

    let mut total_active: i32 = 0;

    // Process each category
    for (group, limit) in [(&downloads, active_downloads), (&seeds, active_seeds)] {
        let mut category_active: i32 = 0;

        for candidate in group {
            let counts_toward_limit = !(dont_count_slow && candidate.is_inactive);

            if candidate.is_active {
                if counts_toward_limit {
                    category_active += 1;
                    total_active += 1;
                }
                // Check if over limit — pause lowest priority active torrents
                let over_category = limit >= 0 && category_active > limit;
                let over_total = active_limit >= 0 && total_active > active_limit;
                if over_category || over_total {
                    decision.to_pause.push(candidate.info_hash);
                    if counts_toward_limit {
                        category_active -= 1;
                        total_active -= 1;
                    }
                }
            } else {
                // Torrent is paused — can we start it?
                let under_category = limit < 0 || category_active < limit;
                let under_total = active_limit < 0 || total_active < active_limit;
                if under_category && under_total {
                    decision.to_resume.push(candidate.info_hash);
                    if counts_toward_limit {
                        category_active += 1;
                        total_active += 1;
                    }
                }
            }
        }
    }

    decision
}
```

Add tests:

```rust
#[test]
fn evaluate_starts_up_to_limit() {
    let candidates = vec![
        QueueCandidate {
            info_hash: make_hash(0), position: 0,
            category: QueueCategory::Downloading, is_active: false, is_inactive: false,
        },
        QueueCandidate {
            info_hash: make_hash(1), position: 1,
            category: QueueCategory::Downloading, is_active: false, is_inactive: false,
        },
        QueueCandidate {
            info_hash: make_hash(2), position: 2,
            category: QueueCategory::Downloading, is_active: false, is_inactive: false,
        },
    ];
    let decision = evaluate(&candidates, 2, 5, 500, true);
    assert_eq!(decision.to_resume.len(), 2);
    assert_eq!(decision.to_resume[0], make_hash(0));
    assert_eq!(decision.to_resume[1], make_hash(1));
    assert!(decision.to_pause.is_empty());
}

#[test]
fn evaluate_pauses_over_limit() {
    let candidates = vec![
        QueueCandidate {
            info_hash: make_hash(0), position: 0,
            category: QueueCategory::Downloading, is_active: true, is_inactive: false,
        },
        QueueCandidate {
            info_hash: make_hash(1), position: 1,
            category: QueueCategory::Downloading, is_active: true, is_inactive: false,
        },
        QueueCandidate {
            info_hash: make_hash(2), position: 2,
            category: QueueCategory::Downloading, is_active: true, is_inactive: false,
        },
    ];
    let decision = evaluate(&candidates, 2, 5, 500, true);
    assert!(decision.to_resume.is_empty());
    assert_eq!(decision.to_pause.len(), 1);
    assert_eq!(decision.to_pause[0], make_hash(2)); // lowest priority paused
}

#[test]
fn evaluate_inactive_dont_count() {
    // 3 active downloads but 2 are inactive — only 1 counts, so all 3 fit in limit of 2
    let candidates = vec![
        QueueCandidate {
            info_hash: make_hash(0), position: 0,
            category: QueueCategory::Downloading, is_active: true, is_inactive: true,
        },
        QueueCandidate {
            info_hash: make_hash(1), position: 1,
            category: QueueCategory::Downloading, is_active: true, is_inactive: true,
        },
        QueueCandidate {
            info_hash: make_hash(2), position: 2,
            category: QueueCategory::Downloading, is_active: true, is_inactive: false,
        },
    ];
    let decision = evaluate(&candidates, 2, 5, 500, true);
    assert!(decision.to_resume.is_empty());
    assert!(decision.to_pause.is_empty()); // all fit
}

#[test]
fn evaluate_respects_active_limit() {
    // 2 downloads + 3 seeds, active_limit = 4
    let candidates = vec![
        QueueCandidate {
            info_hash: make_hash(0), position: 0,
            category: QueueCategory::Downloading, is_active: true, is_inactive: false,
        },
        QueueCandidate {
            info_hash: make_hash(1), position: 1,
            category: QueueCategory::Downloading, is_active: true, is_inactive: false,
        },
        QueueCandidate {
            info_hash: make_hash(10), position: 0,
            category: QueueCategory::Seeding, is_active: true, is_inactive: false,
        },
        QueueCandidate {
            info_hash: make_hash(11), position: 1,
            category: QueueCategory::Seeding, is_active: true, is_inactive: false,
        },
        QueueCandidate {
            info_hash: make_hash(12), position: 2,
            category: QueueCategory::Seeding, is_active: true, is_inactive: false,
        },
    ];
    let decision = evaluate(&candidates, 3, 5, 4, true);
    // 2 downloads + 3 seeds = 5 > active_limit 4 → 1 seed must pause
    assert_eq!(decision.to_pause.len(), 1);
    assert_eq!(decision.to_pause[0], make_hash(12)); // lowest priority seed
}

#[test]
fn evaluate_unlimited_limits() {
    let candidates = vec![
        QueueCandidate {
            info_hash: make_hash(0), position: 0,
            category: QueueCategory::Downloading, is_active: false, is_inactive: false,
        },
        QueueCandidate {
            info_hash: make_hash(1), position: 1,
            category: QueueCategory::Downloading, is_active: false, is_inactive: false,
        },
    ];
    let decision = evaluate(&candidates, -1, -1, -1, true);
    assert_eq!(decision.to_resume.len(), 2); // all started, unlimited
    assert!(decision.to_pause.is_empty());
}
```

**Step 2: Run tests**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session queue`
Expected: ALL PASS (position tests + evaluate tests)

**Step 3: Wire auto-manage timer into SessionActor::run()**

In `SessionActor::run()`, add a new interval timer alongside the existing refill_interval:

```rust
let auto_manage_secs = self.config.auto_manage_interval.max(1);
let mut auto_manage_interval = tokio::time::interval(
    std::time::Duration::from_secs(auto_manage_secs)
);
auto_manage_interval.tick().await; // skip first immediate tick
```

Add a new arm in the `select!` loop:

```rust
_ = auto_manage_interval.tick() => {
    self.evaluate_queue().await;
}
```

**Step 4: Implement evaluate_queue() on SessionActor**

```rust
impl SessionActor {
    async fn evaluate_queue(&mut self) {
        let now = tokio::time::Instant::now();
        let startup_duration = std::time::Duration::from_secs(self.config.auto_manage_startup);
        let mut candidates = Vec::new();

        for (&info_hash, entry) in &self.torrents {
            if !entry.auto_managed {
                continue;
            }

            // Get current stats to compute rate
            let stats = match entry.handle.stats().await {
                Ok(s) => s,
                Err(_) => continue,
            };

            let category = match stats.state {
                TorrentState::Downloading | TorrentState::FetchingMetadata => {
                    crate::queue::QueueCategory::Downloading
                }
                TorrentState::Seeding | TorrentState::Complete => {
                    crate::queue::QueueCategory::Seeding
                }
                TorrentState::Paused => {
                    // Determine what category this torrent was in before pause
                    if stats.pieces_have >= stats.pieces_total && stats.pieces_total > 0 {
                        crate::queue::QueueCategory::Seeding
                    } else {
                        crate::queue::QueueCategory::Downloading
                    }
                }
                TorrentState::Stopped => continue,
            };

            let is_active = stats.state != TorrentState::Paused;

            // Compute rate from delta since last tick
            let download_rate = stats.downloaded.saturating_sub(entry.prev_downloaded)
                / self.config.auto_manage_interval.max(1);
            let upload_rate = stats.uploaded.saturating_sub(entry.prev_uploaded)
                / self.config.auto_manage_interval.max(1);

            let past_startup = entry.started_at
                .map(|t| now.duration_since(t) > startup_duration)
                .unwrap_or(true);

            let is_inactive = past_startup && match category {
                crate::queue::QueueCategory::Downloading => {
                    download_rate < self.config.inactive_down_rate
                }
                crate::queue::QueueCategory::Seeding => {
                    upload_rate < self.config.inactive_up_rate
                }
            };

            candidates.push(crate::queue::QueueCandidate {
                info_hash,
                position: entry.queue_position,
                category,
                is_active,
                is_inactive,
            });
        }

        // Update cached stats for next tick
        for (&info_hash, entry) in &mut self.torrents {
            if let Ok(stats) = entry.handle.stats().await {
                entry.prev_downloaded = stats.downloaded;
                entry.prev_uploaded = stats.uploaded;
            }
        }

        let decision = crate::queue::evaluate(
            &candidates,
            self.config.active_downloads,
            self.config.active_seeds,
            self.config.active_limit,
            self.config.dont_count_slow_torrents,
        );

        // Apply decisions
        for hash in &decision.to_pause {
            if let Some(entry) = self.torrents.get(hash) {
                let _ = entry.handle.pause().await;
                crate::alert::post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::TorrentAutoManaged { info_hash: *hash, paused: true },
                );
            }
        }

        for hash in &decision.to_resume {
            if let Some(entry) = self.torrents.get_mut(hash) {
                let _ = entry.handle.resume().await;
                entry.started_at = Some(tokio::time::Instant::now());
                crate::alert::post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::TorrentAutoManaged { info_hash: *hash, paused: false },
                );
            }
        }
    }
}
```

**Step 5: Run full test suite**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-session`
Expected: ALL PASS

**Step 6: Commit**

```bash
git add crates/ferrite-session/src/queue.rs crates/ferrite-session/src/session.rs
git commit -m "feat(session): auto-manage timer and queue evaluation algorithm (M16)"
```

---

### Task 7: Add queue_position to FastResumeData

**Files:**
- Modify: `crates/ferrite-core/src/resume_data.rs`
- Test: inline tests

**Step 1: Write the failing test**

```rust
#[test]
fn resume_data_queue_position_default() {
    let rd = FastResumeData::new(vec![0; 20], "test".into(), "/tmp".into());
    assert_eq!(rd.queue_position, -1);
}
```

**Step 2: Run to verify failure**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-core resume_data_queue_position`
Expected: FAIL — field doesn't exist

**Step 3: Add field**

Add to `FastResumeData` struct (note: `auto_managed` already exists as `pub auto_managed: i64`):

```rust
    /// Queue position (-1 = not queued).
    #[serde(rename = "queue_position")]
    #[serde(default = "default_neg_one")]
    pub queue_position: i64,
```

Add helper:

```rust
fn default_neg_one() -> i64 { -1 }
```

Update `FastResumeData::new()` to include: `queue_position: -1,`

**Step 4: Run test**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite-core resume_data_queue_position`
Expected: PASS

**Step 5: Update save_resume_data in session.rs**

In the `build_resume_data()` method on TorrentActor (or wherever resume data is built), the queue_position will need to be set. However, since queue_position lives in SessionActor's TorrentEntry (not TorrentActor), the SessionHandle's `save_torrent_resume_data` should set it after getting the data from TorrentActor.

In `SessionActor::handle_save_torrent_resume(&self, info_hash)`, after getting resume data from the TorrentHandle, patch in the queue position:

```rust
async fn handle_save_torrent_resume(&self, info_hash: Id20) -> crate::Result<FastResumeData> {
    let entry = self.torrents.get(&info_hash)
        .ok_or(crate::Error::TorrentNotFound(info_hash))?;
    let mut resume = entry.handle.save_resume_data().await?;
    resume.queue_position = entry.queue_position as i64;
    resume.auto_managed = if entry.auto_managed { 1 } else { 0 };
    // ... existing code for post_alert ...
    Ok(resume)
}
```

**Step 6: Run full suite**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test --workspace`
Expected: ALL PASS

**Step 7: Commit**

```bash
git add crates/ferrite-core/src/resume_data.rs crates/ferrite-session/src/session.rs
git commit -m "feat(core): add queue_position to FastResumeData (M16)"
```

---

### Task 8: Expose Queue API in Facade

**Files:**
- Modify: `crates/ferrite/src/client.rs:28-115` (ClientBuilder)
- Modify: `crates/ferrite/src/session.rs` or wherever session re-exports live
- Test: `crates/ferrite/src/client.rs` (doctest or inline test)

**Step 1: Write the failing test**

```rust
#[test]
fn client_builder_queue_config() {
    let builder = ClientBuilder::new()
        .active_downloads(5)
        .active_seeds(10)
        .active_limit(100)
        .active_checking(2)
        .dont_count_slow_torrents(false)
        .auto_manage_interval(60)
        .auto_manage_startup(120)
        .auto_manage_prefer_seeds(true);
    let config = builder.into_config();
    assert_eq!(config.active_downloads, 5);
    assert_eq!(config.active_seeds, 10);
    assert_eq!(config.active_limit, 100);
    assert_eq!(config.active_checking, 2);
    assert!(!config.dont_count_slow_torrents);
    assert_eq!(config.auto_manage_interval, 60);
    assert_eq!(config.auto_manage_startup, 120);
    assert!(config.auto_manage_prefer_seeds);
}
```

**Step 2: Run to verify failure**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite client_builder_queue`
Expected: FAIL

**Step 3: Add builder methods**

Add to `impl ClientBuilder` (after `alert_channel_size` method):

```rust
    /// Set the maximum number of concurrent auto-managed downloading torrents (-1 = unlimited).
    pub fn active_downloads(mut self, n: i32) -> Self {
        self.config.active_downloads = n;
        self
    }

    /// Set the maximum number of concurrent auto-managed seeding torrents (-1 = unlimited).
    pub fn active_seeds(mut self, n: i32) -> Self {
        self.config.active_seeds = n;
        self
    }

    /// Set the hard cap on all active auto-managed torrents (-1 = unlimited).
    pub fn active_limit(mut self, n: i32) -> Self {
        self.config.active_limit = n;
        self
    }

    /// Set the maximum number of concurrent hash-check operations.
    pub fn active_checking(mut self, n: i32) -> Self {
        self.config.active_checking = n;
        self
    }

    /// Set whether inactive torrents are exempt from download/seed limits.
    pub fn dont_count_slow_torrents(mut self, v: bool) -> Self {
        self.config.dont_count_slow_torrents = v;
        self
    }

    /// Set the interval (seconds) between queue evaluations.
    pub fn auto_manage_interval(mut self, secs: u64) -> Self {
        self.config.auto_manage_interval = secs;
        self
    }

    /// Set the startup grace period (seconds) where a torrent is considered active regardless of speed.
    pub fn auto_manage_startup(mut self, secs: u64) -> Self {
        self.config.auto_manage_startup = secs;
        self
    }

    /// Set whether seeding slots are allocated before download slots.
    pub fn auto_manage_prefer_seeds(mut self, v: bool) -> Self {
        self.config.auto_manage_prefer_seeds = v;
        self
    }
```

**Step 4: Run test**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test -p ferrite client_builder_queue`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/ferrite/src/client.rs
git commit -m "feat(ferrite): expose queue management config on ClientBuilder (M16)"
```

---

### Task 9: Run Full Test Suite and Clippy

**Step 1: Run all tests**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo test --workspace`
Expected: ALL PASS

**Step 2: Run clippy**

Run: `cd /mnt/TempNVME/projects/ferrite && cargo clippy --workspace -- -D warnings`
Expected: zero warnings

**Step 3: Fix any issues found**

Address compiler warnings, unused imports, missing derives, etc.

**Step 4: Commit any fixes**

```bash
git commit -am "fix: address clippy warnings from M16 queue management"
```

---

### Task 10: Version Bump, CHANGELOG, and README

**Files:**
- Modify: `Cargo.toml` (workspace version)
- Modify: `CHANGELOG.md`
- Modify: `README.md`

**Step 1: Bump version**

In root `Cargo.toml`, change `version = "0.16.0"` to `version = "0.17.0"`.

**Step 2: Update CHANGELOG.md**

Add M16 entry following existing format.

**Step 3: Update README.md**

Update milestone status: M16 complete, test count.

**Step 4: Commit**

```bash
git add Cargo.toml CHANGELOG.md README.md
git commit -m "feat: M16 queue management — version bump to 0.17.0"
```

**Step 5: Push to both remotes**

```bash
git push origin main && git push github main
```
