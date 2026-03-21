//! `PeerConnectionHandler` trait — interface between the protocol loop and
//! message handling logic.
//!
//! The trait is designed around the ring-buffer lifecycle:
//! - **Sync** methods (`on_piece_sync`, `on_choke`, etc.) operate while the
//!   ring buffer is still borrowed — they must not `.await`.
//! - **Async** methods (`on_message`, `on_command`, `on_disconnect`) run after
//!   the ring buffer has been advanced, using owned data.

use std::net::SocketAddr;
use std::sync::Arc;

use torrent_wire::Message;

use crate::peer_codec::PeerWriter;
use crate::piece_reservation::AvailabilitySnapshot;
use crate::types::{BlockEntry, PeerCommand};

/// Result of the synchronous piece fast-path.
pub(crate) enum PieceAction {
    /// Block was written to disk and accumulated in the batch.
    Written {
        /// If true, the batch contains a complete piece and should be flushed
        /// immediately (before the next await point).
        flush_now: bool,
    },
    /// No reservation state — the piece message should fall through to
    /// `on_message()` for event-channel delivery.
    Unhandled,
}

/// Trait abstracting the per-peer message handling logic away from the
/// transport-level select! loop.
///
/// `PeerConnection<H>` is generic over `H: PeerConnectionHandler`, giving us
/// monomorphisation (zero vtable overhead) while keeping the connection loop
/// and the handler testable in isolation.
#[allow(dead_code)] // Methods like apply_pending_snapshot, should_transmit_have, in_flight are M118 prerequisites.
pub(crate) trait PeerConnectionHandler: Send + 'static {
    // --- Piece fast-path (SYNC — ring buffer borrowed) ---

    /// Handle a Piece message while the ring buffer is still borrowed.
    ///
    /// Returns `PieceAction::Written` if the block was dispatched to disk,
    /// or `PieceAction::Unhandled` if there is no reservation state and the
    /// message should be forwarded to `on_message`.
    fn on_piece_sync(
        &mut self,
        index: u32,
        begin: u32,
        data_0: &[u8],
        data_1: &[u8],
    ) -> crate::Result<PieceAction>;

    /// Take the accumulated batch of block completions for flushing.
    fn take_batch(&mut self) -> Option<Vec<BlockEntry>>;

    /// Returns `true` when the batch has pending (unflushed) block entries.
    fn has_pending_batch(&self) -> bool;

    // --- Non-piece messages (ASYNC — owned, ring buffer released) ---

    /// Handle a non-piece message (already converted to owned).
    ///
    /// Returns `Ok(Some(response))` when a wire response should be sent.
    fn on_message(
        &mut self,
        msg: Message,
    ) -> impl std::future::Future<Output = crate::Result<Option<Message>>> + Send;

    // --- Commands from actor (ASYNC — needs writer for some commands) ---

    /// Handle a command from the `TorrentActor`.
    ///
    /// Returns `true` if the peer loop should continue, `false` on fatal
    /// write error (caller should break with the error string).
    fn on_command<W: tokio::io::AsyncWrite + Unpin + Send>(
        &mut self,
        cmd: PeerCommand,
        writer: &mut PeerWriter<W>,
    ) -> impl std::future::Future<Output = crate::Result<()>> + Send;

    // --- State intercepts (SYNC — called from select! loop before on_message) ---

    /// Peer sent Unchoke. Clears choking state and resets the semaphore.
    fn on_unchoke(&mut self);

    /// Peer sent Choke. Returns the released piece index (if any) so the
    /// connection loop can send `PieceReleased` asynchronously.
    fn on_choke(&mut self) -> Option<u32>;

    /// Peer sent RejectRequest. Decrements in-flight and returns a permit.
    fn on_reject_request(&mut self);

    /// Update the local bitfield copy from Have/Bitfield/HaveAll/HaveNone.
    fn on_bitfield_update(&mut self, msg: &Message<&[u8]>);

    // --- Dispatch ---

    /// Whether the dispatch arm should be active (unchoked + has state).
    fn can_request(&self) -> bool;

    /// Get the next block to request. Returns `(piece, begin, length)`.
    fn next_block(&mut self) -> Option<(u32, u32, u32)>;

    /// Apply any pending availability snapshot update.
    fn apply_pending_snapshot(&mut self);

    /// The per-peer semaphore that gates request pipelining.
    /// Returns an Arc so the connection loop can hold it independently
    /// without borrowing the handler.
    fn semaphore(&self) -> std::sync::Arc<tokio::sync::Semaphore>;

    /// The notify handle used to wake the dispatch arm when new pieces are
    /// available. `None` if no reservation state exists.
    fn piece_notify(&self) -> Option<std::sync::Arc<tokio::sync::Notify>>;

    // --- Magnet phase ---

    /// Whether bitfield messages should be deferred (num_pieces == 0).
    fn should_defer_bitfield(&self) -> bool;

    /// Store a deferred bitfield for later replay.
    fn defer_bitfield(&mut self, msg: &Message<&[u8]>);

    // --- Lifecycle ---

    /// Whether a Have message for this piece should be sent on the wire.
    /// Always returns `true` for now (M118 will add super-seed filtering).
    fn should_transmit_have(&self, piece: u32) -> bool;

    /// Post-loop cleanup: release held piece, flush batch, notify plugins,
    /// send Disconnected event.
    fn on_disconnect(
        &mut self,
        reason: Option<String>,
    ) -> impl std::future::Future<Output = ()> + Send;

    /// The remote peer address (used for logging in the connection loop).
    fn addr(&self) -> SocketAddr;

    // --- Commands handled inline by the connection loop ---

    /// Handle `StartRequesting` — sets up reservation, dispatch, and batch state.
    #[allow(clippy::too_many_arguments)]
    fn on_start_requesting(
        &mut self,
        atomic_states: Arc<crate::piece_reservation::AtomicPieceStates>,
        availability_snapshot: Arc<AvailabilitySnapshot>,
        piece_notify: Arc<tokio::sync::Notify>,
        disk_handle: Option<crate::disk::DiskHandle>,
        write_error_tx: tokio::sync::mpsc::Sender<crate::disk::DiskWriteError>,
        lengths: torrent_core::Lengths,
        block_maps: Option<Arc<crate::piece_reservation::BlockMaps>>,
        steal_candidates: Option<Arc<crate::piece_reservation::StealCandidates>>,
        piece_write_guards: Option<Arc<crate::piece_reservation::PieceWriteGuards>>,
    );

    /// Handle `UpdateNumPieces` — resizes bitfield and replays deferred
    /// bitfield. Returns an optional replayed Bitfield event message to send.
    fn on_update_num_pieces(
        &mut self,
        n: u32,
    ) -> impl std::future::Future<Output = Option<Message>> + Send;

    /// Handle `SnapshotUpdate` — queues the snapshot for the next dispatch
    /// iteration.
    fn on_snapshot_update(&mut self, snapshot: Arc<AvailabilitySnapshot>);

    /// Current in-flight request count (for the connection loop's pipeline
    /// tracking).
    fn in_flight(&self) -> usize;

    /// Increment in-flight count after sending a Request message.
    fn inc_in_flight(&mut self);
}
