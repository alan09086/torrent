//! `TorrentPeerHandler` — concrete implementation of `PeerConnectionHandler`
//! for the standard BitTorrent peer protocol.
//!
//! This struct owns all per-peer state that was previously local variables in
//! the `run_peer()` select! loop, plus all the message-handling functions that
//! were free functions (`handle_message`, `handle_command`, etc.).

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use torrent_core::{Id20, Lengths};
use torrent_storage::Bitfield;
use torrent_wire::{ExtHandshake, Message, MetadataMessage, MetadataMessageType};

use crate::disk::{DiskHandle, DiskWriteError};
use crate::peer_codec::PeerWriter;
use crate::peer_handler::{PeerConnectionHandler, PieceAction};
use crate::pex::PexMessage;
use crate::piece_reservation::{
    AtomicPieceStates, AvailabilitySnapshot, BlockMaps, PeerDispatchState, PieceWriteGuards,
    StealCandidates,
};
use crate::types::{BlockEntry, PeerCommand, PeerEvent};

// ---------------------------------------------------------------------------
// Constants (moved from peer.rs)
// ---------------------------------------------------------------------------

/// M92: Initial capacity for `PendingBatch` block buffer.
/// Matches a standard 512 KiB piece (512 * 1024 / 16384 = 32 blocks).
const BATCH_INITIAL_CAPACITY: usize = 32;

/// M104: Fixed per-peer queue depth — number of concurrent requests per peer.
/// Replaces AIMD dynamic depth. Matches `fixed_pipeline_depth` setting default.
const INITIAL_QUEUE_DEPTH: usize = 128;

// ---------------------------------------------------------------------------
// ReservationState type alias (moved from peer.rs)
// ---------------------------------------------------------------------------

/// M93: State needed for lock-free dispatch and direct peer-to-disk writes.
/// Tuple: (atomic_states, piece_notify, disk_handle, write_error_tx)
type ReservationState = Option<(
    Arc<AtomicPieceStates>,
    Arc<tokio::sync::Notify>,
    Option<DiskHandle>,
    mpsc::Sender<DiskWriteError>,
)>;

// ---------------------------------------------------------------------------
// PendingBatch (moved from peer.rs)
// ---------------------------------------------------------------------------

/// M92: Accumulates block completions for batched delivery to TorrentActor.
/// Lives in the peer task's stack — no shared state, no locking.
struct PendingBatch {
    /// Block completions awaiting flush.
    blocks: Vec<BlockEntry>,
    /// Per-piece block count for detecting piece completion.
    /// Key: piece index, Value: blocks written so far in this batch.
    piece_counts: rustc_hash::FxHashMap<u32, u32>,
    /// Piece/chunk arithmetic for completion detection.
    lengths: Lengths,
}

impl PendingBatch {
    fn new(lengths: &Lengths) -> Self {
        Self {
            blocks: Vec::with_capacity(BATCH_INITIAL_CAPACITY),
            piece_counts: rustc_hash::FxHashMap::default(),
            lengths: lengths.clone(),
        }
    }

    /// Record a block write. Returns true if the piece is now complete
    /// (caller should flush immediately).
    fn push(&mut self, index: u32, begin: u32, length: u32) -> bool {
        self.blocks.push(BlockEntry {
            index,
            begin,
            length,
        });
        let count = self.piece_counts.entry(index).or_insert(0);
        *count += 1;
        *count >= self.lengths.chunks_in_piece(index)
    }

    /// Take accumulated blocks for sending. Resets internal state.
    /// Pre-allocates the replacement Vec to avoid reallocation on the next batch cycle.
    fn take(&mut self) -> Vec<BlockEntry> {
        self.piece_counts.clear(); // retains HashMap capacity for reuse
        std::mem::replace(&mut self.blocks, Vec::with_capacity(BATCH_INITIAL_CAPACITY))
    }

    fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

// ---------------------------------------------------------------------------
// DeferredBitfield (moved from peer.rs)
// ---------------------------------------------------------------------------

/// Deferred bitfield: when we receive Bitfield/HaveAll/HaveNone before
/// metadata assembly (num_pieces == 0), store it and replay after
/// UpdateNumPieces arrives with the real piece count.
#[derive(Debug)]
enum DeferredBitfield {
    Raw(Vec<u8>),
    HaveAll,
    HaveNone,
}

// ---------------------------------------------------------------------------
// ExtensionState (new — Decision 3A)
// ---------------------------------------------------------------------------

/// Groups all extension ID fields that were previously separate locals in
/// `run_peer()`. Reduces parameter passing to `handle_message()`.
struct ExtensionState {
    // IDs from the remote's ext handshake (used when SENDING to them)
    peer_ut_metadata: Option<u8>,
    peer_ut_pex: Option<u8>,
    peer_lt_trackers: Option<u8>,
    peer_ut_holepunch: Option<u8>,
    // IDs from OUR ext handshake (used for matching INCOMING messages)
    our_ut_metadata: Option<u8>,
    our_ut_pex: Option<u8>,
    our_lt_trackers: Option<u8>,
    our_ut_holepunch: Option<u8>,
    // Peer's extension IDs for plugin names (for sending responses using their IDs)
    peer_ext_ids: std::collections::HashMap<String, u8>,
}

// ---------------------------------------------------------------------------
// TorrentPeerHandler
// ---------------------------------------------------------------------------

/// Concrete handler implementing `PeerConnectionHandler` for the standard
/// BitTorrent peer protocol. Owns all per-peer state and message-handling
/// logic.
pub(crate) struct TorrentPeerHandler {
    // Extension state
    ext_state: ExtensionState,

    // Choke/dispatch state
    peer_choking: bool,
    reservation_state: ReservationState,
    dispatch_state: Option<PeerDispatchState>,
    pending_snapshot: Option<Arc<AvailabilitySnapshot>>,

    // Block maps for steal visibility
    peer_block_maps: Option<Arc<BlockMaps>>,
    /// M120: Per-piece write guards to prevent steal/write races.
    piece_write_guards: Option<Arc<PieceWriteGuards>>,
    block_length: u32,

    // Local copy of the peer's bitfield (avoids shared state lookup)
    local_bitfield: Bitfield,

    // Pipeline state
    in_flight: usize,
    current_effective_depth: usize,
    semaphore: Arc<Semaphore>,

    // Batch state
    pending_batch: Option<PendingBatch>,

    // Magnet phase
    deferred_bitfield: Option<DeferredBitfield>,
    num_pieces: u32,

    // Connection identity
    addr: SocketAddr,
    event_tx: mpsc::Sender<PeerEvent>,

    // Protocol state
    enable_fast: bool,
    info_hash: Id20,
    info_bytes: Option<Bytes>,
    plugins: Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>>,
}

impl TorrentPeerHandler {
    /// Construct a new handler with all the state that was previously
    /// local variables in `run_peer()`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        addr: SocketAddr,
        num_pieces: u32,
        event_tx: mpsc::Sender<PeerEvent>,
        enable_fast: bool,
        info_hash: Id20,
        info_bytes: Option<Bytes>,
        plugins: Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>>,
        our_ut_metadata: Option<u8>,
        our_ut_pex: Option<u8>,
        our_lt_trackers: Option<u8>,
        our_ut_holepunch: Option<u8>,
    ) -> Self {
        Self {
            ext_state: ExtensionState {
                peer_ut_metadata: None,
                peer_ut_pex: None,
                peer_lt_trackers: None,
                peer_ut_holepunch: None,
                our_ut_metadata,
                our_ut_pex,
                our_lt_trackers,
                our_ut_holepunch,
                peer_ext_ids: std::collections::HashMap::new(),
            },
            peer_choking: true,
            reservation_state: None,
            dispatch_state: None,
            pending_snapshot: None,
            peer_block_maps: None,
            piece_write_guards: None,
            block_length: torrent_core::DEFAULT_CHUNK_SIZE,
            local_bitfield: Bitfield::new(num_pieces),
            in_flight: 0,
            current_effective_depth: INITIAL_QUEUE_DEPTH,
            semaphore: Arc::new(Semaphore::new(0)),
            pending_batch: None,
            deferred_bitfield: None,
            num_pieces,
            addr,
            event_tx,
            enable_fast,
            info_hash,
            info_bytes,
            plugins,
        }
    }

    // -----------------------------------------------------------------------
    // Message handlers (moved from free functions in peer.rs)
    // -----------------------------------------------------------------------

    /// Handle an incoming message from the remote peer.
    ///
    /// Returns `Ok(Some(msg))` when a response message should be sent on the wire.
    async fn handle_message(&mut self, msg: Message) -> crate::Result<Option<Message>> {
        let addr = self.addr;
        let num_pieces = self.num_pieces;
        let both_support_fast = self.enable_fast; // only true if BOTH sides support fast
        let ext = &mut self.ext_state;

        match msg {
            Message::Choke => {
                self.event_tx
                    .send(PeerEvent::PeerChoking {
                        peer_addr: addr,
                        choking: true,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
            Message::Unchoke => {
                self.event_tx
                    .send(PeerEvent::PeerChoking {
                        peer_addr: addr,
                        choking: false,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
            Message::Interested => {
                self.event_tx
                    .send(PeerEvent::PeerInterested {
                        peer_addr: addr,
                        interested: true,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
            Message::NotInterested => {
                self.event_tx
                    .send(PeerEvent::PeerInterested {
                        peer_addr: addr,
                        interested: false,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
            Message::Have { index } => {
                self.event_tx
                    .send(PeerEvent::Have {
                        peer_addr: addr,
                        index,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
            Message::Bitfield(data) => {
                let bitfield = Bitfield::from_bytes(data.to_vec(), num_pieces)?;
                self.event_tx
                    .send(PeerEvent::Bitfield {
                        peer_addr: addr,
                        bitfield,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
            Message::Piece {
                index,
                begin,
                data_0,
                data_1,
            } => {
                // Concatenate the two ring-buffer slices into a single Bytes for
                // the event channel.  In the common case data_1 is empty, so we
                // avoid the allocation.
                let data = if data_1.is_empty() {
                    data_0
                } else {
                    let mut combined = BytesMut::with_capacity(data_0.len() + data_1.len());
                    combined.extend_from_slice(&data_0);
                    combined.extend_from_slice(&data_1);
                    combined.freeze()
                };
                self.event_tx
                    .send(PeerEvent::PieceData {
                        peer_addr: addr,
                        index,
                        begin,
                        data,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
            Message::Extended { ext_id: 0, payload } => {
                // Extension handshake
                let ext_hs = ExtHandshake::from_bytes(&payload)?;
                ext.peer_ut_metadata = ext_hs.ext_id("ut_metadata");
                ext.peer_ut_pex = ext_hs.ext_id("ut_pex");
                ext.peer_lt_trackers = ext_hs.ext_id("lt_trackers");
                ext.peer_ut_holepunch = ext_hs.ext_id("ut_holepunch");
                // Capture peer's extension IDs for registered plugins
                for plugin in self.plugins.iter() {
                    if let Some(id) = ext_hs.ext_id(plugin.name()) {
                        ext.peer_ext_ids.insert(plugin.name().to_string(), id);
                    }
                }
                // Notify plugins of the peer's extension handshake
                for plugin in self.plugins.iter() {
                    plugin.on_handshake(&self.info_hash, addr, &ext_hs);
                }
                self.event_tx
                    .send(PeerEvent::ExtHandshake {
                        peer_addr: addr,
                        handshake: ext_hs,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
            Message::Extended { ext_id, payload } => {
                // Routed extension message: the remote sends using OUR assigned IDs
                if Some(ext_id) == ext.our_ut_metadata {
                    let response = self.handle_metadata_message(&payload).await?;
                    if let Some(msg) = response {
                        return Ok(Some(msg));
                    }
                } else if Some(ext_id) == ext.our_ut_pex {
                    Self::handle_pex_message(&payload, &self.event_tx).await?;
                } else if Some(ext_id) == ext.our_lt_trackers {
                    Self::handle_lt_trackers_message(&payload, &self.event_tx).await?;
                } else if Some(ext_id) == ext.our_ut_holepunch {
                    Self::handle_holepunch_message(addr, &payload, &self.event_tx).await?;
                } else {
                    // Dispatch to registered plugins (IDs 10+)
                    let mut handled = false;
                    for (i, plugin) in self.plugins.iter().enumerate() {
                        let our_plugin_id = 10 + i as u8;
                        if ext_id == our_plugin_id {
                            if let Some(response) =
                                plugin.on_message(&self.info_hash, addr, &payload)
                                && let Some(&peer_id) = ext.peer_ext_ids.get(plugin.name())
                            {
                                return Ok(Some(Message::Extended {
                                    ext_id: peer_id,
                                    payload: Bytes::from(response),
                                }));
                            }
                            handled = true;
                            break;
                        }
                    }
                    if !handled {
                        warn!(%addr, ext_id, "unknown extension message");
                    }
                }
            }
            Message::Request {
                index,
                begin,
                length,
            } => {
                self.event_tx
                    .send(PeerEvent::IncomingRequest {
                        peer_addr: addr,
                        index,
                        begin,
                        length,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
            Message::KeepAlive | Message::Cancel { .. } | Message::Port(_) => {
                // Ignored
            }
            Message::HaveAll => {
                if both_support_fast {
                    let mut bitfield = Bitfield::new(num_pieces);
                    for i in 0..num_pieces {
                        bitfield.set(i);
                    }
                    self.event_tx
                        .send(PeerEvent::Bitfield {
                            peer_addr: addr,
                            bitfield,
                        })
                        .await
                        .map_err(|_| crate::Error::Shutdown)?;
                }
            }
            Message::HaveNone => {
                if both_support_fast {
                    let bitfield = Bitfield::new(num_pieces);
                    self.event_tx
                        .send(PeerEvent::Bitfield {
                            peer_addr: addr,
                            bitfield,
                        })
                        .await
                        .map_err(|_| crate::Error::Shutdown)?;
                }
            }
            Message::SuggestPiece(index) => {
                self.event_tx
                    .send(PeerEvent::SuggestPiece {
                        peer_addr: addr,
                        index,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
            Message::RejectRequest {
                index,
                begin,
                length,
            } => {
                self.event_tx
                    .send(PeerEvent::RejectRequest {
                        peer_addr: addr,
                        index,
                        begin,
                        length,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
            Message::AllowedFast(index) => {
                self.event_tx
                    .send(PeerEvent::AllowedFast {
                        peer_addr: addr,
                        index,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
            Message::HashRequest {
                pieces_root,
                base,
                index,
                count,
                proof_layers,
            } => {
                let request = torrent_core::HashRequest {
                    file_root: pieces_root,
                    base,
                    index,
                    count,
                    proof_layers,
                };
                self.event_tx
                    .send(PeerEvent::IncomingHashRequest {
                        peer_addr: addr,
                        request,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
            Message::Hashes {
                pieces_root,
                base,
                index,
                count,
                proof_layers,
                hashes,
            } => {
                let request = torrent_core::HashRequest {
                    file_root: pieces_root,
                    base,
                    index,
                    count,
                    proof_layers,
                };
                self.event_tx
                    .send(PeerEvent::HashesReceived {
                        peer_addr: addr,
                        request,
                        hashes,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
            Message::HashReject {
                pieces_root,
                base,
                index,
                count,
                proof_layers,
            } => {
                let request = torrent_core::HashRequest {
                    file_root: pieces_root,
                    base,
                    index,
                    count,
                    proof_layers,
                };
                self.event_tx
                    .send(PeerEvent::HashRequestRejected {
                        peer_addr: addr,
                        request,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
        }
        Ok(None)
    }

    /// Handle an incoming ut_metadata extension message.
    async fn handle_metadata_message(&self, payload: &[u8]) -> crate::Result<Option<Message>> {
        let meta_msg = MetadataMessage::from_bytes(payload)?;
        match meta_msg.msg_type {
            MetadataMessageType::Data => {
                if let (Some(data), Some(total_size)) = (meta_msg.data, meta_msg.total_size) {
                    self.event_tx
                        .send(PeerEvent::MetadataPiece {
                            peer_addr: self.addr,
                            piece: meta_msg.piece,
                            data,
                            total_size,
                        })
                        .await
                        .map_err(|_| crate::Error::Shutdown)?;
                }
            }
            MetadataMessageType::Reject => {
                self.event_tx
                    .send(PeerEvent::MetadataReject {
                        peer_addr: self.addr,
                        piece: meta_msg.piece,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
            MetadataMessageType::Request => {
                if let Some(remote_id) = self.ext_state.peer_ut_metadata {
                    let response =
                        Self::build_metadata_response(meta_msg.piece, self.info_bytes.as_ref());
                    let payload = response.to_bytes().map_err(crate::Error::Wire)?;
                    return Ok(Some(Message::Extended {
                        ext_id: remote_id,
                        payload,
                    }));
                }
            }
        }
        Ok(None)
    }

    /// Build a BEP 9 metadata response (data or reject) for the given piece index.
    fn build_metadata_response(piece: u32, info_bytes: Option<&Bytes>) -> MetadataMessage {
        const METADATA_PIECE_SIZE: usize = 16384;

        let Some(info) = info_bytes else {
            return MetadataMessage::reject(piece);
        };

        let offset = piece as usize * METADATA_PIECE_SIZE;
        if offset >= info.len() {
            return MetadataMessage::reject(piece);
        }

        let end = (offset + METADATA_PIECE_SIZE).min(info.len());
        let chunk = info.slice(offset..end);
        MetadataMessage::data(piece, info.len() as u64, chunk)
    }

    /// Handle an incoming ut_pex extension message.
    async fn handle_pex_message(
        payload: &[u8],
        event_tx: &mpsc::Sender<PeerEvent>,
    ) -> crate::Result<()> {
        let pex = PexMessage::from_bytes(payload)?;
        let mut new_peers = pex.added_peers();
        new_peers.extend(pex.added_peers6());
        if !new_peers.is_empty() {
            event_tx
                .send(PeerEvent::PexPeers { new_peers })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        Ok(())
    }

    /// Handle an incoming lt_trackers extension message.
    async fn handle_lt_trackers_message(
        payload: &[u8],
        event_tx: &mpsc::Sender<PeerEvent>,
    ) -> crate::Result<()> {
        let msg = crate::lt_trackers::LtTrackersMessage::from_bytes(payload)?;
        if !msg.added.is_empty() {
            event_tx
                .send(PeerEvent::TrackersReceived {
                    tracker_urls: msg.added,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        Ok(())
    }

    /// Handle an incoming ut_holepunch extension message (BEP 55).
    async fn handle_holepunch_message(
        addr: SocketAddr,
        payload: &[u8],
        event_tx: &mpsc::Sender<PeerEvent>,
    ) -> crate::Result<()> {
        use torrent_wire::{HolepunchMessage, HolepunchMsgType};
        let msg = HolepunchMessage::from_bytes(payload)?;
        match msg.msg_type {
            HolepunchMsgType::Rendezvous => {
                event_tx
                    .send(PeerEvent::HolepunchRendezvous {
                        peer_addr: addr,
                        target: msg.addr,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
            HolepunchMsgType::Connect => {
                event_tx
                    .send(PeerEvent::HolepunchConnect {
                        peer_addr: addr,
                        target: msg.addr,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
            HolepunchMsgType::Error => {
                event_tx
                    .send(PeerEvent::HolepunchError {
                        peer_addr: addr,
                        target: msg.addr,
                        error_code: msg.error_code,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
        }
        Ok(())
    }

    /// Handle a command from the TorrentActor — translates PeerCommand to
    /// wire Message and sends it.
    async fn handle_command<W: tokio::io::AsyncWrite + Unpin + Send>(
        &mut self,
        cmd: PeerCommand,
        writer: &mut PeerWriter<W>,
    ) -> crate::Result<()> {
        let ext = &self.ext_state;
        let msg = match cmd {
            PeerCommand::Request {
                index,
                begin,
                length,
            } => Message::Request {
                index,
                begin,
                length,
            },
            PeerCommand::Cancel {
                index,
                begin,
                length,
            } => Message::Cancel {
                index,
                begin,
                length,
            },
            PeerCommand::SetChoking(choking) => {
                if choking {
                    Message::Choke
                } else {
                    Message::Unchoke
                }
            }
            PeerCommand::SetInterested(interested) => {
                if interested {
                    Message::Interested
                } else {
                    Message::NotInterested
                }
            }
            PeerCommand::Have(index) => Message::Have { index },
            PeerCommand::RequestMetadata { piece } => {
                let ext_id = ext.peer_ut_metadata.ok_or_else(|| {
                    crate::Error::Connection("peer does not support ut_metadata".into())
                })?;
                let payload = MetadataMessage::request(piece)
                    .to_bytes()
                    .map_err(crate::Error::Wire)?;
                Message::Extended { ext_id, payload }
            }
            PeerCommand::RejectRequest {
                index,
                begin,
                length,
            } => Message::RejectRequest {
                index,
                begin,
                length,
            },
            PeerCommand::AllowedFast(index) => Message::AllowedFast(index),
            PeerCommand::SendPiece { index, begin, data } => Message::Piece {
                index,
                begin,
                data_0: data,
                data_1: Bytes::new(),
            },
            PeerCommand::SendExtHandshake(hs) => {
                let payload = hs.to_bytes().map_err(crate::Error::Wire)?;
                Message::Extended { ext_id: 0, payload }
            }
            PeerCommand::SuggestPiece(index) => Message::SuggestPiece(index),
            PeerCommand::SendHashRequest(req) => Message::HashRequest {
                pieces_root: req.file_root,
                base: req.base,
                index: req.index,
                count: req.count,
                proof_layers: req.proof_layers,
            },
            PeerCommand::SendHashes { request, hashes } => Message::Hashes {
                pieces_root: request.file_root,
                base: request.base,
                index: request.index,
                count: request.count,
                proof_layers: request.proof_layers,
                hashes,
            },
            PeerCommand::SendHashReject(req) => Message::HashReject {
                pieces_root: req.file_root,
                base: req.base,
                index: req.index,
                count: req.count,
                proof_layers: req.proof_layers,
            },
            PeerCommand::SendPex { message } => {
                if let Some(ext_id) = ext.peer_ut_pex {
                    let payload = message
                        .to_bytes()
                        .map_err(|e| crate::Error::Connection(e.to_string()))?;
                    writer.send(&Message::Extended { ext_id, payload }).await?;
                }
                return Ok(());
            }
            PeerCommand::SendHolepunch(hp_msg) => {
                if let Some(ext_id) = ext.peer_ut_holepunch {
                    let payload = hp_msg.to_bytes();
                    writer.send(&Message::Extended { ext_id, payload }).await?;
                }
                return Ok(());
            }
            PeerCommand::StartRequesting { .. } | PeerCommand::SnapshotUpdate { .. } => {
                // Handled in connection loop, not here
                return Ok(());
            }
            PeerCommand::UpdateNumPieces(_) | PeerCommand::Shutdown => {
                // Should have been handled in the connection loop; this is unreachable.
                return Ok(());
            }
        };
        writer.send(&msg).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PeerConnectionHandler implementation
// ---------------------------------------------------------------------------

impl PeerConnectionHandler for TorrentPeerHandler {
    fn on_piece_sync(
        &mut self,
        index: u32,
        begin: u32,
        data_0: &[u8],
        data_1: &[u8],
    ) -> crate::Result<PieceAction> {
        let data_len = (data_0.len() + data_1.len()) as u32;

        // M75: Permit management for pipeline depth.
        if self.reservation_state.is_some() {
            self.in_flight = self.in_flight.saturating_sub(1);
            // M82: Permit absorption — only return permit if below target depth
            if self.in_flight < self.current_effective_depth {
                self.semaphore.add_permits(1);
            }
        }

        // M103: Track received blocks in shared BlockMaps for steal visibility.
        if let Some(ref bm) = self.peer_block_maps {
            let block_idx = begin / self.block_length;
            bm.mark_received(index, block_idx);
        }

        // M120: Acquire per-piece read guard to prevent steal during write.
        // The guard is held for the duration of the pwrite and dropped after.
        let _write_guard = self
            .piece_write_guards
            .as_ref()
            .and_then(|pwg| pwg.read(index));

        // M110: Direct pwrite from ring buffer slices — no clone, no channel.
        if let Some((_, _, Some(ref disk), ref write_err_tx)) = self.reservation_state
            && let Err(e) = disk.write_block_direct(index, begin, data_0, data_1)
        {
            // Report write error to TorrentActor (same pattern as deferred path).
            let _ = write_err_tx.try_send(DiskWriteError {
                piece: index,
                begin,
                error: match e {
                    crate::Error::Storage(se) => se,
                    other => torrent_storage::Error::Io(std::io::Error::other(other.to_string())),
                },
            });
        }

        // M92: Accumulate block metadata in PendingBatch.
        // Flush immediately on piece completion.
        if self
            .reservation_state
            .as_ref()
            .is_some_and(|(_, _, d, _)| d.is_some())
        {
            if let Some(ref mut batch) = self.pending_batch {
                let piece_complete = batch.push(index, begin, data_len);
                return Ok(PieceAction::Written {
                    flush_now: piece_complete,
                });
            }
            // No batch yet (StartRequesting not received) — signal single block.
            return Ok(PieceAction::Written { flush_now: true });
        }

        // Piece without reservation — fall through to on_message.
        Ok(PieceAction::Unhandled)
    }

    fn take_batch(&mut self) -> Option<Vec<BlockEntry>> {
        self.pending_batch.as_mut().map(PendingBatch::take)
    }

    fn has_pending_batch(&self) -> bool {
        self.pending_batch.as_ref().is_some_and(|b| !b.is_empty())
    }

    async fn on_message(&mut self, msg: Message) -> crate::Result<Option<Message>> {
        self.handle_message(msg).await
    }

    async fn on_command<W: tokio::io::AsyncWrite + Unpin + Send>(
        &mut self,
        cmd: PeerCommand,
        writer: &mut PeerWriter<W>,
    ) -> crate::Result<()> {
        self.handle_command(cmd, writer).await
    }

    fn on_unchoke(&mut self) {
        self.peer_choking = false;
        self.in_flight = 0;
        // M82: Drain stale permits from previous cycle before refilling.
        while let Ok(permit) = self.semaphore.try_acquire() {
            permit.forget();
        }
        self.semaphore.add_permits(self.current_effective_depth);
    }

    fn on_choke(&mut self) -> Option<u32> {
        self.peer_choking = true;
        // M93: Release current piece back to Available via atomic store
        if let Some(ref mut ds) = self.dispatch_state {
            if let Some(piece) = ds.current_piece_index() {
                if let Some((ref atomic_states, ..)) = self.reservation_state {
                    atomic_states.release(piece);
                }
                ds.clear_current_piece();
                return Some(piece);
            }
            ds.clear_current_piece();
        }
        None
    }

    fn on_reject_request(&mut self) {
        if self.reservation_state.is_some() {
            self.in_flight = self.in_flight.saturating_sub(1);
            // M82: Permit absorption — same logic as Piece handler
            if self.in_flight < self.current_effective_depth {
                self.semaphore.add_permits(1);
            }
        }
    }

    fn on_bitfield_update(&mut self, msg: &Message<&[u8]>) {
        match msg {
            Message::Bitfield(data) => {
                if let Ok(bf) = Bitfield::from_bytes(data.to_vec(), self.num_pieces) {
                    self.local_bitfield = bf;
                }
            }
            Message::Have { index } => {
                self.local_bitfield.set(*index);
            }
            Message::HaveAll => {
                self.local_bitfield = Bitfield::new(self.num_pieces);
                for i in 0..self.num_pieces {
                    self.local_bitfield.set(i);
                }
            }
            Message::HaveNone => {
                self.local_bitfield = Bitfield::new(self.num_pieces);
            }
            _ => {}
        }
    }

    fn can_request(&self) -> bool {
        !self.peer_choking && self.dispatch_state.is_some() && self.reservation_state.is_some()
    }

    fn next_block(&mut self) -> Option<(u32, u32, u32)> {
        let ds = self.dispatch_state.as_mut()?;
        let (atomic_states, ..) = self.reservation_state.as_ref()?;

        // Check for snapshot updates from actor
        if let Some(new_snap) = self.pending_snapshot.take() {
            ds.update_snapshot(new_snap);
        }

        ds.next_block(&self.local_bitfield, atomic_states)
            .map(|block| (block.piece, block.begin, block.length))
    }

    fn apply_pending_snapshot(&mut self) {
        if let Some(new_snap) = self.pending_snapshot.take()
            && let Some(ref mut ds) = self.dispatch_state
        {
            ds.update_snapshot(new_snap);
        }
    }

    fn semaphore(&self) -> Arc<Semaphore> {
        Arc::clone(&self.semaphore)
    }

    fn piece_notify(&self) -> Option<Arc<tokio::sync::Notify>> {
        self.reservation_state
            .as_ref()
            .map(|(_, notify, ..)| Arc::clone(notify))
    }

    fn should_defer_bitfield(&self) -> bool {
        self.num_pieces == 0
    }

    fn defer_bitfield(&mut self, msg: &Message<&[u8]>) {
        match msg {
            Message::Bitfield(data) => {
                debug!(addr = %self.addr, data_len = data.len(), "deferring bitfield (num_pieces=0)");
                self.deferred_bitfield = Some(DeferredBitfield::Raw(data.to_vec()));
            }
            Message::HaveAll => {
                debug!(addr = %self.addr, "deferring HaveAll (num_pieces=0)");
                self.deferred_bitfield = Some(DeferredBitfield::HaveAll);
            }
            Message::HaveNone => {
                debug!(addr = %self.addr, "deferring HaveNone (num_pieces=0)");
                self.deferred_bitfield = Some(DeferredBitfield::HaveNone);
            }
            _ => {}
        }
    }

    fn should_transmit_have(&self, piece: u32) -> bool {
        // M118: skip Have if the remote peer already has this piece.
        !self.local_bitfield.get(piece)
    }

    async fn on_disconnect(&mut self, reason: Option<String>) {
        // M93: Release held piece on disconnect
        if let Some(ref ds) = self.dispatch_state
            && let Some(piece) = ds.current_piece_index()
            && let Some((ref atomic_states, ..)) = self.reservation_state
        {
            atomic_states.release(piece);
        }

        // Notify plugins that a peer disconnected
        for plugin in self.plugins.iter() {
            plugin.on_peer_disconnected(&self.info_hash, self.addr);
        }

        // M92: Flush remaining blocks before disconnect
        if let Some(ref mut batch) = self.pending_batch
            && !batch.is_empty()
        {
            let blocks = batch.take();
            let _ = self
                .event_tx
                .send(PeerEvent::PieceBlocksBatch {
                    peer_addr: self.addr,
                    blocks,
                })
                .await;
        }

        // Send disconnect event (best-effort)
        let _ = self
            .event_tx
            .send(PeerEvent::Disconnected {
                peer_addr: self.addr,
                reason,
            })
            .await;
    }

    fn addr(&self) -> SocketAddr {
        self.addr
    }

    fn on_start_requesting(
        &mut self,
        atomic_states: Arc<AtomicPieceStates>,
        availability_snapshot: Arc<AvailabilitySnapshot>,
        piece_notify: Arc<tokio::sync::Notify>,
        disk_handle: Option<DiskHandle>,
        write_error_tx: mpsc::Sender<DiskWriteError>,
        lengths: Lengths,
        block_maps: Option<Arc<BlockMaps>>,
        steal_candidates: Option<Arc<StealCandidates>>,
        piece_write_guards: Option<Arc<PieceWriteGuards>>,
    ) {
        self.reservation_state = Some((atomic_states, piece_notify, disk_handle, write_error_tx));
        let mut ds = PeerDispatchState::new(availability_snapshot, lengths.clone());
        // M103: Wire up block stealing if provided
        if let Some(bm) = block_maps {
            self.peer_block_maps = Some(Arc::clone(&bm));
            ds.set_block_maps(bm);
        }
        if let Some(sc) = steal_candidates {
            ds.set_steal_candidates(sc);
        }
        // M120: Wire up per-piece write guards
        if let Some(pwg) = piece_write_guards {
            self.piece_write_guards = Some(pwg.clone());
            ds.set_piece_write_guards(pwg);
        }
        self.block_length = lengths.chunk_size();
        self.dispatch_state = Some(ds);
        // M92: Create PendingBatch for block batching
        if self.pending_batch.is_none() {
            self.pending_batch = Some(PendingBatch::new(&lengths));
        }
        // Requester arm activates on next loop iteration if unchoked
    }

    async fn on_update_num_pieces(&mut self, n: u32) -> Option<Message> {
        self.num_pieces = n;
        // M76: Resize local bitfield for new piece count
        self.local_bitfield = Bitfield::new(n);
        // Replay deferred bitfield/HaveAll/HaveNone now that we know num_pieces
        if let Some(deferred) = self.deferred_bitfield.take() {
            let bitfield = match deferred {
                DeferredBitfield::Raw(data) => {
                    debug!(addr = %self.addr, num_pieces = n, data_len = data.len(), "replaying deferred bitfield");
                    match Bitfield::from_bytes(data, n) {
                        Ok(bf) => Some(bf),
                        Err(e) => {
                            warn!(addr = %self.addr, "deferred bitfield invalid: {e}");
                            None
                        }
                    }
                }
                DeferredBitfield::HaveAll => {
                    debug!(addr = %self.addr, num_pieces = n, "replaying deferred HaveAll");
                    let mut bf = Bitfield::new(n);
                    for i in 0..n {
                        bf.set(i);
                    }
                    Some(bf)
                }
                DeferredBitfield::HaveNone => {
                    debug!(addr = %self.addr, num_pieces = n, "replaying deferred HaveNone");
                    Some(Bitfield::new(n))
                }
            };
            if let Some(bitfield) = bitfield {
                // M76: Copy replayed bitfield to local state
                self.local_bitfield = bitfield.clone();
                debug!(addr = %self.addr, ones = bitfield.count_ones(), "deferred bitfield sent");
                if self
                    .event_tx
                    .send(PeerEvent::Bitfield {
                        peer_addr: self.addr,
                        bitfield,
                    })
                    .await
                    .is_err()
                {
                    return None; // TorrentActor gone
                }
            }
        }
        None
    }

    fn on_snapshot_update(&mut self, snapshot: Arc<AvailabilitySnapshot>) {
        self.pending_snapshot = Some(snapshot);
        // Will be applied on next dispatch iteration
    }

    fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn inc_in_flight(&mut self) {
        self.in_flight += 1;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use torrent_core::Lengths;

    // Helper: standard torrent with 10 pieces, 512 KiB each, 16 KiB chunks
    fn standard_lengths() -> Lengths {
        Lengths::new(10 * 512 * 1024, 512 * 1024, 16384)
    }

    // Helper: small torrent where last piece has only 1 block
    fn small_last_piece_lengths() -> Lengths {
        Lengths::new(10 * 512 * 1024 + 16384, 512 * 1024, 16384)
    }

    mod pending_batch_tests {
        use super::*;
        use super::super::PendingBatch;

        #[test]
        fn batch_accumulation_full_piece() {
            let lengths = standard_lengths();
            let mut batch = PendingBatch::new(&lengths);
            for i in 0..31 {
                let complete = batch.push(0, i * 16384, 16384);
                assert!(!complete, "piece should not be complete after block {i}");
            }
            assert_eq!(batch.blocks.len(), 31);
            assert!(!batch.is_empty());
            let complete = batch.push(0, 31 * 16384, 16384);
            assert!(complete, "piece should be complete after 32 blocks");
            assert_eq!(batch.blocks.len(), 32);
            let blocks = batch.take();
            assert_eq!(blocks.len(), 32);
            assert!(batch.is_empty());
        }

        #[test]
        fn timer_flush_partial_piece() {
            let lengths = standard_lengths();
            let mut batch = PendingBatch::new(&lengths);
            for i in 0..10 {
                assert!(!batch.push(0, i * 16384, 16384));
            }
            let blocks = batch.take();
            assert_eq!(blocks.len(), 10);
            assert!(batch.is_empty());
            assert_eq!(blocks[0].index, 0);
            assert_eq!(blocks[0].begin, 0);
            assert_eq!(blocks[9].begin, 9 * 16384);
        }

        #[test]
        fn single_block_last_piece() {
            let lengths = small_last_piece_lengths();
            let mut batch = PendingBatch::new(&lengths);
            assert_eq!(lengths.chunks_in_piece(10), 1);
            let complete = batch.push(10, 0, 16384);
            assert!(complete);
            let blocks = batch.take();
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0].index, 10);
        }

        #[test]
        fn multi_piece_accumulation() {
            let lengths = standard_lengths();
            let mut batch = PendingBatch::new(&lengths);
            for i in 0..16 {
                assert!(!batch.push(0, i * 16384, 16384));
                assert!(!batch.push(1, i * 16384, 16384));
            }
            assert_eq!(batch.blocks.len(), 32);
            for i in 16..31 {
                assert!(!batch.push(0, i * 16384, 16384));
            }
            let complete = batch.push(0, 31 * 16384, 16384);
            assert!(complete);
            let blocks = batch.take();
            assert_eq!(blocks.len(), 48);
        }

        #[test]
        fn flush_on_disconnect_preserves_blocks() {
            let lengths = standard_lengths();
            let mut batch = PendingBatch::new(&lengths);
            for i in 0..15 {
                batch.push(0, i * 16384, 16384);
            }
            let blocks = batch.take();
            assert_eq!(blocks.len(), 15);
            assert!(batch.is_empty());
        }

        #[test]
        fn blocks_for_piece_last_piece_exact_multiple() {
            let lengths = Lengths::new(10 * 512 * 1024, 512 * 1024, 16384);
            let batch = PendingBatch::new(&lengths);
            assert_eq!(batch.lengths.chunks_in_piece(0), 32);
            assert_eq!(batch.lengths.chunks_in_piece(9), 32);
        }

        #[test]
        fn empty_batch_take_returns_empty() {
            let lengths = standard_lengths();
            let mut batch = PendingBatch::new(&lengths);
            assert!(batch.is_empty());
            let blocks = batch.take();
            assert!(blocks.is_empty());
        }

        #[test]
        fn pending_batch_cross_piece_boundary() {
            let lengths = Lengths::new(4 * 64 * 1024, 64 * 1024, 16384);
            let mut batch = PendingBatch::new(&lengths);
            assert_eq!(lengths.chunks_in_piece(0), 4);
            for i in 0..3 {
                assert!(!batch.push(0, i * 16384, 16384));
            }
            assert!(batch.push(0, 3 * 16384, 16384));
            let blocks = batch.take();
            assert_eq!(blocks.len(), 4);
            assert!(!batch.push(1, 0, 16384));
            assert!(!batch.push(1, 16384, 16384));
            let blocks = batch.take();
            assert_eq!(blocks.len(), 2);
            assert!(!batch.push(1, 2 * 16384, 16384));
            assert!(!batch.push(1, 3 * 16384, 16384));
            assert_eq!(batch.blocks.len(), 2);
        }
    }

    #[test]
    fn unchoke_sets_full_permits() {
        let semaphore = tokio::sync::Semaphore::new(0);
        let depth: usize = 128;
        while let Ok(permit) = semaphore.try_acquire() {
            permit.forget();
        }
        semaphore.add_permits(depth);
        assert_eq!(semaphore.available_permits(), depth);
        let p1 = semaphore.try_acquire().unwrap();
        let p2 = semaphore.try_acquire().unwrap();
        assert_eq!(semaphore.available_permits(), depth - 2);
        drop(p1);
        drop(p2);
        while let Ok(permit) = semaphore.try_acquire() {
            permit.forget();
        }
        semaphore.add_permits(depth);
        assert_eq!(semaphore.available_permits(), depth);
    }

    // -----------------------------------------------------------------------
    // Handler unit tests
    // -----------------------------------------------------------------------

    mod handler_tests {
        use super::*;
        use std::collections::BTreeSet;
        use std::net::SocketAddr;
        use std::sync::Arc;

        use crate::peer_handler::PeerConnectionHandler;
        use crate::piece_reservation::{AtomicPieceStates, AvailabilitySnapshot};
        use torrent_storage::Bitfield;

        /// Create a minimal `TorrentPeerHandler` for testing.
        fn make_handler(num_pieces: u32) -> (TorrentPeerHandler, mpsc::Receiver<PeerEvent>) {
            let (tx, rx) = mpsc::channel(64);
            let addr: SocketAddr = "127.0.0.1:6881".parse().expect("valid addr");
            let handler = TorrentPeerHandler::new(
                addr,
                num_pieces,
                tx,
                false, // enable_fast
                Id20([0u8; 20]),
                None,  // info_bytes
                Arc::new(vec![]),
                None,  // our_ut_metadata
                None,  // our_ut_pex
                None,  // our_lt_trackers
                None,  // our_ut_holepunch
            );
            (handler, rx)
        }

        /// Build AtomicPieceStates + AvailabilitySnapshot for `n` pieces, all
        /// Available.
        fn make_piece_state(
            n: u32,
        ) -> (Arc<AtomicPieceStates>, Arc<AvailabilitySnapshot>) {
            let we_have = Bitfield::new(n);
            let wanted = {
                let mut bf = Bitfield::new(n);
                for i in 0..n {
                    bf.set(i);
                }
                bf
            };
            let states = Arc::new(AtomicPieceStates::new(n, &we_have, &wanted));
            let availability: Vec<u32> = vec![1; n as usize];
            let snap = Arc::new(AvailabilitySnapshot::build(
                &availability,
                &states,
                &BTreeSet::new(),
                1,
            ));
            (states, snap)
        }

        /// Invoke `on_start_requesting` with no disk handle.
        fn start_requesting_no_disk(handler: &mut TorrentPeerHandler, n: u32) {
            let (states, snap) = make_piece_state(n);
            let notify = Arc::new(tokio::sync::Notify::new());
            let (err_tx, _err_rx) = mpsc::channel(4);
            let lengths = Lengths::new(
                u64::from(n) * 512 * 1024,
                512 * 1024,
                16384,
            );
            handler.on_start_requesting(
                states,
                snap,
                notify,
                None, // no disk handle
                err_tx,
                lengths,
                None, // no block maps
                None, // no steal candidates
                None, // no piece write guards
            );
        }

        // -- Test 1 --
        #[test]
        fn handler_on_piece_sync_unhandled() {
            let (mut handler, _rx) = make_handler(10);
            // No reservation_state — should return Unhandled
            let result = handler
                .on_piece_sync(0, 0, &[0u8; 16384], &[])
                .expect("should not error");
            assert!(
                matches!(result, PieceAction::Unhandled),
                "expected Unhandled without reservation state"
            );
        }

        // -- Test 2 --
        #[test]
        fn handler_on_piece_sync_permit_management() {
            let (mut handler, _rx) = make_handler(10);
            start_requesting_no_disk(&mut handler, 10);
            // Pump in_flight up to 5
            for _ in 0..5 {
                handler.inc_in_flight();
            }
            assert_eq!(handler.in_flight(), 5);
            let initial_permits = handler.semaphore.available_permits();
            // on_piece_sync decrements in_flight and returns a permit
            // (disk_handle is None so it returns Unhandled, but permit
            // management still runs because reservation_state.is_some())
            let result = handler
                .on_piece_sync(0, 0, &[0u8; 16384], &[])
                .expect("should not error");
            assert!(
                matches!(result, PieceAction::Unhandled),
                "no disk handle → Unhandled"
            );
            assert_eq!(handler.in_flight(), 4);
            assert_eq!(
                handler.semaphore.available_permits(),
                initial_permits + 1,
                "semaphore should have gained one permit"
            );
        }

        // -- Test 3 --
        #[test]
        fn handler_on_choke_sets_peer_choking() {
            let (mut handler, _rx) = make_handler(10);
            // Fresh handler: peer_choking = true, no dispatch state
            let released = handler.on_choke();
            assert!(released.is_none(), "no piece to release");
            assert!(
                !handler.can_request(),
                "cannot request when choked"
            );
        }

        // -- Test 4 --
        #[test]
        fn handler_on_unchoke_resets_pipeline() {
            let (mut handler, _rx) = make_handler(10);
            // Initially choked, semaphore has 0 permits
            assert_eq!(handler.semaphore.available_permits(), 0);
            assert!(
                !handler.can_request(),
                "choked + no dispatch → cannot request"
            );
            handler.on_unchoke();
            // Semaphore should now have INITIAL_QUEUE_DEPTH permits
            assert_eq!(
                handler.semaphore.available_permits(),
                INITIAL_QUEUE_DEPTH,
            );
            // Still cannot request (no dispatch_state)
            assert!(
                !handler.can_request(),
                "unchoked but no dispatch_state → cannot request"
            );
        }

        // -- Test 5 --
        #[test]
        fn handler_can_request_false_when_choked() {
            let (handler, _rx) = make_handler(10);
            assert!(
                !handler.can_request(),
                "fresh handler should be choked"
            );
        }

        // -- Test 6 --
        #[test]
        fn handler_next_block_exhausted() {
            let (mut handler, _rx) = make_handler(10);
            assert!(
                handler.next_block().is_none(),
                "no dispatch state → None"
            );
        }

        // -- Test 7 --
        #[tokio::test]
        async fn handler_defer_bitfield_replay() {
            let (mut handler, mut rx) = make_handler(0);
            assert!(
                handler.should_defer_bitfield(),
                "num_pieces=0 → should defer"
            );
            // Defer a HaveAll
            handler.defer_bitfield(&Message::<&[u8]>::HaveAll);
            // Update num_pieces — should replay deferred bitfield
            let _ = handler.on_update_num_pieces(10).await;
            // Check that a Bitfield event was sent
            let event = rx.try_recv().expect("should have received event");
            match event {
                PeerEvent::Bitfield { bitfield, .. } => {
                    assert_eq!(
                        bitfield.count_ones(),
                        10,
                        "replayed HaveAll should set all 10 pieces"
                    );
                }
                other => panic!("expected Bitfield event, got {other:?}"),
            }
        }

        // -- Test 8 --
        #[test]
        fn handler_on_reject_request_decrements_in_flight() {
            let (mut handler, _rx) = make_handler(10);
            start_requesting_no_disk(&mut handler, 10);
            for _ in 0..3 {
                handler.inc_in_flight();
            }
            assert_eq!(handler.in_flight(), 3);
            handler.on_reject_request();
            assert_eq!(handler.in_flight(), 2);
            handler.on_reject_request();
            assert_eq!(handler.in_flight(), 1);
            // Reject at zero should saturate, not underflow
            handler.on_reject_request();
            assert_eq!(handler.in_flight(), 0);
            handler.on_reject_request();
            assert_eq!(handler.in_flight(), 0);
        }

        // -- Test 9 --
        #[test]
        fn handler_on_bitfield_update_tracks_pieces() {
            let (mut handler, _rx) = make_handler(10);
            // Have { index: 5 }
            handler.on_bitfield_update(&Message::<&[u8]>::Have { index: 5 });
            assert!(
                handler.local_bitfield.get(5),
                "piece 5 should be set"
            );
            assert!(
                !handler.local_bitfield.get(4),
                "piece 4 should not be set"
            );
            // HaveAll
            handler.on_bitfield_update(&Message::<&[u8]>::HaveAll);
            for i in 0..10 {
                assert!(
                    handler.local_bitfield.get(i),
                    "piece {i} should be set after HaveAll"
                );
            }
            // HaveNone
            handler.on_bitfield_update(&Message::<&[u8]>::HaveNone);
            for i in 0..10 {
                assert!(
                    !handler.local_bitfield.get(i),
                    "piece {i} should be clear after HaveNone"
                );
            }
        }

        // -- Test 10: M118 broadcast filtering --
        #[test]
        fn broadcast_not_filtered_for_peer_without_piece() {
            let (handler, _rx) = make_handler(10);
            // Fresh handler has empty local_bitfield — peer has no pieces
            for i in 0..10 {
                assert!(
                    handler.should_transmit_have(i),
                    "should_transmit_have({i}) must be true when peer has no pieces"
                );
            }
        }

        // -- Test 10b --
        #[test]
        fn broadcast_filtered_for_peer_with_piece() {
            let (mut handler, _rx) = make_handler(10);
            // Simulate peer announcing Have for piece 42 (index 5)
            handler.on_bitfield_update(&torrent_wire::Message::<&[u8]>::Have { index: 5 });
            assert!(
                !handler.should_transmit_have(5),
                "should NOT transmit Have for piece 5 — peer already has it"
            );
            assert!(
                handler.should_transmit_have(4),
                "should transmit Have for piece 4 — peer does not have it"
            );
        }

        // -- Test 10c --
        #[test]
        fn should_transmit_have_empty_bitfield() {
            let (handler, _rx) = make_handler(10);
            // All pieces should be transmitted when bitfield is empty
            for i in 0..10 {
                assert!(handler.should_transmit_have(i));
            }
        }

        // -- Test 10d --
        #[test]
        fn should_transmit_have_full_bitfield() {
            let (mut handler, _rx) = make_handler(10);
            // Simulate HaveAll — peer has all pieces
            handler.on_bitfield_update(&torrent_wire::Message::<&[u8]>::HaveAll);
            for i in 0..10 {
                assert!(
                    !handler.should_transmit_have(i),
                    "should NOT transmit Have({i}) — peer has all pieces"
                );
            }
        }

        // -- Test 11 --
        #[tokio::test]
        async fn handler_on_disconnect_sends_event() {
            let (mut handler, mut rx) = make_handler(10);
            handler
                .on_disconnect(Some("test reason".to_string()))
                .await;
            let event = rx.try_recv().expect("should have received event");
            match event {
                PeerEvent::Disconnected { reason, .. } => {
                    assert_eq!(reason.as_deref(), Some("test reason"));
                }
                other => panic!("expected Disconnected event, got {other:?}"),
            }
        }

        // -- Test 12 --
        #[test]
        fn handler_on_snapshot_update_queues() {
            let (mut handler, _rx) = make_handler(10);
            let (_states, snap) = make_piece_state(10);
            handler.on_snapshot_update(Arc::clone(&snap));
            // pending_snapshot should be set, but no dispatch_state
            // so next_block still returns None
            assert!(
                handler.next_block().is_none(),
                "no dispatch_state → next_block None"
            );
        }

        // -- Test 13 --
        #[test]
        fn handler_on_start_requesting_enables_dispatch() {
            let (mut handler, _rx) = make_handler(10);
            // Before: no dispatch, no reservation
            assert!(!handler.can_request());
            assert!(handler.next_block().is_none());
            start_requesting_no_disk(&mut handler, 10);
            // Now: dispatch_state and reservation_state are set
            // But still choked, so can_request is false
            assert!(
                !handler.can_request(),
                "choked → still cannot request"
            );
            handler.on_unchoke();
            assert!(
                handler.can_request(),
                "unchoked + dispatch + reservation → can request"
            );
        }

        // -- Test 14: M118 broadcast lagged recovery --
        #[tokio::test]
        async fn broadcast_lagged_receiver_recovers() {
            // Create a very small broadcast channel to force lagging
            let (tx, mut rx) = tokio::sync::broadcast::channel::<u32>(4);
            // Send more messages than capacity → oldest will be dropped
            for i in 0..10 {
                let _ = tx.send(i);
            }
            // First recv should report Lagged
            match rx.recv().await {
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    assert!(n > 0, "should report skipped messages");
                }
                other => panic!("expected Lagged, got {other:?}"),
            }
            // Subsequent recv should succeed with the most recent messages
            let val = rx.recv().await.unwrap();
            assert!(val >= 6, "should receive recent value, got {val}");
        }

        // -- Test 15: M118 broadcast closed --
        #[tokio::test]
        async fn broadcast_channel_closed() {
            let (tx, mut rx) = tokio::sync::broadcast::channel::<u32>(16);
            drop(tx);
            match rx.recv().await {
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                other => panic!("expected Closed, got {other:?}"),
            }
        }
    }
}
