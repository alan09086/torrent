# M118: Broadcast Have Distribution + Per-Peer Filtering

## Context

The current Have distribution uses a two-path system:
1. **Immediate mode** (default, `have_send_delay_ms = 0`): TorrentActor iterates
   all peers on piece verification, sends `PeerCommand::Have(index)` via each
   peer's command channel with redundancy filtering (`!peer.bitfield.get(index)`)
2. **Batched mode** (`have_send_delay_ms > 0`, default 100ms since M108):
   `HaveBuffer` accumulates piece indices, flushes on timer, sends individual
   Haves or a full bitfield when > 50% of pieces changed

Both approaches have problems:
- **Actor bottleneck**: TorrentActor must iterate ALL peers for every verified piece.
  With 200 peers and 5822 pieces, that's ~1.16M peer iterations per download.
- **100ms latency** (batched mode): Peers don't learn about new pieces for up to
  100ms, reducing swarm efficiency for well-seeded torrents.
- **Redundant Haves** (immediate mode): No filtering in batched mode flush —
  all buffered Haves go to all peers regardless of their bitfield.

rqbit uses `tokio::sync::broadcast` for zero-latency Have distribution with
per-peer filtering via the `PeerConnectionHandler::should_transmit_have()` method.

**Depends on M117**: The `should_transmit_have()` method is defined on the
`PeerConnectionHandler` trait introduced in M117.

## Architecture

### Current: TorrentActor-Centric Distribution

```
TorrentActor::on_piece_verified(index)
    │
    ├─ immediate mode: for peer in peers { peer.cmd_tx.send(Have(index)) }
    │                  ↑ actor iterates all peers, one command per peer
    │
    └─ batched mode:   have_buffer.push(index)
                       ↓ 100ms later
                       flush_have_buffer()
                           for peer in peers {
                               for &idx in &pieces {
                                   peer.cmd_tx.send(Have(idx))
                               }
                           }
                       ↑ O(peers * buffered_pieces) commands
```

**Problems:**
- Actor is on the critical path for every Have
- 100ms latency in batched mode
- Per-peer channels can back up with Have messages
- No filtering in batched mode (all Haves to all peers)

### Proposed: Broadcast + Per-Peer Filtering

```
TorrentActor::on_piece_verified(index)
    │
    └─ have_broadcast_tx.send(index)    ← O(1), returns immediately
            │
            ├── peer_1 writer: broadcast_rx.recv() → should_transmit_have(index)?
            │                  YES → serialize Have → write to socket
            │
            ├── peer_2 writer: broadcast_rx.recv() → should_transmit_have(index)?
            │                  NO (peer already has piece) → skip
            │
            └── peer_N writer: ...
```

**Benefits:**
- Actor sends one message, not N
- Zero latency — peers receive immediately
- Per-peer filtering — no redundant Haves
- No HaveBuffer, no timer, no batch flush logic
- Writer loop processes Haves concurrently across peers

## rqbit Reference

### `crates/librqbit/src/torrent_state/live/mod.rs` lines 207-252: broadcast setup

```rust
have_broadcast_tx: tokio::sync::broadcast::Sender<ValidPieceIndex>,

// In constructor:
let (have_broadcast_tx, _) = tokio::sync::broadcast::channel(128);
```

### `crates/librqbit/src/torrent_state/live/mod.rs` line 720-722: transmit_haves

```rust
fn transmit_haves(&self, index: ValidPieceIndex) {
    let _ = self.have_broadcast_tx.send(index);
}
```

Single send, O(1). The `let _ =` ignores the error when no receivers exist
(no connected peers).

### `crates/librqbit/src/peer_connection.rs` lines 110-118: ManagePeerArgs

```rust
struct ManagePeerArgs {
    // ...
    have_broadcast: tokio::sync::broadcast::Receiver<ValidPieceIndex>,
}
```

Each peer connection gets its own receiver via `have_broadcast_tx.subscribe()`.

### `crates/librqbit/src/peer_connection.rs` lines 341-369: writer select! with broadcast

```rust
loop {
    break tokio::select! {
        r = have_broadcast.recv(), if !broadcast_closed => match r {
            Ok(id) => {
                if self.handler.should_transmit_have(id) {
                    WriterRequest::Message(Message::Have(id.get()))
                } else {
                    continue  // Skip — peer already has this piece
                }
            },
            Err(RecvError::Closed) => {
                broadcast_closed = true;
                continue
            },
            _ => continue  // Lagged — skip missed Haves
        },
        r = timeout(keep_alive_interval, outgoing_chan.recv()) => match r {
            Ok(Some(msg)) => msg,
            Ok(None) => return Err(Error::TorrentIsNotLive),
            Err(_) => WriterRequest::Message(Message::KeepAlive),
        }
    };
}
```

Key patterns:
- `have_broadcast.recv()` is one arm in the writer's select loop
- `should_transmit_have(id)` filters before serialization
- `continue` on Lagged — missed Haves are fine (peer will request pieces it needs)
- `broadcast_closed` flag prevents polling a dead channel

### `crates/librqbit/src/torrent_state/live/mod.rs` lines 1191-1203: should_transmit_have

```rust
fn should_transmit_have(&self, id: ValidPieceIndex) -> bool {
    if self.state.shared.options.disable_upload() {
        return false;
    }
    let have = self.state.peers
        .with_live(self.addr, |l| {
            l.bitfield.get(id.get_usize()).map(|p| *p).unwrap_or(true)
        })
        .unwrap_or(true);
    !have  // Transmit only if peer does NOT have the piece
}
```

Checks the peer's bitfield. Returns `false` if:
- Upload is disabled (we don't want peers requesting from us)
- Peer already has the piece (redundant Have)

## Changes

### 1. Broadcast channel on TorrentActor (`torrent-session/src/torrent.rs`, ~15 lines)

```rust
// New field:
have_broadcast_tx: broadcast::Sender<u32>,

// In TorrentActor::new() / registration:
let (have_broadcast_tx, _) = broadcast::channel::<u32>(capacity);
// Capacity: max(128, num_pieces / 4) — enough for burst completions
// during endgame where many pieces verify in quick succession.
```

### 2. on_piece_verified sends broadcast (`torrent-session/src/torrent.rs`, ~10 lines changed)

Replace the current Have distribution with a single broadcast send:

```rust
// Current code (lines 4939-4954):
if self.super_seed.is_none() {
    let already_announced = self.predictive_have_sent.remove(&index);
    if !already_announced {
        if self.have_buffer.is_enabled() {
            self.have_buffer.push(index);
        } else {
            for peer in self.peers.values() {
                if !peer.bitfield.get(index) {
                    let _ = peer.cmd_tx.try_send(PeerCommand::Have(index));
                }
            }
        }
    }
}

// Proposed:
if self.super_seed.is_none() {
    let already_announced = self.predictive_have_sent.remove(&index);
    if !already_announced {
        let _ = self.have_broadcast_tx.send(index);
    }
}
```

One line replaces the entire loop + buffer logic. The `let _ =` handles the
case where no peers are connected (all receivers dropped).

### 3. Peer writer receives broadcast (`torrent-session/src/peer_connection.rs`, ~30 lines)

In the PeerConnection writer select loop (introduced in M117):

```rust
// New field in PeerConnection or passed to run():
have_broadcast_rx: broadcast::Receiver<u32>,

// In writer select! loop:
tokio::select! {
    biased;

    // Have broadcast from TorrentActor
    result = self.have_broadcast_rx.recv() => {
        match result {
            Ok(piece_index) => {
                if self.handler.should_transmit_have(piece_index) {
                    writer.send(&Message::Have { index: piece_index }).await?;
                }
                // else: peer already has this piece, skip
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                debug!(skipped = n, "have broadcast lagged, skipping missed Haves");
                // Lagged is fine — peer will request pieces it needs via Interested
            }
            Err(broadcast::error::RecvError::Closed) => {
                // Torrent shutting down — stop polling broadcast
                // Set a flag to disable this select arm
            }
        }
    }

    // Commands from TorrentActor (requests, cancel, choke, etc.)
    Some(cmd) = cmd_rx.recv() => {
        self.handle_command(cmd, &mut writer).await?;
    }

    // Keep-alive timer
    _ = keep_alive.tick() => {
        writer.send_keep_alive().await?;
    }
}
```

### 4. should_transmit_have implementation (`torrent-session/src/torrent_peer_handler.rs`)

Already defined in M117's `TorrentPeerHandler`:

```rust
fn should_transmit_have(&self, piece: u32) -> bool {
    // Don't transmit if upload is disabled
    if self.upload_disabled {
        return false;
    }
    // Don't transmit if peer already has this piece
    !self.peer_bitfield.get(piece)
}
```

The `peer_bitfield` is maintained by `on_received_message` when processing
`Message::Have` and `Message::Bitfield` from the peer.

### 5. Subscribe on peer spawn (`torrent-session/src/torrent.rs`, ~5 lines)

When spawning a new peer connection, subscribe to the broadcast:

```rust
// In spawn_peer / add_incoming_peer:
let have_broadcast_rx = self.have_broadcast_tx.subscribe();
// Pass to PeerConnection::new() or PeerConnection::run()
```

### 6. Remove HaveBuffer (`torrent-session/src/have_buffer.rs`, delete file)

Remove the entire `HaveBuffer` struct and module:

- Delete `have_buffer.rs` (~131 lines)
- Remove `pub(crate) mod have_buffer;` from `lib.rs`
- Remove `have_buffer` field from `TorrentActor`
- Remove `flush_have_buffer()` method from `TorrentActor`
- Remove `have_send_delay_ms` from Settings (or keep but mark deprecated)
- Remove the `have_buffer flush` timer arm from TorrentActor's select! loop

### 7. Remove PeerCommand::Have and PeerCommand::SendBitfield

These are no longer needed — Haves are distributed via broadcast, not per-peer
commands:

```rust
// Remove from PeerCommand enum:
// Have(u32),                    ← replaced by broadcast
// SendBitfield(Bytes),          ← was HaveBuffer overflow path, no longer needed

// Remove handling in peer.rs / peer_connection.rs:
// PeerCommand::Have(index) => writer.send(Message::Have { index })
// PeerCommand::SendBitfield(data) => writer.send(Message::Bitfield(data))
```

### 8. Lagged receiver handling (~5 lines)

When a peer's writer is slow (e.g., congested TCP connection), the broadcast
receiver will lag behind. `broadcast::Receiver::recv()` returns
`Err(RecvError::Lagged(n))` indicating `n` messages were dropped.

This is acceptable because:
- Haves are advisory — peers discover piece availability through Interested/Request
- A lagged peer will eventually send Interested when it needs pieces
- Better to skip Haves than to buffer indefinitely and increase memory usage

Log at debug level for diagnostics.

## Tests

### New Tests (8)

1. **`broadcast_have_reaches_all_peers`** — Register 5 peers. Verify piece via
   TorrentActor. Check all 5 peer writers receive the broadcast and call
   `should_transmit_have`.

2. **`broadcast_filtered_for_peer_with_piece`** — Peer's bitfield already has
   piece 42. Broadcast Have(42). Verify `should_transmit_have(42)` returns false
   and no Have message is written to the socket.

3. **`broadcast_not_filtered_for_peer_without_piece`** — Peer's bitfield does
   NOT have piece 42. Broadcast Have(42). Verify `should_transmit_have(42)`
   returns true and Have message IS written to the socket.

4. **`broadcast_upload_disabled_filters_all`** — Handler with `upload_disabled = true`.
   Broadcast any Have. Verify `should_transmit_have()` returns false for all pieces.

5. **`broadcast_lagged_receiver_recovers`** — Create broadcast with capacity 4.
   Send 10 Haves before receiver processes any. Verify receiver gets Lagged(6)
   error, then receives the last 4 normally.

6. **`broadcast_channel_capacity`** — Verify channel capacity is set to
   `max(128, num_pieces / 4)` during TorrentActor construction.

7. **`no_have_for_already_known_piece`** — Peer sends us Have(42). We verify
   piece 42. Our broadcast fires Have(42). Verify we do NOT send Have(42) back
   to that peer (they already have it).

8. **`initial_bitfield_unaffected`** — The initial Bitfield send on connection
   (Phase 2 of handshake) is unchanged by this refactor. Verify bitfield is
   still sent correctly after handshake, before broadcast starts.

### Removed Tests

- `immediate_mode_no_buffering` — HaveBuffer deleted
- `batch_mode_returns_haves` — HaveBuffer deleted
- `large_batch_upgrades_to_bitfield` — HaveBuffer deleted

Net test change: +8 new, -3 removed = +5.

### Existing Tests

All remaining 1647+ existing tests must pass. The `PeerCommand::Have` removal
requires updating any test that sends or expects `PeerCommand::Have`. These tests
should be updated to use the broadcast channel instead.

## Success Criteria

- Have latency: < 1ms per peer (was 100ms batch delay)
- Redundant Haves: 0 per peer (was ~50% for well-seeded torrents where peers
  share most pieces)
- Actor time in Have distribution: O(1) (was O(peers))
- HaveBuffer removed: -131 lines
- have_buffer.rs deleted
- All existing tests pass (with updates for PeerCommand::Have removal)
- 8 new tests pass

## Verification

```bash
# Run all tests
cargo test --workspace

# Run new broadcast tests
cargo test -p torrent-session broadcast_
cargo test -p torrent-session no_have_for_
cargo test -p torrent-session initial_bitfield_

# Benchmark (10 trials)
./benchmarks/analyze.sh '<arch-iso-magnet>' -n 10

# Verify Have filtering with debug logging
RUST_LOG=torrent_session::peer_connection=debug \
    ./target/release/torrent-cli download '<magnet>' 2>&1 | grep -c "should_transmit_have.*false"

# Verify no HaveBuffer references remain
grep -r "HaveBuffer\|have_buffer\|have_send_delay" crates/torrent-session/src/

# Clippy
cargo clippy --workspace -- -D warnings
```

## NOT in Scope

| Item | Why | Revisit |
|------|-----|---------|
| Bitfield compression (BEP 6 HaveAll) | HaveAll already handles the complete case; partial bitfield compression adds complexity for marginal bandwidth savings | If bandwidth becomes a concern on metered connections |
| Priority-based Have ordering | All Haves have equal priority; ordering by rarity would add complexity to the broadcast path | If rare-first Have improves download speed |
| Have suppression during endgame | During endgame, Haves help cancel redundant requests; suppression would be counterproductive | Never |
| Removing PeerCommand enum entirely | PeerCommand is still needed for Request, Cancel, Choke, Interested, and other per-peer commands | If broadcast can replace all commands |
| Dynamic broadcast capacity | Fixed `max(128, num_pieces / 4)` is sufficient; dynamic resizing adds complexity | If lagged receiver errors are frequent in production |
