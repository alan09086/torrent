# Semaphore-Based Reactive Pipeline Design

**Date**: 2026-03-07
**Scope**: Replace poll-based batch request dispatch with per-peer `tokio::Semaphore` reactive scheduling. Includes end-game mode conversion with libtorrent-style 1-redundant-copy limit.

## Motivation

The current pipeline uses a fixed-depth counter (`queue_depth = 128`) with batch dispatch — requests are filled in bulk during tick events or state changes. This means after a block arrives, the next request may not go out until the next processing cycle. A semaphore-based approach makes dispatch **reactive**: block received → permit released → request driver immediately wakes and dispatches the next request.

Additionally, end-game mode uses a 200ms batch tick for redundant request refill, and can spray the same block to every connected peer. This wastes bandwidth and adds unnecessary latency.

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Architecture | Per-peer request driver task | Truly reactive, clean separation from peer message handler |
| Permit count source | `Settings::initial_queue_depth` | Already exists (default 128), configurable, future-proofs for adaptive resizing |
| End-game mode | Semaphore too, same permit count | Unified dispatch path, reactive cancel cascade |
| Redundant block limit | Fixed at 1 extra copy per block | Matches libtorrent, prevents bandwidth waste |
| Snub handling | Stop acquiring, drain, probe with 1 | Natural wind-down, no cancel storm |

## Core Architecture

### PeerPipelineState Refactoring

Replace the implicit counter in `pipeline.rs` with a real semaphore:

```rust
pub(crate) struct PeerPipelineState {
    semaphore: Arc<Semaphore>,
    notify: Arc<Notify>,                 // wake driver when new pieces available
    max_permits: usize,                  // from initial_queue_depth setting

    // Existing stats tracking (unchanged)
    ewma_rate_bytes_sec: f64,
    last_second_bytes: u64,
    last_tick: Instant,
    request_times: HashMap<(u32, u32), Instant>,
}
```

**Removed fields**: `queue_depth: usize` (replaced by semaphore).

### Per-Peer Request Driver Task

Each peer spawns a `request_driver` when unchoked:

```
request_driver(semaphore, notify, picker_handle, peer_cmd_tx):
    loop {
        permit = semaphore.acquire().await?
        match picker.pick_block(peer_id):
            Some(block) => {
                peer_cmd_tx.send(Request { block })
                record_request_time(block, now)
            }
            None => {
                permit.forget()          // return permit
                notify.notified().await  // sleep until Have/Bitfield
            }
    }
```

### Permit Lifecycle

| Event | Action |
|-------|--------|
| Peer unchoked + has pieces | Spawn request driver, semaphore starts with `initial_queue_depth` permits |
| Block received | `semaphore.add_permits(1)` — immediately wakes request driver |
| Peer choked | Cancel request driver task, close semaphore |
| Peer unchoked again | Respawn request driver, new semaphore |
| Peer disconnects | Cancel request driver, drop semaphore |
| Request timeout | `semaphore.add_permits(1)` — reclaim lost permit |
| Have / Bitfield received | `notify.notify_one()` — wake driver if sleeping |

## Snub Handling

When snubbed (60s no data), the request driver enters probe mode:

1. **Stop acquiring** new permits (check snub flag before each `acquire`)
2. **Drain naturally** — existing in-flight requests timeout or complete
3. **Probe with 1** — once drained, send a single request and wait for response
4. **Recovery** — on block received, clear snub flag, restore full permits via `semaphore.add_permits(max_permits - current_permits)`

No cancel storm, no semaphore shrinking — just a gated acquire loop.

## End-Game Mode

### Transition

When end-game activates, the request driver switches pickers:

```
block = if end_game_active:
    end_game_picker.pick_block(peer_id)
else:
    normal_picker.pick_block(peer_id)
```

### 1-Redundant-Copy Limit

The end-game picker enforces:

```
end_game_picker.pick_block(peer_id):
    for block in remaining_blocks:
        if block.assigned_peers().len() >= 2:  // original + 1 redundant
            continue
        if block.assigned_peers().contains(peer_id):
            continue
        return Some(block)
    return None
```

### Reactive Cancel Cascade

On block receipt during end-game:

1. Get list of other peers with this block outstanding
2. Send Cancel to each
3. Remove from their `pending_requests`
4. `other_peer.semaphore.add_permits(1)` — freed peer immediately gets new work

This replaces the 200ms batch refill tick entirely.

## Integration Points

### What Gets Added

| Component | Location | Purpose |
|-----------|----------|---------|
| `request_driver` task | `torrent.rs` | Per-peer semaphore acquire → pick → dispatch loop |
| `Semaphore` field | `PeerPipelineState` | `Arc<tokio::sync::Semaphore>` per peer |
| `Notify` field | `PeerPipelineState` | Wake request driver on Have/Bitfield |
| Redundancy check | `end_game.rs` `pick_block()` | `assignees.len() >= 2` guard |

### What Gets Modified

| Component | Change |
|-----------|--------|
| `pipeline.rs` `PeerPipelineState` | Replace `queue_depth` with semaphore; keep EWMA/RTT |
| `torrent.rs` on block received | `semaphore.add_permits(1)` |
| `torrent.rs` on cancel (end-game) | `semaphore.add_permits(1)` on cancelled peer |
| `torrent.rs` on choke | Cancel request driver task |
| `torrent.rs` on unchoke | Spawn request driver task |
| `torrent.rs` on Have/Bitfield | `notify.notify_one()` |
| `torrent.rs` request timeout | `semaphore.add_permits(1)` to reclaim permit |
| `end_game.rs` `pick_block()` | Add 1-redundant-copy limit |

### What Gets Removed

| Component | Why |
|-----------|-----|
| `END_GAME_DEPTH` constant | Replaced by semaphore permit count |
| `end_game_refill_interval` (200ms tick) | Semaphore makes batch refill unnecessary |
| `in_slow_start()` stub | Dead code, always returned false |
| Batch slot-filling in `fill_requests()` | Replaced by request driver's acquire loop |
| `queue_depth` field | Replaced by semaphore |

### What Stays Unchanged

| Component | Why |
|-----------|-----|
| `PendingRequests` HashMap | Still needed for cancel/timeout tracking |
| `request_times` HashMap | RTT measurement preserved for future adaptive sizing |
| EWMA throughput tracking | Peer speed classification for picker |
| Piece selector / picker logic | Only **when** called changes, not **how** |
| Snub timeout (60s) | Same detection, different response |
| `Settings::initial_queue_depth` | Controls semaphore permit count |
| `Settings::max_request_queue_depth` | Ceiling for future adaptive resizing |

## Choke/Unchoke State Machine

```
Choked (initial)
  |
  +-- Unchoke received --> Unchoked
  |                         |
  |                         +-- Spawn request_driver(semaphore, picker, cmd_tx)
  |                         |
  |                         +-- Choke received --> Choked
  |                         |     +-- Cancel request_driver task
  |                         |     +-- Drop semaphore, clear pending_requests
  |                         |
  |                         +-- Snubbed --> Probe mode (1 request at a time)
  |                         |     +-- Block received --> Unchoked (full permits)
  |                         |
  |                         +-- Disconnect --> Cleanup
  |                               +-- Cancel request_driver, drop all state
```

## Future Work (Not In Scope)

- **Adaptive permit sizing**: dynamic `rate * RTT / block_size` adjustment (libtorrent slow-start)
- **Timeout-based escalation**: proactively add redundant copy if a block is outstanding too long
- **Global flow control**: cross-peer semaphore for total outstanding requests
