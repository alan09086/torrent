# M75: Peer-Integrated Request Dispatch

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate cross-task semaphore permit management by moving the block requester into the peer task, matching rqbit's proven architecture.

**Architecture:** Replace the separate `request_driver` task (M73) with a third arm in the peer task's `tokio::select!` loop. The requester acquires semaphore permits, reserves pieces from shared state, and writes Request messages directly to the wire — all in the same task that handles incoming Piece messages and returns permits. This eliminates every cross-task permit handoff that caused M74's leak bugs.

**Tech Stack:** tokio (select!, Semaphore, Notify), parking_lot::RwLock, tokio-util FramedWrite

---

## Key Design Decisions

### Why this works (proven by rqbit at 82 MB/s)

1. **Same-task permit lifecycle**: The task that `forget()`s the permit is the same task that calls `add_permits(1)` on Piece receipt. No cross-task handoff = no leak sites.
2. **RAII on task exit**: When the peer disconnects, the task exits, the `Semaphore` drops. No permits can leak beyond the task's lifetime.
3. **Immediate permit return**: Permits are returned on Piece receipt *before* disk I/O, decoupling flow control from disk latency.
4. **select! cancellation safety**: If the requester future is waiting on `semaphore.acquire()` and another arm fires, the future is dropped. `acquire()` hasn't completed, so no permit was consumed. Clean.

### Semaphore model

- `Semaphore::new(0)` — created locally in peer task, starts empty
- On Unchoke: add permits up to `QUEUE_DEPTH` (128) using `available_permits()` check — prevents accumulation on repeated choke/unchoke cycles
- On Choke: guard disables requester arm; in-flight permits return naturally as Pieces arrive
- On Piece: `semaphore.add_permits(1)` immediately
- On RejectRequest: `semaphore.add_permits(1)` immediately
- On task exit: `Semaphore` drops — no cleanup needed

### Critical architecture note: `handle_message` separation

The peer task's message handling is split: `handle_message()` is a **separate function** (~line 370 in peer.rs) that does NOT have access to the new local state (`semaphore`, `peer_choking`, `reservation_state`). Therefore, permit management and choke tracking must be done **in the main loop body** (the `frame = framed_read.next()` arm), by matching on the message type BEFORE calling `handle_message()`. The `handle_message()` function continues to handle event forwarding to the actor unchanged.

### End-game handling

`PieceReservationState::next_request()` returns `None` when `endgame_active` is true. The requester arm waits on `piece_notify`, which won't fire during end-game. The actor handles end-game dispatch via `PeerCommand::Request` through the existing `cmd_rx` channel, as it did pre-M73.

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/torrent-session/src/peer.rs` | Modify | Add requester arm, local Semaphore, choke tracking, permit returns |
| `crates/torrent-session/src/types.rs` | Modify | Add `PeerCommand::StartRequesting` variant |
| `crates/torrent-session/src/torrent.rs` | Modify | Remove all driver code, remove actor permit returns, send StartRequesting to peers |
| `crates/torrent-session/src/peer_state.rs` | Modify | Remove `semaphore`, `driver_cancel`, `driver_handle` fields |
| `crates/torrent-session/src/request_driver.rs` | Delete | Entire module removed |
| `crates/torrent-session/src/lib.rs` | Modify | Remove `mod request_driver` |

---

## Chunk 1: Atomic Switch — Remove Old Drivers + Add New Requester

**IMPORTANT**: Tasks 1-4 must be implemented together in a single compilation unit. You cannot have both old drivers and new requester active simultaneously (would cause duplicate requests). The commit structure groups them for clarity, but all changes must compile together.

### Task 1: Add `PeerCommand::StartRequesting` variant

This command lets the actor send reservation state to peer tasks after magnet metadata download.

**Files:**
- Modify: `crates/torrent-session/src/types.rs` (PeerCommand enum, ~line 717)

- [ ] **Step 1: Add the variant to PeerCommand**

In `types.rs`, add to the `PeerCommand` enum:

```rust
/// M75: Actor sends reservation state to peer task for integrated dispatch.
/// Sent after metadata download (magnet) or at peer connection (non-magnet).
StartRequesting {
    reservation_state: std::sync::Arc<parking_lot::RwLock<crate::piece_reservation::PieceReservationState>>,
    piece_notify: std::sync::Arc<tokio::sync::Notify>,
},
```

Also in `handle_command()` in `peer.rs` (~line 825), add a no-op match arm so the compiler is satisfied. The actual handling is in the main loop (Task 2):

```rust
PeerCommand::StartRequesting { .. } => {
    // Handled in main loop, not here
    return Ok(());
}
```

- [ ] **Step 2: Compile check**

Run: `cargo check -p torrent-session 2>&1 | head -30`

---

### Task 2: Add requester infrastructure to peer.rs

**Files:**
- Modify: `crates/torrent-session/src/peer.rs`

- [ ] **Step 1: Add the `acquire_and_reserve` helper function**

Add near the top of `peer.rs` (after imports, before `run_peer`). Uses the existing `BlockRequest` type from `piece_reservation`:

```rust
use crate::piece_reservation::{BlockRequest, PieceReservationState};
use tokio::sync::Semaphore;

/// Per-peer request queue depth — matches rqbit's proven 128-permit model.
/// Each permit = one in-flight block request to this peer.
const QUEUE_DEPTH: usize = 128;

/// Acquire a semaphore permit and reserve the next block for this peer.
///
/// Returns when a block is available. If no block is available (all pieces
/// reserved or end-game active), waits on `piece_notify` and retries.
/// The permit is `forget()`-ed before returning — the caller is responsible
/// for calling `semaphore.add_permits(1)` when the block data arrives.
///
/// **Cancellation safe**: if this future is dropped by `select!`, any
/// acquired-but-not-forgotten permit is returned via `Drop`. No piece
/// is reserved until `forget()` is called, which only happens atomically
/// with the reservation.
async fn acquire_and_reserve(
    semaphore: &Semaphore,
    state: &parking_lot::RwLock<PieceReservationState>,
    piece_notify: &tokio::sync::Notify,
    addr: SocketAddr,
) -> BlockRequest {
    loop {
        let permit = semaphore
            .acquire()
            .await
            .expect("peer semaphore closed unexpectedly");

        if let Some(block) = state.write().next_request(addr) {
            // Transfer permit ownership to the Piece receipt handler.
            // The handler calls semaphore.add_permits(1) when data arrives.
            permit.forget();
            return block;
        }

        // No block available (all reserved, or end-game active).
        // Drop the permit (returns it to semaphore) and wait for a
        // piece to become available (verified, freed, or new peer has).
        drop(permit);
        piece_notify.notified().await;
    }
}
```

**Note**: `BlockRequest` already exists in `crate::piece_reservation` (~line 17-25) with fields `piece: u32`, `begin: u32`, `length: u32`. Do NOT define a duplicate.

- [ ] **Step 2: Compile check**

Run: `cargo check -p torrent-session 2>&1 | head -20`
Expected: warnings about unused function, no errors.

---

### Task 3: Integrate requester into the peer task's select! loop

This is the core change. The main `tokio::select!` loop (~line 220 in peer.rs) gains:
- A third arm for block requesting
- Message interception for permit management (BEFORE `handle_message()` is called)
- Local state for choke tracking and reservation state

**Files:**
- Modify: `crates/torrent-session/src/peer.rs` (~lines 195-360)

- [ ] **Step 1: Add local state variables before the main loop**

Find the section just before the main `loop { tokio::select! { ... } }` (~line 215). Add:

```rust
// M75: Peer-integrated request dispatch
let semaphore = Semaphore::new(0); // Start empty, add permits on unchoke
let mut peer_choking = true; // Peers start in choked state
let mut reservation_state: Option<(
    std::sync::Arc<parking_lot::RwLock<PieceReservationState>>,
    std::sync::Arc<tokio::sync::Notify>,
)> = None;
```

- [ ] **Step 2: Add message interception in the wire-read arm**

In the `frame = framed_read.next()` arm (~line 221), BEFORE the existing call to `handle_message()`, add a match block that intercepts Piece/Choke/Unchoke/RejectRequest for permit management. The key point: this runs in the main loop body where our local state is accessible, not inside `handle_message()`.

```rust
frame = framed_read.next() => {
    // ... existing frame parsing to get `msg: Message` ...

    // M75: Intercept messages for permit management BEFORE handle_message.
    // handle_message() is a separate function without access to our local
    // semaphore/choke state, so we handle permit logic here.
    match &msg {
        Message::Piece { .. } => {
            // Return permit immediately, before disk I/O
            if reservation_state.is_some() {
                semaphore.add_permits(1);
            }
        }
        Message::Unchoke => {
            peer_choking = false;
            // Add permits up to QUEUE_DEPTH — prevents accumulation
            // on repeated choke/unchoke cycles
            let available = semaphore.available_permits();
            let to_add = QUEUE_DEPTH.saturating_sub(available);
            if to_add > 0 {
                semaphore.add_permits(to_add);
            }
        }
        Message::Choke => {
            peer_choking = true;
            // Release reserved pieces — peer task owns this now
            if let Some((ref rs, _)) = reservation_state {
                rs.write().release_peer_pieces(addr);
            }
        }
        Message::RejectRequest { .. } => {
            if reservation_state.is_some() {
                semaphore.add_permits(1);
            }
        }
        _ => {}
    }

    // ... existing handle_message() call continues unchanged ...
}
```

**Implementer note**: The exact insertion point depends on how `msg` is bound from the frame. Look for where `handle_message(...)` is called and insert the match block just before it. The message type must be borrowed (`&msg`) so `handle_message` can still consume it.

- [ ] **Step 3: Add requester arm to the select! loop**

Add a third arm after the existing two. Clone the `Arc`s at the top of each loop iteration to avoid borrow conflicts across arms:

```rust
loop {
    // M75: Clone Arc references for requester arm (cheap Arc::clone)
    let rs_clone = reservation_state
        .as_ref()
        .map(|(rs, n)| (std::sync::Arc::clone(rs), std::sync::Arc::clone(n)));
    let can_request = !peer_choking && rs_clone.is_some();

    tokio::select! {
        biased;

        // Arm 1: Wire read (existing, with M75 interception added in Step 2)
        frame = framed_read.next() => { /* ... existing + interception ... */ }

        // Arm 2: Commands from actor (existing)
        cmd = cmd_rx.recv() => { /* ... existing ... */ }

        // Arm 3: Block requesting (M75, peer-integrated dispatch)
        block = async {
            let (ref rs, ref notify) = rs_clone.as_ref().unwrap();
            acquire_and_reserve(&semaphore, rs, notify, addr).await
        }, if can_request => {
            framed_write.send(Message::Request {
                index: block.piece,
                begin: block.begin,
                length: block.length,
            }).await.map_err(|e| crate::Error::Wire(e.into()))?;
        }
    }
}
```

**Key details:**
- `rs_clone` is computed at the top of each iteration (before `select!`), avoiding borrow conflicts with Arm 1 which mutates `reservation_state` via `StartRequesting`
- `can_request` guard: when false, the arm is never polled and `unwrap()` is never reached
- `framed_write.send()` is safe: only one arm body runs per select! iteration, so no concurrent writes with Arm 2's `handle_command()`

- [ ] **Step 4: Handle PeerCommand::StartRequesting in the cmd_rx arm**

In the `cmd = cmd_rx.recv()` arm's match block (where commands are dispatched), add handling for `StartRequesting` BEFORE the call to `handle_command`:

```rust
cmd = cmd_rx.recv() => {
    match cmd {
        None => break None, // channel closed
        Some(PeerCommand::Shutdown) => break Some("shutdown"),
        Some(PeerCommand::StartRequesting { reservation_state: rs, piece_notify }) => {
            // M75: Store reservation state for requester arm
            reservation_state = Some((rs, piece_notify));
            // Requester arm activates on next loop iteration if unchoked
        }
        Some(PeerCommand::UpdateNumPieces(n)) => {
            // ... existing handling ...
        }
        Some(cmd) => {
            handle_command(&mut framed_write, cmd, ...).await?;
        }
    }
}
```

**Implementer note**: The exact match structure depends on how the existing cmd_rx arm is written. The key requirement is that `StartRequesting` is handled in the main loop (setting `reservation_state`), not forwarded to `handle_command()`. `handle_command` should have a no-op arm for this variant (added in Task 1).

- [ ] **Step 5: Compile check**

Run: `cargo check -p torrent-session 2>&1 | head -40`
Fix borrow checker issues iteratively. Common fixes:
- Move `rs_clone` computation to correct scope
- Ensure `msg` lifetime works with both interception and `handle_message`

---

### Task 4: Remove driver infrastructure (atomic with Task 1-3)

Remove all old driver code from actor, PeerState, and request_driver.rs. This MUST be done in the same compilation as Tasks 1-3 — both systems cannot be active simultaneously.

**Files:**
- Modify: `crates/torrent-session/src/torrent.rs`
- Modify: `crates/torrent-session/src/peer_state.rs`
- Delete: `crates/torrent-session/src/request_driver.rs`
- Modify: `crates/torrent-session/src/lib.rs`

- [ ] **Step 1: Remove driver fields from PeerState**

In `peer_state.rs` (~lines 138-143), remove:
```rust
pub semaphore: Option<Arc<tokio::sync::Semaphore>>,
pub driver_cancel: Option<tokio_util::sync::CancellationToken>,
pub driver_handle: Option<tokio::task::JoinHandle<()>>,
```

Remove corresponding imports (`CancellationToken`, `JoinHandle`) if unused. Update `PeerState::new()` / `Default` to remove these fields.

- [ ] **Step 2: Remove ALL driver spawning from torrent.rs**

Search for `request_driver::spawn_driver` — there are **4 call sites**:
1. **Unchoke handler** (~line 3335): spawns driver when peer unchokes
2. **Bitfield handler** (~line 3287): spawns driver when peer sends bitfield and is already unchoked
3. **Peer addition** (~line 5023): spawns driver for pre-unchoked peers on add
4. **respawn_all_drivers** (~line 5367): respawns all drivers after end-game

Remove ALL of these. For the unchoke handler, keep only:
```rust
peer.peer_choking = choking;
// M75: Choke/unchoke dispatch handled by peer task
```

Remove `release_peer_pieces` from the choke path in the actor — peer task handles this now.

- [ ] **Step 3: Remove ALL driver cancellation sites from torrent.rs**

Search for `driver_cancel.take()` — there are **6 cancellation sites**:
1. **Choke handler**: cancels driver when peer chokes
2. **Disconnect handler**: cancels driver when peer disconnects
3. **Snub detection**: cancels driver when peer is snubbed
4. **MSE retry handler** (~line 3621-3626): cancels driver on encryption retry
5. **Zombie pruning** (~line 5749): cancels driver when zombie peer removed
6. **Peer turnover** (~line 5606): cancels driver during turnover disconnects

Remove all of these. For disconnect/snub/zombie/turnover, keep the other cleanup (remove from peers map, etc.) — just remove the driver-specific lines.

For end-game activation: keep `reservation_state.write().set_endgame(true)`. Remove driver cancellation loop. The peer tasks' `next_request()` returns `None` during end-game, naturally parking the requester arms.

- [ ] **Step 4: Remove actor-side permit returns from torrent.rs**

Remove these M74 permit return sites (they're now handled by the peer task):

1. **handle_piece_data** (~line 3794-3805): remove `sem.add_permits(1)` block
2. **RejectRequest handler** (~line 3465-3475): remove `sem.add_permits(1)` block
3. **Write failure path** (~line 3688-3694): remove `sem.add_permits(1)` block

- [ ] **Step 5: Add StartRequesting sends to peer spawn sites**

At the 3 peer spawn sites in torrent.rs (uTP ~line 6070, TCP outbound ~line 6261, TCP inbound ~line 6515), after the peer is spawned and PeerState is stored, send the reservation state:

```rust
// M75: Send reservation state to peer for integrated dispatch
if let (Some(rs), Some(notify)) = (&self.reservation_state, &self.reservation_notify) {
    let _ = cmd_tx.try_send(PeerCommand::StartRequesting {
        reservation_state: Arc::clone(rs),
        piece_notify: Arc::clone(notify),
    });
}
```

- [ ] **Step 6: Add StartRequesting broadcast after magnet metadata**

Find where the actor creates `PieceReservationState` after metadata download. After creating it, broadcast to all connected peers:

```rust
// M75: Inform all connected peers about reservation state
if let (Some(rs), Some(notify)) = (&self.reservation_state, &self.reservation_notify) {
    for peer in self.peers.values() {
        let _ = peer.cmd_tx.try_send(PeerCommand::StartRequesting {
            reservation_state: Arc::clone(rs),
            piece_notify: Arc::clone(notify),
        });
    }
}
```

- [ ] **Step 7: Delete request_driver.rs and remove mod declaration**

```bash
rm crates/torrent-session/src/request_driver.rs
```

In `crates/torrent-session/src/lib.rs`, remove:
```rust
pub(crate) mod request_driver;
```

Remove `use crate::request_driver` from `torrent.rs`.

- [ ] **Step 8: Compile check**

Run: `cargo check -p torrent-session 2>&1 | head -40`
Fix all remaining compilation errors. Common issues:
- References to removed `peer.semaphore` / `peer.driver_cancel` / `peer.driver_handle`
- Missing `BlockRequest` import in `peer.rs`
- `handle_command` not handling `StartRequesting` (should return early / no-op)
- `respawn_all_drivers` function now unused — delete it

- [ ] **Step 9: Commit all changes as atomic unit**

```bash
git add -A crates/torrent-session/
git commit -m "feat: replace request_driver with peer-integrated dispatch (M75)

Move block requester from separate driver tasks into the peer task's
select! loop. Permits are now acquired and returned in the same task,
eliminating all cross-task permit leak sites.

Key changes:
- Add third arm to peer select! loop: acquire permit -> reserve piece -> send Request
- Return permits on Piece/RejectRequest receipt BEFORE disk I/O
- Remove request_driver.rs module entirely
- Remove driver fields from PeerState
- Remove actor-side permit management (handle_piece_data, RejectRequest, write failure)
- Track choke state locally in peer task, release pieces on choke
- Send PeerCommand::StartRequesting from actor to provide reservation state
- Prevent permit accumulation via available_permits() check on unchoke"
```

---

## Chunk 2: Verification & Benchmark

### Task 5: Full test suite and clippy

- [ ] **Step 1: Run clippy**

Run: `cargo clippy --workspace -- -D warnings 2>&1 | tail -20`
Expected: zero warnings. Fix any issues (likely unused imports from removed driver code).

- [ ] **Step 2: Run full test suite**

Run: `cargo test --workspace 2>&1 | grep "^test result:" | awk '{sum += $4} END {print sum " tests passed"}'`
Expected: ~1419 tests pass (some driver-specific tests may be removed, count may decrease slightly).

- [ ] **Step 3: Fix any failures**

If tests fail, investigate and fix. Common issues:
- Tests that directly reference `request_driver` types (delete them)
- Tests that construct `PeerState` with driver fields (remove the fields)
- Integration tests that expect driver behavior (update expectations)

- [ ] **Step 4: Commit fixes if any**

```bash
git add -A && git commit -m "fix: resolve test/clippy issues after dispatch refactor (M75)"
```

---

### Task 6: Benchmark and version bump

- [ ] **Step 1: Build release binary**

```bash
RUSTFLAGS="-C target-cpu=native" cargo build --release -p torrent-cli
```

- [ ] **Step 2: Run 3-trial Arch ISO benchmark**

```bash
MAGNET='magnet:?xt=urn:btih:a4373c326657898d0c588c3ff892a0fac97ffa20&dn=archlinux-2026.03.01-x86_64.iso'
# Run each trial individually with 5-minute timeout
# Record: wall time, CPU time, speed (MB/s), RSS
```

Compare against:
- M74 baseline: 12.2±5.6 MB/s, 25.3±6.0s CPU, 87.5±30.2 MiB RSS
- M72 pre-regression: 28.0 MB/s
- rqbit reference: 82.2 MB/s, 9.8s CPU, 39.4 MiB RSS

**Success criteria:**
- Speed > 20 MB/s (recovery from M73/M74 regression)
- Low variance (stdev < 5 MB/s, indicating no resource leaks)
- RSS stable across trials (no growth pattern)

- [ ] **Step 3: Bump version**

In root `Cargo.toml`: `version = "0.76.0"` → `version = "0.77.0"`

- [ ] **Step 4: Update docs**

Update CHANGELOG.md, README.md, CLAUDE.md with M75 results and benchmark data.

- [ ] **Step 5: Commit and push**

```bash
git add Cargo.toml && git commit -m "chore: bump version to 0.77.0 (M75)"
git add CHANGELOG.md README.md CLAUDE.md && git commit -m "docs: update CHANGELOG, README, CLAUDE.md for M75 v0.77.0"
git push origin main && git push github main
```

---

## Permit Lifecycle — All Sites That Call `add_permits(1)`

To prevent leaks, every forgotten permit MUST be returned exactly once. Here are ALL return sites:

| Site | Location | Trigger |
|------|----------|---------|
| Piece receipt | `peer.rs` main loop, Message::Piece arm | Block data arrives from wire |
| RejectRequest | `peer.rs` main loop, Message::RejectRequest arm | Peer rejects our request |
| Task exit | Implicit: `Semaphore` drops when peer task returns | Disconnect, error, shutdown |

**Sites that do NOT need `add_permits`** (permit was never forgotten):
- `acquire_and_reserve` cancelled by select! → permit's `Drop` returns it
- `acquire_and_reserve` gets `None` from `next_request` → explicit `drop(permit)`

**Invariant check**: `forgotten permits = in-flight blocks = Piece receipts pending`. When the task exits, all are cancelled simultaneously via Semaphore drop.

---

## Risk Assessment

| Risk | Mitigation |
|------|------------|
| Borrow conflicts in select! | Clone `Arc` refs at loop top; `framed_write` used by one arm body at a time |
| Permit accumulation on repeated unchoke | `available_permits()` check caps at QUEUE_DEPTH |
| End-game dispatch regression | `next_request()` returns `None` during end-game; actor's `PeerCommand::Request` via cmd_rx unchanged |
| Magnet metadata phase | `reservation_state = None` → guard disables requester arm; `StartRequesting` sent after metadata |
| `framed_write.send()` failure in requester arm | Connection dead → task exits → Semaphore drops → no cleanup needed |
| `handle_message` doesn't have local state | Permit management done in main loop BEFORE calling `handle_message()` |

---

## Architecture Comparison

```
BEFORE (M73/M74):                    AFTER (M75):

┌─────────────┐                      ┌─────────────────────────────┐
│ Driver Task │──permit.forget()──┐  │ Peer Task                   │
│ (separate)  │                   │  │ ┌─────────┐ ┌───────────┐  │
│ acquire()   │                   │  │ │Requester│ │Wire Reader│  │
│ reserve()   │                   │  │ │acquire()│ │Piece→     │  │
│ cmd_tx.send │                   │  │ │reserve()│ │ add_permit│  │
└─────────────┘                   │  │ │write()  │ │ forward() │  │
       ↓ PeerCommand::Request     │  │ └────┬────┘ └─────┬─────┘  │
┌─────────────┐                   │  │      │            │         │
│ Peer Task   │                   │  │   framed_write  framed_read │
│ wire I/O    │──PeerEvent──┐     │  └──────┼────────────┼─────────┘
└─────────────┘             │     │         │            │
                            ↓     │         ↓            ↑
                    ┌──────────┐  │     ┌────────┐   ┌────────┐
                    │  Actor   │←─┘     │ Wire   │   │ Wire   │
                    │ disk I/O │        └────────┘   └────────┘
                    │ add_perm │              ↕ network ↕
                    │ verify   │
                    └──────────┘        ┌──────────┐
                                        │  Actor   │
                    4+ leak sites       │ disk I/O │
                    (M74 patched 3)     │ verify   │
                                        │ NO perms │
                                        └──────────┘

                                        0 leak sites
                                        (Semaphore drops with task)
```
