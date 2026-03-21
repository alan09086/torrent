//! TorrentActor piece dispatch, availability, endgame, and streaming.
//!
//! This module contains an `impl TorrentActor` block with methods for:
//! - Availability snapshot management (`mark_snapshot_dirty`, `rebuild_availability_snapshot`)
//! - Max in-flight recalculation (`recalc_max_in_flight`)
//! - End-game mode (`check_end_game_activation`, `request_end_game_block`)
//! - Streaming cursor updates (`update_streaming_cursors`)
//! - Interest expression (`maybe_express_interest`)
//! - Upload request serving (`serve_incoming_requests`)
//! - Super-seed piece assignment (`assign_next_piece_for_peer`)

use std::net::SocketAddr;
use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::disk::DiskJobFlags;
use crate::piece_reservation::{AvailabilitySnapshot, PieceState};
use crate::torrent::{TorrentActor, END_GAME_DEPTH, now_unix};
use crate::types::{PeerCommand, TorrentState};

use torrent_core::FilePriority;

impl TorrentActor {
    /// M103: Mark the availability snapshot as stale for deferred rebuild.
    ///
    /// The actual rebuild happens on the next 50ms snapshot tick, batching
    /// multiple rapid events (bitfield, have, disconnect) into one rebuild.
    pub(crate) fn mark_snapshot_dirty(&mut self) {
        self.snapshot_dirty = true;
    }

    /// M93: Rebuild the availability snapshot and broadcast to all peers.
    pub(crate) fn rebuild_availability_snapshot(&mut self) {
        let Some(ref atomic_states) = self.atomic_states else {
            return;
        };
        self.snapshot_generation += 1;
        let snapshot = Arc::new(AvailabilitySnapshot::build(
            &self.availability,
            atomic_states,
            &self.priority_pieces,
            self.snapshot_generation,
        ));
        self.availability_snapshot = Some(Arc::clone(&snapshot));

        // Broadcast new snapshot to all peer tasks
        for peer in self.peers.values() {
            let _ = peer.cmd_tx.try_send(PeerCommand::SnapshotUpdate {
                snapshot: Arc::clone(&snapshot),
            });
        }
    }

    /// M93: Recalculate max_in_flight based on connected peer count.
    pub(crate) fn recalc_max_in_flight(&mut self) {
        let connected = self.peers.len();
        let calculated = 512usize.max(connected.saturating_mul(4));
        self.max_in_flight = calculated.min(self.num_pieces as usize / 2).max(512);
    }

    pub(crate) fn check_end_game_activation(&mut self) {
        // M103: EndGame coexists with block stealing — stealing handles the
        // bulk download (fast peers steal from slow peers' pieces), while
        // EndGame handles the last-pieces problem (duplicate requests for the
        // final few blocks when all pieces are reserved).
        if self.end_game.is_active() || self.state != TorrentState::Downloading {
            return;
        }
        let Some(ref ct) = self.chunk_tracker else {
            return;
        };
        let have = ct.bitfield().count_ones();

        let Some(ref atomic_states) = self.atomic_states else {
            return;
        };
        let in_flight = atomic_states.in_flight_count();
        if have + in_flight >= self.num_pieces && in_flight > 0 {
            use std::collections::HashMap;
            let mut per_peer: HashMap<SocketAddr, Vec<(u32, u32, u32)>> = HashMap::new();
            // M93: Find in-flight pieces from atomic state (not piece_owner,
            // which only tracks pieces after chunks arrive). Use the known
            // owner if available, otherwise assign to a random unchoked peer
            // so endgame can send duplicate requests.
            let fallback_addr = self
                .peers
                .values()
                .find(|p| !p.peer_choking)
                .map(|p| p.addr);
            for piece in 0..self.num_pieces {
                let state = atomic_states.get(piece);
                if state != PieceState::Reserved && state != PieceState::Endgame {
                    continue;
                }
                let addr = self.piece_owner[piece as usize]
                    .and_then(|slab_idx| self.peer_slab.get(slab_idx).copied())
                    .or(fallback_addr);
                if let Some(addr) = addr {
                    let missing = ct.missing_chunks(piece);
                    for (begin, length) in missing {
                        per_peer
                            .entry(addr)
                            .or_default()
                            .push((piece, begin, length));
                    }
                }
            }

            let pending: Vec<_> = per_peer.into_iter().collect();
            self.end_game.activate(&pending);
            info!(
                blocks = self.end_game.block_count(),
                "end-game mode activated"
            );
            // M93: Transition ALL in-flight pieces to Endgame state
            for piece in 0..self.num_pieces {
                let state = atomic_states.get(piece);
                if state == PieceState::Reserved {
                    atomic_states.transition_to_endgame(piece);
                }
            }
        }
    }

    pub(crate) async fn request_end_game_block(&mut self, peer_addr: SocketAddr) {
        let can_request = self
            .peers
            .get(&peer_addr)
            .is_some_and(|p| !p.peer_choking && p.pending_requests.len() < END_GAME_DEPTH);
        if !can_request {
            return;
        }

        let peer_bitfield = match self.peers.get(&peer_addr) {
            Some(p) => p.bitfield.clone(),
            None => return,
        };

        // Fill up to END_GAME_DEPTH slots
        let slots = END_GAME_DEPTH
            - self
                .peers
                .get(&peer_addr)
                .map(|p| p.pending_requests.len())
                .unwrap_or(END_GAME_DEPTH);

        for _ in 0..slots {
            let block = if !self.streaming_pieces.is_empty() {
                self.end_game.pick_block_streaming(
                    peer_addr,
                    &peer_bitfield,
                    &self.streaming_pieces,
                )
            } else if self.config.strict_end_game {
                self.end_game
                    .pick_block_strict(peer_addr, &peer_bitfield, &[])
            } else {
                self.end_game.pick_block(peer_addr, &peer_bitfield)
            };

            if let Some((index, begin, length)) = block
                && let Some(peer) = self.peers.get_mut(&peer_addr)
                && peer
                    .cmd_tx
                    .try_send(PeerCommand::Request {
                        index,
                        begin,
                        length,
                    })
                    .is_ok()
            {
                self.end_game.register_request(index, begin, peer_addr);
                peer.pending_requests.insert(index, begin, length);
            } else {
                break;
            }
        }
    }

    pub(crate) fn update_streaming_cursors(&mut self) {
        // Remove cursors whose receiver has been dropped (FileStream dropped)
        self.streaming_cursors
            .retain(|c| c.cursor_rx.has_changed().is_ok());

        self.streaming_pieces.clear();
        for cursor in &mut self.streaming_cursors {
            // Update cursor piece from position changes
            if cursor.cursor_rx.has_changed().unwrap_or(false) {
                let file_pos = *cursor.cursor_rx.borrow_and_update();
                if let Some(ref lengths) = self.lengths {
                    let abs = cursor.file_offset + file_pos;
                    if abs < lengths.total_length() {
                        cursor.cursor_piece = (abs / lengths.piece_length()) as u32;
                    }
                }
            }

            let end = cursor.cursor_piece + cursor.readahead_pieces;
            for p in cursor.cursor_piece..end.min(self.num_pieces) {
                self.streaming_pieces.insert(p);
            }
        }

        // Build time_critical_pieces from first+last piece of High-priority files
        self.time_critical_pieces.clear();
        if let Some(ref meta) = self.meta
            && let Some(ref lengths) = self.lengths
        {
            let mut offset = 0u64;
            for (i, file) in meta.info.files().iter().enumerate() {
                if self.file_priorities.get(i).copied() == Some(FilePriority::High)
                    && let Some((first, last)) = lengths.file_pieces(offset, file.length)
                {
                    self.time_critical_pieces.insert(first);
                    if last != first {
                        self.time_critical_pieces.insert(last);
                    }
                }
                offset += file.length;
            }
        }
    }

    pub(crate) async fn maybe_express_interest(&mut self, peer_addr: SocketAddr) {
        if self.state != TorrentState::Downloading {
            return;
        }

        let dominated = self.chunk_tracker.as_ref().map(|ct| {
            let we_have = ct.bitfield();
            if let Some(peer) = self.peers.get(&peer_addr) {
                // Check if the peer has any piece we don't
                peer.bitfield.ones().any(|i| !we_have.get(i))
            } else {
                false
            }
        });

        if dominated == Some(true)
            && let Some(peer) = self.peers.get_mut(&peer_addr)
            && !peer.am_interested
        {
            debug!(%peer_addr, "sending Interested");
            peer.am_interested = true;
            let _ = peer.cmd_tx.try_send(PeerCommand::SetInterested(true));
        } else {
            let bf_ones = self
                .peers
                .get(&peer_addr)
                .map(|p| p.bitfield.count_ones())
                .unwrap_or(0);
            if bf_ones > 0 {
                debug!(%peer_addr, ?dominated, bf_ones, "not interested despite bitfield");
            }
        }
    }

    pub(crate) async fn serve_incoming_requests(&mut self) {
        let disk = match &self.disk {
            Some(d) => d.clone(),
            None => return,
        };
        let chunk_tracker = match &self.chunk_tracker {
            Some(ct) => ct,
            None => return,
        };

        // Collect servable requests: peer is unchoked and we have the piece.
        // In share mode, also collect requests for evicted pieces to reject.
        let mut to_serve: Vec<(SocketAddr, u32, u32, u32)> = Vec::new();
        let mut to_reject: Vec<(SocketAddr, u32, u32, u32)> = Vec::new();
        for peer in self.peers.values() {
            if peer.am_choking {
                continue;
            }
            for &(index, begin, length) in &peer.incoming_requests {
                if chunk_tracker.has_piece(index) {
                    to_serve.push((peer.addr, index, begin, length));
                } else if self.share_max_pieces > 0 && self.config.enable_fast {
                    // Share mode: reject requests for pieces we no longer have
                    to_reject.push((peer.addr, index, begin, length));
                }
            }
        }

        // Send RejectRequest for evicted share mode pieces
        for (addr, index, begin, length) in to_reject {
            if let Some(peer) = self.peers.get_mut(&addr) {
                peer.incoming_requests
                    .retain(|&(i, b, l)| !(i == index && b == begin && l == length));
                let _ = peer.cmd_tx.try_send(PeerCommand::RejectRequest {
                    index,
                    begin,
                    length,
                });
            }
        }

        for (addr, index, begin, length) in to_serve {
            let chunk_size = length as u64;

            // Check global upload budget (skip for local peers)
            if !crate::rate_limiter::is_local_network(addr.ip())
                && let Some(ref global) = self.global_upload_bucket
                && !global.lock().try_consume(chunk_size)
            {
                break; // global budget exhausted, serve remaining next refill
            }

            // Check per-torrent upload budget
            if !self.upload_bucket.try_consume(chunk_size) {
                break; // per-torrent budget exhausted this cycle
            }

            match disk
                .read_chunk(index, begin, length, DiskJobFlags::empty())
                .await
            {
                Ok(data) => {
                    if let Some(peer) = self.peers.get_mut(&addr) {
                        peer.incoming_requests
                            .retain(|&(i, b, l)| !(i == index && b == begin && l == length));
                        let _ = peer
                            .cmd_tx
                            .try_send(PeerCommand::SendPiece { index, begin, data });
                        self.uploaded += chunk_size;
                        self.total_upload += chunk_size + 13; // payload + message header
                        self.last_upload = now_unix();
                        self.upload_bytes_interval += chunk_size;
                        peer.upload_bytes_window += chunk_size;
                    }
                }
                Err(e) => {
                    warn!(index, begin, length, "failed to read chunk for upload: {e}");
                }
            }
        }

        if self.check_seed_ratio() {
            self.shutdown_peers().await;
        }
    }

    /// BEP 16: assign the next super-seed piece to a peer.
    pub(crate) async fn assign_next_piece_for_peer(&mut self, peer_addr: SocketAddr) {
        let ss = match &mut self.super_seed {
            Some(ss) => ss,
            None => return,
        };

        if ss.has_assignment(&peer_addr) {
            return;
        }

        let peer_bitfield = match self.peers.get(&peer_addr) {
            Some(p) => p.bitfield.clone(),
            None => return,
        };

        let availability = self.availability.clone();
        if let Some(idx) =
            ss.assign_piece(peer_addr, &peer_bitfield, &availability, self.num_pieces)
            && let Some(peer) = self.peers.get(&peer_addr)
        {
            let _ = peer.cmd_tx.try_send(PeerCommand::Have(idx));
        }
    }
}
