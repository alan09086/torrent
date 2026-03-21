//! TorrentActor state machine, stats, resume data, and file management.
//!
//! This module contains an `impl TorrentActor` block with methods for:
//! - State transitions (`transition_state`)
//! - Storage operations (`handle_move_storage`, `handle_rename_file`)
//! - File completion tracking (`check_file_completion`, `fire_file_completed_alerts`)
//! - Progress computation (`compute_progress`)
//! - Error handling (`handle_clear_error`)
//! - File and flag status (`build_file_status`, `build_flags`, `apply_set_flags`, `apply_unset_flags`)
//! - Statistics (`make_stats`)
//! - Pause/resume (`handle_pause`, `handle_resume`)
//! - Piece verification (`verify_existing_pieces`, `handle_force_recheck`)
//! - Resume data (`build_resume_data`)
//! - File priorities (`handle_set_file_priority`)
//! - Seed ratio checking (`check_seed_ratio`)

use std::sync::Arc;

use tracing::info;

use crate::alert::{AlertKind, post_alert};
use crate::disk::DiskJobFlags;
use crate::peer_state::PeerSource;
use crate::piece_reservation::{AtomicPieceStates, BlockMaps, StealCandidates};
use crate::torrent::{HashResult, TorrentActor, now_unix, relocate_files};
use crate::types::{PeerCommand, TorrentState, TorrentStats};

use torrent_core::FilePriority;
use torrent_storage::TorrentStorage;

impl TorrentActor {
    /// Transition to a new state, firing a StateChanged alert if different.
    pub(crate) fn transition_state(&mut self, new_state: TorrentState) {
        let prev = self.state;
        if prev == new_state {
            return;
        }

        let now = std::time::Instant::now();

        // Accumulate durations for the state we're LEAVING
        if let Some(since) = self.state_duration_since {
            let elapsed = now.duration_since(since).as_secs() as i64;
            match prev {
                TorrentState::Seeding => {
                    self.seeding_duration += elapsed;
                    self.finished_duration += elapsed;
                }
                TorrentState::Complete => {
                    self.finished_duration += elapsed;
                }
                _ => {}
            }
        }

        // Handle active_duration on pause transitions
        if new_state == TorrentState::Paused {
            // Entering paused: accumulate active time and clear timer
            if let Some(since) = self.active_since {
                self.active_duration += now.duration_since(since).as_secs() as i64;
            }
            self.active_since = None;
        } else if prev == TorrentState::Paused {
            // Leaving paused: restart active timer
            self.active_since = Some(now);
        }

        // Track first completion
        if matches!(new_state, TorrentState::Complete | TorrentState::Seeding)
            && !matches!(prev, TorrentState::Complete | TorrentState::Seeding)
            && self.completed_time == 0
        {
            self.completed_time = now_unix();
        }

        // Update state duration tracking
        self.state_duration_since = Some(now);
        self.need_save_resume = true;

        self.state = new_state;
        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::StateChanged {
                info_hash: self.info_hash,
                prev_state: prev,
                new_state,
            },
        );
    }

    /// Handle MoveStorage: relocate data files, re-register storage.
    pub(crate) async fn handle_move_storage(
        &mut self,
        new_path: std::path::PathBuf,
    ) -> crate::Result<()> {
        self.moving_storage = true;

        let meta = match self.meta.as_ref() {
            Some(m) => m,
            None => {
                self.moving_storage = false;
                return Err(crate::Error::Config(
                    "cannot move storage: metadata not available".into(),
                ));
            }
        };

        let file_paths: Vec<std::path::PathBuf> = meta
            .info
            .files()
            .iter()
            .map(|f| f.path.iter().collect::<std::path::PathBuf>())
            .collect();
        let file_lengths: Vec<u64> = meta.info.files().iter().map(|f| f.length).collect();
        // files() already includes the torrent name as the first path component,
        // so src/dst base is just the download directory — no extra join with name.
        let src_base = self.config.download_dir.clone();
        let dst_base = new_path.clone();

        // Relocate files on a blocking thread to avoid starving the async runtime
        let src = src_base.clone();
        let dst = dst_base.clone();
        let paths = file_paths.clone();
        let result = tokio::task::spawn_blocking(move || relocate_files(&src, &dst, &paths))
            .await
            .map_err(|e| crate::Error::Io(std::io::Error::other(e)))
            .and_then(|r| r.map_err(crate::Error::Io));
        if let Err(e) = result {
            self.moving_storage = false;
            return Err(e);
        }

        // Unregister old storage
        self.disk_manager.unregister_torrent(self.info_hash).await;

        // Create new storage at destination
        let lengths = match self.lengths.clone() {
            Some(l) => l,
            None => {
                self.moving_storage = false;
                return Err(crate::Error::Config("lengths not available".into()));
            }
        };
        let prealloc_mode = self.config.preallocate_mode.unwrap_or_else(|| {
            torrent_storage::PreallocateMode::from(
                self.config.storage_mode == torrent_core::StorageMode::Full,
            )
        });
        let storage: Arc<dyn TorrentStorage> = match torrent_storage::FilesystemStorage::new(
            &new_path,
            file_paths,
            file_lengths,
            lengths,
            Some(&self.file_priorities),
            prealloc_mode,
        ) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                self.moving_storage = false;
                return Err(e.into());
            }
        };

        // Re-register with disk manager
        self.disk = Some(
            self.disk_manager
                .register_torrent(self.info_hash, storage)
                .await,
        );

        // Update download dir
        self.config.download_dir = new_path.clone();

        // Fire alert
        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::StorageMoved {
                info_hash: self.info_hash,
                new_path,
            },
        );

        self.moving_storage = false;
        Ok(())
    }

    /// Handle RenameFile: rename a single file within the torrent on disk.
    pub(crate) async fn handle_rename_file(
        &mut self,
        file_index: usize,
        new_name: String,
    ) -> crate::Result<()> {
        let meta = match self.meta.as_ref() {
            Some(m) => m,
            None => {
                return Err(crate::Error::Config(
                    "cannot rename file: metadata not available".into(),
                ));
            }
        };

        let files = meta.info.files();
        if file_index >= files.len() {
            return Err(crate::Error::Config(format!(
                "file index {file_index} out of range (torrent has {} files)",
                files.len()
            )));
        }

        // Compute the old relative path (files() includes torrent name as first component)
        let old_rel: std::path::PathBuf = files[file_index].path.iter().collect();
        let old_path = self.config.download_dir.join(&old_rel);

        // Build new relative path: same parent directory, new filename
        let new_rel = if let Some(parent) = old_rel.parent() {
            parent.join(&new_name)
        } else {
            std::path::PathBuf::from(&new_name)
        };
        let new_path = self.config.download_dir.join(&new_rel);

        // Perform the rename on a blocking thread
        let src = old_path.clone();
        let dst = new_path.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&src, &dst)
        })
        .await
        .map_err(|e| crate::Error::Io(std::io::Error::other(e)))
        .and_then(|r| r.map_err(crate::Error::Io))?;

        // Fire FileRenamed alert
        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::FileRenamed {
                info_hash: self.info_hash,
                index: file_index,
                new_path: new_path.clone(),
            },
        );

        Ok(())
    }

    /// Check if the just-completed piece finishes any file, and fire FileCompleted alerts.
    ///
    /// M116: Uses pre-computed `cached_files` mapping instead of allocating
    /// `meta.info.files()` on every verified piece.
    pub(crate) fn check_file_completion(&self, piece_index: u32) {
        let cached = match self.cached_files.as_ref() {
            Some(c) => c,
            None => return,
        };
        let bitfield = match self.chunk_tracker.as_ref() {
            Some(ct) => ct.bitfield(),
            None => return,
        };

        for entry in &cached.entries {
            // Skip files that don't contain this piece
            if piece_index < entry.first_piece || piece_index > entry.last_piece {
                continue;
            }

            // Check if ALL pieces for this file are complete
            let all_complete = (entry.first_piece..=entry.last_piece).all(|p| bitfield.get(p));

            if all_complete {
                post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::FileCompleted {
                        info_hash: self.info_hash,
                        file_index: entry.index,
                    },
                );
            }
        }
    }

    /// Compute download progress metrics.
    ///
    /// Returns `(total, total_done, total_wanted, total_wanted_done, progress, progress_ppm)`.
    pub(crate) fn compute_progress(&self) -> (u64, u64, u64, u64, f32, u32) {
        let lengths = match &self.lengths {
            Some(l) => l,
            None => return (0, 0, 0, 0, 0.0, 0),
        };

        let total = lengths.total_length();

        let bitfield = self.chunk_tracker.as_ref().map(|ct| ct.bitfield());

        let mut total_done: u64 = 0;
        let mut total_wanted: u64 = 0;
        let mut total_wanted_done: u64 = 0;

        for idx in 0..self.num_pieces {
            let piece_bytes = lengths.piece_size(idx) as u64;
            let have = bitfield.as_ref().map(|bf| bf.get(idx)).unwrap_or(false);

            if have {
                total_done += piece_bytes;
            }

            if self.wanted_pieces.get(idx) {
                total_wanted += piece_bytes;
                if have {
                    total_wanted_done += piece_bytes;
                }
            }
        }

        let progress = if total_wanted == 0 {
            1.0
        } else {
            total_wanted_done as f32 / total_wanted as f32
        };
        let progress_ppm = (progress * 1_000_000.0) as u32;

        (
            total,
            total_done,
            total_wanted,
            total_wanted_done,
            progress,
            progress_ppm,
        )
    }

    /// Clear the error state. If the torrent was paused and had an error, resume it.
    pub(crate) async fn handle_clear_error(&mut self) {
        let had_error = !self.error.is_empty();
        self.error = String::new();
        self.error_file = -1;
        // If we were paused and had an error, resume
        if had_error && self.state == TorrentState::Paused {
            self.handle_resume().await;
        }
    }

    /// Build per-file status based on the current torrent state.
    pub(crate) fn build_file_status(&self) -> Vec<crate::types::FileStatus> {
        let num_files = self.file_priorities.len();
        let (open, mode) = match self.state {
            TorrentState::Seeding => (true, crate::types::FileMode::ReadOnly),
            TorrentState::Downloading
            | TorrentState::Checking
            | TorrentState::FetchingMetadata
            | TorrentState::Complete
            | TorrentState::Sharing => (true, crate::types::FileMode::ReadWrite),
            TorrentState::Paused | TorrentState::Stopped => (false, crate::types::FileMode::Closed),
        };
        vec![crate::types::FileStatus { open, mode }; num_files]
    }

    /// Build the current TorrentFlags from actor state.
    pub(crate) fn build_flags(&self) -> crate::types::TorrentFlags {
        let mut flags = crate::types::TorrentFlags::empty();
        if self.state == TorrentState::Paused {
            flags |= crate::types::TorrentFlags::PAUSED;
        }
        // auto_managed is session-level; torrent actor doesn't track it.
        // We leave AUTO_MANAGED unset at the torrent level.
        if self.config.sequential_download {
            flags |= crate::types::TorrentFlags::SEQUENTIAL_DOWNLOAD;
        }
        if self.config.super_seeding {
            flags |= crate::types::TorrentFlags::SUPER_SEEDING;
        }
        if self.state == TorrentState::Seeding || matches!(self.state, TorrentState::Complete) {
            flags |= crate::types::TorrentFlags::UPLOAD_ONLY;
        }
        flags
    }

    /// Apply set_flags: enable the specified flags.
    pub(crate) async fn apply_set_flags(&mut self, flags: crate::types::TorrentFlags) {
        if flags.contains(crate::types::TorrentFlags::PAUSED) && self.state != TorrentState::Paused
        {
            self.handle_pause().await;
        }
        if flags.contains(crate::types::TorrentFlags::SEQUENTIAL_DOWNLOAD) {
            self.config.sequential_download = true;
        }
        if flags.contains(crate::types::TorrentFlags::SUPER_SEEDING) {
            self.config.super_seeding = true;
            if self.super_seed.is_none() {
                self.super_seed = Some(crate::super_seed::SuperSeedState::new());
            }
        }
        // AUTO_MANAGED and UPLOAD_ONLY are session-level; no-op at torrent level.
    }

    /// Apply unset_flags: disable the specified flags.
    pub(crate) async fn apply_unset_flags(&mut self, flags: crate::types::TorrentFlags) {
        if flags.contains(crate::types::TorrentFlags::PAUSED) && self.state == TorrentState::Paused
        {
            self.handle_resume().await;
        }
        if flags.contains(crate::types::TorrentFlags::SEQUENTIAL_DOWNLOAD) {
            self.config.sequential_download = false;
        }
        if flags.contains(crate::types::TorrentFlags::SUPER_SEEDING) {
            self.config.super_seeding = false;
            self.super_seed = None;
        }
        // AUTO_MANAGED and UPLOAD_ONLY are session-level; no-op at torrent level.
    }

    pub(crate) fn make_stats(&self) -> TorrentStats {
        // ── Single pass over peers ──
        let mut num_seeds = 0usize;
        let mut num_uploads = 0usize;
        let mut download_rate_sum: u64 = 0;
        let mut upload_rate_sum: u64 = 0;
        let mut peers_by_source = std::collections::HashMap::new();

        for peer in self.peers.values() {
            *peers_by_source.entry(peer.source).or_insert(0) += 1;
            download_rate_sum += peer.download_rate;
            upload_rate_sum += peer.upload_rate;
            if self.num_pieces > 0 && peer.bitfield.count_ones() == self.num_pieces {
                num_seeds += 1;
            }
            if !peer.am_choking {
                num_uploads += 1;
            }
        }

        // ── Tracker info (scrape data + current tracker) ──
        let tracker_list = self.tracker_manager.tracker_list();
        let mut num_complete: i32 = -1;
        let mut num_incomplete: i32 = -1;
        let mut current_tracker = String::new();

        for ti in &tracker_list {
            if num_complete == -1
                && let Some(s) = ti.seeders
            {
                num_complete = s as i32;
            }
            if num_incomplete == -1
                && let Some(l) = ti.leechers
            {
                num_incomplete = l as i32;
            }
            if current_tracker.is_empty()
                && matches!(ti.status, crate::tracker_manager::TrackerStatus::Working)
            {
                current_tracker = ti.url.clone();
            }
        }

        // ── Progress ──
        let pieces_have = self
            .chunk_tracker
            .as_ref()
            .map(|ct| ct.bitfield().count_ones())
            .unwrap_or(0);
        let (total, total_done, total_wanted, total_wanted_done, progress, progress_ppm) =
            self.compute_progress();

        // ── Distributed copies ──
        let (distributed_full_copies, distributed_fraction, distributed_copies) =
            self.distributed_copies();

        // ── Active duration (include current active stint) ──
        let active_duration = self.active_duration
            + self
                .active_since
                .map(|since| since.elapsed().as_secs() as i64)
                .unwrap_or(0);

        // ── Finished duration (include current stint if Complete or Seeding) ──
        let finished_duration = self.finished_duration
            + self
                .state_duration_since
                .filter(|_| matches!(self.state, TorrentState::Complete | TorrentState::Seeding))
                .map(|since| since.elapsed().as_secs() as i64)
                .unwrap_or(0);

        // ── Seeding duration (include current stint if Seeding) ──
        let seeding_duration = self.seeding_duration
            + self
                .state_duration_since
                .filter(|_| self.state == TorrentState::Seeding)
                .map(|since| since.elapsed().as_secs() as i64)
                .unwrap_or(0);

        // ── Name ──
        let name = self
            .meta
            .as_ref()
            .map(|m| m.info.name.clone())
            .unwrap_or_default();

        // ── Block size ──
        let block_size = self
            .lengths
            .as_ref()
            .map(|l| l.chunk_size())
            .unwrap_or(16384);

        TorrentStats {
            // ── Original 9 fields (unchanged) ──
            state: self.state,
            downloaded: self.downloaded,
            uploaded: self.uploaded,
            pieces_have,
            pieces_total: self.num_pieces,
            peers_connected: self.peers.len(),
            peers_available: 0, // M107: discovery pool is in adder task channel
            checking_progress: self.checking_progress,
            peers_by_source,

            // ── Identity ──
            info_hashes: self.info_hashes.clone(),
            name,

            // ── State flags ──
            has_metadata: self.meta.is_some(),
            is_seeding: self.state == TorrentState::Seeding,
            is_finished: matches!(self.state, TorrentState::Complete | TorrentState::Seeding),
            is_paused: self.state == TorrentState::Paused,
            auto_managed: false, // session fills this
            sequential_download: self.config.sequential_download,
            super_seeding: self.config.super_seeding,
            has_incoming: self.has_incoming,
            need_save_resume: self.need_save_resume,
            moving_storage: self.moving_storage,

            // ── Progress ──
            progress,
            progress_ppm,
            total_done,
            total,
            total_wanted_done,
            total_wanted,
            block_size,

            // ── Transfer (session counters) ──
            total_download: self.total_download,
            total_upload: self.total_upload,
            total_payload_download: self.downloaded,
            total_payload_upload: self.uploaded,
            total_failed_bytes: self.total_failed_bytes,
            total_redundant_bytes: self.total_redundant_bytes,

            // ── Transfer (all-time = session for now, no persistence yet) ──
            all_time_download: self.total_download,
            all_time_upload: self.total_upload,

            // ── Rates ──
            download_rate: download_rate_sum,
            upload_rate: upload_rate_sum,
            download_payload_rate: download_rate_sum,
            upload_payload_rate: upload_rate_sum,

            // ── Connection details ──
            num_peers: self.peers.len(),
            num_seeds,
            num_complete,
            num_incomplete,
            list_seeds: num_seeds,
            list_peers: self.peers.len(), // M107: discovery pool is in adder task
            connect_candidates: 0,        // M107: discovery pool is in adder task
            num_connections: self.peers.len(),
            num_uploads,

            // ── Limits ──
            connections_limit: self.effective_max_connections(),
            uploads_limit: self.choker.unchoke_slots(),

            // ── Distributed copies ──
            distributed_full_copies,
            distributed_fraction,
            distributed_copies,

            // ── Tracker ──
            current_tracker,
            announcing_to_trackers: !tracker_list.is_empty(),
            announcing_to_lsd: false, // LSD not yet implemented
            announcing_to_dht: self.dht_peers_rx.is_some(),

            // ── Timestamps ──
            added_time: self.added_time,
            completed_time: self.completed_time,
            last_seen_complete: self.last_seen_complete,
            last_upload: self.last_upload,
            last_download: self.last_download,

            // ── Durations ──
            active_duration,
            finished_duration,
            seeding_duration,

            // ── Storage ──
            save_path: self.config.download_dir.to_string_lossy().into_owned(),

            // ── Queue (session fills this) ──
            queue_position: -1,

            // ── Error ──
            error: self.error.clone(),
            error_file: self.error_file,
        }
    }

    pub(crate) async fn handle_pause(&mut self) {
        if self.state == TorrentState::Paused || self.state == TorrentState::Stopped {
            return;
        }
        let prev_state = self.state;
        self.transition_state(TorrentState::Paused);
        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::TorrentPaused {
                info_hash: self.info_hash,
            },
        );
        // Disconnect all peers (non-blocking — peer may already be dead)
        for peer in self.peers.values() {
            let _ = peer.cmd_tx.try_send(PeerCommand::Shutdown);
        }
        self.peers.clear();
        // Announce Stopped to trackers (with timeout to prevent hang)
        if prev_state == TorrentState::Downloading
            || prev_state == TorrentState::Seeding
            || prev_state == TorrentState::Complete
        {
            let left = self.calculate_left();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                self.tracker_manager
                    .announce_stopped(self.uploaded, self.downloaded, left),
            )
            .await;
        }
    }

    pub(crate) async fn handle_resume(&mut self) {
        if self.state != TorrentState::Paused {
            return;
        }
        // Determine appropriate state
        if self.config.share_mode {
            self.transition_state(TorrentState::Sharing);
        } else if let Some(ref ct) = self.chunk_tracker
            && ct.bitfield().count_ones() == self.num_pieces
        {
            self.transition_state(TorrentState::Seeding);
            self.choker.set_seed_mode(true);
        } else {
            self.transition_state(TorrentState::Downloading);
            self.scorer.start_download();
            self.choker.set_seed_mode(false);
        }
        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::TorrentResumed {
                info_hash: self.info_hash,
            },
        );
        // Re-announce Started
        let left = self.calculate_left();
        let result = self
            .tracker_manager
            .announce(
                torrent_tracker::AnnounceEvent::Started,
                self.uploaded,
                self.downloaded,
                left,
            )
            .await;
        self.fire_tracker_alerts(&result.outcomes);
        if !result.peers.is_empty() {
            self.handle_add_peers(result.peers, PeerSource::Tracker);
        }
    }

    pub(crate) async fn verify_existing_pieces(&mut self) {
        let disk = match &self.disk {
            Some(d) => d.clone(),
            None => return,
        };
        let meta = match self.meta.clone() {
            Some(m) => m,
            None => return,
        };

        self.transition_state(TorrentState::Checking);
        self.checking_progress = 0.0;

        let mut verified_count = 0u32;
        let total = self.num_pieces;

        if self.version == torrent_core::TorrentVersion::V2Only {
            // V2Only: use SHA-256 Merkle block verification (sequential, needs &mut self)
            for piece in 0..total {
                let result = self.run_v2_block_verification(piece).await;
                if matches!(result, HashResult::Passed) {
                    if let Some(ref mut ct) = self.chunk_tracker {
                        ct.mark_verified(piece);
                    }
                    verified_count += 1;
                }

                self.checking_progress = (piece + 1) as f32 / total as f32;
                post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::CheckingProgress {
                        info_hash: self.info_hash,
                        progress: self.checking_progress,
                    },
                );
            }
        } else {
            // V1Only / Hybrid: use concurrent SHA-1 piece verification
            let max_concurrent = self.config.hashing_threads.max(1);
            let mut checked_count = 0u32;
            let mut in_flight = tokio::task::JoinSet::new();
            let mut next_piece = 0u32;

            // Seed the pipeline
            while next_piece < total && in_flight.len() < max_concurrent {
                if let Some(expected) = meta.info.piece_hash(next_piece as usize) {
                    let d = disk.clone();
                    let piece = next_piece;
                    in_flight.spawn(async move {
                        let valid = d
                            .verify_piece(piece, expected, DiskJobFlags::empty())
                            .await
                            .unwrap_or(false);
                        (piece, valid)
                    });
                }
                next_piece += 1;
            }

            // Process completions, refill pipeline
            while let Some(result) = in_flight.join_next().await {
                if let Ok((piece, valid)) = result {
                    checked_count += 1;
                    if valid {
                        if let Some(ref mut ct) = self.chunk_tracker {
                            ct.mark_verified(piece);
                        }
                        verified_count += 1;
                    }

                    // Update progress
                    self.checking_progress = checked_count as f32 / total as f32;
                    post_alert(
                        &self.alert_tx,
                        &self.alert_mask,
                        AlertKind::CheckingProgress {
                            info_hash: self.info_hash,
                            progress: self.checking_progress,
                        },
                    );
                }

                // Refill pipeline
                while next_piece < total && in_flight.len() < max_concurrent {
                    if let Some(expected) = meta.info.piece_hash(next_piece as usize) {
                        let d = disk.clone();
                        let piece = next_piece;
                        in_flight.spawn(async move {
                            let valid = d
                                .verify_piece(piece, expected, DiskJobFlags::empty())
                                .await
                                .unwrap_or(false);
                            (piece, valid)
                        });
                    }
                    next_piece += 1;
                }
            }
        }

        // Fire TorrentChecked alert
        self.checking_progress = 0.0;
        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::TorrentChecked {
                info_hash: self.info_hash,
                pieces_have: verified_count,
                pieces_total: total,
            },
        );

        if verified_count > 0 {
            info!(verified_count, total, "resumed with existing pieces");
        }

        if self.config.share_mode {
            self.transition_state(TorrentState::Sharing);
        } else if verified_count == self.num_pieces {
            self.transition_state(TorrentState::Seeding);
            self.choker.set_seed_mode(true);
            info!("all pieces verified, starting as seeder");
        } else {
            self.transition_state(TorrentState::Downloading);
            self.scorer.start_download();
            self.choker.set_seed_mode(false);
        }

        // Fire FileCompleted alerts for any files that are fully verified
        self.fire_file_completed_alerts();
    }

    /// Fire FileCompleted alerts for all files whose pieces are fully verified.
    ///
    /// Used after initial check or force-recheck to emit alerts for complete files.
    pub(crate) fn fire_file_completed_alerts(&self) {
        let meta = match self.meta.as_ref() {
            Some(m) => m,
            None => return,
        };
        let lengths = match self.lengths.as_ref() {
            Some(l) => l,
            None => return,
        };
        let bitfield = match self.chunk_tracker.as_ref() {
            Some(ct) => ct.bitfield(),
            None => return,
        };

        let files = meta.info.files();
        let piece_length = lengths.piece_length();
        let mut file_offset = 0u64;

        for (file_idx, file_entry) in files.iter().enumerate() {
            let file_end = file_offset + file_entry.length;
            if file_entry.length == 0 {
                file_offset = file_end;
                continue;
            }

            let first_piece = (file_offset / piece_length) as u32;
            let last_piece = ((file_end - 1) / piece_length) as u32;

            let mut all_complete = true;
            for p in first_piece..=last_piece {
                if !bitfield.get(p) {
                    all_complete = false;
                    break;
                }
            }

            if all_complete {
                post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::FileCompleted {
                        info_hash: self.info_hash,
                        file_index: file_idx,
                    },
                );
            }

            file_offset = file_end;
        }
    }

    /// Handle a force recheck request: clear all piece state, re-verify,
    /// transition to the appropriate post-check state, then send reply.
    pub(crate) async fn handle_force_recheck(
        &mut self,
        reply: tokio::sync::oneshot::Sender<crate::Result<()>>,
    ) {
        // Disconnect all peers — they hold stale bitfield state (non-blocking)
        for peer in self.peers.values() {
            let _ = peer.cmd_tx.try_send(PeerCommand::Shutdown);
        }
        self.peers.clear();

        // Clear all piece completion state
        if let Some(ref mut ct) = self.chunk_tracker {
            ct.clear();
        }

        // Run the full verification pipeline (transitions through Checking)
        self.verify_existing_pieces().await;

        // M93: Rebuild atomic states after recheck
        if let Some(ct) = &self.chunk_tracker {
            let atomic_states = Arc::new(AtomicPieceStates::new(
                self.num_pieces,
                ct.bitfield(),
                &self.wanted_pieces,
            ));
            self.atomic_states = Some(Arc::clone(&atomic_states));
            self.piece_owner = vec![None; self.num_pieces as usize];
            // M103: Rebuild block stealing state after recheck
            if self.config.use_block_stealing {
                if let Some(ref lengths) = self.lengths {
                    self.block_maps = Some(Arc::new(BlockMaps::new(self.num_pieces, lengths)));
                }
                self.steal_candidates = Some(Arc::new(StealCandidates::new()));
            }
            // M120: Rebuild per-piece write guards
            self.piece_write_guards = Some(Arc::new(
                crate::piece_reservation::PieceWriteGuards::new(self.num_pieces),
            ));
            self.rebuild_availability_snapshot();
        }

        let _ = reply.send(Ok(()));
    }

    pub(crate) fn build_resume_data(&self) -> crate::Result<torrent_core::FastResumeData> {
        let pieces_bytes = match &self.chunk_tracker {
            Some(ct) => ct.bitfield().as_bytes().to_vec(),
            None => Vec::new(),
        };

        let name = self
            .meta
            .as_ref()
            .map(|m| m.info.name.clone())
            .unwrap_or_default();

        let save_path = self.config.download_dir.to_string_lossy().into_owned();

        let mut rd =
            torrent_core::FastResumeData::new(self.info_hash.as_bytes().to_vec(), name, save_path);

        rd.pieces = pieces_bytes;

        rd.total_uploaded = self.uploaded as i64;
        rd.total_downloaded = self.downloaded as i64;

        rd.paused = if self.state == TorrentState::Paused {
            1
        } else {
            0
        };
        rd.seed_mode = if self.state == TorrentState::Seeding {
            1
        } else {
            0
        };
        rd.super_seeding = if self.super_seed.is_some() { 1 } else { 0 };

        // Collect tracker URLs from torrent metadata
        if let Some(ref meta) = self.meta {
            if let Some(ref announce_list) = meta.announce_list {
                rd.trackers = announce_list.clone();
            } else if let Some(ref announce) = meta.announce {
                rd.trackers = vec![vec![announce.clone()]];
            }
            rd.url_seeds = meta.url_list.clone();
            rd.http_seeds = meta.httpseeds.clone();
        }

        // Collect connected peer addresses as compact bytes
        let peer_addrs: Vec<std::net::SocketAddr> = self.peers.keys().copied().collect();
        rd.peers = torrent_tracker::compact::encode_compact_peers(&peer_addrs);
        rd.peers6 = torrent_tracker::compact::encode_compact_peers6(&peer_addrs);

        // Per-file priorities
        rd.file_priority = self
            .file_priorities
            .iter()
            .map(|&p| p as u8 as i64)
            .collect();

        Ok(rd)
    }

    pub(crate) fn handle_set_file_priority(
        &mut self,
        index: usize,
        priority: FilePriority,
    ) -> crate::Result<()> {
        if index >= self.file_priorities.len() {
            return Err(crate::Error::InvalidFileIndex {
                index,
                count: self.file_priorities.len(),
            });
        }

        self.file_priorities[index] = priority;

        // Rebuild wanted_pieces bitfield
        if let Some(ref meta) = self.meta {
            let file_lengths: Vec<u64> = meta.info.files().iter().map(|f| f.length).collect();
            if let Some(ref lengths) = self.lengths {
                self.wanted_pieces = crate::piece_selector::build_wanted_pieces(
                    &self.file_priorities,
                    &file_lengths,
                    lengths,
                );
            }
        }

        Ok(())
    }

    pub(crate) fn check_seed_ratio(&mut self) -> bool {
        if self.state != TorrentState::Seeding {
            return false;
        }
        if let Some(limit) = self.config.seed_ratio_limit
            && self.downloaded > 0
        {
            let ratio = self.uploaded as f64 / self.downloaded as f64;
            if ratio >= limit {
                info!(ratio, limit, "seed ratio reached, stopping");
                self.transition_state(TorrentState::Stopped);
                return true;
            }
        }
        false
    }
}
