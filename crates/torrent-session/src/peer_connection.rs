//! `PeerConnection<H>` — the generic transport-level select! loop.
//!
//! Extracted from the monolithic `run_peer()` function. The loop is generic
//! over `H: PeerConnectionHandler`, allowing the handler to be swapped out
//! without changing the protocol-level machinery.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, warn};

use torrent_wire::Message;

use crate::peer_codec::{FillStatus, PeerReader, PeerWriter};
use crate::peer_handler::{PeerConnectionHandler, PieceAction};
use crate::types::{BlockEntry, PeerCommand, PeerEvent};

/// Transport-level peer connection that drives the select! loop.
///
/// Generic over `H` (handler), `R` (reader half), and `W` (writer half).
/// In production, `H = TorrentPeerHandler`, and R/W are the split halves of
/// an MSE stream. Monomorphisation ensures zero vtable overhead.
pub(crate) struct PeerConnection<H, R, W> {
    handler: H,
    reader: PeerReader<R>,
    writer: PeerWriter<W>,
    cmd_rx: mpsc::Receiver<PeerCommand>,
    event_tx: mpsc::Sender<PeerEvent>,
    flush_interval: tokio::time::Interval,
    addr: SocketAddr,
    // M118: Broadcast receiver for Have messages from TorrentActor.
    have_broadcast_rx: tokio::sync::broadcast::Receiver<u32>,
    broadcast_closed: bool,
}

impl<H, R, W> PeerConnection<H, R, W>
where
    H: PeerConnectionHandler,
    R: crate::vectored_io::AsyncReadVectored,
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    /// Construct a new connection.
    pub(crate) fn new(
        handler: H,
        reader: PeerReader<R>,
        writer: PeerWriter<W>,
        cmd_rx: mpsc::Receiver<PeerCommand>,
        event_tx: mpsc::Sender<PeerEvent>,
        have_broadcast_rx: tokio::sync::broadcast::Receiver<u32>,
    ) -> Self {
        let mut flush_interval = tokio::time::interval(Duration::from_millis(25));
        // The first tick completes immediately; we want to skip it so the
        // flush arm doesn't fire before any data has been accumulated.
        // We cannot `.await` in a constructor, so we use `reset()` to push
        // the first deadline into the future.
        flush_interval.reset();
        let addr = handler.addr();
        Self {
            handler,
            reader,
            writer,
            cmd_rx,
            event_tx,
            flush_interval,
            addr,
            have_broadcast_rx,
            broadcast_closed: false,
        }
    }

    /// Run the main select! loop until the connection terminates.
    ///
    /// On return the handler's `on_disconnect()` has already been called.
    pub(crate) async fn run(mut self) -> crate::Result<()> {
        // Skip the initial immediate tick of the flush interval.
        self.flush_interval.tick().await;

        let disconnect_reason: Option<String> = loop {
            let can_request = self.handler.can_request();
            // Extract Arc handles before select! so the semaphore arm doesn't
            // borrow self.handler during the entire select! expansion.
            let semaphore = self.handler.semaphore();
            let has_pending = self.handler.has_pending_batch();

            tokio::select! {
                biased;

                // ----- ARM 1: Wire reader -----
                status = self.reader.fill_message() => {
                    match status {
                        Ok(FillStatus::Ready) => {
                            if let Err(reason) = self.handle_ready_message().await {
                                break Some(reason);
                            }
                        }
                        Ok(FillStatus::Eof) => {
                            break Some("connection closed".into());
                        }
                        Ok(FillStatus::Oversized(length)) => {
                            let msg = match self.reader.decode_oversized_into(length).await {
                                Ok(m) => m,
                                Err(e) => {
                                    debug!(addr = %self.addr, "oversized decode error: {e}");
                                    break Some(e.to_string());
                                }
                            };
                            if let Err(reason) = self.dispatch_message(msg).await {
                                break Some(reason);
                            }
                        }
                        Err(e) => {
                            debug!(addr = %self.addr, "wire error: {e}");
                            break Some(e.to_string());
                        }
                    }
                }

                // ----- ARM 2: Have broadcast from TorrentActor (M118, rqbit pattern) -----
                result = self.have_broadcast_rx.recv(), if !self.broadcast_closed => {
                    match result {
                        Ok(piece_index) => {
                            if self.handler.should_transmit_have(piece_index)
                                && let Err(e) = self.writer.send(&Message::Have { index: piece_index }).await
                            {
                                debug!(addr = %self.addr, "error sending Have: {e}");
                                break Some(e.to_string());
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            debug!(addr = %self.addr, skipped = n, "have broadcast lagged");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            self.broadcast_closed = true;
                        }
                    }
                }

                // ----- ARM 3: Commands from TorrentActor -----
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(PeerCommand::Shutdown) => {
                            break None;
                        }
                        Some(PeerCommand::StartRequesting {
                            atomic_states,
                            availability_snapshot,
                            piece_notify,
                            disk_handle,
                            write_error_tx,
                            lengths,
                            block_maps,
                            steal_candidates,
                            piece_write_guards,
                        }) => {
                            self.handler.on_start_requesting(
                                atomic_states,
                                availability_snapshot,
                                piece_notify,
                                disk_handle,
                                write_error_tx,
                                lengths,
                                block_maps,
                                steal_candidates,
                                piece_write_guards,
                            );
                        }
                        Some(PeerCommand::UpdateNumPieces(n)) => {
                            let _ = self.handler.on_update_num_pieces(n).await;
                        }
                        Some(PeerCommand::SnapshotUpdate { snapshot }) => {
                            self.handler.on_snapshot_update(snapshot);
                        }
                        Some(cmd) => {
                            if let Err(e) = self.handler.on_command(cmd, &mut self.writer).await {
                                debug!(addr = %self.addr, "error sending message: {e}");
                                break Some(e.to_string());
                            }
                        }
                        None => {
                            break None;
                        }
                    }
                }

                // ----- ARM 4: Lock-free block requesting -----
                // Uses Arc<Semaphore> extracted before select! to avoid
                // borrowing self.handler across await points.
                permit = semaphore.acquire(), if can_request => {
                    let permit = permit.expect("peer semaphore closed unexpectedly");

                    match self.handler.next_block() {
                        Some((piece, begin, length)) => {
                            permit.forget();
                            self.handler.inc_in_flight();
                            self.writer.send(&Message::Request {
                                index: piece,
                                begin,
                                length,
                            }).await?;
                        }
                        None => {
                            // Cursor exhausted — wait for new pieces
                            drop(permit);
                            if let Some(notify) = self.handler.piece_notify() {
                                notify.notified().await;
                            }
                        }
                    }
                }

                // ----- ARM 5: 25ms batch flush timer -----
                _ = self.flush_interval.tick(), if has_pending => {
                    if let Some(blocks) = self.handler.take_batch() {
                        let _ = self.event_tx.send(PeerEvent::PieceBlocksBatch {
                            peer_addr: self.addr,
                            blocks,
                        }).await;
                    }
                }
            }
        };

        // Post-loop cleanup
        self.handler.on_disconnect(disconnect_reason).await;
        Ok(())
    }

    /// Handle a fully-buffered (Ready) message from the ring buffer.
    ///
    /// Returns `Ok(())` to continue the loop, or `Err(reason)` to break.
    /// Extracted from the select! arm to give the borrow checker a clean
    /// `&mut self` scope that doesn't overlap with the semaphore arm.
    async fn handle_ready_message(&mut self) -> Result<(), String> {
        let (msg, consumed) = match self.reader.try_decode() {
            Ok(pair) => pair,
            Err(e) => {
                debug!(addr = %self.addr, "decode error: {e}");
                return Err(e.to_string());
            }
        };

        // --- Piece fast-path: zero-copy pwrite from ring buffer ---
        if let Message::Piece {
            index,
            begin,
            data_0,
            data_1,
        } = &msg
        {
            let idx = *index;
            let bgn = *begin;
            let d0_len = data_0.len();
            let d1_len = data_1.len();
            match self.handler.on_piece_sync(idx, bgn, data_0, data_1) {
                Ok(PieceAction::Written { flush_now }) => {
                    // CRITICAL: advance BEFORE any await — borrowed slices released.
                    self.reader.advance(consumed);
                    if flush_now {
                        if let Some(blocks) = self.handler.take_batch() {
                            self.event_tx
                                .send(PeerEvent::PieceBlocksBatch {
                                    peer_addr: self.addr,
                                    blocks,
                                })
                                .await
                                .map_err(|_| "shutdown".to_string())?;
                        } else {
                            // No batch yet (StartRequesting not received) — send single block.
                            let data_len = (d0_len + d1_len) as u32;
                            self.event_tx
                                .send(PeerEvent::PieceBlocksBatch {
                                    peer_addr: self.addr,
                                    blocks: vec![BlockEntry {
                                        index: idx,
                                        begin: bgn,
                                        length: data_len,
                                    }],
                                })
                                .await
                                .map_err(|_| "shutdown".to_string())?;
                        }
                    }
                    // M116: Yield to prevent busy peers from monopolizing a worker thread.
                    tokio::task::yield_now().await;
                    return Ok(());
                }
                Ok(PieceAction::Unhandled) => {
                    // Piece without reservation — fall through to on_message.
                    let owned = msg.to_owned_bytes();
                    self.reader.advance(consumed);
                    self.dispatch_message(owned).await?;
                    return Ok(());
                }
                Err(e) => {
                    debug!(addr = %self.addr, "piece sync error: {e}");
                    return Err(e.to_string());
                }
            }
        }

        // --- Non-Piece path ---
        // Defer Bitfield/HaveAll/HaveNone when num_pieces is unknown (magnet phase).
        if self.handler.should_defer_bitfield() {
            match &msg {
                Message::Bitfield(_) | Message::HaveAll | Message::HaveNone => {
                    self.handler.defer_bitfield(&msg);
                    self.reader.advance(consumed);
                    return Ok(());
                }
                _ => {}
            }
        }

        // State intercepts (sync, before to_owned + advance)
        match &msg {
            Message::Unchoke => {
                self.handler.on_unchoke();
                self.handler.on_bitfield_update(&msg);
            }
            Message::Choke => {
                // Decision 1A: Explicit choke special-case.
                if let Some(piece) = self.handler.on_choke() {
                    // Advance BEFORE await — Choke is a fixed-field variant.
                    self.reader.advance(consumed);
                    let _ = self
                        .event_tx
                        .send(PeerEvent::PieceReleased {
                            peer_addr: self.addr,
                            piece,
                        })
                        .await;
                    // Forward choke to on_message with owned msg.
                    self.dispatch_message(Message::Choke).await?;
                    tokio::task::yield_now().await;
                    return Ok(());
                }
            }
            Message::RejectRequest { .. } => {
                self.handler.on_reject_request();
            }
            Message::Bitfield(_) | Message::Have { .. } | Message::HaveAll | Message::HaveNone => {
                self.handler.on_bitfield_update(&msg);
            }
            _ => {}
        }

        // Convert borrowed message to owned, then release ring space.
        let owned = msg.to_owned_bytes();
        self.reader.advance(consumed);

        self.dispatch_message(owned).await?;
        // M116: Yield after processing non-piece messages.
        tokio::task::yield_now().await;
        Ok(())
    }

    /// Send an owned message through the handler and optionally write a
    /// response to the wire. Returns `Err(reason)` if the loop should break.
    async fn dispatch_message(&mut self, msg: Message) -> Result<(), String> {
        match self.handler.on_message(msg).await {
            Ok(Some(response)) => {
                if let Err(e) = self.writer.send(&response).await {
                    warn!(addr = %self.addr, "error sending response: {e}");
                    return Err(e.to_string());
                }
            }
            Ok(None) => {}
            Err(e) => {
                warn!(addr = %self.addr, "error handling message: {e}");
            }
        }
        Ok(())
    }
}
