# M16: Torrent Queue Management — Design

## Overview

Queue management controls how many torrents are active simultaneously. Auto-managed torrents are started and stopped based on queue position and configurable limits. Non-auto-managed torrents bypass the queue entirely.

Follows libtorrent's proven model, adapted to our actor architecture.

## Architecture Decision

**SessionActor owns all queue state.** Queue positions, auto-manage decisions, and slot counting live in SessionActor. TorrentActors receive Pause/Resume commands — they don't know about the queue.

Rationale: queue decisions require global knowledge (active counts across all torrents). Centralizing avoids cross-actor roundtrips on every evaluation tick.

## Configuration

New fields on `SessionConfig`:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `active_downloads` | `i32` | 3 | Max concurrent downloading torrents. -1 = unlimited. |
| `active_seeds` | `i32` | 5 | Max concurrent seeding torrents. -1 = unlimited. |
| `active_limit` | `i32` | 500 | Hard cap on all active auto-managed torrents. |
| `active_checking` | `i32` | 1 | Max concurrent hash-check operations. |
| `dont_count_slow_torrents` | `bool` | `true` | Exempt inactive torrents from download/seed limits. |
| `inactive_down_rate` | `u64` | 2048 | Bytes/sec — below this a downloading torrent is "inactive". |
| `inactive_up_rate` | `u64` | 2048 | Bytes/sec — below this a seeding torrent is "inactive". |
| `auto_manage_interval` | `u64` | 30 | Seconds between queue evaluations. |
| `auto_manage_startup` | `u64` | 60 | Grace period — torrent counts as "active" regardless of speed. |
| `auto_manage_prefer_seeds` | `bool` | `false` | When true, seeding slots are allocated before download slots. |

New field on `TorrentConfig` / `AddTorrentParams`:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `auto_managed` | `bool` | `true` | Whether the queue system controls this torrent. |

## Queue State

### TorrentEntry (SessionActor)

Extend the existing `TorrentEntry` struct:

```rust
struct TorrentEntry {
    handle: TorrentHandle,
    info_hash: Id20,
    state: TorrentState,
    // NEW:
    queue_position: i32,         // -1 = not queued, 0+ = position
    auto_managed: bool,          // queue system controls this torrent
    started_at: Option<Instant>, // when last started (for startup grace)
    cached_download_rate: u64,   // updated each auto-manage tick
    cached_upload_rate: u64,     // updated each auto-manage tick
}
```

### Position Semantics

- Positions are dense integers: 0, 1, 2, ...
- Downloads and seeds share one position space — position determines priority within each category.
- When a torrent is added with `auto_managed: true`, it gets `queue_position = next_position` (appended to bottom).
- When a torrent is removed, all positions above it shift down by 1.
- Non-auto-managed torrents have `queue_position = -1`.

## Auto-Manage Algorithm

Runs every `auto_manage_interval` seconds via a new `Interval` in SessionActor's `select!` loop.

```
fn evaluate_queue(&mut self):
    // 1. Collect & classify
    let mut downloading: Vec<&mut TorrentEntry> = []
    let mut seeding: Vec<&mut TorrentEntry> = []
    let mut checking: Vec<&mut TorrentEntry> = []

    for entry in self.torrents where entry.auto_managed:
        match entry.state:
            Downloading | FetchingMetadata => downloading.push(entry)
            Seeding | Complete => seeding.push(entry)
            // checking state (future) => checking.push(entry)
            Paused if was_downloading => downloading.push(entry)
            Paused if was_seeding => seeding.push(entry)

    // 2. Sort each category by queue_position (ascending = highest priority)
    downloading.sort_by_key(|e| e.queue_position)
    seeding.sort_by_key(|e| e.queue_position)

    // 3. Determine slot order
    let categories = if self.config.auto_manage_prefer_seeds {
        [&seeding, &downloading]  // seeds get slots first
    } else {
        [&downloading, &seeding]  // downloads get slots first (default)
    }

    // 4. For each category, start/stop to fit limits
    //    - "inactive" torrents (below rate threshold, past startup grace) don't count
    //    - active_limit is the hard cap across all categories

    // 5. Fire alerts for any state changes
```

### Inactivity Exemption

A torrent is "inactive" (doesn't count toward `active_downloads`/`active_seeds`) when ALL of:
- `dont_count_slow_torrents` is true
- Transfer rate is below `inactive_down_rate` (downloading) or `inactive_up_rate` (seeding)
- Torrent has been running longer than `auto_manage_startup` seconds

Inactive torrents are NOT paused — they just don't consume a slot, allowing more torrents to run.

`active_limit` always applies regardless of inactivity — it's the hard cap.

## Public API

### SessionHandle Methods

```rust
/// Get queue position. Returns -1 if not auto-managed.
pub async fn queue_position(&self, info_hash: Id20) -> Result<i32>

/// Set absolute queue position. Shifts other torrents.
pub async fn set_queue_position(&self, info_hash: Id20, pos: i32) -> Result<()>

/// Move one position up (lower number = higher priority).
pub async fn queue_position_up(&self, info_hash: Id20) -> Result<()>

/// Move one position down.
pub async fn queue_position_down(&self, info_hash: Id20) -> Result<()>

/// Move to position 0 (highest priority).
pub async fn queue_position_top(&self, info_hash: Id20) -> Result<()>

/// Move to last position (lowest priority).
pub async fn queue_position_bottom(&self, info_hash: Id20) -> Result<()>
```

### SessionCommand Variants

```rust
QueuePosition { info_hash: Id20, reply: oneshot::Sender<Result<i32>> },
SetQueuePosition { info_hash: Id20, pos: i32, reply: oneshot::Sender<Result<()>> },
QueuePositionUp { info_hash: Id20, reply: oneshot::Sender<Result<()>> },
QueuePositionDown { info_hash: Id20, reply: oneshot::Sender<Result<()>> },
QueuePositionTop { info_hash: Id20, reply: oneshot::Sender<Result<()>> },
QueuePositionBottom { info_hash: Id20, reply: oneshot::Sender<Result<()>> },
```

### ClientBuilder Methods

```rust
pub fn active_downloads(mut self, n: i32) -> Self
pub fn active_seeds(mut self, n: i32) -> Self
pub fn active_limit(mut self, n: i32) -> Self
pub fn active_checking(mut self, n: i32) -> Self
pub fn dont_count_slow_torrents(mut self, v: bool) -> Self
pub fn auto_manage_interval(mut self, secs: u64) -> Self
pub fn auto_manage_startup(mut self, secs: u64) -> Self
pub fn auto_manage_prefer_seeds(mut self, v: bool) -> Self
```

## Alerts

New `AlertKind` variants:

```rust
/// Queue position changed (manual or due to other torrent removal).
TorrentQueuePositionChanged {
    info_hash: Id20,
    old_pos: i32,
    new_pos: i32,
}

/// Torrent was paused or resumed by the auto-manage system.
TorrentAutoManaged {
    info_hash: Id20,
    paused: bool,  // true = auto-paused, false = auto-resumed
}
```

Category: `STATUS`

## Persistence

Extend `FastResumeData` with:

```rust
pub queue_position: i32,    // #[serde(default)]
pub auto_managed: bool,     // #[serde(default = "default_true")]
```

On session restore, queue positions are loaded from resume data. If multiple torrents claim the same position, they're re-densified in load order.

## Facade Re-exports

- `ClientBuilder` gains queue config methods
- `SessionHandle` gains queue position methods
- `ferrite::prelude` — no new types needed (uses existing `Id20`, `Result`)
- `AlertKind` variants automatically available through existing re-exports

## Interaction with Existing Systems

- **Bandwidth limiter (M14):** Independent. A queued (paused) torrent uses zero bandwidth. Active torrent limits still apply.
- **Selective download (M12):** Independent. File priorities are per-torrent, queue is session-level.
- **End-game (M13):** Only active torrents enter end-game. Auto-paused torrents exit end-game.
- **Resume data (M11):** Queue position persisted. On restore, auto-manage evaluates immediately.
- **Alerts (M15):** Queue changes fire STATUS alerts. Auto-manage pause/resume fires dedicated alert.

## Testing Strategy

- Unit tests for position arithmetic (insert, remove, shift, reorder)
- Unit tests for inactivity classification
- Unit tests for auto-manage algorithm (slot counting, prefer_seeds, limits)
- Integration tests: add N torrents, verify only `active_downloads` are running
- Edge cases: -1 unlimited limits, all torrents inactive, non-auto-managed bypass

## Non-Goals

- Seed rotation / seed cycle counting (future enhancement)
- Per-torrent queue category override
- Checking state management (hash-check queuing deferred — `active_checking` config added but not enforced until hash-check pipeline exists)
