# M117: PeerConnectionHandler Trait

## Context

`peer.rs` is 3375 lines containing a monolithic `run_peer()` function that handles
everything: MSE/PE encryption negotiation, BEP 3 handshake, BEP 10 extension
negotiation, message parsing, piece dispatch, block tracking, event batching,
Have sending, metadata download, PEX, and connection lifecycle management.

This coupling makes it difficult to:
1. Unit test individual message handling paths (must set up full peer connection)
2. Add protocol variants (metadata-only peers, BEP 29 uTorrent transport)
3. Implement M118 broadcast Have filtering (needs per-peer `should_transmit_have`)
4. Profile individual components (everything is interleaved in one function)

rqbit separates these concerns with a `PeerConnectionHandler` trait. Connection
management (read/write loops, timeouts, keep-alive) is generic; torrent logic
(piece writing, block tracking, availability updates) is a trait implementation.

**Current state**: 3375 lines in peer.rs, 14272 lines in torrent.rs.

## Architecture

### Current: Monolithic run_peer

```
run_peer(addr, stream, info_hash, our_peer_id, our_bitfield, num_pieces,
         event_tx, cmd_rx, enable_dht, enable_fast, encryption_mode,
         outbound, anonymous_mode, info_bytes, plugins, enable_holepunch,
         max_message_size)
{
    // Phase 0: MSE/PE encryption (60 lines)
    // Phase 1: BEP 3 handshake (50 lines)
    // Phase 2: Extension negotiation (100 lines)
    // Phase 3: Message loop — select! {
    //     reader: decode + direct pwrite + block tracking + batching  (800 lines)
    //     writer: cmd_rx + Have sending + piece upload + PEX          (400 lines)
    //     timers: flush batch, Have buffer, keep-alive                (200 lines)
    // }
    // Cleanup and event reporting (100 lines)
}
```

17 parameters. Torrent-specific logic (piece writing, block maps, dispatch state)
is tangled with connection management (read/write loops, timeouts).

### Proposed: PeerConnectionHandler Trait + PeerConnection<H>

```
                   ┌─────────────────────────────────┐
                   │      PeerConnectionHandler       │  trait
                   │  on_handshake()                  │
                   │  on_extended_handshake()          │
                   │  on_received_message()            │
                   │  should_send_bitfield()           │
                   │  serialize_bitfield()             │
                   │  should_transmit_have()           │  ← for M118
                   │  on_uploaded_bytes()              │
                   │  read_chunk()                     │
                   │  on_disconnect()                  │
                   └──────────┬──────────────────────┘
                              │ implements
          ┌───────────────────┼───────────────────┐
          │                   │                   │
  TorrentPeerHandler  MetadataPeerHandler   (future handlers)
  - direct pwrite       - ut_metadata only
  - block tracking       - no piece I/O
  - dispatch state       - lightweight
  - event batching
  - PEX handling

  PeerConnection<H: PeerConnectionHandler>
  - owns PeerReader, PeerWriter
  - encryption setup
  - BEP 3 handshake
  - select! { reader, writer, timers }
  - calls handler methods for torrent logic
```

## rqbit Reference

### `crates/librqbit/src/peer_connection.rs` lines 36-55: PeerConnectionHandler trait

```rust
pub trait PeerConnectionHandler {
    fn on_connected(&self, _connection_time: Duration) {}
    fn should_send_bitfield(&self) -> bool;
    fn serialize_bitfield_message_to_buf(&self, buf: &mut [u8]) -> anyhow::Result<usize>;
    fn on_handshake(&self, handshake: Handshake, ckind: ConnectionKind) -> anyhow::Result<()>;
    fn on_extended_handshake(
        &self, extended_handshake: &ExtendedHandshake<ByteBuf>,
    ) -> anyhow::Result<()>;
    async fn on_received_message(&self, msg: Message<'_>) -> anyhow::Result<()>;
    fn should_transmit_have(&self, id: ValidPieceIndex) -> bool;
    fn on_uploaded_bytes(&self, bytes: u32);
    fn read_chunk(&self, chunk: &ChunkInfo, buf: &mut [u8]) -> anyhow::Result<()>;
    fn update_my_extended_handshake(
        &self, _handshake: &mut ExtendedHandshake<ByteBuf>,
    ) -> anyhow::Result<()> { Ok(()) }
}
```

### `crates/librqbit/src/peer_connection.rs` lines 79-87: PeerConnection<H>

```rust
pub(crate) struct PeerConnection<H> {
    handler: H,
    addr: SocketAddr,
    info_hash: Id20,
    peer_id: Id20,
    options: PeerConnectionOptions,
    spawner: BlockingSpawner,
    connector: Arc<StreamConnector>,
}
```

### `crates/librqbit/src/peer_connection.rs` lines 272-511: manage_peer

The `manage_peer` method contains the reader/writer select loop. It calls handler
methods for all torrent-specific logic:

- Reader loop: `handler.on_received_message(message).await` (line 482)
- Writer loop: `handler.should_transmit_have(id)` for broadcast filtering (line 348)
- Writer loop: `handler.read_chunk(&chunk, &mut write_buf[..])` for upload (line 431)
- Writer loop: `handler.on_uploaded_bytes(chunk.size)` (line 459)
- Extension: `handler.on_extended_handshake(h)` (line 478)
- Bitfield: `handler.serialize_bitfield_message_to_buf(&mut *write_buf)` (line 321)

### `crates/librqbit/src/torrent_state/live/mod.rs` lines 1100-1210: TorrentPeerHandler

The handler implementation holds references to shared torrent state and implements
all trait methods. `on_received_message` dispatches by message type, writing pieces
directly via blocking spawner.

## Changes

### 1. PeerConnectionHandler trait (`torrent-session/src/peer_handler.rs`, ~100 lines)

```rust
use std::net::SocketAddr;
use torrent_core::Id20;
use torrent_wire::{ExtHandshake, Handshake, Message};

/// Trait for handling peer wire protocol events.
///
/// Separates connection management (read/write loops, encryption, timeouts)
/// from torrent-specific logic (piece I/O, block tracking, dispatch).
///
/// The generic Message type allows borrowed zero-copy messages from the ring
/// buffer (Message<&[u8]>) as well as owned messages (Message<Bytes>).
pub(crate) trait PeerConnectionHandler: Send + 'static {
    /// Called after successful BEP 3 handshake.
    /// Return Err to reject the connection.
    fn on_handshake(&self, handshake: &Handshake) -> crate::Result<()>;

    /// Called after receiving the peer's BEP 10 extended handshake.
    fn on_extended_handshake(&self, ext: &ExtHandshake) -> crate::Result<()>;

    /// Called for each received peer message (except extended handshake,
    /// which goes to on_extended_handshake).
    ///
    /// For Piece messages, the handler should perform direct pwrite from
    /// the borrowed ring buffer slices.
    async fn on_received_message(&mut self, msg: Message<&[u8]>) -> crate::Result<()>;

    /// Whether to send our bitfield after handshake.
    fn should_send_bitfield(&self) -> bool;

    /// Serialize our bitfield message into buf. Returns bytes written.
    fn serialize_bitfield(&self, buf: &mut [u8]) -> crate::Result<usize>;

    /// Whether to transmit a Have message for this piece to this peer.
    /// Called from the writer loop when a broadcast Have is received.
    /// Return false if the peer already has the piece.
    fn should_transmit_have(&self, piece: u32) -> bool;

    /// Notify the handler that bytes were uploaded to this peer.
    fn on_uploaded_bytes(&self, bytes: u32);

    /// Read a chunk from storage for upload to this peer.
    /// Writes piece data directly into buf. Returns bytes written.
    fn read_chunk(&self, index: u32, begin: u32, length: u32, buf: &mut [u8]) -> crate::Result<usize>;

    /// Called when the connection is being closed.
    fn on_disconnect(&mut self, reason: &str);

    /// Address of the connected peer.
    fn addr(&self) -> SocketAddr;
}
```

### 2. PeerConnection<H> struct (`torrent-session/src/peer_connection.rs`, ~400 lines)

New file. Extracts connection management from `run_peer()`:

```rust
use crate::peer_codec::{PeerReader, PeerWriter};
use crate::peer_handler::PeerConnectionHandler;

pub(crate) struct PeerConnectionOptions {
    pub read_write_timeout: Duration,
    pub keep_alive_interval: Duration,
    pub max_message_size: usize,
}

pub(crate) struct PeerConnection<H: PeerConnectionHandler> {
    handler: H,
    options: PeerConnectionOptions,
}

impl<H: PeerConnectionHandler> PeerConnection<H> {
    pub fn new(handler: H, options: PeerConnectionOptions) -> Self {
        Self { handler, options }
    }

    /// Run the peer connection lifecycle.
    /// Stream is already handshake-validated and optionally encrypted.
    pub async fn run<R, W>(
        &mut self,
        reader: PeerReader<R>,
        writer: PeerWriter<W>,
        mut cmd_rx: mpsc::Receiver<PeerCommand>,
    ) -> crate::Result<()>
    where
        R: AsyncReadVectored,
        W: AsyncWrite + Unpin,
    {
        let mut reader = reader;
        let mut writer = writer;

        // The reader/writer select! loop (extracted from run_peer Phase 3)
        let mut flush_interval = tokio::time::interval(Duration::from_millis(25));
        let mut keep_alive = tokio::time::interval(self.options.keep_alive_interval);

        loop {
            tokio::select! {
                biased;

                // Reader: decode message, dispatch to handler
                result = reader.fill_message() => {
                    match result? {
                        FillStatus::Ok => {
                            if let Some(msg) = reader.try_decode()? {
                                self.handler.on_received_message(msg).await?;
                                tokio::task::yield_now().await;
                            }
                        }
                        FillStatus::Eof => break,
                    }
                }

                // Writer: commands from TorrentActor
                Some(cmd) = cmd_rx.recv() => {
                    self.handle_command(cmd, &mut writer).await?;
                }

                // Batch flush timer
                _ = flush_interval.tick() => {
                    // Handler-specific flush logic
                }

                // Keep-alive
                _ = keep_alive.tick() => {
                    writer.send_keep_alive().await?;
                }
            }
        }

        self.handler.on_disconnect("connection closed");
        Ok(())
    }
}
```

### 3. TorrentPeerHandler (`torrent-session/src/torrent_peer_handler.rs`, ~500 lines)

New file. Contains all torrent-specific peer logic extracted from `run_peer()`:

```rust
pub(crate) struct TorrentPeerHandler {
    addr: SocketAddr,
    info_hash: Id20,
    our_bitfield: Bitfield,
    num_pieces: u32,

    // Shared state references
    piece_states: Arc<AtomicPieceStates>,
    disk: Option<DiskHandle>,
    event_tx: mpsc::Sender<PeerEvent>,
    block_maps: Option<Arc<BlockMaps>>,
    steal_candidates: Option<Arc<StealCandidates>>,

    // Per-peer state (moved from run_peer locals)
    dispatch_state: Option<PeerDispatchState>,
    peer_bitfield: Bitfield,
    pending_batch: PendingBatch,
    am_choking: bool,
    am_interested: bool,
    peer_choking: bool,
    peer_interested: bool,
    extensions: Option<ExtensionState>,
}

impl PeerConnectionHandler for TorrentPeerHandler {
    fn on_handshake(&self, handshake: &Handshake) -> crate::Result<()> {
        // Validate info hash, set extension support flags
        ...
    }

    fn on_extended_handshake(&self, ext: &ExtHandshake) -> crate::Result<()> {
        // Record peer's extension IDs, metadata size
        ...
    }

    async fn on_received_message(&mut self, msg: Message<&[u8]>) -> crate::Result<()> {
        match msg {
            Message::Piece { index, begin, data_0, data_1 } => {
                // M110: Direct pwrite from ring buffer slices
                if let Some(ref disk) = self.disk {
                    disk.write_block_vectored(*index, *begin, data_0, data_1)?;
                }
                // Block tracking (M103)
                if let Some(ref bm) = self.block_maps {
                    let block_idx = *begin / DEFAULT_CHUNK_SIZE as u32;
                    bm.mark_received(*index, block_idx);
                }
                // Batch tracking (M92)
                let piece_complete = self.pending_batch.push(*index, *begin, data_0.len() as u32 + data_1.len() as u32);
                if piece_complete {
                    self.flush_batch().await?;
                }
            }
            Message::Choke => {
                self.peer_choking = true;
                // Release dispatch state pieces
                ...
            }
            Message::Unchoke => {
                self.peer_choking = false;
                // Resume requesting
                ...
            }
            Message::Have { index } => {
                self.peer_bitfield.set(*index);
                // Update availability
                ...
            }
            Message::Bitfield(data) => {
                self.peer_bitfield = Bitfield::from_bytes(data.to_vec(), self.num_pieces);
                // Update availability
                ...
            }
            // ... other message types extracted from run_peer
            _ => {}
        }
        Ok(())
    }

    fn should_transmit_have(&self, piece: u32) -> bool {
        // M118 preparation: filter based on peer's bitfield
        !self.peer_bitfield.get(piece)
    }

    fn read_chunk(&self, index: u32, begin: u32, length: u32, buf: &mut [u8]) -> crate::Result<usize> {
        // Read from disk for upload
        if let Some(ref disk) = self.disk {
            disk.read_chunk(index, begin, length, buf)?;
            Ok(length as usize)
        } else {
            Err(crate::Error::NoDisk)
        }
    }

    fn on_disconnect(&mut self, reason: &str) {
        // Release pieces, update stats, send PeerEvent::Disconnected
        ...
    }

    fn addr(&self) -> SocketAddr {
        self.addr
    }
}
```

### 4. Refactor peer.rs (~3375 -> ~1500 lines)

What stays in peer.rs:
- `run_peer()` function signature (public API, unchanged)
- Phase 0: MSE/PE encryption negotiation (~60 lines)
- Phase 1: BEP 3 handshake (~50 lines)
- Phase 2: Extension negotiation dispatch (~30 lines)
- Construction of `TorrentPeerHandler` with all required state
- Construction of `PeerConnection<TorrentPeerHandler>`
- Call to `peer_connection.run(reader, writer, cmd_rx)`
- `PendingBatch` struct definition (~80 lines, could move to handler file)
- Constants (HANDSHAKE_SIZE, BATCH_*, etc.)

What moves to `peer_connection.rs`:
- The select! loop (~300 lines)
- Keep-alive logic
- Command dispatch
- Reader/writer coordination

What moves to `torrent_peer_handler.rs`:
- All message handling (~800 lines)
- Block tracking integration
- Dispatch state management
- PEX handling
- Extension message handling
- Batch flush logic
- Upload logic

### 5. MetadataPeerHandler (optional, ~120 lines)

Simplified handler for metadata-only connections (ut_metadata BEP 9):

```rust
pub(crate) struct MetadataPeerHandler {
    addr: SocketAddr,
    info_hash: Id20,
    metadata_tx: mpsc::Sender<MetadataEvent>,
    extensions: Option<ExtensionState>,
}

impl PeerConnectionHandler for MetadataPeerHandler {
    async fn on_received_message(&mut self, msg: Message<&[u8]>) -> crate::Result<()> {
        // Only handle Extended messages (ut_metadata)
        if let Message::Extended { ext_id, payload } = msg {
            // Parse metadata piece, forward to metadata downloader
            ...
        }
        Ok(())
    }

    fn should_send_bitfield(&self) -> bool { false }
    fn should_transmit_have(&self, _piece: u32) -> bool { false }
    fn read_chunk(&self, ..) -> crate::Result<usize> { Err(crate::Error::NotSeeding) }
    // ... other methods return defaults
}
```

This enables cleaner metadata download flow and avoids polluting the main handler
with metadata-only logic paths.

## Tests

### New Tests (10)

1. **`peer_handler_on_piece_writes_direct`** — Create `TorrentPeerHandler` with mock
   disk. Send a Piece message. Verify `write_block_vectored` was called with
   correct index, begin, and data slices.

2. **`peer_handler_on_choke_releases_pieces`** — Handler has 3 pieces in dispatch
   state. Receive Choke. Verify dispatch state is cleared and `PeerEvent::PieceReleased`
   is sent for each piece.

3. **`peer_handler_should_transmit_have_basic`** — Peer's bitfield has pieces
   0, 2, 4. Verify `should_transmit_have(0)` returns false (peer has it),
   `should_transmit_have(1)` returns true (peer doesn't have it).

4. **`peer_handler_on_disconnect_cleanup`** — Call `on_disconnect`. Verify all
   reserved pieces are released and `PeerEvent::Disconnected` is sent.

5. **`peer_connection_reader_writer_select`** — Create PeerConnection with a mock
   handler. Feed messages to the reader, commands to the writer. Verify both
   arms fire and handler methods are called.

6. **`peer_connection_graceful_shutdown`** — Close the reader (EOF). Verify
   `on_disconnect` is called and the run method returns Ok.

7. **`peer_connection_timeout_handling`** — Don't send any data. Verify the reader
   times out and the connection is closed.

8. **`metadata_handler_ut_metadata_only`** — MetadataPeerHandler receives a
   ut_metadata Data message. Verify it's forwarded to `metadata_tx`. Verify
   Piece messages are ignored (no disk operations).

9. **`trait_dispatch_polymorphism`** — Create both TorrentPeerHandler and
   MetadataPeerHandler. Verify both can be used with `PeerConnection<H>` via
   the trait (compile-time check + runtime behavior test).

10. **`handler_read_chunk_for_seeding`** — TorrentPeerHandler with mock disk
    containing piece data. Call `read_chunk`. Verify correct data is returned
    in the buffer.

### Existing Tests

All 1647+ existing tests must pass. The `run_peer()` public API is unchanged —
it now internally constructs a `PeerConnection<TorrentPeerHandler>` and calls
`run()`. All behavior is preserved.

## Success Criteria

- `peer.rs` reduced from ~3375 to ~1500 lines
- New `peer_connection.rs` ~400 lines
- New `torrent_peer_handler.rs` ~500 lines
- New `peer_handler.rs` ~100 lines
- All 1647+ existing tests pass
- 10 new unit tests pass
- No performance regression (benchmark within 5% of baseline)
- `should_transmit_have()` method available on handler (M118 prerequisite)

## Verification

```bash
# Run all tests
cargo test --workspace

# Run new tests
cargo test -p torrent-session peer_handler_
cargo test -p torrent-session peer_connection_
cargo test -p torrent-session metadata_handler_
cargo test -p torrent-session handler_read_chunk_
cargo test -p torrent-session trait_dispatch_

# Line count verification
wc -l crates/torrent-session/src/peer.rs
wc -l crates/torrent-session/src/peer_connection.rs
wc -l crates/torrent-session/src/torrent_peer_handler.rs
wc -l crates/torrent-session/src/peer_handler.rs

# Benchmark (verify no regression)
./benchmarks/analyze.sh '<arch-iso-magnet>' -n 10

# Clippy
cargo clippy --workspace -- -D warnings
```

## NOT in Scope

| Item | Why | Revisit |
|------|-----|---------|
| Changing the actor model | TorrentActor stays as the single-owner event loop; this refactor only separates peer connection logic from torrent logic | Not planned |
| Lock-based shared state (rqbit pattern) | rqbit uses `DashMap` + `RwLock` for shared torrent state; we keep the actor model with channels | If actor overhead becomes the bottleneck |
| Reducing allocation counts | This is a structural/testability change, not a performance change; alloc reduction is M114-M116 | N/A |
| Removing PeerCommand enum | PeerCommand is still the actor-to-peer channel type; handler trait is for message handling | If handler trait makes commands redundant |
| Async trait object dispatch | Handler is used via monomorphized generics, not `dyn PeerConnectionHandler`; no vtable overhead | If runtime handler switching is needed |
