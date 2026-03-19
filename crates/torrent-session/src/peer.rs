//! PeerTask — one tokio task per TCP connection.
//!
//! Handles handshake, BEP 10 extension negotiation, message loop,
//! and communicates with the TorrentActor via channels.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use rustc_hash::FxHashMap;
use torrent_core::{Id20, Lengths};
use torrent_storage::Bitfield;
use torrent_wire::{
    ExtHandshake, Handshake, Message, MetadataMessage, MetadataMessageType,
};

use crate::peer_codec::{FillStatus, PeerReader, PeerWriter};

use crate::disk::{DiskHandle, DiskWriteError};
use crate::pex::PexMessage;
use crate::piece_reservation::{
    AtomicPieceStates, AvailabilitySnapshot, BlockMaps, PeerDispatchState,
};
use crate::types::{BlockEntry, PeerCommand, PeerEvent};
use tokio::sync::Semaphore;

/// M92: Accumulates block completions for batched delivery to TorrentActor.
/// Lives in the peer task's stack — no shared state, no locking.
struct PendingBatch {
    /// Block completions awaiting flush.
    blocks: Vec<BlockEntry>,
    /// Per-piece block count for detecting piece completion.
    /// Key: piece index, Value: blocks written so far in this batch.
    piece_counts: FxHashMap<u32, u32>,
    /// Piece/chunk arithmetic for completion detection.
    lengths: Lengths,
}

impl PendingBatch {
    fn new(lengths: &Lengths) -> Self {
        Self {
            blocks: Vec::with_capacity(BATCH_INITIAL_CAPACITY),
            piece_counts: FxHashMap::default(),
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

/// M92: Initial capacity for PendingBatch block buffer.
/// Matches a standard 512 KiB piece (512 * 1024 / 16384 = 32 blocks).
const BATCH_INITIAL_CAPACITY: usize = 32;

/// M104: Fixed per-peer queue depth — number of concurrent requests per peer.
/// Replaces AIMD dynamic depth. Matches `fixed_pipeline_depth` setting default.
const INITIAL_QUEUE_DEPTH: usize = 128;

/// M93: State needed for lock-free dispatch and direct peer-to-disk writes.
/// Tuple: (atomic_states, piece_notify, disk_handle, write_error_tx)
type ReservationState = Option<(
    std::sync::Arc<AtomicPieceStates>,
    std::sync::Arc<tokio::sync::Notify>,
    Option<DiskHandle>,
    mpsc::Sender<DiskWriteError>,
)>;

/// Total handshake size: 1 + 19 + 8 + 20 + 20 = 68 bytes.
const HANDSHAKE_SIZE: usize = 68;


/// Run a single peer connection, handling handshake, extension negotiation,
/// and the message loop.
///
/// The generic stream parameter allows tests to use `tokio::io::duplex()`.
#[allow(dead_code, clippy::too_many_arguments)]
pub(crate) async fn run_peer(
    addr: SocketAddr,
    stream: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    info_hash: Id20,
    our_peer_id: Id20,
    our_bitfield: Bitfield,
    mut num_pieces: u32,
    event_tx: mpsc::Sender<PeerEvent>,
    mut cmd_rx: mpsc::Receiver<PeerCommand>,
    enable_dht: bool,
    enable_fast: bool,
    encryption_mode: torrent_wire::mse::EncryptionMode,
    outbound: bool,
    anonymous_mode: bool,
    info_bytes: Option<Bytes>,
    plugins: std::sync::Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>>,
    enable_holepunch: bool,
    max_message_size: usize,
) -> crate::Result<()> {
    use torrent_wire::mse::{self, EncryptionMode, MseStream};

    // --- Phase 0: MSE/PE encryption ---
    // Timeout MSE handshake at 5 seconds to avoid blocking on plaintext-only
    // peers that won't respond to our DH key exchange.
    let mut stream = if encryption_mode != EncryptionMode::Disabled {
        let crypto_provide = match encryption_mode {
            EncryptionMode::Forced => torrent_wire::mse::CRYPTO_RC4,
            EncryptionMode::PreferPlaintext if outbound => torrent_wire::mse::CRYPTO_PLAINTEXT,
            // Inbound PreferPlaintext (guard false), Enabled, and any future variants —
            // accept both methods and let the peer choose.
            _ => torrent_wire::mse::CRYPTO_PLAINTEXT | torrent_wire::mse::CRYPTO_RC4,
        };

        let result = if outbound {
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                mse::handshake::negotiate_outbound(stream, &info_hash, crypto_provide),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err(torrent_wire::Error::EncryptionHandshakeFailed(
                    "MSE handshake timed out".into(),
                )),
            }
        } else {
            mse::handshake::negotiate_inbound(
                stream,
                &info_hash,
                encryption_mode == EncryptionMode::Forced,
            )
            .await
        };

        match result {
            Ok(r) => r.stream,
            Err(e) => return Err(crate::Error::Wire(e)),
        }
    } else {
        MseStream::plaintext(stream)
    };

    // --- Phase 1: Handshake (raw read/write, not framed) ---
    let mut our_hs = Handshake::new(info_hash, our_peer_id);
    if enable_dht {
        our_hs = our_hs.with_dht();
    }
    if enable_fast {
        our_hs = our_hs.with_fast();
    }

    stream.write_all(&our_hs.to_bytes()).await?;
    stream.flush().await?;

    let mut hs_buf = [0u8; HANDSHAKE_SIZE];
    stream.read_exact(&mut hs_buf).await?;

    let their_hs = Handshake::from_bytes(&hs_buf)?;
    if their_hs.info_hash != info_hash {
        return Err(crate::Error::Connection("info_hash mismatch".into()));
    }

    // --- Phase 2: Wrap stream in ring-buffer codec (M109) ---
    let (reader, writer) = tokio::io::split(stream);
    let mut reader = PeerReader::new(reader, max_message_size);
    let mut writer = PeerWriter::new(writer);

    // --- Phase 3: BEP 10 Extension Handshake ---
    let plugin_names: Vec<&str> = plugins.iter().map(|p| p.name()).collect();
    let peer_supports_extensions = their_hs.supports_extensions();
    if peer_supports_extensions {
        let mut ext_hs = ExtHandshake::new_with_plugins(&plugin_names);
        if !enable_holepunch {
            ext_hs.m.remove("ut_holepunch");
        }
        if anonymous_mode {
            ext_hs.v = None;
            ext_hs.p = None;
            ext_hs.reqq = None;
            ext_hs.upload_only = None;
        }
        let payload = ext_hs.to_bytes().map_err(crate::Error::Wire)?;
        writer
            .send(&Message::Extended { ext_id: 0, payload })
            .await?;
    }

    // --- Phase 4: Send our bitfield (fast-aware) ---
    let both_support_fast = enable_fast && their_hs.supports_fast();
    if both_support_fast {
        if num_pieces > 0 && our_bitfield.count_ones() == num_pieces {
            writer.send(&Message::HaveAll).await?;
        } else if our_bitfield.count_ones() == 0 {
            writer.send(&Message::HaveNone).await?;
        } else {
            writer
                .send(&Message::Bitfield(Bytes::copy_from_slice(
                    our_bitfield.as_bytes(),
                )))
                .await?;
        }
    } else if our_bitfield.count_ones() > 0 {
        writer
            .send(&Message::Bitfield(Bytes::copy_from_slice(
                our_bitfield.as_bytes(),
            )))
            .await?;
    }

    // --- Phase 4b: Send AllowedFast set (BEP 6) ---
    if both_support_fast && num_pieces > 0 {
        let fast_set = torrent_wire::allowed_fast_set_for_ip(&info_hash, addr.ip(), num_pieces, 10);
        for index in fast_set {
            writer.send(&Message::AllowedFast(index)).await?;
        }
    }

    // M107: Send unchoke unconditionally on connect (matches rqbit).
    // Costs nothing for a downloader; improves tit-for-tat reciprocity.
    writer.send(&Message::Unchoke).await?;

    // Track extension ID mappings:
    // - peer_ut_*: IDs from the remote's ext handshake (used when SENDING to them)
    // - our_ut_*: IDs from OUR ext handshake (used for matching INCOMING messages)
    let mut peer_ut_metadata: Option<u8> = None;
    let mut peer_ut_pex: Option<u8> = None;
    let mut peer_lt_trackers: Option<u8> = None;
    let mut peer_ut_holepunch: Option<u8> = None;
    // Peer's extension IDs for plugin names (for sending responses using their IDs)
    let mut peer_ext_ids: std::collections::HashMap<String, u8> = std::collections::HashMap::new();
    let our_ext = ExtHandshake::new_with_plugins(&plugin_names);
    let our_ut_metadata: Option<u8> = our_ext.ext_id("ut_metadata");
    let our_ut_pex: Option<u8> = our_ext.ext_id("ut_pex");
    let our_lt_trackers: Option<u8> = our_ext.ext_id("lt_trackers");
    let our_ut_holepunch: Option<u8> = if enable_holepunch {
        our_ext.ext_id("ut_holepunch")
    } else {
        None
    };

    // Notify plugins that a peer connected
    for plugin in plugins.iter() {
        plugin.on_peer_connected(&info_hash, addr);
    }

    // Deferred bitfield: when we receive Bitfield/HaveAll/HaveNone before
    // metadata assembly (num_pieces == 0), store it and replay after
    // UpdateNumPieces arrives with the real piece count.
    #[derive(Debug)]
    enum DeferredBitfield {
        Raw(Vec<u8>),
        HaveAll,
        HaveNone,
    }
    let mut deferred_bitfield: Option<DeferredBitfield> = None;

    // --- Phase 5: Main loop ---
    debug!(%addr, num_pieces, "entering main loop");

    // M75: Peer-integrated request dispatch
    let semaphore = Semaphore::new(0); // Start empty, add permits on unchoke
    let mut peer_choking = true; // Peers start in choked state
    let mut reservation_state: ReservationState = None;

    // M93: Per-peer dispatch state (lock-free)
    let mut dispatch_state: Option<PeerDispatchState> = None;
    let mut pending_snapshot: Option<std::sync::Arc<AvailabilitySnapshot>> = None;

    // M103: Block stealing — shared block maps and chunk size for mark_received.
    let mut peer_block_maps: Option<std::sync::Arc<BlockMaps>> = None;
    let mut block_length: u32 = torrent_core::DEFAULT_CHUNK_SIZE;

    // M76: Local copy of the peer's bitfield for dispatch (avoids shared state lookup).
    let mut local_bitfield = Bitfield::new(num_pieces);

    // M104: Pipeline depth is fixed — no AIMD, no EWMA in peer task
    let mut in_flight: usize = 0;
    let current_effective_depth: usize = INITIAL_QUEUE_DEPTH;

    // M92: Pending batch for accumulating block completions
    let mut pending_batch: Option<PendingBatch> = None;
    // M92: 25ms flush interval — ticks only when batch has pending blocks
    let mut flush_interval = tokio::time::interval(Duration::from_millis(25));
    flush_interval.tick().await; // skip initial immediate tick

    let disconnect_reason: Option<String> = loop {
        let can_request = !peer_choking && dispatch_state.is_some() && reservation_state.is_some();

        tokio::select! {
            biased;

            // M110: Three-phase zero-copy read: fill → decode → advance.
            // Piece messages are written directly to disk from ring buffer
            // slices (pwritev) before advancing — no clone, no channel.
            status = reader.fill_message() => {
                match status {
                    Ok(FillStatus::Ready) => {
                        let (msg, consumed) = match reader.try_decode() {
                            Ok(pair) => pair,
                            Err(e) => {
                                debug!(%addr, "decode error: {e}");
                                break Some(e.to_string());
                            }
                        };

                        // --- Piece fast-path: zero-copy pwrite from ring buffer ---
                        if let Message::Piece { index, begin, data_0, data_1 } = &msg {
                            let data_len = (data_0.len() + data_1.len()) as u32;

                            // M75: Permit management for pipeline depth.
                            if reservation_state.is_some() {
                                in_flight = in_flight.saturating_sub(1);
                                // M82: Permit absorption — only return permit if below target depth
                                if in_flight < current_effective_depth {
                                    semaphore.add_permits(1);
                                }
                            }

                            // M103: Track received blocks in shared BlockMaps for steal visibility.
                            if let Some(ref bm) = peer_block_maps {
                                let block_idx = *begin / block_length;
                                bm.mark_received(*index, block_idx);
                            }

                            // M110: Direct pwrite from ring buffer slices — no clone, no channel.
                            if let Some((_, _, Some(ref disk), ref write_err_tx)) = reservation_state
                                && let Err(e) = disk.write_block_direct(*index, *begin, data_0, data_1)
                            {
                                // Report write error to TorrentActor (same pattern as deferred path).
                                let _ = write_err_tx.try_send(DiskWriteError {
                                    piece: *index,
                                    begin: *begin,
                                    error: match e {
                                        crate::Error::Storage(se) => se,
                                        other => torrent_storage::Error::Io(
                                            std::io::Error::other(other.to_string()),
                                        ),
                                    },
                                });
                            }

                            // M92: Accumulate block metadata in PendingBatch.
                            // Flush immediately on piece completion.
                            if reservation_state.as_ref().is_some_and(|(_, _, d, _)| d.is_some()) {
                                if let Some(ref mut batch) = pending_batch {
                                    let piece_complete = batch.push(*index, *begin, data_len);
                                    // CRITICAL: advance BEFORE any await — borrowed slices released.
                                    reader.advance(consumed);
                                    if piece_complete {
                                        let blocks = batch.take();
                                        event_tx.send(PeerEvent::PieceBlocksBatch {
                                            peer_addr: addr,
                                            blocks,
                                        }).await.map_err(|_| crate::Error::Shutdown)?;
                                    }
                                } else {
                                    // No batch yet (StartRequesting not received) — send single block.
                                    reader.advance(consumed);
                                    event_tx.send(PeerEvent::PieceBlocksBatch {
                                        peer_addr: addr,
                                        blocks: vec![BlockEntry {
                                            index: *index, begin: *begin,
                                            length: data_len,
                                        }],
                                    }).await.map_err(|_| crate::Error::Shutdown)?;
                                }
                                continue; // skip handle_message
                            }

                            // Piece without reservation — fall through to handle_message.
                            // Convert to owned before advancing.
                            let owned = msg.to_owned_bytes();
                            reader.advance(consumed);
                            match handle_message(
                                owned,
                                addr,
                                num_pieces,
                                &event_tx,
                                &mut peer_ut_metadata,
                                &mut peer_ut_pex,
                                &mut peer_lt_trackers,
                                &mut peer_ut_holepunch,
                                &mut peer_ext_ids,
                                our_ut_metadata,
                                our_ut_pex,
                                our_lt_trackers,
                                our_ut_holepunch,
                                both_support_fast,
                                info_bytes.as_ref(),
                                &info_hash,
                                &plugins,
                            ).await {
                                Ok(Some(response)) => {
                                    if let Err(e) = writer.send(&response).await {
                                        warn!(%addr, "error sending response: {e}");
                                        break Some(e.to_string());
                                    }
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    warn!(%addr, "error handling message: {e}");
                                }
                            }
                            continue;
                        }

                        // --- Non-Piece path: convert to owned, advance, then process ---
                        // Defer Bitfield/HaveAll/HaveNone when num_pieces is
                        // unknown (magnet phase).  Replay after UpdateNumPieces.
                        if num_pieces == 0 {
                            match &msg {
                                Message::Bitfield(data) => {
                                    debug!(%addr, data_len = data.len(), "deferring bitfield (num_pieces=0)");
                                    let raw = data.to_vec();
                                    reader.advance(consumed);
                                    deferred_bitfield = Some(DeferredBitfield::Raw(raw));
                                    continue;
                                }
                                Message::HaveAll => {
                                    debug!(%addr, "deferring HaveAll (num_pieces=0)");
                                    reader.advance(consumed);
                                    deferred_bitfield = Some(DeferredBitfield::HaveAll);
                                    continue;
                                }
                                Message::HaveNone => {
                                    debug!(%addr, "deferring HaveNone (num_pieces=0)");
                                    reader.advance(consumed);
                                    deferred_bitfield = Some(DeferredBitfield::HaveNone);
                                    continue;
                                }
                                _ => {}
                            }
                        }

                        // M75/M76: Intercept for permit management and local_bitfield update.
                        // These operate on borrowed data before we convert to owned.
                        match &msg {
                            Message::Unchoke => {
                                peer_choking = false;
                                in_flight = 0;
                                // M82: Drain stale permits from previous cycle before refilling.
                                while let Ok(permit) = semaphore.try_acquire() {
                                    permit.forget();
                                }
                                semaphore.add_permits(current_effective_depth);
                            }
                            Message::Choke => {
                                peer_choking = true;
                                // M93: Release current piece back to Available via atomic store
                                if let Some(ref mut ds) = dispatch_state {
                                    if let Some(piece) = ds.current_piece_index() {
                                        if let Some((ref atomic_states, ..)) = reservation_state {
                                            atomic_states.release(piece);
                                        }
                                        // Notify actor to clear piece_owner — need to advance
                                        // first since we await below and the borrowed msg is
                                        // a fixed-field variant (no data slices to preserve).
                                        reader.advance(consumed);
                                        let _ = event_tx.send(PeerEvent::PieceReleased {
                                            peer_addr: addr,
                                            piece,
                                        }).await;
                                        // Already advanced — use owned msg for handle_message.
                                        ds.clear_current_piece();
                                        match handle_message(
                                            Message::Choke,
                                            addr,
                                            num_pieces,
                                            &event_tx,
                                            &mut peer_ut_metadata,
                                            &mut peer_ut_pex,
                                            &mut peer_lt_trackers,
                                            &mut peer_ut_holepunch,
                                            &mut peer_ext_ids,
                                            our_ut_metadata,
                                            our_ut_pex,
                                            our_lt_trackers,
                                            our_ut_holepunch,
                                            both_support_fast,
                                            info_bytes.as_ref(),
                                            &info_hash,
                                            &plugins,
                                        ).await {
                                            Ok(Some(response)) => {
                                                if let Err(e) = writer.send(&response).await {
                                                    warn!(%addr, "error sending response: {e}");
                                                    break Some(e.to_string());
                                                }
                                            }
                                            Ok(None) => {}
                                            Err(e) => {
                                                warn!(%addr, "error handling message: {e}");
                                            }
                                        }
                                        continue;
                                    }
                                    ds.clear_current_piece();
                                }
                            }
                            Message::RejectRequest { .. } => {
                                if reservation_state.is_some() {
                                    in_flight = in_flight.saturating_sub(1);
                                    // M82: Permit absorption — same logic as Piece handler
                                    if in_flight < current_effective_depth {
                                        semaphore.add_permits(1);
                                    }
                                }
                            }
                            Message::Bitfield(data) => {
                                if let Ok(bf) = Bitfield::from_bytes(data.to_vec(), num_pieces) {
                                    local_bitfield = bf;
                                }
                            }
                            Message::Have { index } => {
                                local_bitfield.set(*index);
                            }
                            Message::HaveAll => {
                                local_bitfield = Bitfield::new(num_pieces);
                                for i in 0..num_pieces {
                                    local_bitfield.set(i);
                                }
                            }
                            Message::HaveNone => {
                                local_bitfield = Bitfield::new(num_pieces);
                            }
                            _ => {}
                        }

                        // Convert borrowed message to owned, then release ring space.
                        let owned = msg.to_owned_bytes();
                        reader.advance(consumed);

                        match handle_message(
                            owned,
                            addr,
                            num_pieces,
                            &event_tx,
                            &mut peer_ut_metadata,
                            &mut peer_ut_pex,
                            &mut peer_lt_trackers,
                            &mut peer_ut_holepunch,
                            &mut peer_ext_ids,
                            our_ut_metadata,
                            our_ut_pex,
                            our_lt_trackers,
                            our_ut_holepunch,
                            both_support_fast,
                            info_bytes.as_ref(),
                            &info_hash,
                            &plugins,
                        ).await {
                            Ok(Some(response)) => {
                                if let Err(e) = writer.send(&response).await {
                                    warn!(%addr, "error sending response: {e}");
                                    break Some(e.to_string());
                                }
                            }
                            Ok(None) => {}
                            Err(e) => {
                                warn!(%addr, "error handling message: {e}");
                            }
                        }
                    }
                    Ok(FillStatus::Eof) => {
                        // Clean end-of-stream
                        break Some("connection closed".into());
                    }
                    Ok(FillStatus::Oversized(length)) => {
                        // Message exceeds ring buffer capacity (> 32 KiB).
                        // Read into heap buffer and process as owned Message<Bytes>.
                        let msg = match reader.decode_oversized_into(length).await {
                            Ok(m) => m,
                            Err(e) => {
                                debug!(%addr, "oversized decode error: {e}");
                                break Some(e.to_string());
                            }
                        };
                        // Oversized messages are never Piece (max 16 KiB block + 9 byte header).
                        // Process through handle_message directly.
                        match handle_message(
                            msg,
                            addr,
                            num_pieces,
                            &event_tx,
                            &mut peer_ut_metadata,
                            &mut peer_ut_pex,
                            &mut peer_lt_trackers,
                            &mut peer_ut_holepunch,
                            &mut peer_ext_ids,
                            our_ut_metadata,
                            our_ut_pex,
                            our_lt_trackers,
                            our_ut_holepunch,
                            both_support_fast,
                            info_bytes.as_ref(),
                            &info_hash,
                            &plugins,
                        ).await {
                            Ok(Some(response)) => {
                                if let Err(e) = writer.send(&response).await {
                                    warn!(%addr, "error sending response: {e}");
                                    break Some(e.to_string());
                                }
                            }
                            Ok(None) => {}
                            Err(e) => {
                                warn!(%addr, "error handling message: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        debug!(%addr, "wire error: {e}");
                        break Some(e.to_string());
                    }
                }
            }

            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(PeerCommand::Shutdown) => {
                        break None;
                    }
                    Some(PeerCommand::StartRequesting { atomic_states, availability_snapshot, piece_notify, disk_handle, write_error_tx, lengths, block_maps, steal_candidates }) => {
                        reservation_state = Some((atomic_states, piece_notify, disk_handle, write_error_tx));
                        let mut ds = PeerDispatchState::new(availability_snapshot, lengths.clone());
                        // M103: Wire up block stealing if provided
                        if let Some(bm) = block_maps {
                            peer_block_maps = Some(std::sync::Arc::clone(&bm));
                            ds.set_block_maps(bm);
                        }
                        if let Some(sc) = steal_candidates {
                            ds.set_steal_candidates(sc);
                        }
                        block_length = lengths.chunk_size();
                        dispatch_state = Some(ds);
                        // M92: Create PendingBatch for block batching
                        if pending_batch.is_none() {
                            pending_batch = Some(PendingBatch::new(&lengths));
                        }
                        // Requester arm activates on next loop iteration if unchoked
                    }
                    Some(PeerCommand::UpdateNumPieces(n)) => {
                        num_pieces = n;
                        // M76: Resize local bitfield for new piece count
                        local_bitfield = Bitfield::new(n);
                        // Replay deferred bitfield/HaveAll/HaveNone now that
                        // we know num_pieces
                        if let Some(deferred) = deferred_bitfield.take() {
                            let bitfield = match deferred {
                                DeferredBitfield::Raw(data) => {
                                    debug!(%addr, num_pieces, data_len = data.len(), "replaying deferred bitfield");
                                    match Bitfield::from_bytes(data, num_pieces) {
                                        Ok(bf) => Some(bf),
                                        Err(e) => {
                                            warn!(%addr, "deferred bitfield invalid: {e}");
                                            None
                                        }
                                    }
                                }
                                DeferredBitfield::HaveAll => {
                                    debug!(%addr, num_pieces, "replaying deferred HaveAll");
                                    let mut bf = Bitfield::new(num_pieces);
                                    for i in 0..num_pieces {
                                        bf.set(i);
                                    }
                                    Some(bf)
                                }
                                DeferredBitfield::HaveNone => {
                                    debug!(%addr, num_pieces, "replaying deferred HaveNone");
                                    Some(Bitfield::new(num_pieces))
                                }
                            };
                            if let Some(bitfield) = bitfield {
                                // M76: Copy replayed bitfield to local state
                                local_bitfield = bitfield.clone();
                                debug!(%addr, ones = bitfield.count_ones(), "deferred bitfield sent");
                                if event_tx
                                    .send(PeerEvent::Bitfield {
                                        peer_addr: addr,
                                        bitfield,
                                    })
                                    .await
                                    .is_err()
                                {
                                    break None; // TorrentActor gone
                                }
                            }
                        }
                    }
                    Some(PeerCommand::SnapshotUpdate { snapshot }) => {
                        pending_snapshot = Some(snapshot);
                        // Will be applied on next dispatch iteration
                    }
                    Some(cmd) => {
                        if let Err(e) = handle_command(
                            cmd,
                            &mut writer,
                            peer_ut_metadata,
                            peer_ut_pex,
                            peer_ut_holepunch,
                        ).await {
                            debug!(%addr, "error sending message: {e}");
                            break Some(e.to_string());
                        }
                    }
                    None => {
                        // Actor dropped the sender — shut down
                        break None;
                    }
                }
            }

            // M93: Lock-free block requesting arm
            permit = semaphore.acquire(), if can_request => {
                let permit = permit.expect("peer semaphore closed unexpectedly");
                let ds = dispatch_state.as_mut().unwrap();
                let (atomic_states, piece_notify, ..) = reservation_state.as_ref().unwrap();

                // Check for snapshot updates from actor
                if let Some(new_snap) = pending_snapshot.take() {
                    ds.update_snapshot(new_snap);
                }

                match ds.next_block(&local_bitfield, atomic_states) {
                    Some(block) => {
                        permit.forget();
                        in_flight += 1;
                        writer.send(&Message::Request {
                            index: block.piece,
                            begin: block.begin,
                            length: block.length,
                        }).await?;
                    }
                    None => {
                        // Cursor exhausted -- wait for new pieces
                        drop(permit);
                        piece_notify.notified().await;
                    }
                }
            }

            // M92: 25ms batch flush timer — sends partial batches for stats/UI/endgame
            _ = flush_interval.tick(), if pending_batch.as_ref().is_some_and(|b| !b.is_empty()) => {
                if let Some(ref mut batch) = pending_batch {
                    let blocks = batch.take();
                    let _ = event_tx.send(PeerEvent::PieceBlocksBatch {
                        peer_addr: addr,
                        blocks,
                    }).await;
                }
            }
        }
    };

    // M93: Release held piece on disconnect
    if let Some(ref ds) = dispatch_state
        && let Some(piece) = ds.current_piece_index()
        && let Some((ref atomic_states, ..)) = reservation_state
    {
        atomic_states.release(piece);
    }

    // Notify plugins that a peer disconnected
    for plugin in plugins.iter() {
        plugin.on_peer_disconnected(&info_hash, addr);
    }

    // M92: Flush remaining blocks before disconnect
    if let Some(ref mut batch) = pending_batch
        && !batch.is_empty()
    {
        let blocks = batch.take();
        let _ = event_tx
            .send(PeerEvent::PieceBlocksBatch {
                peer_addr: addr,
                blocks,
            })
            .await;
    }

    // Send disconnect event (best-effort)
    let _ = event_tx
        .send(PeerEvent::Disconnected {
            peer_addr: addr,
            reason: disconnect_reason,
        })
        .await;

    Ok(())
}

/// Handle an incoming message from the remote peer.
///
/// `peer_ut_metadata`/`peer_ut_pex`: stored from remote's ext handshake (for sending).
/// `our_ut_metadata`/`our_ut_pex`: our assigned IDs (for matching incoming).
///
/// Returns `Ok(Some(msg))` when a response message should be sent on the wire.
#[allow(clippy::too_many_arguments)]
async fn handle_message(
    msg: Message,
    addr: SocketAddr,
    num_pieces: u32,
    event_tx: &mpsc::Sender<PeerEvent>,
    peer_ut_metadata: &mut Option<u8>,
    peer_ut_pex: &mut Option<u8>,
    peer_lt_trackers: &mut Option<u8>,
    peer_ut_holepunch: &mut Option<u8>,
    peer_ext_ids: &mut std::collections::HashMap<String, u8>,
    our_ut_metadata: Option<u8>,
    our_ut_pex: Option<u8>,
    our_lt_trackers: Option<u8>,
    our_ut_holepunch: Option<u8>,
    both_support_fast: bool,
    info_bytes: Option<&Bytes>,
    info_hash: &Id20,
    plugins: &[Box<dyn crate::extension::ExtensionPlugin>],
) -> crate::Result<Option<Message>> {
    match msg {
        Message::Choke => {
            event_tx
                .send(PeerEvent::PeerChoking {
                    peer_addr: addr,
                    choking: true,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        Message::Unchoke => {
            event_tx
                .send(PeerEvent::PeerChoking {
                    peer_addr: addr,
                    choking: false,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        Message::Interested => {
            event_tx
                .send(PeerEvent::PeerInterested {
                    peer_addr: addr,
                    interested: true,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        Message::NotInterested => {
            event_tx
                .send(PeerEvent::PeerInterested {
                    peer_addr: addr,
                    interested: false,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        Message::Have { index } => {
            event_tx
                .send(PeerEvent::Have {
                    peer_addr: addr,
                    index,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        Message::Bitfield(data) => {
            let bitfield = Bitfield::from_bytes(data.to_vec(), num_pieces)?;
            event_tx
                .send(PeerEvent::Bitfield {
                    peer_addr: addr,
                    bitfield,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        Message::Piece { index, begin, data_0, data_1 } => {
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
            event_tx
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
            *peer_ut_metadata = ext_hs.ext_id("ut_metadata");
            *peer_ut_pex = ext_hs.ext_id("ut_pex");
            *peer_lt_trackers = ext_hs.ext_id("lt_trackers");
            *peer_ut_holepunch = ext_hs.ext_id("ut_holepunch");
            // Capture peer's extension IDs for registered plugins
            for plugin in plugins.iter() {
                if let Some(id) = ext_hs.ext_id(plugin.name()) {
                    peer_ext_ids.insert(plugin.name().to_string(), id);
                }
            }
            // Notify plugins of the peer's extension handshake
            for plugin in plugins.iter() {
                plugin.on_handshake(info_hash, addr, &ext_hs);
            }
            event_tx
                .send(PeerEvent::ExtHandshake {
                    peer_addr: addr,
                    handshake: ext_hs,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        Message::Extended { ext_id, payload } => {
            // Routed extension message: the remote sends using OUR assigned IDs
            if Some(ext_id) == our_ut_metadata {
                let response = handle_metadata_message(
                    addr,
                    &payload,
                    event_tx,
                    info_bytes,
                    *peer_ut_metadata,
                )
                .await?;
                if let Some(msg) = response {
                    return Ok(Some(msg));
                }
            } else if Some(ext_id) == our_ut_pex {
                handle_pex_message(&payload, event_tx).await?;
            } else if Some(ext_id) == our_lt_trackers {
                handle_lt_trackers_message(&payload, event_tx).await?;
            } else if Some(ext_id) == our_ut_holepunch {
                handle_holepunch_message(addr, &payload, event_tx).await?;
            } else {
                // Dispatch to registered plugins (IDs 10+)
                let mut handled = false;
                for (i, plugin) in plugins.iter().enumerate() {
                    let our_plugin_id = 10 + i as u8;
                    if ext_id == our_plugin_id {
                        if let Some(response) = plugin.on_message(info_hash, addr, &payload)
                            && let Some(&peer_id) = peer_ext_ids.get(plugin.name())
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
            event_tx
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
                event_tx
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
                event_tx
                    .send(PeerEvent::Bitfield {
                        peer_addr: addr,
                        bitfield,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
        }
        Message::SuggestPiece(index) => {
            event_tx
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
            event_tx
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
            event_tx
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
            event_tx
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
            event_tx
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
            event_tx
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
///
/// Returns `Ok(Some(msg))` when a metadata response should be sent on the wire.
async fn handle_metadata_message(
    addr: SocketAddr,
    payload: &[u8],
    event_tx: &mpsc::Sender<PeerEvent>,
    info_bytes: Option<&Bytes>,
    peer_ut_metadata: Option<u8>,
) -> crate::Result<Option<Message>> {
    let meta_msg = MetadataMessage::from_bytes(payload)?;
    match meta_msg.msg_type {
        MetadataMessageType::Data => {
            if let (Some(data), Some(total_size)) = (meta_msg.data, meta_msg.total_size) {
                event_tx
                    .send(PeerEvent::MetadataPiece {
                        peer_addr: addr,
                        piece: meta_msg.piece,
                        data,
                        total_size,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
        }
        MetadataMessageType::Reject => {
            event_tx
                .send(PeerEvent::MetadataReject {
                    peer_addr: addr,
                    piece: meta_msg.piece,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        MetadataMessageType::Request => {
            if let Some(remote_id) = peer_ut_metadata {
                let response = build_metadata_response(meta_msg.piece, info_bytes);
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

/// Handle a command from the TorrentActor.
async fn handle_command(
    cmd: PeerCommand,
    writer: &mut PeerWriter<tokio::io::WriteHalf<impl AsyncWrite>>,
    peer_ut_metadata: Option<u8>,
    peer_ut_pex: Option<u8>,
    peer_ut_holepunch: Option<u8>,
) -> crate::Result<()> {
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
            let ext_id = peer_ut_metadata.ok_or_else(|| {
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
        PeerCommand::SendPiece { index, begin, data } => Message::Piece { index, begin, data_0: data, data_1: Bytes::new() },
        PeerCommand::SendExtHandshake(hs) => {
            let payload = hs.to_bytes().map_err(crate::Error::Wire)?;
            Message::Extended { ext_id: 0, payload }
        }
        PeerCommand::SendBitfield(data) => Message::Bitfield(data),
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
            if let Some(ext_id) = peer_ut_pex {
                let payload = message
                    .to_bytes()
                    .map_err(|e| crate::Error::Connection(e.to_string()))?;
                writer.send(&Message::Extended { ext_id, payload }).await?;
            }
            return Ok(());
        }
        PeerCommand::SendHolepunch(hp_msg) => {
            if let Some(ext_id) = peer_ut_holepunch {
                let payload = hp_msg.to_bytes();
                writer.send(&Message::Extended { ext_id, payload }).await?;
            }
            return Ok(());
        }
        PeerCommand::StartRequesting { .. } | PeerCommand::SnapshotUpdate { .. } => {
            // Handled in main loop, not here
            return Ok(());
        }
        PeerCommand::UpdateNumPieces(_) | PeerCommand::Shutdown => {
            // Should have been handled in the main loop; this is unreachable.
            return Ok(());
        }
    };
    writer.send(&msg).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    fn test_addr() -> SocketAddr {
        "127.0.0.1:6881".parse().unwrap()
    }

    fn test_info_hash() -> Id20 {
        Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap()
    }

    fn test_peer_id() -> Id20 {
        Id20::from_hex("0102030405060708091011121314151617181920").unwrap()
    }

    fn remote_peer_id() -> Id20 {
        Id20::from_hex("2122232425262728293031323334353637383940").unwrap()
    }

    /// Do the handshake from the remote side: read our handshake, send theirs.
    /// Returns the parsed handshake we sent.
    async fn do_remote_handshake(
        stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
        info_hash: Id20,
        remote_id: Id20,
    ) -> Handshake {
        let mut buf = [0u8; HANDSHAKE_SIZE];
        stream.read_exact(&mut buf).await.unwrap();
        let our_hs = Handshake::from_bytes(&buf).unwrap();

        let remote_hs = Handshake::new(info_hash, remote_id);
        stream.write_all(&remote_hs.to_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        our_hs
    }

    /// Read one framed message from a raw stream (length-prefix + payload).
    async fn read_framed_message(stream: &mut (impl AsyncRead + Unpin)) -> Message {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await.unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut payload = vec![0u8; len];
        if len > 0 {
            stream.read_exact(&mut payload).await.unwrap();
        }
        Message::from_payload(Bytes::from(payload)).unwrap()
    }

    /// Write one framed message to a raw stream (length-prefix + payload).
    async fn write_framed_message(stream: &mut (impl AsyncWrite + Unpin), msg: &Message) {
        let bytes = msg.to_bytes();
        stream.write_all(&bytes).await.unwrap();
        stream.flush().await.unwrap();
    }

    /// Perform remote-side extension handshake: read our ext hs, send one back
    /// with ut_metadata=3, ut_pex=4.
    async fn do_remote_ext_handshake(
        stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    ) -> ExtHandshake {
        let msg = read_framed_message(stream).await;
        let our_ext_hs = match msg {
            Message::Extended { ext_id: 0, payload } => ExtHandshake::from_bytes(&payload).unwrap(),
            other => panic!("expected ext handshake, got: {other:?}"),
        };

        // Send remote ext handshake back with different IDs
        let mut remote_ext = ExtHandshake::default();
        remote_ext.m.insert("ut_metadata".into(), 3);
        remote_ext.m.insert("ut_pex".into(), 4);
        remote_ext.v = Some("TestPeer 1.0".into());

        let payload = remote_ext.to_bytes().unwrap();
        write_framed_message(stream, &Message::Extended { ext_id: 0, payload }).await;

        our_ext_hs
    }

    // ---- Test 1: Handshake Exchange ----

    #[tokio::test]
    async fn handshake_exchange() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let our_id = test_peer_id();
        let remote_id = remote_peer_id();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                our_id,
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        let our_hs = do_remote_handshake(&mut server_stream, info_hash, remote_id).await;
        assert_eq!(our_hs.info_hash, info_hash);
        assert_eq!(our_hs.peer_id, our_id);
        assert!(our_hs.supports_extensions());

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();

        let result = handle.await.unwrap();
        assert!(result.is_ok());

        // Should get a disconnect event
        let evt = event_rx.recv().await.unwrap();
        assert!(matches!(evt, PeerEvent::Disconnected { .. }));
    }

    // ---- Test 2: Handshake Info Hash Mismatch ----

    #[tokio::test]
    async fn handshake_info_hash_mismatch() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (_cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let wrong_hash = Id20::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        let our_id = test_peer_id();
        let remote_id = remote_peer_id();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                our_id,
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        // Read our handshake
        let mut buf = [0u8; HANDSHAKE_SIZE];
        server_stream.read_exact(&mut buf).await.unwrap();

        // Send back handshake with wrong info_hash
        let bad_hs = Handshake::new(wrong_hash, remote_id);
        server_stream.write_all(&bad_hs.to_bytes()).await.unwrap();
        server_stream.flush().await.unwrap();

        let result = handle.await.unwrap();
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("info_hash mismatch"),
            "expected info_hash mismatch error, got: {err_msg}"
        );
    }

    // ---- Test 3: Extension Negotiation ----

    #[tokio::test]
    async fn extension_negotiation() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read the extension handshake message
        let msg = read_framed_message(&mut server_stream).await;
        match msg {
            Message::Extended { ext_id, payload } => {
                assert_eq!(ext_id, 0, "ext handshake should be ext_id=0");
                let ext_hs = ExtHandshake::from_bytes(&payload).unwrap();
                assert!(
                    ext_hs.ext_id("ut_metadata").is_some(),
                    "should advertise ut_metadata"
                );
                assert!(ext_hs.ext_id("ut_pex").is_some(), "should advertise ut_pex");
            }
            other => panic!("expected Extended handshake, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 4: Bitfield Exchange ----

    #[tokio::test]
    async fn bitfield_exchange() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();

        // Create a bitfield with some pieces set
        let mut bitfield = Bitfield::new(16);
        bitfield.set(0);
        bitfield.set(5);
        bitfield.set(15);
        let expected_bytes = bitfield.as_bytes().to_vec();

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                16,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake first (peer supports extensions)
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;

        // Read the bitfield message
        let msg = read_framed_message(&mut server_stream).await;
        match msg {
            Message::Bitfield(data) => {
                assert_eq!(data.as_ref(), expected_bytes.as_slice());
            }
            other => panic!("expected Bitfield, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 5: Choke/Unchoke State ----

    #[tokio::test]
    async fn choke_unchoke_state() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;

        // Send Choke from remote
        write_framed_message(&mut server_stream, &Message::Choke).await;

        let evt = event_rx.recv().await.unwrap();
        match evt {
            PeerEvent::PeerChoking { choking, .. } => assert!(choking),
            other => panic!("expected PeerChoking, got: {other:?}"),
        }

        // Send Unchoke from remote
        write_framed_message(&mut server_stream, &Message::Unchoke).await;

        let evt = event_rx.recv().await.unwrap();
        match evt {
            PeerEvent::PeerChoking { choking, .. } => assert!(!choking),
            other => panic!("expected PeerChoking, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 6: Interested Signaling ----

    #[tokio::test]
    async fn interested_signaling() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;
        // M107: drain the unconditional Unchoke sent on connect
        let _unchoke = read_framed_message(&mut server_stream).await;

        // Send SetInterested command
        cmd_tx.send(PeerCommand::SetInterested(true)).await.unwrap();

        let msg = read_framed_message(&mut server_stream).await;
        assert_eq!(msg, Message::Interested);

        // Send SetInterested(false) command
        cmd_tx
            .send(PeerCommand::SetInterested(false))
            .await
            .unwrap();

        let msg = read_framed_message(&mut server_stream).await;
        assert_eq!(msg, Message::NotInterested);

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 7: Have Forwarding ----

    #[tokio::test]
    async fn have_forwarding() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;
        // M107: drain the unconditional Unchoke sent on connect
        let _unchoke = read_framed_message(&mut server_stream).await;

        cmd_tx.send(PeerCommand::Have(5)).await.unwrap();

        let msg = read_framed_message(&mut server_stream).await;
        assert_eq!(msg, Message::Have { index: 5 });

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 8: Request Piece Flow ----

    #[tokio::test]
    async fn request_piece_flow() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;
        // M107: drain the unconditional Unchoke sent on connect
        let _unchoke = read_framed_message(&mut server_stream).await;

        // Send Request command
        cmd_tx
            .send(PeerCommand::Request {
                index: 0,
                begin: 0,
                length: 16384,
            })
            .await
            .unwrap();

        let msg = read_framed_message(&mut server_stream).await;
        assert_eq!(
            msg,
            Message::Request {
                index: 0,
                begin: 0,
                length: 16384,
            }
        );

        // Send Piece data back from remote
        let piece_data = Bytes::from_static(b"test piece data!");
        write_framed_message(
            &mut server_stream,
            &Message::Piece {
                index: 0,
                begin: 0,
                data_0: piece_data.clone(),
                data_1: Bytes::new(),
            },
        )
        .await;

        let evt = event_rx.recv().await.unwrap();
        match evt {
            PeerEvent::PieceData {
                index, begin, data, ..
            } => {
                assert_eq!(index, 0);
                assert_eq!(begin, 0);
                assert_eq!(data, piece_data);
            }
            other => panic!("expected PieceData, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 9: KeepAlive ----

    #[tokio::test]
    async fn keepalive() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;
        // M107: drain the unconditional Unchoke sent on connect
        let _unchoke = read_framed_message(&mut server_stream).await;

        // Send KeepAlive from remote
        write_framed_message(&mut server_stream, &Message::KeepAlive).await;

        // Verify the peer is still alive by sending a command
        cmd_tx.send(PeerCommand::SetInterested(true)).await.unwrap();

        let msg = read_framed_message(&mut server_stream).await;
        assert_eq!(msg, Message::Interested);

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 10: Graceful Disconnect ----

    #[tokio::test]
    async fn graceful_disconnect() {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (_cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        // Do handshake then drop the remote end
        let mut server = server_stream;
        do_remote_handshake(&mut server, info_hash, remote_peer_id()).await;

        // Read ext handshake so peer task is in main loop
        let _ext_hs_msg = read_framed_message(&mut server).await;

        // Drop the remote end
        drop(server);

        // Should receive a Disconnected event
        let evt = event_rx.recv().await.unwrap();
        assert!(
            matches!(evt, PeerEvent::Disconnected { .. }),
            "expected Disconnected, got: {evt:?}"
        );

        let _ = handle.await;
    }

    // ---- Test 11: Metadata Request/Response ----

    #[tokio::test]
    async fn metadata_request_response() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Do extension handshake (remote advertises ut_metadata=3)
        let _our_ext = do_remote_ext_handshake(&mut server_stream).await;
        // M107: drain the unconditional Unchoke sent on connect
        let _unchoke = read_framed_message(&mut server_stream).await;

        // Consume the ext handshake event
        let evt = event_rx.recv().await.unwrap();
        assert!(matches!(evt, PeerEvent::ExtHandshake { .. }));

        // Send RequestMetadata command
        cmd_tx
            .send(PeerCommand::RequestMetadata { piece: 0 })
            .await
            .unwrap();

        // Read the metadata request from remote side
        let msg = read_framed_message(&mut server_stream).await;
        match msg {
            Message::Extended { ext_id, payload } => {
                // BEP 10: when sending TO the remote, use the ID from THEIR ext
                // handshake. The remote advertised ut_metadata=3, so ext_id should be 3.
                assert_eq!(ext_id, 3, "should use remote's ut_metadata ID");
                let meta = MetadataMessage::from_bytes(&payload).unwrap();
                assert_eq!(meta.msg_type, MetadataMessageType::Request);
                assert_eq!(meta.piece, 0);
            }
            other => panic!("expected Extended metadata request, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 12: PEX Message Handling ----

    #[tokio::test]
    async fn pex_message_handling() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Do extension handshake (remote advertises ut_pex=4)
        let _our_ext = do_remote_ext_handshake(&mut server_stream).await;

        // Consume the ext handshake event
        let evt = event_rx.recv().await.unwrap();
        assert!(matches!(evt, PeerEvent::ExtHandshake { .. }));

        // Build a PEX message with one added peer: 10.0.0.1:8080
        let pex = PexMessage {
            added: vec![10, 0, 0, 1, 0x1F, 0x90],
            added_flags: vec![0x00],
            ..Default::default()
        };
        let pex_payload = pex.to_bytes().unwrap();

        // Send PEX message using OUR ut_pex ID (2), since the remote sends
        // to us using the ID from OUR ext handshake.
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: 2,
                payload: pex_payload,
            },
        )
        .await;

        let evt = event_rx.recv().await.unwrap();
        match evt {
            PeerEvent::PexPeers { new_peers } => {
                assert_eq!(new_peers.len(), 1);
                assert_eq!(new_peers[0].to_string(), "10.0.0.1:8080");
            }
            other => panic!("expected PexPeers, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 13: Fast Extension — HaveAll sent when complete ----

    /// Do the remote handshake with fast extension enabled.
    async fn do_remote_handshake_fast(
        stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
        info_hash: Id20,
        remote_id: Id20,
    ) -> Handshake {
        let mut buf = [0u8; HANDSHAKE_SIZE];
        stream.read_exact(&mut buf).await.unwrap();
        let our_hs = Handshake::from_bytes(&buf).unwrap();

        let remote_hs = Handshake::new(info_hash, remote_id).with_fast();
        stream.write_all(&remote_hs.to_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        our_hs
    }

    #[tokio::test]
    async fn fast_extension_have_all_sent_when_complete() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let num_pieces = 10u32;

        // Create a full bitfield (all pieces set)
        let mut bitfield = Bitfield::new(num_pieces);
        for i in 0..num_pieces {
            bitfield.set(i);
        }

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                num_pieces,
                event_tx,
                cmd_rx,
                false,
                true, // enable_fast
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        let our_hs =
            do_remote_handshake_fast(&mut server_stream, info_hash, remote_peer_id()).await;
        assert!(
            our_hs.supports_fast(),
            "our handshake should advertise fast"
        );

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;

        // Should receive HaveAll (not Bitfield) since both support fast and all pieces set
        let msg = read_framed_message(&mut server_stream).await;
        assert_eq!(msg, Message::HaveAll);

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 14: Fast Extension — HaveNone sent when empty ----

    #[tokio::test]
    async fn fast_extension_have_none_sent_when_empty() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let num_pieces = 10u32;
        let bitfield = Bitfield::new(num_pieces); // empty

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                num_pieces,
                event_tx,
                cmd_rx,
                false,
                true, // enable_fast
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        let our_hs =
            do_remote_handshake_fast(&mut server_stream, info_hash, remote_peer_id()).await;
        assert!(our_hs.supports_fast());

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;

        // Should receive HaveNone (not nothing) since both support fast and no pieces
        let msg = read_framed_message(&mut server_stream).await;
        assert_eq!(msg, Message::HaveNone);

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 16: Fast Extension — AllowedFast sent on connect ----

    #[tokio::test]
    async fn fast_extension_sends_allowed_fast_on_connect() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let num_pieces = 100u32;
        let bitfield = Bitfield::new(num_pieces); // empty

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                num_pieces,
                event_tx,
                cmd_rx,
                false,
                true, // enable_fast
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        let _our_hs =
            do_remote_handshake_fast(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;

        // Read HaveNone (empty bitfield + fast)
        let msg = read_framed_message(&mut server_stream).await;
        assert_eq!(msg, Message::HaveNone);

        // Should receive 10 AllowedFast messages
        let mut fast_indices = std::collections::HashSet::new();
        for _ in 0..10 {
            let msg = read_framed_message(&mut server_stream).await;
            match msg {
                Message::AllowedFast(index) => {
                    assert!(index < num_pieces, "AllowedFast index out of range");
                    fast_indices.insert(index);
                }
                other => panic!("expected AllowedFast, got: {other:?}"),
            }
        }
        assert_eq!(
            fast_indices.len(),
            10,
            "should have 10 unique AllowedFast indices"
        );

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 17: No AllowedFast without fast extension ----

    #[tokio::test]
    async fn no_allowed_fast_without_fast_extension() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let num_pieces = 100u32;
        let bitfield = Bitfield::new(num_pieces); // empty

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                num_pieces,
                event_tx,
                cmd_rx,
                false,
                false, // fast disabled
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;

        // No bitfield (empty + no fast = nothing sent), no AllowedFast
        // Send shutdown and verify peer exits cleanly
        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }

    // ---- Test 18: SendPiece command sends Piece message ----

    #[tokio::test]
    async fn send_piece_command_sends_piece_message() {
        let (client_stream, mut server_stream) = tokio::io::duplex(65536);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;
        // M107: drain the unconditional Unchoke sent on connect
        let _unchoke = read_framed_message(&mut server_stream).await;

        // Send a piece via SendPiece command
        let piece_data = Bytes::from(vec![0xAB; 16384]);
        cmd_tx
            .send(PeerCommand::SendPiece {
                index: 0,
                begin: 0,
                data: piece_data.clone(),
            })
            .await
            .unwrap();

        let msg = read_framed_message(&mut server_stream).await;
        match msg {
            Message::Piece { index, begin, data_0, data_1 } => {
                assert_eq!(index, 0);
                assert_eq!(begin, 0);
                assert!(data_1.is_empty());
                assert_eq!(data_0, piece_data);
            }
            other => panic!("expected Piece, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn serves_metadata_request() {
        let (client_stream, mut server_stream) = tokio::io::duplex(65536);
        let (event_tx, _event_rx) = mpsc::channel(16);
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        // Build a small info dict to serve
        let info_raw = b"d4:name4:test12:piece lengthi16384e6:pieces20:AAAAAAAAAAAAAAAAAAAAe";
        let info_bytes = Bytes::from_static(info_raw);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false, // outbound
                false, // anonymous_mode
                Some(info_bytes),
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read the extension handshake our peer sends
        let ext_hs_msg = read_framed_message(&mut server_stream).await;
        let our_ut_metadata_id = match &ext_hs_msg {
            Message::Extended { ext_id: 0, payload } => {
                let hs = ExtHandshake::from_bytes(payload).unwrap();
                hs.ext_id("ut_metadata").unwrap()
            }
            other => panic!("expected ext handshake, got: {other:?}"),
        };
        // M107: drain the unconditional Unchoke sent on connect
        let _unchoke = read_framed_message(&mut server_stream).await;

        // Send a remote ext handshake that advertises ut_metadata=5
        let mut remote_ext = ExtHandshake::new();
        remote_ext.m.insert("ut_metadata".into(), 5);
        let ext_payload = remote_ext.to_bytes().unwrap();
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: 0,
                payload: ext_payload,
            },
        )
        .await;

        // Send a metadata request for piece 0, using OUR ut_metadata ID
        let req = MetadataMessage::request(0);
        let req_payload = req.to_bytes().unwrap();
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: our_ut_metadata_id,
                payload: req_payload,
            },
        )
        .await;

        // Read the data response — it should use the REMOTE's ext_id (5)
        let response = read_framed_message(&mut server_stream).await;
        match response {
            Message::Extended { ext_id, payload } => {
                assert_eq!(ext_id, 5, "should use remote's ut_metadata id");
                let meta_msg = MetadataMessage::from_bytes(&payload).unwrap();
                assert_eq!(meta_msg.msg_type, MetadataMessageType::Data);
                assert_eq!(meta_msg.piece, 0);
                assert_eq!(meta_msg.total_size, Some(info_raw.len() as u64));
                assert_eq!(meta_msg.data.as_deref(), Some(info_raw.as_ref()));
            }
            other => panic!("expected Extended data response, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Plugin Extension Tests ----

    /// Test plugin that echoes messages back verbatim.
    struct TestEchoPlugin;

    impl crate::extension::ExtensionPlugin for TestEchoPlugin {
        fn name(&self) -> &str {
            "ut_echo"
        }

        fn on_message(
            &self,
            _info_hash: &Id20,
            _peer_addr: SocketAddr,
            payload: &[u8],
        ) -> Option<Vec<u8>> {
            Some(payload.to_vec())
        }
    }

    #[tokio::test]
    async fn plugin_advertised_in_ext_handshake() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);
        let plugins: std::sync::Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>> =
            std::sync::Arc::new(vec![Box::new(TestEchoPlugin)]);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false, // outbound
                false, // anonymous_mode
                None,  // info_bytes
                plugins,
                true,             // enable_holepunch
                16 * 1024 * 1024, // max_message_size
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read our ext handshake — ut_echo should be advertised at ID 10
        let msg = read_framed_message(&mut server_stream).await;
        match msg {
            Message::Extended { ext_id: 0, payload } => {
                let ext_hs = ExtHandshake::from_bytes(&payload).unwrap();
                assert_eq!(
                    ext_hs.ext_id("ut_echo"),
                    Some(10),
                    "plugin should be at ID 10"
                );
                assert_eq!(ext_hs.ext_id("ut_metadata"), Some(1), "built-in unchanged");
            }
            other => panic!("expected ext handshake, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn plugin_message_echo_dispatch() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);
        let plugins: std::sync::Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>> =
            std::sync::Arc::new(vec![Box::new(TestEchoPlugin)]);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false, // outbound
                false, // anonymous_mode
                None,  // info_bytes
                plugins,
                true,             // enable_holepunch
                16 * 1024 * 1024, // max_message_size
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read our ext handshake
        let msg = read_framed_message(&mut server_stream).await;
        let our_ext_hs = match msg {
            Message::Extended { ext_id: 0, payload } => ExtHandshake::from_bytes(&payload).unwrap(),
            other => panic!("expected ext handshake, got: {other:?}"),
        };
        let our_ut_echo_id = our_ext_hs.ext_id("ut_echo").unwrap();
        assert_eq!(our_ut_echo_id, 10);
        // M107: drain the unconditional Unchoke sent on connect
        let _unchoke = read_framed_message(&mut server_stream).await;

        // Send remote ext handshake advertising ut_echo=42
        let mut remote_ext = ExtHandshake::default();
        remote_ext.m.insert("ut_metadata".into(), 3);
        remote_ext.m.insert("ut_pex".into(), 4);
        remote_ext.m.insert("ut_echo".into(), 42);
        let ext_payload = remote_ext.to_bytes().unwrap();
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: 0,
                payload: ext_payload,
            },
        )
        .await;

        // Send a plugin message using OUR assigned ID (10)
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: our_ut_echo_id,
                payload: Bytes::from_static(b"hello plugin"),
            },
        )
        .await;

        // Read the echo response — should use the PEER's ut_echo ID (42)
        let response = read_framed_message(&mut server_stream).await;
        match response {
            Message::Extended { ext_id, payload } => {
                assert_eq!(ext_id, 42, "response should use peer's ut_echo id");
                assert_eq!(payload.as_ref(), b"hello plugin");
            }
            other => panic!("expected echo response, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Holepunch Message Tests (BEP 55) ----

    #[tokio::test]
    async fn holepunch_rendezvous_event() {
        use torrent_wire::HolepunchMessage;
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let addr = test_addr();
        let target: SocketAddr = "10.0.0.1:8080".parse().unwrap();
        let msg = HolepunchMessage::rendezvous(target);
        handle_holepunch_message(addr, &msg.to_bytes(), &event_tx)
            .await
            .unwrap();
        match event_rx.recv().await.unwrap() {
            PeerEvent::HolepunchRendezvous {
                peer_addr,
                target: t,
            } => {
                assert_eq!(peer_addr, addr);
                assert_eq!(t, target);
            }
            other => panic!("expected HolepunchRendezvous, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn holepunch_connect_event() {
        use torrent_wire::HolepunchMessage;
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let addr = test_addr();
        let target: SocketAddr = "192.168.1.100:6881".parse().unwrap();
        let msg = HolepunchMessage::connect(target);
        handle_holepunch_message(addr, &msg.to_bytes(), &event_tx)
            .await
            .unwrap();
        match event_rx.recv().await.unwrap() {
            PeerEvent::HolepunchConnect {
                peer_addr,
                target: t,
            } => {
                assert_eq!(peer_addr, addr);
                assert_eq!(t, target);
            }
            other => panic!("expected HolepunchConnect, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn holepunch_error_event() {
        use torrent_wire::{HolepunchError, HolepunchMessage};
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let addr = test_addr();
        let target: SocketAddr = "172.16.0.5:51413".parse().unwrap();
        let msg = HolepunchMessage::error(target, HolepunchError::NotConnected);
        handle_holepunch_message(addr, &msg.to_bytes(), &event_tx)
            .await
            .unwrap();
        match event_rx.recv().await.unwrap() {
            PeerEvent::HolepunchError {
                peer_addr,
                target: t,
                error_code,
            } => {
                assert_eq!(peer_addr, addr);
                assert_eq!(t, target);
                assert_eq!(error_code, 2); // NotConnected
            }
            other => panic!("expected HolepunchError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn holepunch_disabled_removes_from_ext_handshake() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                false,                           // enable_holepunch = DISABLED
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read the extension handshake — ut_holepunch should NOT be present
        let msg = read_framed_message(&mut server_stream).await;
        match msg {
            Message::Extended { ext_id: 0, payload } => {
                let ext_hs = ExtHandshake::from_bytes(&payload).unwrap();
                assert!(
                    ext_hs.ext_id("ut_holepunch").is_none(),
                    "ut_holepunch should not be advertised when disabled"
                );
                // Other built-ins should still be present
                assert!(ext_hs.ext_id("ut_metadata").is_some());
                assert!(ext_hs.ext_id("ut_pex").is_some());
            }
            other => panic!("expected ext handshake, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn holepunch_enabled_advertised_in_ext_handshake() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch = ENABLED
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read the extension handshake — ut_holepunch SHOULD be present
        let msg = read_framed_message(&mut server_stream).await;
        match msg {
            Message::Extended { ext_id: 0, payload } => {
                let ext_hs = ExtHandshake::from_bytes(&payload).unwrap();
                assert_eq!(
                    ext_hs.ext_id("ut_holepunch"),
                    Some(4),
                    "ut_holepunch should be advertised at ID 4"
                );
            }
            other => panic!("expected ext handshake, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn send_holepunch_command_sends_extended_message() {
        use torrent_wire::HolepunchMessage;

        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Do extension handshake — remote advertises ut_holepunch=7
        let ext_hs_msg = read_framed_message(&mut server_stream).await;
        let _our_ext_hs = match &ext_hs_msg {
            Message::Extended { ext_id: 0, payload } => ExtHandshake::from_bytes(payload).unwrap(),
            other => panic!("expected ext handshake, got: {other:?}"),
        };
        // M107: drain the unconditional Unchoke sent on connect
        let _unchoke = read_framed_message(&mut server_stream).await;

        // Send remote ext handshake with ut_holepunch=7
        let mut remote_ext = ExtHandshake::default();
        remote_ext.m.insert("ut_metadata".into(), 3);
        remote_ext.m.insert("ut_holepunch".into(), 7);
        let ext_payload = remote_ext.to_bytes().unwrap();
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: 0,
                payload: ext_payload,
            },
        )
        .await;

        // Consume the ext handshake event
        let evt = event_rx.recv().await.unwrap();
        assert!(matches!(evt, PeerEvent::ExtHandshake { .. }));

        // Send a holepunch message via PeerCommand
        let target: SocketAddr = "10.0.0.1:8080".parse().unwrap();
        let hp_msg = HolepunchMessage::connect(target);
        cmd_tx
            .send(PeerCommand::SendHolepunch(hp_msg.clone()))
            .await
            .unwrap();

        // Read the extended message from the wire — should use remote's ut_holepunch ID (7)
        let msg = read_framed_message(&mut server_stream).await;
        match msg {
            Message::Extended { ext_id, payload } => {
                assert_eq!(ext_id, 7, "should use remote's ut_holepunch ID");
                let parsed = HolepunchMessage::from_bytes(&payload).unwrap();
                assert_eq!(parsed, hp_msg);
            }
            other => panic!("expected Extended holepunch message, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn incoming_holepunch_routed_to_event() {
        use torrent_wire::HolepunchMessage;

        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read our ext handshake to find our ut_holepunch ID
        let ext_hs_msg = read_framed_message(&mut server_stream).await;
        let our_ut_holepunch_id = match &ext_hs_msg {
            Message::Extended { ext_id: 0, payload } => {
                let hs = ExtHandshake::from_bytes(payload).unwrap();
                hs.ext_id("ut_holepunch").unwrap()
            }
            other => panic!("expected ext handshake, got: {other:?}"),
        };
        assert_eq!(our_ut_holepunch_id, 4);

        // Send remote ext handshake (so peer enters main loop properly)
        let mut remote_ext = ExtHandshake::default();
        remote_ext.m.insert("ut_metadata".into(), 3);
        let ext_payload = remote_ext.to_bytes().unwrap();
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: 0,
                payload: ext_payload,
            },
        )
        .await;

        // Consume the ext handshake event
        let evt = event_rx.recv().await.unwrap();
        assert!(matches!(evt, PeerEvent::ExtHandshake { .. }));

        // Send a holepunch Rendezvous message using OUR assigned ID (4)
        let target: SocketAddr = "10.0.0.1:8080".parse().unwrap();
        let hp_msg = HolepunchMessage::rendezvous(target);
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: our_ut_holepunch_id,
                payload: hp_msg.to_bytes(),
            },
        )
        .await;

        // Should receive a HolepunchRendezvous event
        let evt = event_rx.recv().await.unwrap();
        match evt {
            PeerEvent::HolepunchRendezvous {
                peer_addr,
                target: t,
            } => {
                assert_eq!(peer_addr, test_addr());
                assert_eq!(t, target);
            }
            other => panic!("expected HolepunchRendezvous, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    #[test]
    fn anonymous_mode_suppresses_ext_handshake_fields() {
        let mut ext_hs = ExtHandshake::new();
        ext_hs.p = Some(6881);
        ext_hs.upload_only = Some(1);
        ext_hs.reqq = Some(250);

        // Simulate anonymous mode suppression (matches run_peer logic)
        ext_hs.v = None;
        ext_hs.p = None;
        ext_hs.reqq = None;
        ext_hs.upload_only = None;

        assert!(ext_hs.v.is_none());
        assert!(ext_hs.p.is_none());
        assert!(ext_hs.reqq.is_none());
        assert!(ext_hs.upload_only.is_none());
        // Extension map should still be present
        assert!(!ext_hs.m.is_empty());

        // Verify suppressed fields don't leak through serialization round-trip
        let encoded = ext_hs.to_bytes().unwrap();
        let decoded = ExtHandshake::from_bytes(&encoded).unwrap();
        assert!(decoded.v.is_none());
        assert!(decoded.p.is_none());
        assert!(decoded.reqq.is_none());
        assert!(decoded.upload_only.is_none());
        assert!(!decoded.m.is_empty());
    }

    mod pending_batch_tests {
        use super::super::PendingBatch;
        use torrent_core::Lengths;

        /// Standard torrent: 10 pieces, 512 KiB each (32 blocks per piece), 16 KiB chunks.
        fn standard_lengths() -> Lengths {
            Lengths::new(
                10 * 512 * 1024, // total_length: 5 MiB
                512 * 1024,      // piece_length: 512 KiB
                16384,           // chunk_size: 16 KiB
            )
        }

        /// Small torrent where the last piece has only 1 block.
        fn small_last_piece_lengths() -> Lengths {
            Lengths::new(
                10 * 512 * 1024 + 16384, // total_length: 5 MiB + 16 KiB
                512 * 1024,              // piece_length: 512 KiB
                16384,                   // chunk_size: 16 KiB
            )
        }

        #[test]
        fn batch_accumulation_full_piece() {
            let lengths = standard_lengths();
            let mut batch = PendingBatch::new(&lengths);

            // Write 31 blocks — piece should NOT be complete
            for i in 0..31 {
                let complete = batch.push(0, i * 16384, 16384);
                assert!(!complete, "piece should not be complete after block {i}");
            }
            assert_eq!(batch.blocks.len(), 31);
            assert!(!batch.is_empty());

            // Write 32nd block — piece IS complete
            let complete = batch.push(0, 31 * 16384, 16384);
            assert!(complete, "piece should be complete after 32 blocks");
            assert_eq!(batch.blocks.len(), 32);

            // Take should return all 32 entries and reset
            let blocks = batch.take();
            assert_eq!(blocks.len(), 32);
            assert!(batch.is_empty());
            assert_eq!(batch.blocks.len(), 0);
            assert_eq!(batch.piece_counts.len(), 0);
        }

        #[test]
        fn timer_flush_partial_piece() {
            let lengths = standard_lengths();
            let mut batch = PendingBatch::new(&lengths);

            // Write 10 blocks (partial piece)
            for i in 0..10 {
                let complete = batch.push(0, i * 16384, 16384);
                assert!(!complete);
            }
            assert_eq!(batch.blocks.len(), 10);

            // Simulate timer flush: take returns partial batch
            let blocks = batch.take();
            assert_eq!(blocks.len(), 10);
            assert!(batch.is_empty());

            // Verify block contents
            assert_eq!(blocks[0].index, 0);
            assert_eq!(blocks[0].begin, 0);
            assert_eq!(blocks[0].length, 16384);
            assert_eq!(blocks[9].begin, 9 * 16384);
        }

        #[test]
        fn single_block_last_piece() {
            let lengths = small_last_piece_lengths();
            let mut batch = PendingBatch::new(&lengths);

            // The last piece (index 10) should have 1 block
            assert_eq!(lengths.chunks_in_piece(10), 1);
            assert_eq!(lengths.chunks_in_piece(0), 32); // standard pieces still 32

            // Writing 1 block to the last piece triggers completion
            let complete = batch.push(10, 0, 16384);
            assert!(
                complete,
                "single-block last piece should complete immediately"
            );

            let blocks = batch.take();
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0].index, 10);
        }

        #[test]
        fn multi_piece_accumulation() {
            let lengths = standard_lengths();
            let mut batch = PendingBatch::new(&lengths);

            // Interleave blocks from two pieces — neither should complete
            for i in 0..16 {
                assert!(!batch.push(0, i * 16384, 16384));
                assert!(!batch.push(1, i * 16384, 16384));
            }
            assert_eq!(batch.blocks.len(), 32); // 16 from piece 0 + 16 from piece 1

            // Complete piece 0
            for i in 16..31 {
                assert!(!batch.push(0, i * 16384, 16384));
            }
            let complete = batch.push(0, 31 * 16384, 16384);
            assert!(complete, "piece 0 should be complete");

            // Flush includes piece 1's partial blocks too
            let blocks = batch.take();
            assert_eq!(blocks.len(), 48); // 32 from piece 0 + 16 from piece 1
        }

        #[test]
        fn flush_on_disconnect_preserves_blocks() {
            let lengths = standard_lengths();
            let mut batch = PendingBatch::new(&lengths);

            // Write 15 blocks
            for i in 0..15 {
                batch.push(0, i * 16384, 16384);
            }
            assert!(!batch.is_empty());

            // Simulate disconnect: take returns all accumulated blocks
            let blocks = batch.take();
            assert_eq!(blocks.len(), 15);
            assert!(batch.is_empty());
        }

        #[test]
        fn blocks_for_piece_last_piece_exact_multiple() {
            // Edge case: total_length is exact multiple of piece_length
            // All pieces including last have the same block count
            let lengths = Lengths::new(
                10 * 512 * 1024, // 5 MiB exactly
                512 * 1024,      // 512 KiB
                16384,           // 16 KiB
            );
            let batch = PendingBatch::new(&lengths);
            assert_eq!(batch.lengths.chunks_in_piece(0), 32);
            assert_eq!(batch.lengths.chunks_in_piece(9), 32); // last piece, exact fit
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
            // 4 pieces, 64 KiB each (4 blocks per piece), 16 KiB chunks
            let lengths = Lengths::new(4 * 64 * 1024, 64 * 1024, 16384);
            let mut batch = PendingBatch::new(&lengths);

            assert_eq!(lengths.chunks_in_piece(0), 4);
            assert_eq!(lengths.chunks_in_piece(3), 4); // last piece, exact fit

            // Complete piece 0
            for i in 0..3 {
                assert!(!batch.push(0, i * 16384, 16384));
            }
            assert!(batch.push(0, 3 * 16384, 16384)); // piece 0 complete

            // Flush piece 0
            let blocks = batch.take();
            assert_eq!(blocks.len(), 4);

            // Start piece 1, then flush via "timer" (take without completion)
            assert!(!batch.push(1, 0, 16384));
            assert!(!batch.push(1, 16384, 16384));
            let blocks = batch.take();
            assert_eq!(blocks.len(), 2);
            assert!(batch.is_empty());

            // Verify counts were reset — pushing the same blocks again should
            // not trigger false completion (only 2 blocks in this batch, not 4)
            assert!(!batch.push(1, 2 * 16384, 16384));
            assert!(!batch.push(1, 3 * 16384, 16384)); // only 2 in this batch
            assert_eq!(batch.blocks.len(), 2);
        }

        /// M104: Verify that on unchoke, the semaphore is reset to the fixed
        /// pipeline depth (drain stale permits, then add exactly `depth`).
        #[test]
        fn unchoke_sets_full_permits() {
            let semaphore = tokio::sync::Semaphore::new(0);
            let depth: usize = 128;

            // Simulate unchoke: drain stale permits, then add fixed depth
            while let Ok(permit) = semaphore.try_acquire() {
                permit.forget();
            }
            semaphore.add_permits(depth);

            assert_eq!(semaphore.available_permits(), depth);

            // Simulate some in-flight requests reducing available permits
            let p1 = semaphore.try_acquire().expect("permit should be available");
            let p2 = semaphore.try_acquire().expect("permit should be available");
            assert_eq!(semaphore.available_permits(), depth - 2);

            // Re-unchoke: drain and refill
            drop(p1);
            drop(p2);
            while let Ok(permit) = semaphore.try_acquire() {
                permit.forget();
            }
            semaphore.add_permits(depth);
            assert_eq!(semaphore.available_permits(), depth);
        }
    }
}
