# Semaphore-Based Reactive Pipeline Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace poll-based batch request dispatch with per-peer `tokio::Semaphore` reactive scheduling, and convert end-game to semaphore dispatch with libtorrent-style 1-redundant-copy limit.

**Architecture:** Each peer gets a dedicated `request_driver` tokio task that loops on `semaphore.acquire() → pick_block() → send_request()`. Block receipt releases a permit, immediately waking the driver. End-game uses the same mechanism with a redundancy-limited picker. The 200ms batch tick and counter-based slot filling are removed entirely.

**Tech Stack:** Rust, tokio (Semaphore, Notify, JoinHandle), existing piece_selector and end_game pickers.

**Design Doc:** `docs/plans/2026-03-07-semaphore-pipeline-design.md`

---

## Task 1: Refactor PeerPipelineState to Use Semaphore

**Files:**
- Modify: `crates/torrent-session/src/pipeline.rs` (full rewrite of struct + constructor)
- Test: `crates/torrent-session/src/pipeline.rs` (inline `#[cfg(test)]` module, lines 153-243)

**Context:** The current `PeerPipelineState` (lines 24-37) uses a plain `queue_depth: usize` field. We replace it with `Arc<tokio::sync::Semaphore>` and add `Arc<tokio::sync::Notify>` for waking the request driver when new pieces become available. All EWMA/RTT tracking stays unchanged.

**Step 1: Update the struct definition and imports**

Replace the struct at lines 24-37. Add tokio imports at the top of the file:

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Notify, Semaphore};
```

New struct:

```rust
pub(crate) struct PeerPipelineState {
    /// Real tokio semaphore — permits control how many requests can be in flight.
    semaphore: Arc<Semaphore>,
    /// Wakes the request driver when new pieces become available (Have/Bitfield).
    notify: Arc<Notify>,
    /// Maximum permits this semaphore was created with (from settings.initial_queue_depth).
    max_permits: usize,

    // --- stats tracking (unchanged) ---
    last_second_bytes: u64,
    ewma_rate_bytes_sec: f64,
    last_tick: Instant,
    request_times: HashMap<(u32, u32), Instant>,
}
```

**Step 2: Update the constructor**

Replace `new()` (currently around lines 39-52). The old constructor took `initial_queue_depth` and `max_queue_depth` and clamped. New version:

```rust
impl PeerPipelineState {
    pub fn new(initial_queue_depth: usize) -> Self {
        let depth = initial_queue_depth.max(1);
        Self {
            semaphore: Arc::new(Semaphore::new(depth)),
            notify: Arc::new(Notify::new()),
            max_permits: depth,
            last_second_bytes: 0,
            ewma_rate_bytes_sec: 0.0,
            last_tick: Instant::now(),
            request_times: HashMap::new(),
        }
    }
}
```

**Step 3: Update accessor methods**

Replace `queue_depth()` (line ~62) and remove `in_slow_start()` (lines 67-68):

```rust
/// Returns a clone of the semaphore Arc for use by the request driver task.
pub fn semaphore(&self) -> Arc<Semaphore> {
    Arc::clone(&self.semaphore)
}

/// Returns a clone of the notify Arc for use by the request driver task.
pub fn notify(&self) -> Arc<Notify> {
    Arc::clone(&self.notify)
}

/// Current maximum permits (for stats/logging).
pub fn max_permits(&self) -> usize {
    self.max_permits
}

/// Number of permits currently available (not acquired).
pub fn available_permits(&self) -> usize {
    self.semaphore.available_permits()
}

/// Wake the request driver (call on Have/Bitfield).
pub fn wake_driver(&self) {
    self.notify.notify_one();
}

/// Release one permit back to the semaphore (call on block received or cancel).
pub fn release_permit(&self) {
    self.semaphore.add_permits(1);
}

/// Restore permits to max capacity (call on snub recovery).
pub fn restore_full_permits(&self, currently_in_flight: usize) {
    let to_add = self.max_permits.saturating_sub(currently_in_flight);
    if to_add > 0 {
        self.semaphore.add_permits(to_add);
    }
}
```

**Step 4: Keep existing methods unchanged**

These methods stay as-is (just verify they still compile):
- `request_sent(piece, begin, now)` — lines 76-80
- `block_received(piece, begin, length, now)` — lines 85-100 (still returns `Option<Duration>`, still updates EWMA bytes)
- `tick()` — lines 102-112 (EWMA update)
- `ewma_rate()` — returns `ewma_rate_bytes_sec`
- `timed_out_blocks(timeout, now)` — lines 134-140
- `reset_to_slow_start()` — rename to `reset_permits()`, just restores semaphore to max

Update `reset_to_slow_start()` → `reset_permits()`:

```rust
/// Reset semaphore to full capacity (was reset_to_slow_start).
pub fn reset_permits(&self, currently_in_flight: usize) {
    self.restore_full_permits(currently_in_flight);
}
```

**Step 5: Remove dead code**

- Remove `queue_depth` field and its getter
- Remove `max_queue_depth` field
- Remove `in_slow_start()` method (always returned false)
- Remove `DEFAULT_QUEUE_DEPTH` constant (permit count comes from settings)

**Step 6: Update existing unit tests**

The existing 8 tests (lines 153-243) test the old counter-based API. Update them to use the new semaphore API. Key changes:
- `PeerPipelineState::new(128)` instead of `new(128, 250)`
- Replace `assert_eq!(p.queue_depth(), 128)` with `assert_eq!(p.max_permits(), 128)`
- Test `release_permit()` increments `available_permits()`
- Test `restore_full_permits()` after partial drain
- Test `wake_driver()` doesn't panic

**Step 7: Add new unit tests for semaphore behaviour**

```rust
#[tokio::test]
async fn test_semaphore_acquire_and_release() {
    let p = PeerPipelineState::new(2);
    let sem = p.semaphore();
    // Acquire both permits
    let _p1 = sem.acquire().await.unwrap();
    let _p2 = sem.acquire().await.unwrap();
    assert_eq!(p.available_permits(), 0);
    // Release one
    p.release_permit();
    assert_eq!(p.available_permits(), 1);
}

#[tokio::test]
async fn test_restore_full_permits() {
    let p = PeerPipelineState::new(4);
    let sem = p.semaphore();
    // Acquire 3 permits (simulate 3 in-flight requests)
    let _p1 = sem.acquire().await.unwrap();
    let _p2 = sem.acquire().await.unwrap();
    let _p3 = sem.acquire().await.unwrap();
    assert_eq!(p.available_permits(), 1);
    // Restore with 3 in-flight
    p.restore_full_permits(3);
    assert_eq!(p.available_permits(), 2); // 4 max - 3 in_flight + 1 already available = wait, let me reconsider
    // Actually: max=4, currently_in_flight=3, so to_add = 4-3=1, available goes from 1 to 2
    // But we want max - in_flight permits available = 4-3 = 1...
    // The semaphore already has 1 available (4 created - 3 acquired). restore adds max-in_flight=1 more.
    // That would make 2 available, which is wrong. We need to be smarter.
}

#[tokio::test]
async fn test_notify_wakes_driver() {
    let p = PeerPipelineState::new(1);
    let notify = p.notify();
    let woke = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let woke2 = woke.clone();
    let handle = tokio::spawn(async move {
        notify.notified().await;
        woke2.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    tokio::task::yield_now().await;
    p.wake_driver();
    handle.await.unwrap();
    assert!(woke.load(std::sync::atomic::Ordering::SeqCst));
}
```

**Step 8: Run tests**

Run: `cargo test -p torrent-session --lib pipeline -- --nocapture`
Expected: All tests pass.

**Step 9: Run clippy**

Run: `cargo clippy -p torrent-session -- -D warnings`
Expected: No warnings.

**Step 10: Commit**

```bash
git add crates/torrent-session/src/pipeline.rs
git commit -m "refactor: replace counter-based pipeline with tokio::Semaphore

Replace queue_depth counter with Arc<Semaphore> and add Arc<Notify> for
reactive request dispatch. Remove in_slow_start() stub and dead fields.
EWMA/RTT tracking unchanged."
```

---

## Task 2: Implement the Request Driver Function

**Files:**
- Create: `crates/torrent-session/src/request_driver.rs`
- Modify: `crates/torrent-session/src/lib.rs` (add module declaration)

**Context:** This is the core async loop that replaces batch dispatch. It acquires a semaphore permit, asks the picker for a block, sends the request to the peer, and loops. When no work is available, it sleeps on a `Notify` until Have/Bitfield arrives.

**Step 1: Create the module file with the driver function signature**

The driver needs access to:
- The peer's semaphore (to acquire permits)
- The peer's notify (to wake when new pieces available)
- A channel/callback to pick blocks (we'll use a channel to the torrent actor)
- The peer's command sender (to send Request messages)
- A cancellation token (to stop on choke/disconnect)
- A snubbed flag (atomic, to enter probe mode)

```rust
//! Per-peer request driver task.
//!
//! Each unchoked peer spawns a `request_driver` that loops:
//! semaphore.acquire() → pick_block() → send_request()
//!
//! Block receipt in the main torrent loop calls semaphore.add_permits(1),
//! immediately waking this driver to dispatch the next request.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, Notify, Semaphore};

use crate::peer_command::PeerCommand;

/// Message sent from the request driver to the torrent actor to request a block pick.
pub(crate) enum DriverMessage {
    /// Request the torrent actor to pick blocks and send them for this peer.
    /// The driver has `available_slots` permits acquired and ready.
    NeedBlocks { available_slots: usize },
}

/// Runs the per-peer request driver loop.
///
/// Returns when the `cancel` token is triggered (choke/disconnect) or an
/// unrecoverable error occurs.
pub(crate) async fn request_driver(
    semaphore: Arc<Semaphore>,
    notify: Arc<Notify>,
    snubbed: Arc<AtomicBool>,
    driver_tx: mpsc::Sender<DriverMessage>,
    cancel: tokio_util::sync::CancellationToken,
) {
    loop {
        // Check cancellation before blocking on semaphore
        if cancel.is_cancelled() {
            return;
        }

        // Snub probe mode: wait for drain, then single request
        if snubbed.load(Ordering::Acquire) {
            // Sleep until either: snub clears (notify), or cancel
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = notify.notified() => continue, // snub cleared or new pieces
            }
        }

        // Acquire a permit (blocks until capacity available)
        let permit = tokio::select! {
            _ = cancel.cancelled() => return,
            result = semaphore.acquire() => {
                match result {
                    Ok(permit) => permit,
                    Err(_) => return, // semaphore closed
                }
            }
        };

        // Tell the torrent actor we need a block for this peer.
        // The actor will pick a block, send the Request to the peer,
        // and track it in pending_requests.
        // We forget the permit — the torrent actor "owns" it now and
        // will add_permits(1) when the block is received.
        permit.forget();

        if driver_tx.send(DriverMessage::NeedBlocks { available_slots: 1 }).await.is_err() {
            return; // torrent actor dropped, peer is gone
        }
    }
}
```

**Design note:** The driver does NOT directly call the picker or send requests to the peer. Instead, it sends `NeedBlocks` to the torrent actor, which does the actual pick + dispatch. This avoids sharing the picker across tasks and keeps the existing synchronization model (torrent actor owns all mutable state). The driver's sole job is **timing** — acquiring permits and signalling readiness.

**Step 2: Add module declaration**

In `crates/torrent-session/src/lib.rs`, add:
```rust
mod request_driver;
```

**Step 3: Add `tokio-util` dependency if not present**

Check `crates/torrent-session/Cargo.toml` for `tokio-util`. If missing, add:
```toml
tokio-util = { version = "0.7", features = ["rt"] }
```

The `CancellationToken` is in `tokio_util::sync`.

**Step 4: Write unit tests for the driver**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_driver_sends_need_blocks() {
        let sem = Arc::new(Semaphore::new(2));
        let notify = Arc::new(Notify::new());
        let snubbed = Arc::new(AtomicBool::new(false));
        let cancel = tokio_util::sync::CancellationToken::new();
        let (tx, mut rx) = mpsc::channel(16);

        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move {
            request_driver(sem, notify, snubbed, tx, cancel2).await;
        });

        // Should receive 2 NeedBlocks (one per permit)
        let msg1 = rx.recv().await.unwrap();
        assert!(matches!(msg1, DriverMessage::NeedBlocks { available_slots: 1 }));
        let msg2 = rx.recv().await.unwrap();
        assert!(matches!(msg2, DriverMessage::NeedBlocks { available_slots: 1 }));

        // No more permits — driver should be blocked on acquire
        // Cancel to clean up
        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_driver_stops_on_cancel() {
        let sem = Arc::new(Semaphore::new(0)); // no permits
        let notify = Arc::new(Notify::new());
        let snubbed = Arc::new(AtomicBool::new(false));
        let cancel = tokio_util::sync::CancellationToken::new();
        let (tx, _rx) = mpsc::channel(16);

        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move {
            request_driver(sem, notify, snubbed, tx, cancel2).await;
        });

        // Driver is blocked on acquire (0 permits). Cancel it.
        cancel.cancel();
        handle.await.unwrap(); // should return promptly
    }

    #[tokio::test]
    async fn test_driver_snub_mode_waits_for_notify() {
        let sem = Arc::new(Semaphore::new(10));
        let notify = Arc::new(Notify::new());
        let snubbed = Arc::new(AtomicBool::new(true)); // start snubbed
        let cancel = tokio_util::sync::CancellationToken::new();
        let (tx, mut rx) = mpsc::channel(16);

        let notify2 = notify.clone();
        let snubbed2 = snubbed.clone();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move {
            request_driver(sem, notify2, snubbed2, tx, cancel2).await;
        });

        // Give driver time to enter snub wait
        tokio::task::yield_now().await;

        // Should NOT have sent any messages (snubbed)
        assert!(rx.try_recv().is_err());

        // Clear snub and wake
        snubbed.store(false, Ordering::Release);
        notify.notify_one();

        // Now should get a NeedBlocks
        let msg = rx.recv().await.unwrap();
        assert!(matches!(msg, DriverMessage::NeedBlocks { .. }));

        cancel.cancel();
        handle.await.unwrap();
    }
}
```

**Step 5: Run tests**

Run: `cargo test -p torrent-session --lib request_driver -- --nocapture`
Expected: All 3 tests pass.

**Step 6: Run clippy**

Run: `cargo clippy -p torrent-session -- -D warnings`
Expected: No warnings.

**Step 7: Commit**

```bash
git add crates/torrent-session/src/request_driver.rs crates/torrent-session/src/lib.rs crates/torrent-session/Cargo.toml
git commit -m "feat: add per-peer request_driver task with semaphore scheduling

New request_driver async loop that acquires semaphore permits and
signals the torrent actor to pick and dispatch blocks. Supports
cancellation (choke/disconnect) and snub probe mode."
```

---

## Task 3: Wire Request Driver into Peer Lifecycle

**Files:**
- Modify: `crates/torrent-session/src/peer_state.rs` (add driver fields to PeerState)
- Modify: `crates/torrent-session/src/torrent.rs` (spawn/cancel driver, handle DriverMessage)

**Context:** This is the main integration task. We spawn the request driver on unchoke, cancel it on choke/disconnect, and handle its `NeedBlocks` messages in the torrent actor's event loop. We also wire up `release_permit()` on block received and `wake_driver()` on Have/Bitfield.

**Step 1: Add driver fields to PeerState**

In `crates/torrent-session/src/peer_state.rs`, add to the `PeerState` struct (around line 83):

```rust
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// Add these fields to PeerState:
/// Cancellation token for the request driver task.
pub driver_cancel: Option<CancellationToken>,
/// Receiver end — torrent actor reads NeedBlocks from this.
pub driver_rx: Option<mpsc::Receiver<crate::request_driver::DriverMessage>>,
/// Shared snub flag readable by the driver.
pub snubbed_flag: Arc<AtomicBool>,
/// Handle to the spawned driver task.
pub driver_handle: Option<tokio::task::JoinHandle<()>>,
```

Initialize these in the `PeerState` constructor (wherever `PeerState::new()` or struct literal is):
```rust
driver_cancel: None,
driver_rx: None,
snubbed_flag: Arc::new(AtomicBool::new(false)),
driver_handle: None,
```

**Step 2: Add spawn/cancel helper methods to torrent actor**

In `torrent.rs`, add helper methods on the torrent actor (or as free functions):

```rust
/// Spawn the request driver for a peer. Called on unchoke.
fn spawn_request_driver(&mut self, peer_addr: SocketAddr) {
    let peer = match self.peers.get_mut(&peer_addr) {
        Some(p) => p,
        None => return,
    };

    // Cancel any existing driver
    if let Some(cancel) = peer.driver_cancel.take() {
        cancel.cancel();
    }

    let semaphore = peer.pipeline.semaphore();
    let notify = peer.pipeline.notify();
    let snubbed = Arc::clone(&peer.snubbed_flag);
    let cancel = CancellationToken::new();
    let (tx, rx) = mpsc::channel(32);

    let cancel2 = cancel.clone();
    let handle = tokio::spawn(async move {
        crate::request_driver::request_driver(semaphore, notify, snubbed, tx, cancel2).await;
    });

    peer.driver_cancel = Some(cancel);
    peer.driver_rx = Some(rx);
    peer.driver_handle = Some(handle);
}

/// Cancel the request driver for a peer. Called on choke/disconnect.
fn cancel_request_driver(&mut self, peer_addr: SocketAddr) {
    let peer = match self.peers.get_mut(&peer_addr) {
        Some(p) => p,
        None => return,
    };

    if let Some(cancel) = peer.driver_cancel.take() {
        cancel.cancel();
    }
    peer.driver_rx = None;
    // JoinHandle will complete on its own after cancel
    peer.driver_handle = None;
}
```

**Step 3: Call spawn on unchoke**

Find the unchoke handler in `torrent.rs` (search for where `peer_interested` or unchoke state is set). After setting the peer as unchoked, call:

```rust
self.spawn_request_driver(peer_addr);
```

This replaces the current call to `self.request_pieces_from_peer(peer_addr)` that batch-fills slots.

**Step 4: Call cancel on choke**

Find the choke handler. After setting the peer as choked and clearing pending requests, call:

```rust
self.cancel_request_driver(peer_addr);
```

**Step 5: Call cancel on disconnect**

Find the peer disconnect/removal handler. Before removing the peer from the map, call:

```rust
self.cancel_request_driver(peer_addr);
```

**Step 6: Handle DriverMessage in the torrent actor event loop**

In the main `select!` loop of the torrent actor (around line 1680), add a branch that polls all active `driver_rx` channels. When `NeedBlocks` arrives, call the existing `request_pieces_from_peer` logic for that peer (but only for 1 slot, since the driver acquired 1 permit):

```rust
// In the main event loop, poll driver channels:
// This requires collecting driver_rx from all peers and selecting on them.
// One approach: use a separate mpsc that all drivers share, tagged with peer_addr.
```

**Alternative (simpler) approach:** Instead of per-peer channels, use a single `mpsc::Sender<(SocketAddr, DriverMessage)>` shared by all drivers. The torrent actor has one receiver to poll:

```rust
// In torrent actor state:
driver_msg_tx: mpsc::Sender<(SocketAddr, DriverMessage)>,
driver_msg_rx: mpsc::Receiver<(SocketAddr, DriverMessage)>,

// In main select! loop:
Some((peer_addr, msg)) = self.driver_msg_rx.recv() => {
    match msg {
        DriverMessage::NeedBlocks { available_slots } => {
            self.dispatch_blocks_for_peer(peer_addr, available_slots).await;
        }
    }
}
```

Update `spawn_request_driver` to clone the shared `driver_msg_tx` and wrap it:

```rust
let shared_tx = self.driver_msg_tx.clone();
let peer_addr_copy = peer_addr;
let (tx, _) = mpsc::channel(1); // dummy, not used
// Instead, the driver sends to shared_tx with peer_addr tag:
// This requires modifying request_driver to accept Sender<(SocketAddr, DriverMessage)>
// OR wrap it in a small adapter.
```

**Design decision:** Modify `request_driver` to accept `mpsc::Sender<(SocketAddr, DriverMessage)>` and a `peer_addr: SocketAddr` so it can tag messages. This is cleaner than per-peer channels.

**Step 7: Implement `dispatch_blocks_for_peer`**

This is a trimmed version of the existing `request_pieces_from_peer` (line 5114) that dispatches exactly `available_slots` blocks:

```rust
async fn dispatch_blocks_for_peer(&mut self, peer_addr: SocketAddr, slots: usize) {
    // Use existing pick_blocks + send logic from request_pieces_from_peer
    // but limited to `slots` blocks instead of filling to queue_depth
    // If no block available, call peer.pipeline.release_permit() to return it
    // and the semaphore stays at correct count
}
```

The existing `request_pieces_from_peer` logic (lines 5220-5327) is reused — just change the slot count source from `queue_depth - pending_requests.len()` to the `slots` parameter from the driver.

**Step 8: Wire `release_permit()` on block received**

In `handle_piece_data` (line 3515), after `peer.pending_requests.remove(index, begin)` (line 3591), add:

```rust
peer.pipeline.release_permit();
```

**Step 9: Wire `wake_driver()` on Have/Bitfield**

In the Have message handler, after updating the peer's bitfield, add:

```rust
peer.pipeline.wake_driver();
```

Same for the Bitfield message handler.

**Step 10: Wire `release_permit()` on request timeout**

In the request timeout handler (where timed-out blocks are retried), after removing the timed-out request from pending_requests, add:

```rust
peer.pipeline.release_permit();
```

**Step 11: Sync snubbed flag with snub detection**

In the snub detection code (around line 2151), when setting `peer.snubbed = true`, also:

```rust
peer.snubbed_flag.store(true, std::sync::atomic::Ordering::Release);
```

In snub recovery (line 3596-3599), when clearing snub:

```rust
peer.snubbed_flag.store(false, std::sync::atomic::Ordering::Release);
peer.pipeline.wake_driver(); // wake from snub sleep
peer.pipeline.restore_full_permits(peer.pending_requests.len());
```

**Step 12: Run the full test suite**

Run: `cargo test --workspace`
Expected: All existing tests pass. Some may need adjustment if they directly construct PeerState or PeerPipelineState.

**Step 13: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: No warnings.

**Step 14: Commit**

```bash
git add crates/torrent-session/src/peer_state.rs crates/torrent-session/src/torrent.rs crates/torrent-session/src/request_driver.rs
git commit -m "feat: wire request driver into peer lifecycle

Spawn request_driver on unchoke, cancel on choke/disconnect.
Handle NeedBlocks in torrent actor event loop. Release permits
on block received, timeout, and cancel. Wake driver on Have/Bitfield.
Sync snubbed atomic flag with snub detection."
```

---

## Task 4: Add 1-Redundant-Copy Limit to End-Game Picker

**Files:**
- Modify: `crates/torrent-session/src/end_game.rs` (lines 83-98, pick_block method)
- Test: `crates/torrent-session/src/end_game.rs` (inline tests, lines 220-418)

**Context:** Currently `pick_block` returns any block not already assigned to the requesting peer. We add a guard: skip blocks that already have 2+ assignees (original + 1 redundant).

**Step 1: Write the failing test**

Add to the test module in `end_game.rs`:

```rust
#[test]
fn test_pick_block_skips_fully_redundant() {
    let mut eg = EndGame::new();
    let peer_a = addr(1);
    let peer_b = addr(2);
    let peer_c = addr(3);

    // Register a block assigned to peer_a and peer_b (2 assignees = max redundancy)
    eg.add_block(0, 0, 16384);
    eg.register_request(0, 0, peer_a);
    eg.register_request(0, 0, peer_b);

    // peer_c should NOT get this block — it already has 2 assignees
    let have_all = |_: u32| true;
    let result = eg.pick_block(peer_c, &have_all);
    assert!(result.is_none(), "should not assign block with 2 assignees to a 3rd peer");
}

#[test]
fn test_pick_block_allows_one_redundant() {
    let mut eg = EndGame::new();
    let peer_a = addr(1);
    let peer_b = addr(2);

    // Register a block assigned only to peer_a (1 assignee — room for 1 redundant)
    eg.add_block(0, 0, 16384);
    eg.register_request(0, 0, peer_a);

    // peer_b should get this block as the redundant copy
    let have_all = |_: u32| true;
    let result = eg.pick_block(peer_b, &have_all);
    assert!(result.is_some(), "should allow 1 redundant copy");
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p torrent-session --lib end_game::tests::test_pick_block_skips_fully_redundant -- --nocapture`
Expected: FAIL (currently no redundancy limit).

**Step 3: Add the redundancy guard to pick_block**

In `pick_block` (lines 83-98), add a check in the block iteration loop:

```rust
// Inside the loop over blocks:
if block_entry.assigned_peers().len() >= 2 {
    continue; // original + 1 redundant = max
}
```

The exact insertion point depends on the current loop structure. It goes after the "skip if already assigned to this peer" check and before returning the block.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p torrent-session --lib end_game -- --nocapture`
Expected: All tests pass including the 2 new ones.

**Step 5: Commit**

```bash
git add crates/torrent-session/src/end_game.rs
git commit -m "feat: limit end-game redundancy to 1 copy per block

Match libtorrent's end-game strategy: each block can be requested from
at most 2 peers (original + 1 redundant). Prevents bandwidth waste
from spraying the same block to every connected peer."
```

---

## Task 5: Convert End-Game Dispatch to Semaphore

**Files:**
- Modify: `crates/torrent-session/src/torrent.rs` (end-game dispatch + remove 200ms tick)
- Modify: `crates/torrent-session/src/request_driver.rs` (no changes needed — driver already works for both modes)

**Context:** The request driver already handles end-game transparently — when end-game is active, `dispatch_blocks_for_peer` uses the end-game picker instead of the normal picker. The main changes are: (1) reactive cancel cascade releases permits, and (2) the 200ms batch tick is removed.

**Step 1: Update cancel cascade to release permits**

In the end-game block received handler (around lines 3610-3622 in `torrent.rs`), after sending Cancel to other peers and removing from their pending_requests, add:

```rust
for other_peer_addr in cancel_peers {
    if let Some(other_peer) = self.peers.get_mut(&other_peer_addr) {
        other_peer.cmd_tx.try_send(PeerCommand::Cancel { index, begin, length }).ok();
        other_peer.pending_requests.remove(index, begin);
        other_peer.pipeline.release_permit(); // freed → driver wakes → new useful work
    }
}
```

**Step 2: Update dispatch_blocks_for_peer for end-game**

In `dispatch_blocks_for_peer`, add end-game branch:

```rust
let block = if self.end_game.is_active() {
    self.end_game.pick_block(peer_addr, &peer_has_piece_fn)
} else {
    self.piece_selector.pick_blocks(&ctx, missing_chunks_fn)
};
```

This replaces the separate `request_end_game_block` function (line 5354).

**Step 3: Remove the 200ms end-game refill tick**

In the main event loop (around lines 2195-2203), remove the `end_game_refill_interval` tick branch entirely. The semaphore-driven cancel cascade now handles this reactively.

**Step 4: Remove `END_GAME_DEPTH` constant**

Remove the constant at lines 1475-1478. End-game now uses the same semaphore permit count as normal mode.

**Step 5: Remove `request_end_game_block` function**

Remove the separate end-game dispatch function (line 5354+). All dispatch goes through `dispatch_blocks_for_peer`.

**Step 6: Run the full test suite**

Run: `cargo test --workspace`
Expected: All tests pass. End-game tests may need updates if they relied on the batch tick timing.

**Step 7: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: No warnings.

**Step 8: Commit**

```bash
git add crates/torrent-session/src/torrent.rs
git commit -m "feat: convert end-game to semaphore dispatch, remove 200ms tick

End-game cancel cascade now releases permits on cancelled peers,
immediately waking their request drivers for new useful work.
Remove END_GAME_DEPTH constant, end_game_refill_interval tick,
and separate request_end_game_block function."
```

---

## Task 6: Remove Legacy Batch Dispatch

**Files:**
- Modify: `crates/torrent-session/src/torrent.rs` (remove old request_pieces_from_peer batch logic)
- Modify: `crates/torrent-session/src/settings.rs` (clean up unused fields)

**Context:** With the request driver handling all dispatch, the old batch-fill logic in `request_pieces_from_peer` is dead code. The proactive refill in the 1s pipeline tick (lines 2180-2189) is also unnecessary.

**Step 1: Remove or repurpose `request_pieces_from_peer`**

The function at line 5114 was the old batch dispatch. If `dispatch_blocks_for_peer` (Task 3) fully replaces it, remove `request_pieces_from_peer`. If any callers remain (e.g., initial unchoke), redirect them to `spawn_request_driver`.

**Step 2: Remove proactive refill from pipeline tick**

In the 1s pipeline tick (lines 2180-2189), remove the loop that calls `request_pieces_from_peer` for peers with available slots. The request driver handles this reactively now.

Keep the parts of the tick that:
- Call `peer.pipeline.tick()` for EWMA updates
- Detect snubbed peers
- Handle timeouts

**Step 3: Mark `request_queue_time` as deprecated in settings**

In `settings.rs`, the `request_queue_time: 3.0` field was already unused. Add a deprecation comment or remove it if no public API depends on it. If it's part of a serialized config format, keep it but document it's ignored.

**Step 4: Run the full test suite**

Run: `cargo test --workspace`
Expected: All tests pass.

**Step 5: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: No warnings.

**Step 6: Commit**

```bash
git add crates/torrent-session/src/torrent.rs crates/torrent-session/src/settings.rs
git commit -m "refactor: remove legacy batch dispatch and proactive refill

Remove request_pieces_from_peer batch loop and proactive refill from
pipeline tick. All request dispatch now goes through the per-peer
request driver. Mark request_queue_time as deprecated."
```

---

## Task 7: Integration Testing and Verification

**Files:**
- Modify: `crates/torrent-session/tests/` or existing integration test files
- Run: Full workspace tests + clippy + real download test

**Context:** Verify the semaphore pipeline works end-to-end with a real torrent download.

**Step 1: Run existing integration tests**

Run: `cargo test --workspace`
Expected: All tests pass. Fix any failures from the refactoring.

**Step 2: Run clippy clean**

Run: `cargo clippy --workspace -- -D warnings`
Expected: Clean.

**Step 3: Manual download test**

Test with a well-seeded torrent (e.g., Arch Linux ISO) to verify:
- Download completes successfully
- Speed is comparable to pre-refactor (within 10%)
- End-game activates and completes without hanging
- Choke/unchoke cycles don't cause panics or leaked tasks
- Log output shows semaphore-based dispatch (no batch fill messages)

Run: `cargo run --release -- download <arch-linux-magnet>`

**Step 4: Verify no task leaks**

Check that after download completes, all request driver tasks are cleaned up. No lingering tokio tasks. The `tokio-console` tool can help here if available, or add debug logging to driver shutdown.

**Step 5: Commit any test fixes**

```bash
git add -A
git commit -m "test: fix integration tests for semaphore pipeline

Update tests that relied on batch dispatch timing or counter-based
queue depth checks."
```

---

## Task 8: Documentation and Version Bump

**Files:**
- Modify: `Cargo.toml` (workspace version bump)
- Modify: `CHANGELOG.md`
- Modify: `README.md` (if pipeline is mentioned)

**Step 1: Bump version**

In root `Cargo.toml`, bump the workspace version (e.g., 0.67.0 → 0.68.0).

**Step 2: Update CHANGELOG.md**

Add entry:

```markdown
## v0.68.0

### Changed
- **Semaphore-based reactive pipeline**: Replaced poll-based batch request dispatch with per-peer `tokio::Semaphore` scheduling. Requests are now dispatched immediately when capacity frees up (zero-latency reactive dispatch).
- **End-game 1-redundant-copy limit**: End-game mode now limits each block to at most 2 assignees (original + 1 redundant), matching libtorrent's strategy. Prevents bandwidth waste from spraying blocks to all peers.
- **Reactive cancel cascade**: End-game cancels immediately free permits on cancelled peers, waking their request drivers for new useful work. Replaces the 200ms batch refill tick.

### Removed
- `END_GAME_DEPTH` constant (replaced by semaphore permit count)
- 200ms end-game refill tick (replaced by reactive cancel cascade)
- `in_slow_start()` stub (always returned false)
- Legacy batch slot-filling in `request_pieces_from_peer`
```

**Step 3: Commit**

```bash
git add Cargo.toml CHANGELOG.md
git commit -m "chore: bump version to 0.68.0, update changelog

Semaphore-based reactive pipeline, end-game 1-redundant-copy limit,
reactive cancel cascade."
```

---

## Task Summary

| Task | Description | Depends On |
|------|-------------|------------|
| 1 | Refactor PeerPipelineState to use Semaphore | — |
| 2 | Implement request_driver function | 1 |
| 3 | Wire request driver into peer lifecycle | 1, 2 |
| 4 | Add 1-redundant-copy limit to end-game | — |
| 5 | Convert end-game to semaphore dispatch | 3, 4 |
| 6 | Remove legacy batch dispatch | 3, 5 |
| 7 | Integration testing and verification | 6 |
| 8 | Documentation and version bump | 7 |

Tasks 1-3 are the critical path. Task 4 can be done in parallel with 1-3.
