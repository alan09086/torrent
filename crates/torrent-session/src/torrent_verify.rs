//! TorrentActor hash verification, smart banning, parole, and BEP 52 hash coordination.

use std::net::SocketAddr;

use tracing::{debug, error, info, warn};

use crate::alert::{AlertKind, post_alert};
use crate::disk::DiskJobFlags;
use crate::piece_reservation::PieceState;
use crate::torrent::{HashResult, TorrentActor, now_unix, serve_hashes};
use crate::types::{PeerCommand, TorrentState};

use torrent_core::DEFAULT_CHUNK_SIZE;

impl TorrentActor {
    /// M92: Process a single block completion — extracted from `handle_chunk_written()`.
    /// Called once per block in a batch. Returns `true` if this block completed a piece
    /// and piece verification was triggered.
    ///
    /// IMPORTANT: This is a line-for-line extraction of `handle_chunk_written()`.
    /// Every edge case (duplicate detection, end-game cancels, v2/hybrid verification,
    /// predictive Have, smart banning) is preserved identically.
    pub(crate) async fn process_block_completion(
        &mut self,
        peer_addr: SocketAddr,
        index: u32,
        begin: u32,
        length: u32,
    ) -> bool {
        // Skip duplicate blocks — in end-game mode or after timeout re-requests,
        // the same block may arrive from multiple peers. Writing it to the store
        // buffer would overwrite valid data that's pending verification.
        if let Some(ref ct) = self.chunk_tracker
            && ct.has_chunk(index, begin)
        {
            self.total_download += length as u64 + 13;
            // Remove from pending_requests to free pipeline slots. Without this,
            // the peer accumulates phantom entries from already-verified pieces
            // and eventually has zero available pipeline slots — permanent stall.
            if let Some(peer) = self.peers.get_mut(&peer_addr) {
                peer.pending_requests.remove(index, begin);
            }
            // Remove from end-game tracker so pick_block won't return this
            // block again. The normal path calls block_received which does
            // this, but we skip that path for duplicates.
            if self.end_game.is_active() {
                self.end_game.block_received(index, begin, peer_addr);
            }
            // Peer task already returned its permit on direct write
            return false;
        }

        // NOTE: No disk write here — the peer task has already written to disk.

        self.downloaded += length as u64;
        self.total_download += length as u64 + 13; // payload + message header
        self.last_download = now_unix();
        self.need_save_resume = true;

        // M93: Track piece ownership (actor learns about peer's CAS reservation via chunk arrival)
        if let Some(slab_idx) = self.peer_slab.slot_of(&peer_addr)
            && self.piece_owner.get(index as usize) == Some(&None)
        {
            self.piece_owner[index as usize] = Some(slab_idx);
            // M103: Add to steal queue if piece has unrequested blocks
            if let (Some(sc), Some(bm)) = (&self.steal_candidates, &self.block_maps)
                && let Some(lengths) = &self.lengths
            {
                let total_blocks = lengths.chunks_in_piece(index);
                if bm.next_unrequested(index, total_blocks).is_some() {
                    sc.push(index);
                }
            }
        }

        // Smart banning: track which peers contribute to each piece
        self.piece_contributors
            .entry(index)
            .or_default()
            .insert(peer_addr.ip());

        let now = std::time::Instant::now();
        if let Some(peer) = self.peers.get_mut(&peer_addr) {
            peer.pending_requests.remove(index, begin);
            peer.download_bytes_window += length as u64;
            // M106: Capture RTT from block receipt for scoring
            if let Some(rtt) = peer.pipeline.block_received(index, begin, length, now) {
                let rtt_secs = rtt.as_secs_f64();
                peer.avg_rtt = Some(crate::peer_scorer::ewma_update(
                    peer.avg_rtt.unwrap_or(rtt_secs),
                    rtt_secs,
                    crate::peer_scorer::RTT_EWMA_ALPHA,
                ));
            }
            peer.blocks_completed = peer.blocks_completed.saturating_add(1);
            peer.last_data_received = Some(now);
            // Clear snub if snubbed
            if peer.snubbed {
                peer.snubbed = false;
            }
        }
        // M104: Clear backoff — peer sent real data, so connection is healthy
        self.connect_backoff.remove(&peer_addr);

        // End-game: cancel this block on all other peers. The 200ms end-game
        // refill tick will re-stock freed peers — no reactive cascade needed.
        if self.end_game.is_active() {
            let cancels = self.end_game.block_received(index, begin, peer_addr);
            for (cancel_addr, ci, cb, cl) in cancels {
                if let Some(cancel_peer) = self.peers.get_mut(&cancel_addr) {
                    let _ = cancel_peer.cmd_tx.try_send(PeerCommand::Cancel {
                        index: ci,
                        begin: cb,
                        length: cl,
                    });
                    cancel_peer.pending_requests.remove(ci, cb);
                }
            }
        }

        // Track chunk completion
        let piece_complete = if let Some(ref mut ct) = self.chunk_tracker {
            ct.chunk_received(index, begin)
        } else {
            false
        };

        if piece_complete && !self.pending_verify.contains(&index) {
            // M44/M118: Predictive piece announce — broadcast Have before verification
            if self.config.predictive_piece_announce_ms > 0
                && !self.predictive_have_sent.contains(&index)
            {
                self.predictive_have_sent.insert(index);
                let _ = self.have_broadcast_tx.send(index);
            }

            // M100: Flush deferred writes before verification — ensures all
            // blocks are on disk so read_piece() sees complete data.
            if let Some(ref disk) = self.disk {
                disk.flush_piece_writes(index).await;
            }

            match self.version {
                torrent_core::TorrentVersion::V1Only => {
                    // Async: fire-and-forget, result via verify_result_rx
                    if let Some(ref disk) = self.disk
                        && let Some(expected) = self
                            .meta
                            .as_ref()
                            .and_then(|m| m.info.piece_hash(index as usize))
                    {
                        self.pending_verify.insert(index);
                        let generation = self
                            .piece_generations
                            .get(index as usize)
                            .copied()
                            .unwrap_or(0);
                        disk.enqueue_verify(index, expected, generation, &self.verify_result_tx);
                    }
                }
                torrent_core::TorrentVersion::V2Only => {
                    // Blocking: needs mutable hash_picker for Merkle tree
                    self.verify_and_mark_piece_v2(index).await;
                }
                torrent_core::TorrentVersion::Hybrid => {
                    // Blocking: needs both v1+v2 decision matrix
                    self.verify_and_mark_piece_hybrid(index).await;
                }
            }
        }

        // Peer task already returned its permit on direct write.
        // End-game dispatch still happens here.
        if self.end_game.is_active() {
            self.request_end_game_block(peer_addr).await;
        }

        piece_complete
    }

    pub(crate) async fn verify_and_mark_piece(&mut self, index: u32) {
        match self.version {
            torrent_core::TorrentVersion::V1Only => {
                self.verify_and_mark_piece_v1(index).await;
            }
            torrent_core::TorrentVersion::V2Only => {
                self.verify_and_mark_piece_v2(index).await;
            }
            torrent_core::TorrentVersion::Hybrid => {
                self.verify_and_mark_piece_hybrid(index).await;
            }
        }
    }

    /// SHA-1 piece verification (v1 torrents).
    async fn verify_and_mark_piece_v1(&mut self, index: u32) {
        let expected_hash = self
            .meta
            .as_ref()
            .and_then(|m| m.info.piece_hash(index as usize));

        let verified = if let (Some(disk), Some(expected)) = (&self.disk, expected_hash) {
            disk.verify_piece(index, expected, DiskJobFlags::empty())
                .await
                .unwrap_or(false)
        } else {
            false
        };

        if verified {
            self.on_piece_verified(index).await;
        } else {
            self.on_piece_hash_failed(index).await;
        }
    }

    /// SHA-256 per-block Merkle verification (v2 torrents, BEP 52).
    pub(crate) async fn verify_and_mark_piece_v2(&mut self, index: u32) {
        let result = self.run_v2_block_verification(index).await;
        match result {
            HashResult::Passed => self.on_piece_verified(index).await,
            HashResult::Failed => self.on_piece_hash_failed(index).await,
            HashResult::NotApplicable => {
                // Blocks stored, will resolve when piece-layer hashes arrive
            }
        }
    }

    /// Run SHA-256 per-block Merkle verification and return a `HashResult`
    /// without triggering side effects (no `on_piece_verified`/`on_piece_hash_failed`).
    ///
    /// Extracted from `verify_and_mark_piece_v2` so it can be reused in hybrid
    /// dual-verification without double-firing callbacks.
    pub(crate) async fn run_v2_block_verification(&mut self, index: u32) -> HashResult {
        let disk = match &self.disk {
            Some(d) => d.clone(),
            None => return HashResult::NotApplicable,
        };

        let lengths = match &self.lengths {
            Some(l) => l.clone(),
            None => return HashResult::NotApplicable,
        };

        // Flush write buffer before reading back for hashing
        if let Err(e) = disk.flush_piece(index).await {
            warn!(index, "failed to flush piece for v2 verification: {e}");
            return HashResult::NotApplicable;
        }

        // Compute SHA-256 of each 16 KiB block and feed to Merkle tree
        let num_chunks = lengths.chunks_in_piece(index);
        let blocks_per_piece = (lengths.piece_length() as u32) / DEFAULT_CHUNK_SIZE;
        let mut all_ok = true;

        for chunk_idx in 0..num_chunks {
            let (begin, length) = match lengths.chunk_info(index, chunk_idx) {
                Some(info) => info,
                None => continue,
            };

            let block_hash = match disk
                .hash_block(index, begin, length, DiskJobFlags::empty())
                .await
            {
                Ok(h) => h,
                Err(e) => {
                    warn!(index, chunk_idx, "failed to hash block: {e}");
                    all_ok = false;
                    break;
                }
            };

            if let Some(ref mut picker) = self.hash_picker {
                // Gap 6: single-file assumption — multi-file mapping deferred to M35
                let file_index = 0;
                let global_block = index * blocks_per_piece + chunk_idx;
                // Gap 10: 3-arg set_block_hash (offset removed)
                match picker.set_block_hash(file_index, global_block, block_hash) {
                    torrent_core::SetBlockResult::Ok => {
                        if let Some(ref mut ct) = self.chunk_tracker {
                            ct.mark_block_verified(index, chunk_idx);
                        }
                    }
                    torrent_core::SetBlockResult::Unknown => {
                        // Piece-layer hash not yet available — stored for deferred verification
                        debug!(
                            index,
                            chunk_idx, "block hash stored, awaiting piece-layer hashes"
                        );
                    }
                    torrent_core::SetBlockResult::HashFailed => {
                        warn!(index, chunk_idx, "block hash failed Merkle verification");
                        return HashResult::Failed;
                    }
                }
            }
        }

        if all_ok
            && self
                .chunk_tracker
                .as_ref()
                .is_some_and(|ct| ct.all_blocks_verified(index))
        {
            HashResult::Passed
        } else {
            // Either a disk error (all_ok = false) or blocks stored awaiting piece-layer hashes
            HashResult::NotApplicable
        }
    }

    /// Dual SHA-1 + SHA-256 verification for hybrid torrents.
    ///
    /// Runs both v1 (whole-piece SHA-1) and v2 (per-block SHA-256 Merkle)
    /// verification. Decision matrix:
    /// - Both Passed → piece verified
    /// - Both Failed → piece hash failed (normal re-request / parole path)
    /// - One Passed + one Failed → inconsistent hashes (fatal, pauses torrent)
    /// - Any NotApplicable → deferred (v2 blocks stored, will resolve later)
    pub(crate) async fn verify_and_mark_piece_hybrid(&mut self, index: u32) {
        // ── v1 verification (SHA-1 whole-piece) ──
        let v1_result = {
            let expected_hash = self
                .meta
                .as_ref()
                .and_then(|m| m.info.piece_hash(index as usize));

            if let (Some(disk), Some(expected)) = (&self.disk, expected_hash) {
                match disk
                    .verify_piece(index, expected, DiskJobFlags::empty())
                    .await
                {
                    Ok(true) => HashResult::Passed,
                    Ok(false) => HashResult::Failed,
                    Err(_) => HashResult::NotApplicable,
                }
            } else {
                HashResult::NotApplicable
            }
        };

        // ── v2 verification (SHA-256 per-block Merkle) ──
        let v2_result = self.run_v2_block_verification(index).await;

        // ── Decision matrix ──
        match (v1_result, v2_result) {
            // Both agree: piece is good
            (HashResult::Passed, HashResult::Passed) => {
                self.on_piece_verified(index).await;
            }
            // Both agree: piece is bad
            (HashResult::Failed, HashResult::Failed) => {
                self.on_piece_hash_failed(index).await;
            }
            // One passes, one fails: the .torrent metadata is inconsistent
            (HashResult::Passed, HashResult::Failed) | (HashResult::Failed, HashResult::Passed) => {
                self.on_inconsistent_hashes(index).await;
            }
            // v2 deferred (awaiting piece-layer hashes) but v1 passed: defer the whole thing.
            // When piece-layer hashes arrive, handle_hashes_received will re-verify.
            (HashResult::Passed, HashResult::NotApplicable) => {
                debug!(
                    index,
                    "hybrid: v1 passed, v2 deferred — waiting for piece-layer hashes"
                );
            }
            // v2 deferred but v1 failed: fail immediately (no point waiting for v2).
            (HashResult::Failed, HashResult::NotApplicable) => {
                self.on_piece_hash_failed(index).await;
            }
            // v1 not applicable (missing meta/disk): defer
            (HashResult::NotApplicable, _) => {
                debug!(index, "hybrid: v1 not applicable — deferring");
            }
        }
    }

    /// Common success path after a piece passes verification (v1 SHA-1 or v2 Merkle).
    /// Handle a result from the HashPool (M96).
    pub(crate) async fn handle_hash_result(&mut self, result: crate::hash_pool::HashResult) {
        self.pending_verify.remove(&result.piece);

        // Staleness check
        let current_gen = self
            .piece_generations
            .get(result.piece as usize)
            .copied()
            .unwrap_or(0);
        if result.generation != current_gen {
            tracing::debug!(
                piece = result.piece,
                result_gen = result.generation,
                current_gen,
                "discarding stale hash result"
            );
            return;
        }

        // Guard: ignore results for already-verified pieces
        let dominated = self
            .chunk_tracker
            .as_ref()
            .map(|ct| ct.bitfield().get(result.piece))
            .unwrap_or(false);
        if dominated {
            return;
        }

        if result.passed {
            self.on_piece_verified(result.piece).await;
        } else {
            self.on_piece_hash_failed(result.piece).await;
        }
    }

    pub(crate) async fn on_piece_verified(&mut self, index: u32) {
        if let Some(ref mut ct) = self.chunk_tracker {
            ct.mark_verified(index);
        }
        self.piece_contributors.remove(&index);
        // Remove stale end-game blocks for this piece. Without this,
        // pick_block() returns these blocks, peers request them, has_chunk
        // rejects them, and register_request accumulates until every peer
        // is registered for every stale block — permanent stall.
        if self.end_game.is_active() {
            self.end_game.remove_piece(index);
        }
        // M93: Mark piece complete atomically
        if let Some(ref states) = self.atomic_states {
            states.mark_complete(index);
        }
        // M103: Clean up block stealing state
        if let Some(ref sc) = self.steal_candidates {
            sc.remove(index);
        }
        if let (Some(bm), Some(lengths)) = (&self.block_maps, &self.lengths) {
            bm.clear(index, lengths.chunks_in_piece(index));
        }
        self.piece_owner[index as usize] = None;
        // M103: Mark snapshot dirty for reactive rebuild
        self.mark_snapshot_dirty();
        info!(index, "piece verified");
        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::PieceFinished {
                info_hash: self.info_hash,
                piece: index,
            },
        );

        // Notify FileStream consumers of piece completion
        let _ = self.piece_ready_tx.send(index);
        if let Some(ref ct) = self.chunk_tracker {
            let _ = self.have_watch_tx.send(ct.bitfield().clone());
        }

        // Check if the completed piece finishes any file (FileCompleted alert)
        self.check_file_completion(index);

        // Handle parole success: the parole peer delivered a good piece,
        // so the original contributors are the likely offenders.
        if let Some(parole) = self.parole_pieces.remove(&index) {
            self.apply_parole_success(index, parole).await;
        }

        // M118: Broadcast Have to all peers via broadcast channel (skip in super-seed mode).
        // Per-peer filtering (skip if peer already has piece) is done receiver-side
        // in PeerConnection via should_transmit_have().
        if self.super_seed.is_none() {
            let already_announced = self.predictive_have_sent.remove(&index);
            if !already_announced {
                let _ = self.have_broadcast_tx.send(index);
            }
        }

        // M44: suggest newly-verified piece to peers that don't have it
        if self.config.suggest_mode {
            let max_suggest = self.config.max_suggest_pieces;
            let peer_addrs: Vec<SocketAddr> = self.peers.keys().copied().collect();
            for peer_addr in peer_addrs {
                let already = self.suggested_to_peers.entry(peer_addr).or_default();
                if already.len() >= max_suggest {
                    continue;
                }
                let should_suggest = self
                    .peers
                    .get(&peer_addr)
                    .is_some_and(|p| !p.bitfield.get(index));
                if should_suggest
                    && !already.contains(&index)
                    && let Some(peer) = self.peers.get(&peer_addr)
                {
                    let _ = peer.cmd_tx.try_send(PeerCommand::SuggestPiece(index));
                    already.insert(index);
                }
            }
        }

        // Share mode LRU: track piece, evict oldest if over capacity.
        // In share mode, we never "finish" — we keep cycling pieces.
        if self.share_max_pieces > 0 {
            self.share_lru.push_back(index);
            while self.share_lru.len() > self.share_max_pieces {
                if let Some(evicted) = self.share_lru.pop_front() {
                    if let Some(ref mut ct) = self.chunk_tracker {
                        ct.clear_piece(evicted);
                    }
                    // Re-add to wanted so it can be re-downloaded later
                    if evicted < self.wanted_pieces.len() {
                        self.wanted_pieces.set(evicted);
                    }
                    debug!(evicted, "share mode: evicted piece from LRU");
                }
            }
        }

        // Check if download is complete (skip in share mode — never finishes)
        if self.share_max_pieces == 0
            && let Some(ref ct) = self.chunk_tracker
            && ct.bitfield().count_ones() == self.num_pieces
        {
            info!("download complete, transitioning to seeding");
            post_alert(
                &self.alert_tx,
                &self.alert_mask,
                AlertKind::TorrentFinished {
                    info_hash: self.info_hash,
                },
            );
            self.end_game.deactivate();
            self.transition_state(TorrentState::Seeding);
            self.choker.set_seed_mode(true);
            // BEP 21: broadcast upload-only status
            if self.config.upload_only_announce {
                let hs = torrent_wire::ExtHandshake::new_upload_only();
                for peer in self.peers.values() {
                    let _ = peer
                        .cmd_tx
                        .try_send(PeerCommand::SendExtHandshake(hs.clone()));
                }
            }
            // Announce completion to trackers
            let result = self
                .tracker_manager
                .announce_completed(self.uploaded, self.downloaded)
                .await;
            self.fire_tracker_alerts(&result.outcomes);
        }
    }

    /// Common failure path after a piece fails hash verification.
    pub(crate) async fn on_piece_hash_failed(&mut self, index: u32) {
        let contributors: Vec<std::net::IpAddr> = self
            .piece_contributors
            .remove(&index)
            .unwrap_or_default()
            .into_iter()
            .collect();

        warn!(
            index,
            contributors = contributors.len(),
            "piece hash verification failed"
        );

        self.total_failed_bytes += self
            .lengths
            .as_ref()
            .map(|l| l.piece_size(index) as u64)
            .unwrap_or(0);

        self.predictive_have_sent.remove(&index);

        // Check if this is a parole failure
        if let Some(parole) = self.parole_pieces.remove(&index) {
            self.apply_parole_failure(index, parole);
        } else {
            // First failure: enter parole if enabled
            self.enter_parole(index, contributors.clone());
        }

        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::HashFailed {
                info_hash: self.info_hash,
                piece: index,
                contributors,
            },
        );
        if let Some(ref mut ct) = self.chunk_tracker {
            ct.mark_failed(index);
        }
        // M96: Increment generation to invalidate any in-flight hash for this piece
        if let Some(g) = self.piece_generations.get_mut(index as usize) {
            *g += 1;
        }
        // M93: Release piece back to Available
        if let Some(ref states) = self.atomic_states {
            states.release(index);
        }
        // M103: Clean up block stealing state on hash failure
        if let Some(ref sc) = self.steal_candidates {
            sc.remove(index);
        }
        if let (Some(bm), Some(lengths)) = (&self.block_maps, &self.lengths) {
            bm.clear(index, lengths.chunks_in_piece(index));
        }
        self.piece_owner[index as usize] = None;
        self.mark_snapshot_dirty();
        if let Some(ref notify) = self.reservation_notify {
            notify.notify_one();
        }
        // Hash failure in end-game: deactivate and resume normal mode
        if self.end_game.is_active() {
            self.end_game.deactivate();
            info!(index, "end-game deactivated due to hash failure");
            // M93: Transition Endgame pieces back
            if let Some(ref atomic_states) = self.atomic_states {
                for piece in 0..self.num_pieces {
                    if atomic_states.get(piece) == PieceState::Endgame {
                        if self.piece_owner[piece as usize].is_some() {
                            atomic_states.force_reserved(piece);
                        } else {
                            atomic_states.release(piece);
                        }
                    }
                }
            }
            if let Some(ref notify) = self.reservation_notify {
                notify.notify_waiters();
            }
        }
    }

    /// Handle v1/v2 hash inconsistency — the torrent data itself is corrupt.
    ///
    /// Matching libtorrent: destroy hash picker, pause the torrent, post error alert.
    async fn on_inconsistent_hashes(&mut self, piece: u32) {
        let info_hash = self.info_hash;
        error!(
            piece,
            info_hash = %info_hash,
            "v1 and v2 hashes are inconsistent — torrent data is corrupt"
        );

        // Destroy hash picker (Merkle tree state is untrustworthy)
        self.hash_picker = None;

        // Post specific inconsistency alert
        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::InconsistentHashes { info_hash, piece },
        );

        // Post generic error alert for broader consumers
        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::TorrentError {
                info_hash,
                message: format!("v1 and v2 hashes do not describe the same data (piece {piece})"),
            },
        );

        // Pause the torrent — transition state to Paused
        self.transition_state(TorrentState::Paused);
    }

    // ── BEP 52 hash event handlers (M34) ────────────────────────────────

    /// Process received piece-layer or block-layer hashes from a peer.
    pub(crate) async fn handle_hashes_received(
        &mut self,
        peer_addr: SocketAddr,
        request: torrent_core::HashRequest,
        hashes: Vec<torrent_core::Id32>,
    ) {
        let Some(ref mut picker) = self.hash_picker else {
            debug!(peer = %peer_addr, "received hashes but no hash picker (v1 torrent)");
            return;
        };

        match picker.add_hashes(&request, &hashes) {
            Ok(result) => {
                if !result.valid {
                    warn!(peer = %peer_addr, "received hashes failed Merkle proof validation");
                    return;
                }
                for piece in result.hash_passed {
                    self.on_piece_verified(piece).await;
                }
                for piece in result.hash_failed {
                    warn!(piece, "piece failed after hash layer received");
                    post_alert(
                        &self.alert_tx,
                        &self.alert_mask,
                        AlertKind::HashFailed {
                            info_hash: self.info_hash,
                            piece,
                            contributors: Vec::new(),
                        },
                    );
                }
            }
            Err(e) => {
                warn!(peer = %peer_addr, "invalid hashes: {e}");
            }
        }
    }

    /// Handle an incoming hash request from a peer (serve or reject).
    pub(crate) async fn handle_incoming_hash_request(
        &self,
        peer_addr: SocketAddr,
        request: torrent_core::HashRequest,
    ) {
        let peer = match self.peers.get(&peer_addr) {
            Some(p) => p,
            None => return,
        };

        match serve_hashes(
            self.meta_v2.as_ref(),
            self.version,
            self.lengths.as_ref(),
            &request,
        ) {
            Some(hashes) => {
                let _ = peer
                    .cmd_tx
                    .try_send(PeerCommand::SendHashes { request, hashes });
            }
            None => {
                let _ = peer.cmd_tx.try_send(PeerCommand::SendHashReject(request));
            }
        }
    }

    // ── Smart banning helpers (M25) ────────────────────────────────────

    /// Enter parole mode for a failed piece: save the original contributors
    /// and mark the piece for single-peer re-download.
    fn enter_parole(&mut self, index: u32, contributors: Vec<std::net::IpAddr>) {
        let use_parole = crate::timed_lock::TimedGuard::new(
            self.ban_manager.read(),
            &self.lock_timing,
            "ban_manager",
        )
        .use_parole();
        if !use_parole || contributors.is_empty() {
            // Parole disabled or no contributors to blame — strike everyone
            let mut mgr = self.ban_manager.write();
            for &ip in &contributors {
                if mgr.record_strike(ip) {
                    info!(%ip, "peer banned (no parole, hash failure threshold)");
                }
            }
            return;
        }

        info!(
            index,
            contributors = contributors.len(),
            "entering parole mode"
        );
        self.parole_pieces.insert(
            index,
            crate::ban::ParoleState {
                original_contributors: contributors.into_iter().collect(),
                parole_peer: None,
            },
        );
    }

    /// Parole piece verified successfully — the original contributors sent bad data.
    async fn apply_parole_success(&mut self, index: u32, parole: crate::ban::ParoleState) {
        info!(index, "parole success — striking original contributors");
        let mut banned_ips = Vec::new();
        {
            let mut mgr = self.ban_manager.write();
            for ip in &parole.original_contributors {
                if mgr.record_strike(*ip) {
                    info!(%ip, "peer banned (parole confirmed bad data)");
                    banned_ips.push(*ip);
                }
            }
        }
        // Disconnect and fire alerts for newly banned peers
        for ip in banned_ips {
            self.disconnect_banned_ip(ip).await;
            post_alert(
                &self.alert_tx,
                &self.alert_mask,
                AlertKind::PeerBanned {
                    info_hash: self.info_hash,
                    addr: std::net::SocketAddr::new(ip, 0),
                },
            );
        }
    }

    /// Parole piece failed again — the parole peer itself sent bad data.
    fn apply_parole_failure(&mut self, index: u32, parole: crate::ban::ParoleState) {
        if let Some(parole_ip) = parole.parole_peer {
            info!(index, %parole_ip, "parole failure — striking parole peer");
            let mut mgr = self.ban_manager.write();
            if mgr.record_strike(parole_ip) {
                info!(%parole_ip, "parole peer banned");
            }
        }
        // Don't re-enter parole for the same piece — ambiguous situation
    }
}
