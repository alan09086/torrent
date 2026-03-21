//! TorrentActor peer lifecycle, connectivity, choking, holepunch, and web seeds.
//!
//! This module contains peer-related methods extracted from `torrent.rs`:
//! - Connection count helpers (`effective_max_connections`, `transport_peer_counts`)
//! - Peer discovery and info (`handle_add_peers`, `build_peer_info`)
//! - Peer event handling (`handle_peer_event`)
//! - Peer disconnection (`disconnect_banned_ip`, `disconnect_peer`)
//! - Peer scoring and turnover (`run_scored_turnover`)
//! - Outbound connection spawning (`handle_adder_connect`)
//! - Inbound connection spawning (`spawn_peer_from_stream`, `spawn_peer_from_stream_with_mode`)
//! - BEP 55 holepunch relay and initiation

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::alert::{AlertKind, post_alert};
use crate::peer::run_peer;
use crate::peer_adder::ConnectPeer;
use crate::peer_scorer::PeerScorer;
use crate::peer_state::{PeerSource, PeerState};
use crate::torrent::{
    HOLEPUNCH_COOLDOWN, HOLEPUNCH_MAX_TRACKED, TorrentActor, is_i2p_synthetic_addr, now_unix,
    should_attempt_holepunch,
};
use crate::types::{PeerCommand, PeerEvent, PeerInfo, TorrentState};

use torrent_storage::Bitfield;

impl TorrentActor {
    /// Returns the effective maximum connection count for this torrent.
    ///
    /// If `max_connections > 0`, use that override; otherwise fall back to `config.max_peers`.
    pub(crate) fn effective_max_connections(&self) -> usize {
        if self.max_connections > 0 {
            self.max_connections
        } else {
            self.config.max_peers
        }
    }

    /// Count connected peers by transport type.
    pub(crate) fn transport_peer_counts(&self) -> (usize, usize) {
        let mut tcp = 0;
        let mut utp = 0;
        for peer in self.peers.values() {
            match peer.transport {
                Some(crate::rate_limiter::PeerTransport::Tcp) => tcp += 1,
                Some(crate::rate_limiter::PeerTransport::Utp) => utp += 1,
                None => {}
            }
        }
        (tcp, utp)
    }

    /// M107: Feed discovered peers into the adder task channel.
    /// All filtering (ban, IP filter, dedup, backoff) is handled by the adder task.
    pub(crate) fn handle_add_peers(&mut self, peers: Vec<SocketAddr>, source: PeerSource) {
        if let Some(ref tx) = self.peer_tx {
            for addr in peers {
                let _ = tx.send((addr, source));
            }
        }
    }

    pub(crate) fn build_peer_info(&self) -> Vec<PeerInfo> {
        self.peers
            .values()
            .map(|peer| {
                let client = peer
                    .ext_handshake
                    .as_ref()
                    .and_then(|h| h.v.clone())
                    .unwrap_or_default();
                PeerInfo {
                    addr: peer.addr,
                    client,
                    peer_choking: peer.peer_choking,
                    peer_interested: peer.peer_interested,
                    am_choking: peer.am_choking,
                    am_interested: peer.am_interested,
                    download_rate: peer.download_rate,
                    upload_rate: peer.upload_rate,
                    num_pieces: peer.bitfield.count_ones(),
                    source: peer.source,
                    supports_fast: peer.supports_fast,
                    upload_only: peer.upload_only,
                    snubbed: peer.snubbed,
                    connected_duration_secs: peer.connected_at.elapsed().as_secs(),
                    num_pending_requests: peer.pending_requests.len(),
                    num_incoming_requests: peer.incoming_requests.len(),
                }
            })
            .collect()
    }

    pub(crate) async fn handle_peer_event(&mut self, event: PeerEvent) {
        match event {
            PeerEvent::Bitfield {
                peer_addr,
                bitfield,
            } => {
                let ones = bitfield.count_ones();
                let is_choking = self
                    .peers
                    .get(&peer_addr)
                    .map(|p| p.peer_choking)
                    .unwrap_or(true);
                debug!(%peer_addr, ones, is_choking, state = ?self.state, "bitfield event received");
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.bitfield = bitfield;
                    if self.num_pieces > 0 && peer.bitfield.count_ones() == self.num_pieces {
                        self.last_seen_complete = now_unix();
                    }
                }
                // M93: Update availability from peer bitfield
                if let Some(peer) = self.peers.get(&peer_addr) {
                    for piece in 0..self.num_pieces {
                        if peer.bitfield.get(piece) {
                            self.availability[piece as usize] += 1;
                        }
                    }
                    let _slot = self.peer_slab.insert(peer_addr);
                    self.recalc_max_in_flight();
                    self.mark_snapshot_dirty();
                }
                // M75: Peer-integrated dispatch — no driver spawn needed here.
                // The peer task's requester arm activates when unchoked + has reservation state.
                // BEP 16: assign a piece in super-seed mode
                self.assign_next_piece_for_peer(peer_addr).await;
                // Check if we're interested in this peer
                self.maybe_express_interest(peer_addr).await;
            }
            PeerEvent::Have { peer_addr, index } => {
                if let Some(peer) = self.peers.get_mut(&peer_addr)
                    && !peer.bitfield.get(index)
                {
                    peer.bitfield.set(index);
                    // M93: Update availability for this piece
                    if (index as usize) < self.availability.len() {
                        self.availability[index as usize] += 1;
                    }
                    // M103: Mark snapshot dirty for reactive rebuild
                    self.mark_snapshot_dirty();
                }
                // BEP 16: Have-back detection in super-seed mode
                if let Some(ref mut ss) = self.super_seed
                    && ss.peer_reported_have(peer_addr, index)
                {
                    self.assign_next_piece_for_peer(peer_addr).await;
                }
                self.maybe_express_interest(peer_addr).await;
            }
            PeerEvent::PieceData {
                peer_addr,
                index,
                begin,
                data,
            } => {
                self.handle_piece_data(peer_addr, index, begin, data).await;
            }
            PeerEvent::PeerChoking { peer_addr, choking } => {
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.peer_choking = choking;
                }
                // M75: Peer-integrated dispatch — unchoke/choke tracked locally
                // in the peer task via message interception. No driver to spawn/cancel.
                // The peer task handles release_peer_pieces on choke.
            }
            PeerEvent::PeerInterested {
                peer_addr,
                interested,
            } => {
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.peer_interested = interested;
                }
            }
            PeerEvent::ExtHandshake {
                peer_addr,
                handshake,
            } => {
                // In FetchingMetadata mode, if we learn the metadata_size, start requesting.
                // M107: Request ALL missing pieces from this peer (full redundancy —
                // every peer gets all pieces, first complete set wins).
                if self.state == TorrentState::FetchingMetadata
                    && let Some(size) = handshake.metadata_size
                    && let Some(ref mut dl) = self.metadata_downloader
                {
                    if size > self.config.max_metadata_size {
                        warn!(
                            size,
                            max = self.config.max_metadata_size,
                            %peer_addr,
                            "peer advertises metadata_size exceeding limit, ignoring"
                        );
                    } else {
                        dl.set_total_size(size);
                        let pieces = dl.request_all_from_peer(peer_addr);
                        if let Some(peer) = self.peers.get(&peer_addr) {
                            for piece in pieces {
                                let _ = peer
                                    .cmd_tx
                                    .send(PeerCommand::RequestMetadata { piece })
                                    .await;
                            }
                        }
                    }
                }
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    // BEP 21: mark upload-only peers
                    peer.upload_only = handshake.is_upload_only();
                    // BEP 55: detect holepunch support
                    peer.supports_holepunch = handshake.ext_id("ut_holepunch").is_some();
                    peer.appears_nated = peer.source != PeerSource::Incoming;
                    peer.ext_handshake = Some(handshake);
                }
                // M108: Spawn PEX send task if peer supports ut_pex and PEX is enabled
                if self.config.enable_pex
                    && let Some(peer) = self.peers.get(&peer_addr)
                    && let Some(ref hs) = peer.ext_handshake
                    && hs.ext_id("ut_pex").is_some()
                {
                    let recipient_is_i2p = is_i2p_synthetic_addr(&peer_addr);
                    tokio::spawn(crate::pex::pex_send_task(
                        peer_addr,
                        peer.cmd_tx.clone(),
                        std::sync::Arc::clone(&self.live_outgoing_peers),
                        self.config.allow_i2p_mixed,
                        recipient_is_i2p,
                    ));
                }
            }
            PeerEvent::MetadataPiece {
                peer_addr: _,
                piece,
                data,
                total_size,
            } => {
                if let Some(ref mut dl) = self.metadata_downloader {
                    dl.set_total_size(total_size);
                    let complete = dl.piece_received(piece, data);
                    if complete {
                        self.try_assemble_metadata().await;
                    }
                }
            }
            PeerEvent::MetadataReject { peer_addr, piece } => {
                if let Some(ref mut dl) = self.metadata_downloader {
                    dl.mark_rejected(peer_addr);
                    debug!(%peer_addr, piece, "peer rejected metadata request, blacklisted");
                }
            }
            PeerEvent::PexPeers { new_peers } => {
                if self.config.enable_pex {
                    self.handle_add_peers(new_peers, PeerSource::Pex);
                }
            }
            PeerEvent::TrackersReceived { tracker_urls } => {
                for url in tracker_urls {
                    if self
                        .tracker_manager
                        .add_tracker_url_validated(&url, &self.config.url_security)
                    {
                        debug!(url = %url, "added tracker from lt_trackers");
                    }
                }
            }
            PeerEvent::IncomingRequest {
                peer_addr,
                index,
                begin,
                length,
            } => {
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    if peer.incoming_requests.len() < self.config.max_outstanding_requests {
                        peer.incoming_requests.push((index, begin, length));
                    } else {
                        debug!(
                            %peer_addr,
                            queue = peer.incoming_requests.len(),
                            "dropping incoming request: max_outstanding_requests exceeded"
                        );
                    }
                }
                self.serve_incoming_requests().await;
            }
            PeerEvent::RejectRequest {
                peer_addr,
                index,
                begin,
                length: _,
            } => {
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.pending_requests.remove(index, begin);
                    // M75: Permit returned by peer task on RejectRequest receipt
                }
                debug!(index, %peer_addr, "request rejected by peer");
            }
            PeerEvent::AllowedFast { peer_addr, index } => {
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.allowed_fast.insert(index);
                }
            }
            PeerEvent::SuggestPiece { peer_addr, index } => {
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.suggested_pieces.insert(index);
                }
            }
            PeerEvent::TransportIdentified {
                peer_addr,
                transport,
            } => {
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.transport = Some(transport);
                }
            }
            PeerEvent::Disconnected { peer_addr, reason } => {
                debug!(%peer_addr, ?reason, "peer disconnected");
                // M108: Count every disconnection for connection success rate diagnostics.
                self.connect_failures = self.connect_failures.saturating_add(1);
                let reason_str = reason.as_deref().unwrap_or("peer disconnected");
                // M106: Use extracted disconnect helper (handles super_seed,
                // slab, availability, shutdown, alert, suggested_to_peers).
                self.disconnect_peer(peer_addr, reason_str);
                self.recalc_max_in_flight();
                self.mark_snapshot_dirty();
                if let Some(ref notify) = self.reservation_notify {
                    notify.notify_waiters();
                }
                // M104: Per-peer exponential backoff — prevent hammering failed peers
                let attempt = self
                    .connect_backoff
                    .get(&peer_addr)
                    .map_or(0, |r| r.value().1);
                let next_attempt = attempt.saturating_add(1);
                let delay_ms = 200u64
                    .saturating_mul(1u64 << next_attempt.min(10))
                    .min(30_000);
                let earliest_retry = std::time::Instant::now() + Duration::from_millis(delay_ms);
                self.connect_backoff
                    .insert(peer_addr, (earliest_retry, next_attempt));

                self.i2p_destinations.remove(&peer_addr);
            }
            PeerEvent::WebSeedPieceData { url, index, data } => {
                self.handle_web_seed_piece_data(url, index, data).await;
            }
            PeerEvent::WebSeedError {
                url,
                piece,
                message,
            } => {
                self.handle_web_seed_error(url, piece, message);
            }
            PeerEvent::HashesReceived {
                peer_addr,
                request,
                hashes,
            } => {
                self.handle_hashes_received(peer_addr, request, hashes)
                    .await;
            }
            PeerEvent::HashRequestRejected { peer_addr, request } => {
                if let Some(ref mut picker) = self.hash_picker {
                    picker.hashes_rejected(&request);
                }
                debug!(peer = %peer_addr, file_root = %request.file_root, "v2 hash request rejected");
            }
            PeerEvent::IncomingHashRequest { peer_addr, request } => {
                self.handle_incoming_hash_request(peer_addr, request).await;
            }
            PeerEvent::HolepunchRendezvous { peer_addr, target } => {
                if self.config.enable_holepunch {
                    self.handle_holepunch_rendezvous(peer_addr, target).await;
                }
            }
            PeerEvent::HolepunchConnect {
                peer_addr: _,
                target,
            } => {
                if self.config.enable_holepunch {
                    self.handle_holepunch_connect(target).await;
                }
            }
            PeerEvent::HolepunchError {
                peer_addr: _,
                target,
                error_code,
            } => {
                let message = torrent_wire::HolepunchError::from_u32(error_code)
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| format!("unknown error code {error_code}"));
                debug!(%target, %error_code, %message, "holepunch rendezvous failed");
                post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::HolepunchFailed {
                        info_hash: self.info_hash,
                        addr: target,
                        error_code: Some(error_code),
                        message,
                    },
                );
            }
            PeerEvent::MseRetry { peer_addr, cmd_tx } => {
                // MSE handshake failed, peer is retrying with plaintext.
                // Update the peer state with the new command channel.
                // M75: No driver to cancel — peer task's cmd_rx will be the new one.
                // Re-send StartRequesting to the new cmd_tx so the peer task gets
                // the reservation state on the retry connection.
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    debug!(%peer_addr, "MSE retry: updating cmd_tx for plaintext attempt");
                    peer.cmd_tx = cmd_tx;
                    // M93: Re-send lock-free dispatch state to the new peer task
                    if let (Some(atomic_states), Some(snapshot), Some(notify)) = (
                        &self.atomic_states,
                        &self.availability_snapshot,
                        &self.reservation_notify,
                    ) && let Some(ref lengths) = self.lengths
                    {
                        let _ = peer.cmd_tx.try_send(PeerCommand::StartRequesting {
                            atomic_states: Arc::clone(atomic_states),
                            availability_snapshot: Arc::clone(snapshot),
                            piece_notify: Arc::clone(notify),
                            disk_handle: self.disk.clone(),
                            write_error_tx: self.write_error_tx.clone(),
                            lengths: lengths.clone(),
                            block_maps: self.block_maps.clone(),
                            steal_candidates: self.steal_candidates.clone(),
                            piece_write_guards: self.piece_write_guards.clone(),
                        });
                    }
                }
            }
            PeerEvent::PieceReleased { peer_addr, piece } => {
                // M93: Actor-side piece_owner cleanup after peer released a piece
                if let Some(slab_idx) = self.peer_slab.slot_of(&peer_addr)
                    && self.piece_owner.get(piece as usize) == Some(&Some(slab_idx))
                {
                    self.piece_owner[piece as usize] = None;
                }
                // Snapshot will be rebuilt on next timer tick; notify waiting peers now
                if let Some(ref notify) = self.reservation_notify {
                    notify.notify_one();
                }
            }
            PeerEvent::PieceBlocksBatch { peer_addr, blocks } => {
                self.handle_piece_blocks_batch(peer_addr, blocks).await;
            }
        }
    }

    /// Disconnect all peers matching a banned IP.
    pub(crate) async fn disconnect_banned_ip(&mut self, ip: std::net::IpAddr) {
        // Remove from connected peers
        let addrs_to_remove: Vec<SocketAddr> = self
            .peers
            .keys()
            .filter(|a| a.ip() == ip)
            .copied()
            .collect();
        for addr in addrs_to_remove {
            if let Some(peer) = self.peers.remove(&addr) {
                let _ = peer.cmd_tx.try_send(PeerCommand::Shutdown);
                self.peers_connected.remove(&addr);
                // M108: Update live peers for PEX send tasks
                self.live_outgoing_peers.write().remove(&addr);
                post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::PeerDisconnected {
                        info_hash: self.info_hash,
                        addr,
                        reason: Some("banned".into()),
                    },
                );
            }
        }
        // M107: No need to clean available_peers — adder task checks ban list on its own.
    }

    /// M106: Extracted disconnect helper — consolidates all peer cleanup into one place.
    ///
    /// Performs: BEP 16 super-seed cleanup, peer removal, end-game cleanup,
    /// slab/piece-owner release, availability decrement, cmd_tx shutdown,
    /// PeerDisconnected alert, and suggested_to_peers cleanup.
    pub(crate) fn disconnect_peer(&mut self, addr: SocketAddr, reason: &str) {
        // BEP 55: attempt holepunch for NAT-related connection failures (M112)
        if self.config.enable_holepunch && should_attempt_holepunch(reason) {
            let now = Instant::now();
            self.holepunch_cooldowns
                .retain(|_, t| now.duration_since(*t) < HOLEPUNCH_COOLDOWN);
            if self.holepunch_cooldowns.len() < HOLEPUNCH_MAX_TRACKED
                && !self.holepunch_cooldowns.contains_key(&addr)
            {
                self.holepunch_cooldowns.insert(addr, now);
                self.holepunch_pending.push(addr);
            }
        }

        // BEP 16: clean up super-seed assignment
        if let Some(ref mut ss) = self.super_seed {
            ss.peer_disconnected(addr);
        }
        if let Some(peer) = self.peers.remove(&addr) {
            // End-game cleanup
            if self.end_game.is_active() {
                self.end_game.peer_disconnected(addr);
            }
            // M93: Release all pieces owned by this peer
            if let Some(slab_idx) = self.peer_slab.remove_by_addr(&addr) {
                for piece in 0..self.num_pieces {
                    if self.piece_owner[piece as usize] == Some(slab_idx) {
                        self.piece_owner[piece as usize] = None;
                        if let Some(ref states) = self.atomic_states {
                            states.release(piece);
                        }
                    }
                }
            }
            // Update availability
            for piece in 0..self.num_pieces {
                if peer.bitfield.get(piece) {
                    self.availability[piece as usize] =
                        self.availability[piece as usize].saturating_sub(1);
                }
            }
            // Send shutdown command (non-blocking — peer may have a full channel)
            let _ = peer.cmd_tx.try_send(PeerCommand::Shutdown);
            // M107: Remove from shared connected set (wakes adder task via semaphore)
            self.peers_connected.remove(&addr);
            // M108: Update live peers for PEX send tasks
            self.live_outgoing_peers.write().remove(&addr);
            post_alert(
                &self.alert_tx,
                &self.alert_mask,
                AlertKind::PeerDisconnected {
                    info_hash: self.info_hash,
                    addr,
                    reason: Some(reason.into()),
                },
            );
        }
        self.suggested_to_peers.remove(&addr);
    }

    /// M106: Score-based peer turnover — replaces rate-based run_peer_turnover().
    ///
    /// Runs on every 1s tick but actual churn is gated by `scorer.should_churn()`.
    /// Snubbed low-score peers are fast-evicted regardless of the churn timer.
    pub(crate) async fn run_scored_turnover(&mut self) {
        if self.state != TorrentState::Downloading {
            return;
        }

        // M107: Bypass scoring when disabled (A/B benchmarking)
        if self.config.disable_peer_scoring {
            return;
        }

        let now = std::time::Instant::now();

        // Snub fast-path: evict snubbed peers below threshold regardless of churn timer
        let snub_evict: Vec<SocketAddr> = self
            .peers
            .iter()
            .filter(|(_, p)| p.snubbed && p.score < self.scorer.min_score_threshold())
            .map(|(addr, _)| *addr)
            .collect();
        for addr in &snub_evict {
            self.disconnect_peer(*addr, "snubbed low-score eviction");
        }
        if !snub_evict.is_empty() {
            self.recalc_max_in_flight();
            self.mark_snapshot_dirty();
        }

        // Regular churn gate
        if !self.scorer.should_churn(now) {
            return;
        }
        self.scorer.mark_churned(now);

        // Skip during endgame
        if self.end_game.is_active() {
            return;
        }

        // Small swarm protection
        let scored_count = self
            .peers
            .values()
            .filter(|p| !self.scorer.is_in_probation(p.connected_at))
            .count();
        if scored_count < 10 {
            return;
        }

        // Build swarm context for dampening
        let ctx = PeerScorer::build_swarm_context(
            self.peers
                .values()
                .filter(|p| !self.scorer.is_in_probation(p.connected_at))
                .map(|p| (p.pipeline.ewma_rate(), p.avg_rtt, p.score)),
        );
        let churn_pct = self.scorer.churn_percent(ctx.median_score);

        // Collect parole IPs
        let parole_ips: std::collections::HashSet<std::net::IpAddr> = self
            .parole_pieces
            .values()
            .filter_map(|p| p.parole_peer)
            .collect();

        // Build eligible set
        let mut eligible: Vec<(SocketAddr, f64)> = self
            .peers
            .iter()
            .filter(|(_, p)| {
                !self.scorer.is_in_probation(p.connected_at)
                    && p.bitfield.count_ones() != self.num_pieces
                    && !parole_ips.contains(&p.addr.ip())
            })
            .filter(|(_, p)| p.score < self.scorer.min_score_threshold())
            .map(|(addr, p)| (*addr, p.score))
            .collect();

        if eligible.is_empty() {
            return;
        }

        eligible.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        let n = ((eligible.len() as f64 * churn_pct).floor() as usize).max(1);
        let to_disconnect: Vec<SocketAddr> =
            eligible.into_iter().take(n).map(|(addr, _)| addr).collect();

        for addr in &to_disconnect {
            self.disconnect_peer(*addr, "scored turnover");
        }
        self.recalc_max_in_flight();
        self.mark_snapshot_dirty();

        // M107: Replacement is now async via adder task — report 0 replaced here;
        // the semaphore release from disconnect_peer will wake the adder naturally.
        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::PeerTurnover {
                info_hash: self.info_hash,
                disconnected: to_disconnect.len(),
                replaced: 0,
            },
        );
    }

    /// M107: Handle a connect request from the peer adder task.
    /// The adder has already validated and acquired a semaphore permit.
    pub(crate) fn handle_adder_connect(&mut self, connect_peer: ConnectPeer) {
        // M108: Count every connect request that reaches the actor (before dedup).
        self.connect_attempts = self.connect_attempts.saturating_add(1);

        let ConnectPeer {
            addr,
            source,
            permit,
        } = connect_peer;

        // Final check — peer might have connected between adder validation and here
        if self.peers.contains_key(&addr) {
            drop(permit);
            return;
        }

        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let bitfield = if self.super_seed.is_some() {
            // Super seeding: send empty bitfield to hide our pieces
            Bitfield::new(self.num_pieces)
        } else {
            self.chunk_tracker
                .as_ref()
                .map(|ct| ct.bitfield().clone())
                .unwrap_or_else(|| Bitfield::new(self.num_pieces))
        };

        self.peers
            .insert(addr, PeerState::new(addr, self.num_pieces, cmd_tx, source));
        self.peers_connected.insert(addr, ());
        // M108: Update live peers for PEX send tasks
        self.live_outgoing_peers.write().insert(addr);
        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::PeerConnected {
                info_hash: self.info_hash,
                addr,
            },
        );
        // M93: Send lock-free dispatch state to peer for integrated dispatch
        if let (Some(atomic_states), Some(snapshot), Some(notify)) = (
            &self.atomic_states,
            &self.availability_snapshot,
            &self.reservation_notify,
        ) && let Some(peer) = self.peers.get(&addr)
            && let Some(ref lengths) = self.lengths
        {
            let _ = peer.cmd_tx.try_send(PeerCommand::StartRequesting {
                atomic_states: Arc::clone(atomic_states),
                availability_snapshot: Arc::clone(snapshot),
                piece_notify: Arc::clone(notify),
                disk_handle: self.disk.clone(),
                write_error_tx: self.write_error_tx.clone(),
                lengths: lengths.clone(),
                block_maps: self.block_maps.clone(),
                steal_candidates: self.steal_candidates.clone(),
                piece_write_guards: self.piece_write_guards.clone(),
            });
        }

        // I2P outbound: route through SAM instead of TCP/uTP
        if let Some(i2p_dest) = self.i2p_destinations.get(&addr).cloned() {
            if let Some(ref sam) = self.sam_session {
                let sam = Arc::clone(sam);
                let event_tx = self.event_tx.clone();
                let info_hash = self.info_hash;
                let peer_id = self.our_peer_id;
                let num_pieces = self.num_pieces;
                let enable_dht = self.config.enable_dht;
                let enable_fast = self.config.enable_fast;
                let anonymous_mode = self.config.anonymous_mode;
                let max_message_size = self.config.max_message_size;
                let peer_connect_timeout = self.config.peer_connect_timeout;
                let info_bytes = self.meta.as_ref().and_then(|m| m.info_bytes.clone());
                let plugins = Arc::clone(&self.plugins);
                let have_broadcast_tx = self.have_broadcast_tx.clone();

                tokio::spawn(async move {
                    let _permit = permit; // M107: hold for peer lifetime
                    // Apply connect timeout consistent with TCP path
                    let connect_result = if peer_connect_timeout > 0 {
                        match tokio::time::timeout(
                            Duration::from_secs(peer_connect_timeout),
                            sam.connect(&i2p_dest),
                        )
                        .await
                        {
                            Ok(result) => result,
                            Err(_) => {
                                debug!(%addr, timeout_secs = peer_connect_timeout, "I2P SAM connect timed out");
                                let _ = event_tx
                                    .send(PeerEvent::Disconnected {
                                        peer_addr: addr,
                                        reason: Some("I2P connect: timeout".into()),
                                    })
                                    .await;
                                return;
                            }
                        }
                    } else {
                        sam.connect(&i2p_dest).await
                    };

                    match connect_result {
                        Ok(sam_stream) => {
                            let tcp_stream = sam_stream.into_inner();
                            let _ = event_tx
                                .send(PeerEvent::TransportIdentified {
                                    peer_addr: addr,
                                    transport: crate::rate_limiter::PeerTransport::Tcp,
                                })
                                .await;
                            // I2P provides encryption; disable MSE
                            match run_peer(
                                addr,
                                tcp_stream,
                                info_hash,
                                peer_id,
                                bitfield,
                                num_pieces,
                                event_tx.clone(),
                                cmd_rx,
                                enable_dht,
                                enable_fast,
                                torrent_wire::mse::EncryptionMode::Disabled,
                                true, // outbound
                                anonymous_mode,
                                info_bytes,
                                plugins,
                                false, // enable_holepunch — N/A for I2P
                                max_message_size,
                                have_broadcast_tx.subscribe(),
                            )
                            .await
                            {
                                Ok(()) => {
                                    debug!(%addr, "I2P peer session ended normally");
                                }
                                Err(e) => {
                                    let _ = event_tx
                                        .send(PeerEvent::Disconnected {
                                            peer_addr: addr,
                                            reason: Some(e.to_string()),
                                        })
                                        .await;
                                }
                            }
                        }
                        Err(e) => {
                            debug!(%addr, error = %e, "I2P SAM connect failed");
                            let _ = event_tx
                                .send(PeerEvent::Disconnected {
                                    peer_addr: addr,
                                    reason: Some(format!("I2P connect: {e}")),
                                })
                                .await;
                        }
                    }
                });
                return; // Skip TCP/uTP path
            }
            // No SAM session — can't connect to I2P peer, clean up
            self.peers.remove(&addr);
            self.peers_connected.remove(&addr);
            // M108: Update live peers for PEX send tasks
            self.live_outgoing_peers.write().remove(&addr);
            return;
        }

        let info_hash = self.info_hash;
        let peer_id = self.our_peer_id;
        let num_pieces = self.num_pieces;
        let event_tx = self.event_tx.clone();
        let enable_dht = self.config.enable_dht;
        let enable_fast = self.config.enable_fast;
        let encryption_mode = self.config.encryption_mode;
        let enable_utp = self.config.enable_utp;
        let proxy_config = self.config.proxy.clone();
        let use_proxy = proxy_config.proxy_type != crate::proxy::ProxyType::None
            && proxy_config.proxy_peer_connections;
        let anonymous_mode = self.config.anonymous_mode;
        let enable_holepunch = self.config.enable_holepunch;
        let max_message_size = self.config.max_message_size;
        let peer_connect_timeout = self.config.peer_connect_timeout;
        let info_bytes = self.meta.as_ref().and_then(|m| m.info_bytes.clone());
        let factory = Arc::clone(&self.factory);
        // Pick the uTP socket matching the peer's address family
        let utp_socket = if addr.is_ipv6() {
            self.utp_socket_v6.clone()
        } else {
            self.utp_socket.clone()
        };
        let plugins = Arc::clone(&self.plugins);
        let have_broadcast_tx = self.have_broadcast_tx.clone();

        // SSL torrent: pre-build client TLS config for outbound connections (M42).
        // If ssl_cert is present in the torrent metadata and we have an ssl_manager,
        // TCP connections will be wrapped in TLS.
        let ssl_client_config = self
            .meta
            .as_ref()
            .and_then(|m| m.ssl_cert.as_deref())
            .and_then(|cert| {
                self.ssl_manager
                    .as_ref()
                    .and_then(|mgr| match mgr.client_config(cert) {
                        Ok(cfg) => Some(cfg),
                        Err(e) => {
                            warn!(%addr, error = %e, "failed to build SSL client config");
                            None
                        }
                    })
            });

        tokio::spawn(async move {
            let _permit = permit; // M107: hold for peer lifetime

            // M107 Change 5: TCP+uTP race with 1s TCP head start.
            //
            // When uTP is available, we race TCP (starts immediately) against
            // uTP (starts after 1s delay, or immediately if TCP fails first).
            // First transport to connect wins; the loser is cancelled by select!.
            //
            // When uTP is unavailable (proxy active, disabled, or no socket),
            // we fall through to the TCP-only path.
            if enable_utp
                && !use_proxy
                && let Some(socket) = utp_socket
            {
                // Signal from TCP future: notify uTP to start early if TCP fails
                let tcp_failed = Arc::new(tokio::sync::Notify::new());

                let tcp_notify = Arc::clone(&tcp_failed);
                let tcp_factory = Arc::clone(&factory);
                // Note: use_proxy is guaranteed false here (checked above),
                // so TCP connects directly via the transport factory.
                let tcp_fut = async move {
                    let result = if peer_connect_timeout > 0 {
                        match tokio::time::timeout(
                            Duration::from_secs(peer_connect_timeout),
                            tcp_factory.connect_tcp(addr),
                        )
                        .await
                        {
                            Ok(r) => r,
                            Err(_) => {
                                debug!(%addr, timeout_secs = peer_connect_timeout, "TCP connect timed out");
                                Err(std::io::Error::new(
                                    std::io::ErrorKind::TimedOut,
                                    "connect timeout",
                                ))
                            }
                        }
                    } else {
                        tcp_factory.connect_tcp(addr).await
                    };
                    if result.is_err() {
                        tcp_notify.notify_one();
                    }
                    result
                };

                let utp_fut = async {
                    // Wait for either: 1s head-start delay OR early TCP failure
                    tokio::select! {
                        biased;
                        () = tcp_failed.notified() => {
                            debug!(%addr, "TCP failed early, starting uTP immediately");
                        }
                        () = tokio::time::sleep(Duration::from_secs(1)) => {
                            debug!(%addr, "1s head-start elapsed, starting uTP");
                        }
                    }
                    socket.connect(addr).await
                };

                // Pin both futures for select!
                tokio::pin!(tcp_fut);
                tokio::pin!(utp_fut);

                // Race the two transports. `biased` gives TCP slight priority
                // when both complete in the same poll cycle (expected: TCP
                // started first, so it should win ties).
                tokio::select! {
                    biased;
                    tcp_result = &mut tcp_fut => {
                        match tcp_result {
                            Ok(stream) => {
                                debug!(%addr, "TCP won the race");
                                let _ = event_tx
                                    .send(PeerEvent::TransportIdentified {
                                        peer_addr: addr,
                                        transport: crate::rate_limiter::PeerTransport::Tcp,
                                    })
                                    .await;
                                // SSL torrent: wrap TCP stream in TLS (M42)
                                if let Some(ref client_config) = ssl_client_config {
                                    match torrent_wire::ssl::connect_tls(
                                        stream,
                                        info_hash,
                                        Arc::clone(client_config),
                                    )
                                    .await
                                    {
                                        Ok(tls_stream) => {
                                            debug!(%addr, "SSL/TLS connection established");
                                            let _ = run_peer(
                                                addr,
                                                tls_stream,
                                                info_hash,
                                                peer_id,
                                                bitfield,
                                                num_pieces,
                                                event_tx,
                                                cmd_rx,
                                                enable_dht,
                                                enable_fast,
                                                torrent_wire::mse::EncryptionMode::Disabled,
                                                true, // outbound
                                                anonymous_mode,
                                                info_bytes,
                                                plugins,
                                                enable_holepunch,
                                                max_message_size,
                                                have_broadcast_tx.subscribe(),
                                            )
                                            .await;
                                        }
                                        Err(e) => {
                                            debug!(%addr, error = %e, "SSL/TLS handshake failed");
                                            let _ = event_tx
                                                .send(PeerEvent::Disconnected {
                                                    peer_addr: addr,
                                                    reason: Some(format!("TLS handshake failed: {e}")),
                                                })
                                                .await;
                                        }
                                    }
                                } else {
                                    match run_peer(
                                        addr,
                                        stream,
                                        info_hash,
                                        peer_id,
                                        bitfield.clone(),
                                        num_pieces,
                                        event_tx.clone(),
                                        cmd_rx,
                                        enable_dht,
                                        enable_fast,
                                        encryption_mode,
                                        true, // outbound
                                        anonymous_mode,
                                        info_bytes.clone(),
                                        Arc::clone(&plugins),
                                        enable_holepunch,
                                        max_message_size,
                                        have_broadcast_tx.subscribe(),
                                    )
                                    .await
                                    {
                                        Ok(()) => debug!(%addr, "TCP peer session ended normally"),
                                        Err(e) => {
                                            // MSE fallback for TCP
                                            let retry_mode = match encryption_mode {
                                                torrent_wire::mse::EncryptionMode::Enabled => {
                                                    Some(torrent_wire::mse::EncryptionMode::Disabled)
                                                }
                                                torrent_wire::mse::EncryptionMode::PreferPlaintext => {
                                                    Some(torrent_wire::mse::EncryptionMode::Enabled)
                                                }
                                                _ => None,
                                            };
                                            if let Some(fallback_mode) = retry_mode {
                                                debug!(%addr, error = %e, ?fallback_mode, "TCP MSE failed, retrying with fallback");
                                                if let Ok(tcp2) = factory.connect_tcp(addr).await {
                                                    let (retry_tx, retry_rx) = mpsc::channel(256);
                                                    let _ = event_tx
                                                        .send(PeerEvent::MseRetry {
                                                            peer_addr: addr,
                                                            cmd_tx: retry_tx,
                                                        })
                                                        .await;
                                                    match run_peer(
                                                        addr,
                                                        tcp2,
                                                        info_hash,
                                                        peer_id,
                                                        bitfield,
                                                        num_pieces,
                                                        event_tx.clone(),
                                                        retry_rx,
                                                        enable_dht,
                                                        enable_fast,
                                                        fallback_mode,
                                                        true,
                                                        anonymous_mode,
                                                        info_bytes,
                                                        plugins,
                                                        enable_holepunch,
                                                        max_message_size,
                                                        have_broadcast_tx.subscribe(),
                                                    )
                                                    .await
                                                    {
                                                        Ok(()) => {
                                                            debug!(%addr, "TCP fallback session ended normally")
                                                        }
                                                        Err(e2) => {
                                                            debug!(%addr, error = %e2, "TCP fallback also failed");
                                                            let _ = event_tx
                                                                .send(PeerEvent::Disconnected {
                                                                    peer_addr: addr,
                                                                    reason: Some(e2.to_string()),
                                                                })
                                                                .await;
                                                        }
                                                    }
                                                } else {
                                                    debug!(%addr, "TCP fallback reconnect failed");
                                                    let _ = event_tx
                                                        .send(PeerEvent::Disconnected {
                                                            peer_addr: addr,
                                                            reason: Some(e.to_string()),
                                                        })
                                                        .await;
                                                }
                                            } else {
                                                debug!(%addr, error = %e, "TCP peer session failed");
                                                let _ = event_tx
                                                    .send(PeerEvent::Disconnected {
                                                        peer_addr: addr,
                                                        reason: Some(e.to_string()),
                                                    })
                                                    .await;
                                            }
                                        }
                                    }
                                }
                            }
                            Err(tcp_err) => {
                                // TCP failed; wait for uTP result (already unblocked
                                // via Notify, so it should be connecting or will soon)
                                debug!(%addr, error = %tcp_err, "TCP lost the race (failed)");
                                match utp_fut.await {
                                    Ok(stream) => {
                                        debug!(%addr, "uTP connected after TCP failure");
                                        let _ = event_tx
                                            .send(PeerEvent::TransportIdentified {
                                                peer_addr: addr,
                                                transport: crate::rate_limiter::PeerTransport::Utp,
                                            })
                                            .await;
                                        match run_peer(
                                            addr,
                                            stream,
                                            info_hash,
                                            peer_id,
                                            bitfield.clone(),
                                            num_pieces,
                                            event_tx.clone(),
                                            cmd_rx,
                                            enable_dht,
                                            enable_fast,
                                            encryption_mode,
                                            true, // outbound
                                            anonymous_mode,
                                            info_bytes.clone(),
                                            Arc::clone(&plugins),
                                            enable_holepunch,
                                            max_message_size,
                                            have_broadcast_tx.subscribe(),
                                        )
                                        .await
                                        {
                                            Ok(()) => debug!(%addr, "uTP peer session ended normally"),
                                            Err(e) => {
                                                // MSE fallback for uTP
                                                let retry_mode = match encryption_mode {
                                                    torrent_wire::mse::EncryptionMode::Enabled => {
                                                        Some(torrent_wire::mse::EncryptionMode::Disabled)
                                                    }
                                                    torrent_wire::mse::EncryptionMode::PreferPlaintext => {
                                                        Some(torrent_wire::mse::EncryptionMode::Enabled)
                                                    }
                                                    _ => None,
                                                };
                                                if let Some(fallback_mode) = retry_mode {
                                                    debug!(%addr, error = %e, ?fallback_mode, "uTP MSE failed, retrying with fallback");
                                                    if let Ok(Ok(stream2)) = tokio::time::timeout(
                                                        Duration::from_secs(5),
                                                        socket.connect(addr),
                                                    )
                                                    .await
                                                    {
                                                        let (retry_tx, retry_rx) = mpsc::channel(256);
                                                        let _ = event_tx
                                                            .send(PeerEvent::MseRetry {
                                                                peer_addr: addr,
                                                                cmd_tx: retry_tx,
                                                            })
                                                            .await;
                                                        match run_peer(
                                                            addr,
                                                            stream2,
                                                            info_hash,
                                                            peer_id,
                                                            bitfield,
                                                            num_pieces,
                                                            event_tx.clone(),
                                                            retry_rx,
                                                            enable_dht,
                                                            enable_fast,
                                                            fallback_mode,
                                                            true,
                                                            anonymous_mode,
                                                            info_bytes.clone(),
                                                            Arc::clone(&plugins),
                                                            enable_holepunch,
                                                            max_message_size,
                                                            have_broadcast_tx.subscribe(),
                                                        )
                                                        .await
                                                        {
                                                            Ok(()) => {
                                                                debug!(%addr, "uTP fallback session ended normally")
                                                            }
                                                            Err(e2) => {
                                                                debug!(%addr, error = %e2, "uTP fallback also failed");
                                                                let _ = event_tx
                                                                    .send(PeerEvent::Disconnected {
                                                                        peer_addr: addr,
                                                                        reason: Some(e2.to_string()),
                                                                    })
                                                                    .await;
                                                            }
                                                        }
                                                    } else {
                                                        debug!(%addr, "uTP fallback reconnect failed");
                                                        let _ = event_tx
                                                            .send(PeerEvent::Disconnected {
                                                                peer_addr: addr,
                                                                reason: Some(e.to_string()),
                                                            })
                                                            .await;
                                                    }
                                                } else {
                                                    debug!(%addr, error = %e, "uTP peer session failed");
                                                    let _ = event_tx
                                                        .send(PeerEvent::Disconnected {
                                                            peer_addr: addr,
                                                            reason: Some(e.to_string()),
                                                        })
                                                        .await;
                                                }
                                            }
                                        }
                                    }
                                    Err(utp_err) => {
                                        // Both transports failed
                                        debug!(
                                            %addr,
                                            tcp_error = %tcp_err,
                                            utp_error = %utp_err,
                                            "both TCP and uTP failed"
                                        );
                                        let _ = event_tx
                                            .send(PeerEvent::Disconnected {
                                                peer_addr: addr,
                                                reason: Some(format!(
                                                    "TCP: {tcp_err}; uTP: {utp_err}"
                                                )),
                                            })
                                            .await;
                                    }
                                }
                            }
                        }
                    }
                    utp_result = &mut utp_fut => {
                        match utp_result {
                            Ok(stream) => {
                                debug!(%addr, "uTP won the race");
                                let _ = event_tx
                                    .send(PeerEvent::TransportIdentified {
                                        peer_addr: addr,
                                        transport: crate::rate_limiter::PeerTransport::Utp,
                                    })
                                    .await;
                                match run_peer(
                                    addr,
                                    stream,
                                    info_hash,
                                    peer_id,
                                    bitfield.clone(),
                                    num_pieces,
                                    event_tx.clone(),
                                    cmd_rx,
                                    enable_dht,
                                    enable_fast,
                                    encryption_mode,
                                    true, // outbound
                                    anonymous_mode,
                                    info_bytes.clone(),
                                    Arc::clone(&plugins),
                                    enable_holepunch,
                                    max_message_size,
                                    have_broadcast_tx.subscribe(),
                                )
                                .await
                                {
                                    Ok(()) => debug!(%addr, "uTP peer session ended normally"),
                                    Err(e) => {
                                        // MSE fallback for uTP
                                        let retry_mode = match encryption_mode {
                                            torrent_wire::mse::EncryptionMode::Enabled => {
                                                Some(torrent_wire::mse::EncryptionMode::Disabled)
                                            }
                                            torrent_wire::mse::EncryptionMode::PreferPlaintext => {
                                                Some(torrent_wire::mse::EncryptionMode::Enabled)
                                            }
                                            _ => None,
                                        };
                                        if let Some(fallback_mode) = retry_mode {
                                            debug!(%addr, error = %e, ?fallback_mode, "uTP MSE failed, retrying with fallback");
                                            if let Ok(Ok(stream2)) = tokio::time::timeout(
                                                Duration::from_secs(5),
                                                socket.connect(addr),
                                            )
                                            .await
                                            {
                                                let (retry_tx, retry_rx) = mpsc::channel(256);
                                                let _ = event_tx
                                                    .send(PeerEvent::MseRetry {
                                                        peer_addr: addr,
                                                        cmd_tx: retry_tx,
                                                    })
                                                    .await;
                                                match run_peer(
                                                    addr,
                                                    stream2,
                                                    info_hash,
                                                    peer_id,
                                                    bitfield,
                                                    num_pieces,
                                                    event_tx.clone(),
                                                    retry_rx,
                                                    enable_dht,
                                                    enable_fast,
                                                    fallback_mode,
                                                    true,
                                                    anonymous_mode,
                                                    info_bytes.clone(),
                                                    Arc::clone(&plugins),
                                                    enable_holepunch,
                                                    max_message_size,
                                                    have_broadcast_tx.subscribe(),
                                                )
                                                .await
                                                {
                                                    Ok(()) => {
                                                        debug!(%addr, "uTP fallback session ended normally")
                                                    }
                                                    Err(e2) => {
                                                        debug!(%addr, error = %e2, "uTP fallback also failed");
                                                        let _ = event_tx
                                                            .send(PeerEvent::Disconnected {
                                                                peer_addr: addr,
                                                                reason: Some(e2.to_string()),
                                                            })
                                                            .await;
                                                    }
                                                }
                                            } else {
                                                debug!(%addr, "uTP fallback reconnect failed");
                                                let _ = event_tx
                                                    .send(PeerEvent::Disconnected {
                                                        peer_addr: addr,
                                                        reason: Some(e.to_string()),
                                                    })
                                                    .await;
                                            }
                                        } else {
                                            debug!(%addr, error = %e, "uTP peer session failed");
                                            let _ = event_tx
                                                .send(PeerEvent::Disconnected {
                                                    peer_addr: addr,
                                                    reason: Some(e.to_string()),
                                                })
                                                .await;
                                        }
                                    }
                                }
                            }
                            Err(utp_err) => {
                                // uTP finished first with error; TCP still running.
                                // Wait for TCP result.
                                debug!(%addr, error = %utp_err, "uTP lost the race (failed), waiting for TCP");
                                match tcp_fut.await {
                                    Ok(stream) => {
                                        debug!(%addr, "TCP connected after uTP failure");
                                        let _ = event_tx
                                            .send(PeerEvent::TransportIdentified {
                                                peer_addr: addr,
                                                transport: crate::rate_limiter::PeerTransport::Tcp,
                                            })
                                            .await;
                                        if let Some(ref client_config) = ssl_client_config {
                                            match torrent_wire::ssl::connect_tls(
                                                stream,
                                                info_hash,
                                                Arc::clone(client_config),
                                            )
                                            .await
                                            {
                                                Ok(tls_stream) => {
                                                    debug!(%addr, "SSL/TLS connection established");
                                                    let _ = run_peer(
                                                        addr,
                                                        tls_stream,
                                                        info_hash,
                                                        peer_id,
                                                        bitfield,
                                                        num_pieces,
                                                        event_tx,
                                                        cmd_rx,
                                                        enable_dht,
                                                        enable_fast,
                                                        torrent_wire::mse::EncryptionMode::Disabled,
                                                        true, // outbound
                                                        anonymous_mode,
                                                        info_bytes,
                                                        plugins,
                                                        enable_holepunch,
                                                        max_message_size,
                                                        have_broadcast_tx.subscribe(),
                                                    )
                                                    .await;
                                                }
                                                Err(e) => {
                                                    debug!(%addr, error = %e, "SSL/TLS handshake failed");
                                                    let _ = event_tx
                                                        .send(PeerEvent::Disconnected {
                                                            peer_addr: addr,
                                                            reason: Some(format!("TLS handshake failed: {e}")),
                                                        })
                                                        .await;
                                                }
                                            }
                                        } else {
                                            match run_peer(
                                                addr,
                                                stream,
                                                info_hash,
                                                peer_id,
                                                bitfield.clone(),
                                                num_pieces,
                                                event_tx.clone(),
                                                cmd_rx,
                                                enable_dht,
                                                enable_fast,
                                                encryption_mode,
                                                true, // outbound
                                                anonymous_mode,
                                                info_bytes.clone(),
                                                Arc::clone(&plugins),
                                                enable_holepunch,
                                                max_message_size,
                                                have_broadcast_tx.subscribe(),
                                            )
                                            .await
                                            {
                                                Ok(()) => debug!(%addr, "TCP peer session ended normally"),
                                                Err(e) => {
                                                    // MSE fallback for TCP
                                                    let retry_mode = match encryption_mode {
                                                        torrent_wire::mse::EncryptionMode::Enabled => {
                                                            Some(torrent_wire::mse::EncryptionMode::Disabled)
                                                        }
                                                        torrent_wire::mse::EncryptionMode::PreferPlaintext => {
                                                            Some(torrent_wire::mse::EncryptionMode::Enabled)
                                                        }
                                                        _ => None,
                                                    };
                                                    if let Some(fallback_mode) = retry_mode {
                                                        debug!(%addr, error = %e, ?fallback_mode, "TCP MSE failed, retrying with fallback");
                                                        if let Ok(tcp2) = factory.connect_tcp(addr).await {
                                                            let (retry_tx, retry_rx) = mpsc::channel(256);
                                                            let _ = event_tx
                                                                .send(PeerEvent::MseRetry {
                                                                    peer_addr: addr,
                                                                    cmd_tx: retry_tx,
                                                                })
                                                                .await;
                                                            match run_peer(
                                                                addr,
                                                                tcp2,
                                                                info_hash,
                                                                peer_id,
                                                                bitfield,
                                                                num_pieces,
                                                                event_tx.clone(),
                                                                retry_rx,
                                                                enable_dht,
                                                                enable_fast,
                                                                fallback_mode,
                                                                true,
                                                                anonymous_mode,
                                                                info_bytes,
                                                                plugins,
                                                                enable_holepunch,
                                                                max_message_size,
                                                                have_broadcast_tx.subscribe(),
                                                            )
                                                            .await
                                                            {
                                                                Ok(()) => {
                                                                    debug!(%addr, "TCP fallback session ended normally")
                                                                }
                                                                Err(e2) => {
                                                                    debug!(%addr, error = %e2, "TCP fallback also failed");
                                                                    let _ = event_tx
                                                                        .send(PeerEvent::Disconnected {
                                                                            peer_addr: addr,
                                                                            reason: Some(e2.to_string()),
                                                                        })
                                                                        .await;
                                                                }
                                                            }
                                                        } else {
                                                            debug!(%addr, "TCP fallback reconnect failed");
                                                            let _ = event_tx
                                                                .send(PeerEvent::Disconnected {
                                                                    peer_addr: addr,
                                                                    reason: Some(e.to_string()),
                                                                })
                                                                .await;
                                                        }
                                                    } else {
                                                        debug!(%addr, error = %e, "TCP peer session failed");
                                                        let _ = event_tx
                                                            .send(PeerEvent::Disconnected {
                                                                peer_addr: addr,
                                                                reason: Some(e.to_string()),
                                                            })
                                                            .await;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    Err(tcp_err) => {
                                        // Both transports failed
                                        debug!(
                                            %addr,
                                            tcp_error = %tcp_err,
                                            utp_error = %utp_err,
                                            "both TCP and uTP failed"
                                        );
                                        let _ = event_tx
                                            .send(PeerEvent::Disconnected {
                                                peer_addr: addr,
                                                reason: Some(format!(
                                                    "TCP: {tcp_err}; uTP: {utp_err}"
                                                )),
                                            })
                                            .await;
                                    }
                                }
                            }
                        }
                    }
                }
                return;
            }

            // TCP-only path — uTP unavailable (proxy active, disabled, or no socket)
            let tcp_result = if use_proxy {
                crate::proxy::connect_through_proxy(&proxy_config, addr)
                    .await
                    .map(crate::transport::BoxedStream::new)
            } else if peer_connect_timeout > 0 {
                match tokio::time::timeout(
                    Duration::from_secs(peer_connect_timeout),
                    factory.connect_tcp(addr),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => {
                        debug!(%addr, timeout_secs = peer_connect_timeout, "TCP connect timed out");
                        Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "connect timeout",
                        ))
                    }
                }
            } else {
                factory.connect_tcp(addr).await
            };
            match tcp_result {
                Ok(stream) => {
                    let _ = event_tx
                        .send(PeerEvent::TransportIdentified {
                            peer_addr: addr,
                            transport: crate::rate_limiter::PeerTransport::Tcp,
                        })
                        .await;
                    // SSL torrent: wrap TCP stream in TLS (M42)
                    if let Some(ref client_config) = ssl_client_config {
                        match torrent_wire::ssl::connect_tls(
                            stream,
                            info_hash,
                            Arc::clone(client_config),
                        )
                        .await
                        {
                            Ok(tls_stream) => {
                                debug!(%addr, "SSL/TLS connection established");
                                // TLS provides encryption; disable MSE
                                let _ = run_peer(
                                    addr,
                                    tls_stream,
                                    info_hash,
                                    peer_id,
                                    bitfield,
                                    num_pieces,
                                    event_tx,
                                    cmd_rx,
                                    enable_dht,
                                    enable_fast,
                                    torrent_wire::mse::EncryptionMode::Disabled,
                                    true, // outbound
                                    anonymous_mode,
                                    info_bytes,
                                    plugins,
                                    enable_holepunch,
                                    max_message_size,
                                    have_broadcast_tx.subscribe(),
                                )
                                .await;
                            }
                            Err(e) => {
                                debug!(%addr, error = %e, "SSL/TLS handshake failed");
                                let _ = event_tx
                                    .send(PeerEvent::Disconnected {
                                        peer_addr: addr,
                                        reason: Some(format!("TLS handshake failed: {e}")),
                                    })
                                    .await;
                            }
                        }
                    } else {
                        match run_peer(
                            addr,
                            stream,
                            info_hash,
                            peer_id,
                            bitfield.clone(),
                            num_pieces,
                            event_tx.clone(),
                            cmd_rx,
                            enable_dht,
                            enable_fast,
                            encryption_mode,
                            true, // outbound
                            anonymous_mode,
                            info_bytes.clone(),
                            Arc::clone(&plugins),
                            enable_holepunch,
                            max_message_size,
                            have_broadcast_tx.subscribe(),
                        )
                        .await
                        {
                            Ok(()) => debug!(%addr, "TCP peer session ended normally"),
                            Err(e) => {
                                // MSE fallback for TCP: determine retry mode.
                                let retry_mode = match encryption_mode {
                                    torrent_wire::mse::EncryptionMode::Enabled => {
                                        Some(torrent_wire::mse::EncryptionMode::Disabled)
                                    }
                                    torrent_wire::mse::EncryptionMode::PreferPlaintext => {
                                        Some(torrent_wire::mse::EncryptionMode::Enabled)
                                    }
                                    _ => None,
                                };
                                if let Some(fallback_mode) = retry_mode {
                                    debug!(%addr, error = %e, ?fallback_mode, "TCP MSE failed, retrying with fallback");
                                    if let Ok(tcp2) = factory.connect_tcp(addr).await {
                                        let (retry_tx, retry_rx) = mpsc::channel(256);
                                        let _ = event_tx
                                            .send(PeerEvent::MseRetry {
                                                peer_addr: addr,
                                                cmd_tx: retry_tx,
                                            })
                                            .await;
                                        match run_peer(
                                            addr,
                                            tcp2,
                                            info_hash,
                                            peer_id,
                                            bitfield,
                                            num_pieces,
                                            event_tx.clone(),
                                            retry_rx,
                                            enable_dht,
                                            enable_fast,
                                            fallback_mode,
                                            true,
                                            anonymous_mode,
                                            info_bytes,
                                            plugins,
                                            enable_holepunch,
                                            max_message_size,
                                            have_broadcast_tx.subscribe(),
                                        )
                                        .await
                                        {
                                            Ok(()) => {
                                                debug!(%addr, "TCP fallback session ended normally")
                                            }
                                            Err(e2) => {
                                                debug!(%addr, error = %e2, "TCP fallback also failed");
                                                let _ = event_tx
                                                    .send(PeerEvent::Disconnected {
                                                        peer_addr: addr,
                                                        reason: Some(e2.to_string()),
                                                    })
                                                    .await;
                                            }
                                        }
                                    } else {
                                        debug!(%addr, "TCP fallback reconnect failed");
                                        let _ = event_tx
                                            .send(PeerEvent::Disconnected {
                                                peer_addr: addr,
                                                reason: Some(e.to_string()),
                                            })
                                            .await;
                                    }
                                } else {
                                    debug!(%addr, error = %e, "TCP peer session failed");
                                    let _ = event_tx
                                        .send(PeerEvent::Disconnected {
                                            peer_addr: addr,
                                            reason: Some(e.to_string()),
                                        })
                                        .await;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = event_tx
                        .send(PeerEvent::Disconnected {
                            peer_addr: addr,
                            reason: Some(e.to_string()),
                        })
                        .await;
                }
            }
        });
    }
}

impl TorrentActor {
    /// Spawn a peer task from an already-connected stream (for incoming connections and tests).
    pub(crate) fn spawn_peer_from_stream(
        &mut self,
        addr: SocketAddr,
        stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    ) {
        self.has_incoming = true;
        self.spawn_peer_from_stream_with_mode(addr, stream, None);
    }

    /// Spawn a peer task with an optional encryption mode override.
    ///
    /// When `mode_override` is `Some`, that mode is used instead of the torrent config's
    /// encryption mode. This is used for uTP inbound peers where MSE has already been
    /// ruled out by the session routing layer.
    pub(crate) fn spawn_peer_from_stream_with_mode(
        &mut self,
        addr: SocketAddr,
        stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
        mode_override: Option<torrent_wire::mse::EncryptionMode>,
    ) {
        if self.peers.contains_key(&addr) {
            return;
        }
        // M107: Acquire semaphore permit for inbound peer (non-blocking)
        let permit = match self.peer_semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                debug!(%addr, "rejected inbound peer: at connection capacity");
                return;
            }
        };

        // Reject banned incoming peers
        if crate::timed_lock::TimedGuard::new(
            self.ban_manager.read(),
            &self.lock_timing,
            "ban_manager",
        )
        .is_banned(&addr.ip())
        {
            debug!(%addr, "rejected banned incoming peer");
            return;
        }

        // Reject IP-filtered incoming peers
        if crate::timed_lock::TimedGuard::new(self.ip_filter.read(), &self.lock_timing, "ip_filter")
            .is_blocked(addr.ip())
        {
            debug!(%addr, "rejected IP-filtered incoming peer");
            post_alert(
                &self.alert_tx,
                &self.alert_mask,
                AlertKind::PeerBlocked { addr },
            );
            return;
        }

        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let bitfield = if self.super_seed.is_some() {
            // Super seeding: send empty bitfield to hide our pieces
            Bitfield::new(self.num_pieces)
        } else {
            self.chunk_tracker
                .as_ref()
                .map(|ct| ct.bitfield().clone())
                .unwrap_or_else(|| Bitfield::new(self.num_pieces))
        };

        self.peers.insert(
            addr,
            PeerState::new(addr, self.num_pieces, cmd_tx, PeerSource::Incoming),
        );
        self.peers_connected.insert(addr, ());
        // M108: Update live peers for PEX send tasks
        self.live_outgoing_peers.write().insert(addr);
        // Identify transport for incoming peers (M45)
        let transport = if mode_override.is_some() {
            crate::rate_limiter::PeerTransport::Utp
        } else {
            crate::rate_limiter::PeerTransport::Tcp
        };
        if let Some(peer) = self.peers.get_mut(&addr) {
            peer.transport = Some(transport);
        }
        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::PeerConnected {
                info_hash: self.info_hash,
                addr,
            },
        );
        // M93: Send lock-free dispatch state to peer for integrated dispatch
        if let (Some(atomic_states), Some(snapshot), Some(notify)) = (
            &self.atomic_states,
            &self.availability_snapshot,
            &self.reservation_notify,
        ) && let Some(peer) = self.peers.get(&addr)
            && let Some(ref lengths) = self.lengths
        {
            let _ = peer.cmd_tx.try_send(PeerCommand::StartRequesting {
                atomic_states: Arc::clone(atomic_states),
                availability_snapshot: Arc::clone(snapshot),
                piece_notify: Arc::clone(notify),
                disk_handle: self.disk.clone(),
                write_error_tx: self.write_error_tx.clone(),
                lengths: lengths.clone(),
                block_maps: self.block_maps.clone(),
                steal_candidates: self.steal_candidates.clone(),
                piece_write_guards: self.piece_write_guards.clone(),
            });
        }

        let info_hash = self.info_hash;
        let peer_id = self.our_peer_id;
        let num_pieces = self.num_pieces;
        let event_tx = self.event_tx.clone();
        let enable_dht = self.config.enable_dht;
        let enable_fast = self.config.enable_fast;
        let encryption_mode = mode_override.unwrap_or(self.config.encryption_mode);
        let anonymous_mode = self.config.anonymous_mode;
        let enable_holepunch = self.config.enable_holepunch;
        let max_message_size = self.config.max_message_size;
        let info_bytes = self.meta.as_ref().and_then(|m| m.info_bytes.clone());
        let plugins = Arc::clone(&self.plugins);
        let have_broadcast_tx = self.have_broadcast_tx.clone();

        tokio::spawn(async move {
            let _permit = permit; // M107: hold for peer lifetime
            let _ = run_peer(
                addr,
                stream,
                info_hash,
                peer_id,
                bitfield,
                num_pieces,
                event_tx,
                cmd_rx,
                enable_dht,
                enable_fast,
                encryption_mode,
                false, // inbound
                anonymous_mode,
                info_bytes,
                plugins,
                enable_holepunch,
                max_message_size,
                have_broadcast_tx.subscribe(),
            )
            .await;
        });
    }

    // ── BEP 55 Holepunch (M40) ──

    /// Handle a Rendezvous request from an initiator peer.
    ///
    /// We act as relay: validate the request, then forward Connect messages to
    /// both the initiator and the target so they can perform simultaneous open.
    async fn handle_holepunch_rendezvous(
        &mut self,
        initiator_addr: SocketAddr,
        target: SocketAddr,
    ) {
        use torrent_wire::HolepunchMessage;

        debug!(%initiator_addr, %target, "holepunch: processing rendezvous request");

        // Cannot relay to ourselves
        if target == initiator_addr {
            debug!(%initiator_addr, "holepunch: rendezvous target == initiator (NoSelf)");
            if let Some(peer) = self.peers.get(&initiator_addr) {
                let _ = peer
                    .cmd_tx
                    .try_send(PeerCommand::SendHolepunch(HolepunchMessage::error(
                        target,
                        torrent_wire::HolepunchError::NoSelf,
                    )));
            }
            return;
        }

        // Target must be connected to us
        let target_peer = match self.peers.get(&target) {
            Some(p) => p,
            None => {
                debug!(%initiator_addr, %target, "holepunch: target not connected (NotConnected)");
                if let Some(peer) = self.peers.get(&initiator_addr) {
                    let _ =
                        peer.cmd_tx
                            .try_send(PeerCommand::SendHolepunch(HolepunchMessage::error(
                                target,
                                torrent_wire::HolepunchError::NotConnected,
                            )));
                }
                return;
            }
        };

        // Target must support holepunch
        if !target_peer.supports_holepunch {
            debug!(%initiator_addr, %target, "holepunch: target doesn't support holepunch (NoSupport)");
            if let Some(peer) = self.peers.get(&initiator_addr) {
                let _ = peer
                    .cmd_tx
                    .try_send(PeerCommand::SendHolepunch(HolepunchMessage::error(
                        target,
                        torrent_wire::HolepunchError::NoSupport,
                    )));
            }
            return;
        }

        // Forward Connect to target: "connect to the initiator"
        let _ = target_peer
            .cmd_tx
            .try_send(PeerCommand::SendHolepunch(HolepunchMessage::connect(
                initiator_addr,
            )));

        // Forward Connect to initiator: "connect to the target"
        if let Some(initiator) = self.peers.get(&initiator_addr) {
            let _ =
                initiator
                    .cmd_tx
                    .try_send(PeerCommand::SendHolepunch(HolepunchMessage::connect(
                        target,
                    )));
        }

        debug!(%initiator_addr, %target, "holepunch: relayed connect to both peers");
    }

    /// Handle a Connect message from the relay — initiate simultaneous open
    /// by connecting to the target via uTP (preferred) then TCP fallback.
    async fn handle_holepunch_connect(&mut self, target: SocketAddr) {
        debug!(%target, "holepunch: received connect, initiating simultaneous open");

        // Don't connect if we're already connected to the target
        if self.peers.contains_key(&target) {
            debug!(%target, "holepunch: already connected to target, ignoring");
            return;
        }

        // Check peer limit
        if self.peers.len() >= self.effective_max_connections() {
            debug!(%target, "holepunch: at max peers, ignoring connect");
            return;
        }

        // Skip banned/filtered peers
        if crate::timed_lock::TimedGuard::new(
            self.ban_manager.read(),
            &self.lock_timing,
            "ban_manager",
        )
        .is_banned(&target.ip())
        {
            debug!(%target, "holepunch: target is banned");
            return;
        }
        if crate::timed_lock::TimedGuard::new(self.ip_filter.read(), &self.lock_timing, "ip_filter")
            .is_blocked(target.ip())
        {
            debug!(%target, "holepunch: target is IP-filtered");
            post_alert(
                &self.alert_tx,
                &self.alert_mask,
                AlertKind::PeerBlocked { addr: target },
            );
            return;
        }

        // Set up peer state and channels (same pattern as try_connect_peers)
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let bitfield = if self.super_seed.is_some() {
            Bitfield::new(self.num_pieces)
        } else {
            self.chunk_tracker
                .as_ref()
                .map(|ct| ct.bitfield().clone())
                .unwrap_or_else(|| Bitfield::new(self.num_pieces))
        };

        self.peers.insert(
            target,
            PeerState::new(target, self.num_pieces, cmd_tx, PeerSource::Pex),
        );
        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::PeerConnected {
                info_hash: self.info_hash,
                addr: target,
            },
        );
        // M93: Send lock-free dispatch state to peer for integrated dispatch
        if let (Some(atomic_states), Some(snapshot), Some(notify)) = (
            &self.atomic_states,
            &self.availability_snapshot,
            &self.reservation_notify,
        ) && let Some(peer) = self.peers.get(&target)
            && let Some(ref lengths) = self.lengths
        {
            let _ = peer.cmd_tx.try_send(PeerCommand::StartRequesting {
                atomic_states: Arc::clone(atomic_states),
                availability_snapshot: Arc::clone(snapshot),
                piece_notify: Arc::clone(notify),
                disk_handle: self.disk.clone(),
                write_error_tx: self.write_error_tx.clone(),
                lengths: lengths.clone(),
                block_maps: self.block_maps.clone(),
                steal_candidates: self.steal_candidates.clone(),
                piece_write_guards: self.piece_write_guards.clone(),
            });
        }

        // Capture variables for the spawned task (mirrors try_connect_peers)
        let info_hash = self.info_hash;
        let peer_id = self.our_peer_id;
        let num_pieces = self.num_pieces;
        let event_tx = self.event_tx.clone();
        let alert_tx = self.alert_tx.clone();
        let alert_mask = Arc::clone(&self.alert_mask);
        let enable_dht = self.config.enable_dht;
        let enable_fast = self.config.enable_fast;
        let encryption_mode = self.config.encryption_mode;
        let enable_utp = self.config.enable_utp;
        let anonymous_mode = self.config.anonymous_mode;
        let enable_holepunch = self.config.enable_holepunch;
        let max_message_size = self.config.max_message_size;
        let info_bytes = self.meta.as_ref().and_then(|m| m.info_bytes.clone());
        let utp_socket = if target.is_ipv6() {
            self.utp_socket_v6.clone()
        } else {
            self.utp_socket.clone()
        };
        let plugins = Arc::clone(&self.plugins);
        let factory = Arc::clone(&self.factory);
        let have_broadcast_tx = self.have_broadcast_tx.clone();

        tokio::spawn(async move {
            // Try uTP first (5s timeout) — preferred for holepunch as NAT traversal
            // works better with UDP-based protocols
            if enable_utp && let Some(socket) = utp_socket {
                match tokio::time::timeout(Duration::from_secs(5), socket.connect(target)).await {
                    Ok(Ok(stream)) => {
                        debug!(%target, "holepunch: uTP connection established");
                        post_alert(
                            &alert_tx,
                            &alert_mask,
                            AlertKind::HolepunchSucceeded {
                                info_hash,
                                addr: target,
                            },
                        );
                        let _ = run_peer(
                            target,
                            stream,
                            info_hash,
                            peer_id,
                            bitfield,
                            num_pieces,
                            event_tx,
                            cmd_rx,
                            enable_dht,
                            enable_fast,
                            encryption_mode,
                            true, // outbound
                            anonymous_mode,
                            info_bytes,
                            plugins,
                            enable_holepunch,
                            max_message_size,
                            have_broadcast_tx.subscribe(),
                        )
                        .await;
                        return; // uTP succeeded — don't fall through to TCP
                    }
                    Ok(Err(e)) => {
                        debug!(%target, error = %e, "holepunch: uTP connect failed, trying TCP");
                    }
                    Err(_) => {
                        debug!(%target, "holepunch: uTP connect timed out, trying TCP");
                    }
                }
            }

            // TCP fallback — only reached if uTP didn't succeed
            match tokio::time::timeout(Duration::from_secs(10), factory.connect_tcp(target)).await {
                Ok(Ok(stream)) => {
                    debug!(%target, "holepunch: TCP connection established");
                    post_alert(
                        &alert_tx,
                        &alert_mask,
                        AlertKind::HolepunchSucceeded {
                            info_hash,
                            addr: target,
                        },
                    );
                    let _ = run_peer(
                        target,
                        stream,
                        info_hash,
                        peer_id,
                        bitfield,
                        num_pieces,
                        event_tx,
                        cmd_rx,
                        enable_dht,
                        enable_fast,
                        encryption_mode,
                        true, // outbound
                        anonymous_mode,
                        info_bytes,
                        plugins,
                        enable_holepunch,
                        max_message_size,
                        have_broadcast_tx.subscribe(),
                    )
                    .await;
                }
                Ok(Err(e)) => {
                    let message = format!("TCP connect failed: {e}");
                    debug!(%target, %message, "holepunch: connection failed");
                    post_alert(
                        &alert_tx,
                        &alert_mask,
                        AlertKind::HolepunchFailed {
                            info_hash,
                            addr: target,
                            error_code: None,
                            message,
                        },
                    );
                    let _ = event_tx
                        .send(PeerEvent::Disconnected {
                            peer_addr: target,
                            reason: Some(format!("holepunch TCP connect failed: {e}")),
                        })
                        .await;
                }
                Err(_) => {
                    let message = "TCP connect timed out".to_string();
                    debug!(%target, "holepunch: TCP connect timed out");
                    post_alert(
                        &alert_tx,
                        &alert_mask,
                        AlertKind::HolepunchFailed {
                            info_hash,
                            addr: target,
                            error_code: None,
                            message,
                        },
                    );
                    let _ = event_tx
                        .send(PeerEvent::Disconnected {
                            peer_addr: target,
                            reason: Some("holepunch TCP connect timed out".to_string()),
                        })
                        .await;
                }
            }
        });
    }

    /// Try to initiate a holepunch connection to `target` via a connected relay peer.
    ///
    /// Finds a relay peer that supports holepunch and sends a Rendezvous message
    /// asking the relay to broker a connection to `target`.
    pub(crate) async fn try_holepunch(&mut self, target: SocketAddr) {
        use torrent_wire::HolepunchMessage;

        if !self.config.enable_holepunch {
            return;
        }

        // Don't holepunch if already connected
        if self.peers.contains_key(&target) {
            return;
        }

        // Find a relay: a connected peer that supports holepunch and has completed
        // the extension handshake (and isn't the target itself)
        let relay_addr = self
            .peers
            .iter()
            .find(|(addr, peer)| {
                **addr != target && peer.supports_holepunch && peer.ext_handshake.is_some()
            })
            .map(|(addr, _)| *addr);

        let Some(relay_addr) = relay_addr else {
            debug!(%target, "holepunch: no suitable relay found");
            return;
        };

        debug!(%target, %relay_addr, "holepunch: sending rendezvous via relay");
        if let Some(relay) = self.peers.get(&relay_addr) {
            let _ =
                relay
                    .cmd_tx
                    .try_send(PeerCommand::SendHolepunch(HolepunchMessage::rendezvous(
                        target,
                    )));
        }
    }
}
