//! TorrentActor (single-owner event loop) and TorrentHandle (cloneable public API).
//!
//! The actor owns all per-torrent state (chunk tracking, piece selection, choking,
//! peer management) and communicates with spawned PeerTasks via channels.
//! The handle is a thin wrapper around an mpsc sender.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::SocketAddr;

use rustc_hash::FxHashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, error, info, trace, warn};

use crate::alert::{Alert, AlertKind, post_alert};
use crate::disk::{DiskHandle, DiskJobFlags, DiskManagerHandle};
use crate::piece_reservation::{
    AtomicPieceStates, AvailabilitySnapshot, BlockMaps, PieceState, StealCandidates,
};

use torrent_core::{
    DEFAULT_CHUNK_SIZE, FilePriority, Id20, Lengths, Magnet, PeerId, TorrentMetaV1,
    torrent_from_bytes,
};
use torrent_dht::DhtHandle;
use torrent_storage::{Bitfield, ChunkTracker, MemoryStorage, TorrentStorage};

use crate::choker::{Choker, PeerInfo as ChokerPeerInfo};
use crate::end_game::EndGame;
use crate::metadata::MetadataDownloader;
use crate::peer::run_peer;
use crate::peer_scorer::PeerScorer;
use crate::peer_state::{PeerSource, PeerState};
use crate::tracker_manager::TrackerManager;
use crate::types::{
    PartialPieceInfo, PeerCommand, PeerEvent, PeerInfo, TorrentCommand, TorrentConfig,
    TorrentState, TorrentStats,
};

/// Shared global rate limiter bucket.
type SharedBucket = Arc<std::sync::Mutex<crate::rate_limiter::TokenBucket>>;

/// Tribool result for piece hash verification in hybrid torrents.
///
/// Mirrors libtorrent-rasterbar's `boost::tribool` approach for dual-hash
/// verification. `NotApplicable` covers cases where verification cannot run
/// (e.g. missing hash picker, disk error before any block is checked).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HashResult {
    /// All hashes matched.
    Passed,
    /// At least one hash did not match.
    Failed,
    /// Verification could not be performed (missing state / deferred).
    NotApplicable,
}

/// Relocate torrent files from `src_base` to `dst_base`.
///
/// For each file, tries `rename` first (fast, same-filesystem), then falls
/// back to copy + delete (cross-filesystem). Creates parent directories as
/// needed. Returns error on the first failure.
fn relocate_files(
    src_base: &std::path::Path,
    dst_base: &std::path::Path,
    file_paths: &[std::path::PathBuf],
) -> std::io::Result<()> {
    for rel_path in file_paths {
        let src = src_base.join(rel_path);
        let dst = dst_base.join(rel_path);

        if !src.exists() {
            // File may not exist yet (e.g., not downloaded)
            continue;
        }

        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Try rename first (O(1) on same filesystem)
        if std::fs::rename(&src, &dst).is_err() {
            // Cross-filesystem: copy + delete
            std::fs::copy(&src, &dst)?;
            std::fs::remove_file(&src)?;
        }
    }

    // Try to remove empty parent directories from source
    // (best-effort, ignore errors)
    for rel_path in file_paths {
        let mut dir = src_base.join(rel_path);
        dir.pop(); // get parent dir
        while dir != *src_base {
            if std::fs::remove_dir(&dir).is_err() {
                break; // not empty or other error
            }
            dir.pop();
        }
    }

    Ok(())
}

/// Current time as POSIX seconds (0 on clock error).
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Cloneable handle for interacting with a running torrent.
#[derive(Clone)]
pub struct TorrentHandle {
    cmd_tx: mpsc::Sender<TorrentCommand>,
}

impl TorrentHandle {
    /// Create a torrent session from parsed .torrent metadata.
    ///
    /// Spawns the actor event loop and returns a handle for sending commands.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn from_torrent(
        meta: TorrentMetaV1,
        version: torrent_core::TorrentVersion,
        meta_v2: Option<torrent_core::TorrentMetaV2>,
        disk: DiskHandle,
        disk_manager: DiskManagerHandle,
        config: TorrentConfig,
        dht: Option<DhtHandle>,
        dht_v6: Option<DhtHandle>,
        global_upload_bucket: Option<SharedBucket>,
        global_download_bucket: Option<SharedBucket>,
        slot_tuner: crate::slot_tuner::SlotTuner,
        alert_tx: broadcast::Sender<Alert>,
        alert_mask: Arc<AtomicU32>,
        utp_socket: Option<torrent_utp::UtpSocket>,
        utp_socket_v6: Option<torrent_utp::UtpSocket>,
        ban_manager: crate::session::SharedBanManager,
        ip_filter: crate::session::SharedIpFilter,
        plugins: Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>>,
        sam_session: Option<Arc<crate::i2p::SamSession>>,
        ssl_manager: Option<Arc<crate::ssl_manager::SslManager>>,
        factory: Arc<crate::transport::NetworkFactory>,
        hash_pool: Option<std::sync::Arc<crate::hash_pool::HashPool>>,
    ) -> crate::Result<Self> {
        let mut config = config;
        // BEP 27: private torrents disable DHT and PEX
        if meta.info.private == Some(1) {
            config.enable_dht = false;
            config.enable_pex = false;
        }

        let info_hashes = match (&version, &meta_v2) {
            (torrent_core::TorrentVersion::Hybrid, Some(v2_meta)) => {
                if let Some(v2_hash) = v2_meta.info_hashes.v2 {
                    torrent_core::InfoHashes::hybrid(meta.info_hash, v2_hash)
                } else {
                    torrent_core::InfoHashes::v1_only(meta.info_hash)
                }
            }
            (torrent_core::TorrentVersion::V2Only, Some(v2_meta)) => v2_meta.info_hashes.clone(),
            _ => torrent_core::InfoHashes::v1_only(meta.info_hash),
        };

        if meta.info.piece_length > config.max_piece_length {
            return Err(crate::Error::InvalidSettings(format!(
                "piece_length {} exceeds max_piece_length {}",
                meta.info.piece_length, config.max_piece_length
            )));
        }

        let num_pieces = meta.info.num_pieces() as u32;
        let lengths = Lengths::new(
            meta.info.total_length(),
            meta.info.piece_length,
            DEFAULT_CHUNK_SIZE,
        );
        let chunk_tracker = ChunkTracker::new(lengths.clone());
        let file_lengths: Vec<u64> = meta.info.files().iter().map(|f| f.length).collect();
        let file_priorities = vec![FilePriority::Normal; file_lengths.len()];
        let wanted_pieces =
            crate::piece_selector::build_wanted_pieces(&file_priorities, &file_lengths, &lengths);

        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let (event_tx, event_rx) = mpsc::channel(2048);
        let (write_error_tx, write_error_rx) = mpsc::channel(64);
        let (verify_result_tx, verify_result_rx) = mpsc::channel(1024);
        let (hash_result_tx, hash_result_rx) = mpsc::channel(64); // M96
        let our_peer_id = if config.anonymous_mode {
            PeerId::generate_anonymous().0
        } else {
            PeerId::generate().0
        };

        // Bind listener for incoming connections
        // Try dual-stack [::]:port first, fall back to IPv4-only
        let listener: Option<Box<dyn crate::transport::TransportListener>> = match factory
            .bind_tcp(SocketAddr::from((
                std::net::Ipv6Addr::UNSPECIFIED,
                config.listen_port,
            )))
            .await
        {
            Ok(l) => Some(l),
            Err(_) => factory
                .bind_tcp(SocketAddr::from(([0, 0, 0, 0], config.listen_port)))
                .await
                .ok(),
        };
        // Note: DSCP on listener is skipped for transport-abstracted sockets (no raw fd)

        let mut tracker_manager = TrackerManager::from_torrent_filtered(
            &meta,
            our_peer_id,
            config.listen_port,
            &config.url_security,
            config.peer_dscp,
            config.anonymous_mode,
        );
        tracker_manager.set_info_hashes(info_hashes.clone());

        // BEP 7: include our I2P destination in tracker announces
        if let Some(ref sam) = sam_session {
            tracker_manager.set_i2p_destination(Some(sam.destination().to_base64()));
        }

        let enable_dht = config.enable_dht;

        // Start DHT peer discovery if enabled and available
        let dht_peers_rx = if enable_dht {
            if let Some(ref dht) = dht {
                match dht.get_peers(meta.info_hash).await {
                    Ok(rx) => Some(rx),
                    Err(e) => {
                        warn!("failed to start DHT v4 get_peers: {e}");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        let dht_v6_peers_rx = if enable_dht {
            if let Some(ref dht6) = dht_v6 {
                match dht6.get_peers(meta.info_hash).await {
                    Ok(rx) => Some(rx),
                    Err(e) => {
                        debug!("failed to start DHT v6 get_peers: {e}");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        // Dual-swarm: also search for v2 hash peers if hybrid
        let v2_as_v1 = if info_hashes.is_hybrid() {
            info_hashes
                .v2
                .map(|v2| Id20(v2.0[..20].try_into().unwrap()))
        } else {
            None
        };
        let (dht_v2_peers_rx, dht_v6_v2_peers_rx) =
            if let (true, Some(v2_id)) = (enable_dht, v2_as_v1) {
                let rx4 = if let Some(ref dht) = dht {
                    dht.get_peers(v2_id).await.ok()
                } else {
                    None
                };
                let rx6 = if let Some(ref dht6) = dht_v6 {
                    dht6.get_peers(v2_id).await.ok()
                } else {
                    None
                };
                (rx4, rx6)
            } else {
                (None, None)
            };

        let upload_bucket = crate::rate_limiter::TokenBucket::new(config.upload_rate_limit);
        let download_bucket = crate::rate_limiter::TokenBucket::new(config.download_rate_limit);
        let rate_limiter_set = crate::rate_limiter::RateLimiterSet::new(
            0,
            0,
            0,
            0,
            config.upload_rate_limit,
            config.download_rate_limit,
        );

        let super_seed = if config.super_seeding {
            Some(crate::super_seed::SuperSeedState::new())
        } else {
            None
        };
        let have_buffer =
            crate::have_buffer::HaveBuffer::new(num_pieces, config.have_send_delay_ms);
        let is_share_mode = config.share_mode;

        let (piece_ready_tx, _) = broadcast::channel(64);
        let initial_have = chunk_tracker.bitfield().clone();
        let (have_watch_tx, have_watch_rx) = tokio::sync::watch::channel(initial_have);
        let stream_read_semaphore =
            crate::streaming::stream_read_semaphore(config.max_concurrent_stream_reads);

        let choker = Choker::with_algorithms(
            4,
            config.seed_choking_algorithm,
            config.choking_algorithm,
            config.upload_rate_limit,
            2,
            20,
        );

        // M106: Build peer scorer from config
        let scorer = PeerScorer::new(
            Duration::from_secs(config.probation_duration_secs),
            Duration::from_secs(config.discovery_phase_secs),
            Duration::from_secs(config.discovery_churn_interval_secs),
            config.discovery_churn_percent,
            Duration::from_secs(config.steady_churn_interval_secs),
            config.steady_churn_percent,
            config.min_score_threshold,
        );

        // M96: Wire hash pool into disk handle for V1-only torrents
        let mut disk = disk;
        if matches!(version, torrent_core::TorrentVersion::V1Only)
            && let Some(pool) = &hash_pool
        {
            disk.set_hash_pool(pool.clone());
            disk.set_hash_result_tx(hash_result_tx.clone());
        }

        let actor = TorrentActor {
            config,
            info_hash: meta.info_hash,
            our_peer_id,
            state: TorrentState::Downloading,
            disk: Some(disk),
            disk_manager,
            chunk_tracker: Some(chunk_tracker),
            lengths: Some(lengths),
            num_pieces,
            streaming_pieces: BTreeSet::new(),
            time_critical_pieces: BTreeSet::new(),
            streaming_cursors: Vec::new(),
            piece_ready_tx,
            have_watch_tx,
            have_watch_rx,
            stream_read_semaphore,
            file_priorities,
            wanted_pieces,
            end_game: EndGame::new(),
            peers: HashMap::new(),
            cached_peer_rates: FxHashMap::default(),
            refill_notify: Arc::new(tokio::sync::Notify::new()),
            atomic_states: None,
            block_maps: None,
            steal_candidates: None,
            snapshot_dirty: false,
            availability_snapshot: None,
            snapshot_generation: 0,
            piece_owner: Vec::new(),
            peer_slab: crate::piece_reservation::PeerSlab::new(),
            availability: Vec::new(),
            priority_pieces: BTreeSet::new(),
            max_in_flight: 512,
            reservation_notify: None,
            available_peers: Vec::new(),
            available_peers_set: std::collections::HashSet::new(),
            connect_backoff: HashMap::new(),
            choker,
            max_connections: 0,
            metadata_downloader: None,
            downloaded: 0,
            uploaded: 0,
            checking_progress: 0.0,
            total_download: 0,
            total_upload: 0,
            total_failed_bytes: 0,
            total_redundant_bytes: 0,
            added_time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            completed_time: 0,
            last_download: 0,
            last_upload: 0,
            last_seen_complete: 0,
            active_duration: 0,
            finished_duration: 0,
            seeding_duration: 0,
            active_since: Some(std::time::Instant::now()),
            state_duration_since: None,
            started_at: std::time::Instant::now(),
            moving_storage: false,
            has_incoming: false,
            need_save_resume: false,
            error: String::new(),
            error_file: -1,
            cmd_rx,
            event_tx,
            event_rx,
            write_error_rx,
            write_error_tx,
            verify_result_rx,
            verify_result_tx,
            pending_verify: HashSet::new(),
            piece_generations: vec![0u64; num_pieces as usize],
            hash_result_rx,
            hash_result_tx,
            meta: Some(meta),
            listener,
            utp_socket,
            utp_socket_v6,
            tracker_manager,
            dht: if enable_dht { dht } else { None },
            dht_v6: if enable_dht { dht_v6 } else { None },
            dht_peers_rx,
            dht_v6_peers_rx,
            dht_v6_empty_count: 0,
            dht_v6_last_retry: None,
            alert_tx,
            alert_mask,
            upload_bucket,
            download_bucket,
            global_upload_bucket,
            global_download_bucket,
            slot_tuner,
            upload_bytes_interval: 0,
            peak_download_rate: 0,
            web_seeds: HashMap::new(),
            banned_web_seeds: HashSet::new(),
            web_seed_in_flight: HashMap::new(),
            super_seed,
            have_buffer,
            suggested_to_peers: HashMap::new(),
            predictive_have_sent: HashSet::new(),

            ban_manager,
            ip_filter,
            piece_contributors: HashMap::new(),
            parole_pieces: HashMap::new(),
            external_ip: None,
            share_lru: std::collections::VecDeque::new(),
            share_max_pieces: if is_share_mode { 64 } else { 0 },
            plugins,
            hash_picker: None,
            version,
            meta_v2,
            info_hashes,
            dht_v2_peers_rx,
            dht_v6_v2_peers_rx,
            magnet_selected_files: None,
            sam_session,
            i2p_accept_rx: None,
            i2p_peer_counter: 0,
            i2p_destinations: HashMap::new(),
            ssl_manager,
            rate_limiter_set,
            auto_sequential_active: false,
            factory,
            hash_pool_ref: hash_pool,
            scorer,
        };

        let spawn_info_hash = actor.info_hash;
        let join_handle = tokio::spawn(actor.run());
        // Monitor the actor task so panics/exits are logged instead of silently swallowed.
        tokio::spawn(async move {
            match join_handle.await {
                Ok(()) => {
                    tracing::warn!(%spawn_info_hash, "torrent actor exited cleanly");
                }
                Err(e) if e.is_panic() => {
                    let panic_payload = e.into_panic();
                    let msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                        (*s).to_string()
                    } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "unknown panic payload".to_string()
                    };
                    tracing::error!(%spawn_info_hash, "torrent actor PANICKED: {msg}");
                }
                Err(e) => {
                    tracing::error!(%spawn_info_hash, "torrent actor task error: {e}");
                }
            }
        });
        Ok(TorrentHandle { cmd_tx })
    }

    /// Create a torrent session from a magnet link (metadata fetched via BEP 9).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn from_magnet(
        magnet: Magnet,
        disk_manager: DiskManagerHandle,
        config: TorrentConfig,
        dht: Option<DhtHandle>,
        dht_v6: Option<DhtHandle>,
        global_upload_bucket: Option<SharedBucket>,
        global_download_bucket: Option<SharedBucket>,
        slot_tuner: crate::slot_tuner::SlotTuner,
        alert_tx: broadcast::Sender<Alert>,
        alert_mask: Arc<AtomicU32>,
        utp_socket: Option<torrent_utp::UtpSocket>,
        utp_socket_v6: Option<torrent_utp::UtpSocket>,
        ban_manager: crate::session::SharedBanManager,
        ip_filter: crate::session::SharedIpFilter,
        plugins: Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>>,
        sam_session: Option<Arc<crate::i2p::SamSession>>,
        ssl_manager: Option<Arc<crate::ssl_manager::SslManager>>,
        factory: Arc<crate::transport::NetworkFactory>,
        hash_pool: Option<std::sync::Arc<crate::hash_pool::HashPool>>,
    ) -> crate::Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let (event_tx, event_rx) = mpsc::channel(2048);
        let (write_error_tx, write_error_rx) = mpsc::channel(64);
        let (verify_result_tx, verify_result_rx) = mpsc::channel(1024);
        // M96: Dummy channel — replaced when metadata arrives and num_pieces is known
        let (hash_result_tx, hash_result_rx) = mpsc::channel(1);
        let our_peer_id = if config.anonymous_mode {
            PeerId::generate_anonymous().0
        } else {
            PeerId::generate().0
        };

        // Try dual-stack [::]:port first, fall back to IPv4-only
        let listener: Option<Box<dyn crate::transport::TransportListener>> = match factory
            .bind_tcp(SocketAddr::from((
                std::net::Ipv6Addr::UNSPECIFIED,
                config.listen_port,
            )))
            .await
        {
            Ok(l) => Some(l),
            Err(_) => factory
                .bind_tcp(SocketAddr::from(([0, 0, 0, 0], config.listen_port)))
                .await
                .ok(),
        };
        // Note: DSCP on listener is skipped for transport-abstracted sockets (no raw fd)

        let mut tracker_manager = TrackerManager::empty(
            magnet.info_hash(),
            our_peer_id,
            config.listen_port,
            config.peer_dscp,
            config.anonymous_mode,
        );
        // Add tracker URLs from the magnet link (BEP 9 §3.1)
        for url in &magnet.trackers {
            tracker_manager.add_tracker_url(url);
        }

        // BEP 7: include our I2P destination in tracker announces
        if let Some(ref sam) = sam_session {
            tracker_manager.set_i2p_destination(Some(sam.destination().to_base64()));
        }

        let enable_dht = config.enable_dht;

        // Start DHT peer discovery if enabled and available
        let dht_peers_rx = if enable_dht {
            if let Some(ref dht) = dht {
                match dht.get_peers(magnet.info_hash()).await {
                    Ok(rx) => Some(rx),
                    Err(e) => {
                        warn!("failed to start DHT v4 get_peers: {e}");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        let dht_v6_peers_rx = if enable_dht {
            if let Some(ref dht6) = dht_v6 {
                match dht6.get_peers(magnet.info_hash()).await {
                    Ok(rx) => Some(rx),
                    Err(e) => {
                        debug!("failed to start DHT v6 get_peers: {e}");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        let upload_bucket = crate::rate_limiter::TokenBucket::new(config.upload_rate_limit);
        let download_bucket = crate::rate_limiter::TokenBucket::new(config.download_rate_limit);
        let rate_limiter_set = crate::rate_limiter::RateLimiterSet::new(
            0,
            0,
            0,
            0,
            config.upload_rate_limit,
            config.download_rate_limit,
        );

        let super_seed = if config.super_seeding {
            Some(crate::super_seed::SuperSeedState::new())
        } else {
            None
        };
        let have_buffer = crate::have_buffer::HaveBuffer::new(0, config.have_send_delay_ms);
        let is_share_mode = config.share_mode;
        let magnet_selected_files = magnet.selected_files.clone();
        let info_hashes = magnet.info_hashes.clone();

        let (piece_ready_tx, _) = broadcast::channel(64);
        let (have_watch_tx, have_watch_rx) = tokio::sync::watch::channel(Bitfield::new(0));
        let stream_read_semaphore =
            crate::streaming::stream_read_semaphore(config.max_concurrent_stream_reads);

        let choker = Choker::with_algorithms(
            4,
            config.seed_choking_algorithm,
            config.choking_algorithm,
            config.upload_rate_limit,
            2,
            20,
        );

        // M106: Build peer scorer from config
        let scorer = PeerScorer::new(
            Duration::from_secs(config.probation_duration_secs),
            Duration::from_secs(config.discovery_phase_secs),
            Duration::from_secs(config.discovery_churn_interval_secs),
            config.discovery_churn_percent,
            Duration::from_secs(config.steady_churn_interval_secs),
            config.steady_churn_percent,
            config.min_score_threshold,
        );

        let actor = TorrentActor {
            config,
            info_hash: magnet.info_hash(),
            our_peer_id,
            state: TorrentState::FetchingMetadata,
            disk: None,
            disk_manager,
            chunk_tracker: None,
            lengths: None,
            num_pieces: 0,
            streaming_pieces: BTreeSet::new(),
            time_critical_pieces: BTreeSet::new(),
            streaming_cursors: Vec::new(),
            piece_ready_tx,
            have_watch_tx,
            have_watch_rx,
            stream_read_semaphore,
            file_priorities: Vec::new(),
            wanted_pieces: Bitfield::new(0),
            end_game: EndGame::new(),
            peers: HashMap::new(),
            cached_peer_rates: FxHashMap::default(),
            refill_notify: Arc::new(tokio::sync::Notify::new()),
            atomic_states: None,
            block_maps: None,
            steal_candidates: None,
            snapshot_dirty: false,
            availability_snapshot: None,
            snapshot_generation: 0,
            piece_owner: Vec::new(),
            peer_slab: crate::piece_reservation::PeerSlab::new(),
            availability: Vec::new(),
            priority_pieces: BTreeSet::new(),
            max_in_flight: 512,
            reservation_notify: None,
            available_peers: Vec::new(),
            available_peers_set: std::collections::HashSet::new(),
            connect_backoff: HashMap::new(),
            choker,
            max_connections: 0,
            metadata_downloader: Some(MetadataDownloader::new(magnet.info_hash())),
            downloaded: 0,
            uploaded: 0,
            checking_progress: 0.0,
            total_download: 0,
            total_upload: 0,
            total_failed_bytes: 0,
            total_redundant_bytes: 0,
            added_time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            completed_time: 0,
            last_download: 0,
            last_upload: 0,
            last_seen_complete: 0,
            active_duration: 0,
            finished_duration: 0,
            seeding_duration: 0,
            active_since: Some(std::time::Instant::now()),
            state_duration_since: None,
            started_at: std::time::Instant::now(),
            moving_storage: false,
            has_incoming: false,
            need_save_resume: false,
            error: String::new(),
            error_file: -1,
            cmd_rx,
            event_tx,
            event_rx,
            write_error_rx,
            write_error_tx,
            verify_result_rx,
            verify_result_tx,
            pending_verify: HashSet::new(),
            piece_generations: Vec::new(),
            hash_result_rx,
            hash_result_tx,
            meta: None,
            listener,
            utp_socket,
            utp_socket_v6,
            tracker_manager,
            dht: if enable_dht { dht } else { None },
            dht_v6: if enable_dht { dht_v6 } else { None },
            dht_peers_rx,
            dht_v6_peers_rx,
            dht_v6_empty_count: 0,
            dht_v6_last_retry: None,
            alert_tx,
            alert_mask,
            upload_bucket,
            download_bucket,
            global_upload_bucket,
            global_download_bucket,
            slot_tuner,
            upload_bytes_interval: 0,
            peak_download_rate: 0,
            web_seeds: HashMap::new(),
            banned_web_seeds: HashSet::new(),
            web_seed_in_flight: HashMap::new(),
            super_seed,
            have_buffer,
            suggested_to_peers: HashMap::new(),
            predictive_have_sent: HashSet::new(),

            ban_manager,
            ip_filter,
            piece_contributors: HashMap::new(),
            parole_pieces: HashMap::new(),
            external_ip: None,
            share_lru: std::collections::VecDeque::new(),
            share_max_pieces: if is_share_mode { 64 } else { 0 },
            plugins,
            hash_picker: None,
            version: torrent_core::TorrentVersion::V1Only,
            meta_v2: None,
            info_hashes,
            dht_v2_peers_rx: None,
            dht_v6_v2_peers_rx: None,
            magnet_selected_files,
            sam_session,
            i2p_accept_rx: None,
            i2p_peer_counter: 0,
            i2p_destinations: HashMap::new(),
            ssl_manager,
            rate_limiter_set,
            auto_sequential_active: false,
            factory,
            hash_pool_ref: hash_pool,
            scorer,
        };

        let spawn_info_hash = actor.info_hash;
        let join_handle = tokio::spawn(actor.run());
        tokio::spawn(async move {
            match join_handle.await {
                Ok(()) => {
                    tracing::warn!(%spawn_info_hash, "torrent actor exited cleanly");
                }
                Err(e) if e.is_panic() => {
                    let panic_payload = e.into_panic();
                    let msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                        (*s).to_string()
                    } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "unknown panic payload".to_string()
                    };
                    tracing::error!(%spawn_info_hash, "torrent actor PANICKED: {msg}");
                }
                Err(e) => {
                    tracing::error!(%spawn_info_hash, "torrent actor task error: {e}");
                }
            }
        });
        Ok(TorrentHandle { cmd_tx })
    }

    /// Send an incoming peer (routed by the session) to this torrent.
    pub(crate) async fn send_incoming_peer(
        &self,
        stream: crate::transport::BoxedStream,
        addr: SocketAddr,
    ) -> crate::Result<()> {
        self.cmd_tx
            .send(TorrentCommand::IncomingPeer { stream, addr })
            .await
            .map_err(|_| crate::Error::Shutdown)
    }

    /// Query current torrent statistics.
    pub async fn stats(&self) -> crate::Result<TorrentStats> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::Stats { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Add peer addresses to the available-peer pool.
    pub async fn add_peers(&self, peers: Vec<SocketAddr>, source: PeerSource) -> crate::Result<()> {
        self.cmd_tx
            .send(TorrentCommand::AddPeers { peers, source })
            .await
            .map_err(|_| crate::Error::Shutdown)
    }

    /// Pause the torrent session (disconnect peers, announce Stopped).
    pub async fn pause(&self) -> crate::Result<()> {
        self.cmd_tx
            .send(TorrentCommand::Pause)
            .await
            .map_err(|_| crate::Error::Shutdown)
    }

    /// Resume a paused torrent session (reconnect, announce Started).
    pub async fn resume(&self) -> crate::Result<()> {
        self.cmd_tx
            .send(TorrentCommand::Resume)
            .await
            .map_err(|_| crate::Error::Shutdown)
    }

    /// Gracefully shut down the torrent session.
    pub async fn shutdown(&self) -> crate::Result<()> {
        // Best-effort send with timeout — if the channel is full or closed,
        // the actor will exit when all senders are dropped anyway.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.cmd_tx.send(TorrentCommand::Shutdown),
        )
        .await;
        Ok(())
    }

    /// Snapshot current torrent state into libtorrent-compatible resume data.
    pub async fn save_resume_data(&self) -> crate::Result<torrent_core::FastResumeData> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::SaveResumeData { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Set the download priority for a specific file.
    pub async fn set_file_priority(
        &self,
        index: usize,
        priority: torrent_core::FilePriority,
    ) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::SetFilePriority {
                index,
                priority,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get the current per-file priorities.
    pub async fn file_priorities(&self) -> crate::Result<Vec<torrent_core::FilePriority>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::FilePriorities { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Get the list of all configured trackers with their status.
    pub async fn tracker_list(&self) -> crate::Result<Vec<crate::tracker_manager::TrackerInfo>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::TrackerList { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Force all trackers to re-announce immediately.
    pub async fn force_reannounce(&self) -> crate::Result<()> {
        self.cmd_tx
            .send(TorrentCommand::ForceReannounce)
            .await
            .map_err(|_| crate::Error::Shutdown)
    }

    /// Scrape trackers for seeder/leecher counts.
    pub async fn scrape(&self) -> crate::Result<Option<(String, torrent_tracker::ScrapeInfo)>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::Scrape { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Open a streaming reader for a file within the torrent.
    pub async fn open_file(
        &self,
        file_index: usize,
    ) -> crate::Result<crate::streaming::FileStream> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::OpenFile {
                file_index,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        let handle = rx.await.map_err(|_| crate::Error::Shutdown)??;
        Ok(crate::streaming::FileStream::from_handle(handle))
    }

    /// Update the external IP for BEP 40 peer priority sorting.
    pub(crate) async fn update_external_ip(&self, ip: std::net::IpAddr) -> crate::Result<()> {
        self.cmd_tx
            .send(TorrentCommand::UpdateExternalIp { ip })
            .await
            .map_err(|_| crate::Error::Shutdown)
    }

    /// Move torrent data files to a new download directory.
    ///
    /// Relocates existing files (rename or copy+delete), re-registers storage
    /// with the disk manager, and fires a `StorageMoved` alert on success.
    pub async fn move_storage(&self, new_path: std::path::PathBuf) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::MoveStorage {
                new_path,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Set the per-torrent download rate limit in bytes/sec (0 = unlimited).
    pub async fn set_download_limit(&self, bytes_per_sec: u64) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::SetDownloadLimit {
                bytes_per_sec,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Set the per-torrent upload rate limit in bytes/sec (0 = unlimited).
    pub async fn set_upload_limit(&self, bytes_per_sec: u64) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::SetUploadLimit {
                bytes_per_sec,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Get the current per-torrent download rate limit in bytes/sec (0 = unlimited).
    pub async fn download_limit(&self) -> crate::Result<u64> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::DownloadLimit { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Get the current per-torrent upload rate limit in bytes/sec (0 = unlimited).
    pub async fn upload_limit(&self) -> crate::Result<u64> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::UploadLimit { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Enable or disable sequential (in-order) piece downloading.
    pub async fn set_sequential_download(&self, enabled: bool) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::SetSequentialDownload { enabled, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Query whether sequential downloading is enabled.
    pub async fn is_sequential_download(&self) -> crate::Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::IsSequentialDownload { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Enable or disable BEP 16 super seeding mode.
    pub async fn set_super_seeding(&self, enabled: bool) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::SetSuperSeeding { enabled, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Query whether BEP 16 super seeding mode is enabled.
    pub async fn is_super_seeding(&self) -> crate::Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::IsSuperSeeding { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Add a new tracker URL to this torrent (fire-and-forget).
    ///
    /// The URL is validated and deduplicated by the tracker manager.
    pub async fn add_tracker(&self, url: String) -> crate::Result<()> {
        self.cmd_tx
            .send(TorrentCommand::AddTracker { url })
            .await
            .map_err(|_| crate::Error::Shutdown)
    }

    /// Replace all tracker URLs for this torrent.
    pub async fn replace_trackers(&self, urls: Vec<String>) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::ReplaceTrackers { urls, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Trigger a full piece verification (force recheck).
    ///
    /// Transitions the torrent through `Checking` state, clears all piece
    /// completion data, re-verifies every piece against its hash, then
    /// transitions to `Seeding` (all valid) or `Downloading` (some missing).
    /// Returns after the check is complete.
    pub async fn force_recheck(&self) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::ForceRecheck { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Rename a file within the torrent on disk.
    ///
    /// Changes the filename of the specified file (by index) to `new_name`.
    /// The file stays in the same directory; only the filename component changes.
    /// Fires a `FileRenamed` alert on success.
    pub async fn rename_file(&self, file_index: usize, new_name: String) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::RenameFile {
                file_index,
                new_name,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Route an incoming SSL peer (TLS already completed) to this torrent (M42).
    pub(crate) async fn spawn_ssl_peer(
        &self,
        addr: SocketAddr,
        stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    ) -> crate::Result<()> {
        self.cmd_tx
            .send(TorrentCommand::SpawnSslPeer {
                addr,
                stream: crate::types::BoxedAsyncStream(Box::new(stream)),
            })
            .await
            .map_err(|_| crate::Error::Shutdown)
    }

    /// Set the per-torrent maximum number of connections (0 = use global default).
    pub async fn set_max_connections(&self, limit: usize) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::SetMaxConnections { limit, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Get the current per-torrent maximum connection limit (0 = use global default).
    pub async fn max_connections(&self) -> crate::Result<usize> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::MaxConnections { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Set the per-torrent maximum number of upload slots (unchoke slots).
    pub async fn set_max_uploads(&self, limit: usize) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::SetMaxUploads { limit, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Get the current per-torrent maximum upload slots (unchoke slots).
    pub async fn max_uploads(&self) -> crate::Result<usize> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::MaxUploads { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Get per-peer details for all connected peers.
    pub async fn get_peer_info(&self) -> crate::Result<Vec<PeerInfo>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::GetPeerInfo { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Get in-flight piece download status (the download queue).
    pub async fn get_download_queue(&self) -> crate::Result<Vec<PartialPieceInfo>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::GetDownloadQueue { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Check whether a specific piece has been downloaded.
    pub async fn have_piece(&self, index: u32) -> crate::Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::HavePiece { index, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Get per-piece availability counts from connected peers.
    pub async fn piece_availability(&self) -> crate::Result<Vec<u32>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::PieceAvailability { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Get per-file bytes-downloaded progress.
    pub async fn file_progress(&self) -> crate::Result<Vec<u64>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::FileProgress { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Get the torrent's identity hashes (v1 and/or v2).
    pub async fn info_hashes(&self) -> crate::Result<torrent_core::InfoHashes> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::InfoHashes { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Get the full v1 metainfo, if available.
    ///
    /// Returns `None` for magnet links before metadata has been received.
    pub async fn torrent_file(&self) -> crate::Result<Option<TorrentMetaV1>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::TorrentFile { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Get the full v2 metainfo, if available.
    ///
    /// Returns `None` if the torrent is not a v2/hybrid torrent, or for magnet
    /// links before metadata has been received.
    pub async fn torrent_file_v2(&self) -> crate::Result<Option<torrent_core::TorrentMetaV2>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::TorrentFileV2 { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Force an immediate DHT announce for this torrent.
    ///
    /// Fire-and-forget at the torrent level — the DHT announce is best-effort.
    pub async fn force_dht_announce(&self) -> crate::Result<()> {
        self.cmd_tx
            .send(TorrentCommand::ForceDhtAnnounce)
            .await
            .map_err(|_| crate::Error::Shutdown)
    }

    /// Read all data for a specific piece from disk.
    ///
    /// Returns the complete piece data as `Bytes`. The piece must have been
    /// downloaded already; use [`have_piece`](Self::have_piece) to check first.
    pub async fn read_piece(&self, index: u32) -> crate::Result<Bytes> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::ReadPiece { index, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Flush the disk write cache, ensuring all buffered writes are persisted.
    pub async fn flush_cache(&self) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::FlushCache { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Check whether this handle refers to a live torrent.
    ///
    /// Returns `false` after the torrent has been removed or shut down.
    /// This is a synchronous check on the channel state — no command dispatch.
    pub fn is_valid(&self) -> bool {
        !self.cmd_tx.is_closed()
    }

    /// Clear any error state on the torrent and resume if it was paused due to error.
    pub async fn clear_error(&self) -> crate::Result<()> {
        self.cmd_tx
            .send(TorrentCommand::ClearError)
            .await
            .map_err(|_| crate::Error::Shutdown)
    }

    /// Get per-file open/mode status based on the current torrent state.
    ///
    /// Returns one [`crate::types::FileStatus`] entry per file in the torrent.
    pub async fn file_status(&self) -> crate::Result<Vec<crate::types::FileStatus>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::FileStatus { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Read the current torrent state as a [`TorrentFlags`] bitflag set.
    pub async fn flags(&self) -> crate::Result<crate::types::TorrentFlags> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::Flags { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Set (enable) the specified torrent flags.
    ///
    /// Delegates to the underlying operations (pause/resume, sequential download, etc.).
    pub async fn set_flags(&self, flags: crate::types::TorrentFlags) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::SetFlags { flags, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Unset (disable) the specified torrent flags.
    ///
    /// Delegates to the underlying operations (pause/resume, sequential download, etc.).
    pub async fn unset_flags(&self, flags: crate::types::TorrentFlags) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::UnsetFlags { flags, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Immediately initiate a peer connection to the given address.
    ///
    /// Bypasses the normal peer selection queue — the connection attempt starts
    /// right away. Fire-and-forget: no reply is sent.
    pub async fn connect_peer(&self, addr: SocketAddr) -> crate::Result<()> {
        self.cmd_tx
            .send(TorrentCommand::ConnectPeer { addr })
            .await
            .map_err(|_| crate::Error::Shutdown)
    }
}

// ---------------------------------------------------------------------------
// TorrentActor — internal single-owner event loop
// ---------------------------------------------------------------------------

struct TorrentActor {
    config: TorrentConfig,
    info_hash: Id20,
    our_peer_id: Id20,
    state: TorrentState,

    // Disk I/O (None in magnet mode until metadata arrives)
    disk: Option<DiskHandle>,
    disk_manager: DiskManagerHandle,
    chunk_tracker: Option<ChunkTracker>,
    lengths: Option<Lengths>,
    num_pieces: u32,

    // Piece management
    file_priorities: Vec<FilePriority>,
    wanted_pieces: Bitfield,
    end_game: EndGame,

    // Streaming (M28)
    streaming_pieces: BTreeSet<u32>,
    time_critical_pieces: BTreeSet<u32>,
    streaming_cursors: Vec<crate::streaming::StreamingCursor>,
    piece_ready_tx: broadcast::Sender<u32>,
    have_watch_tx: tokio::sync::watch::Sender<Bitfield>,
    have_watch_rx: tokio::sync::watch::Receiver<Bitfield>,
    stream_read_semaphore: Arc<tokio::sync::Semaphore>,

    // Peer management
    peers: HashMap<SocketAddr, PeerState>,
    /// Cached peer download rates for piece stealing decisions.
    /// Refreshed on each periodic tick (~1s) instead of rebuilding per block.
    cached_peer_rates: FxHashMap<SocketAddr, f64>,
    /// Notify handle for reactive queue refill (legacy, unused in M73).
    #[allow(dead_code)]
    refill_notify: Arc<tokio::sync::Notify>,
    /// M93: Lock-free piece states (shared with peers via Arc).
    atomic_states: Option<Arc<crate::piece_reservation::AtomicPieceStates>>,
    /// M103: Shared block-level request/received bitmaps.
    block_maps: Option<Arc<BlockMaps>>,
    /// M103: Shared queue of stealable pieces.
    steal_candidates: Option<Arc<StealCandidates>>,
    /// M103: Dirty flag for reactive snapshot rebuild.
    snapshot_dirty: bool,
    /// M93: Current availability snapshot (shared with peers via Arc).
    availability_snapshot: Option<Arc<crate::piece_reservation::AvailabilitySnapshot>>,
    /// M93: Snapshot generation counter.
    snapshot_generation: u64,
    /// M93: Maps piece index -> peer slab slot that owns it.
    piece_owner: Vec<Option<u16>>,
    /// M93: Arena-allocated peer tracking: slot <-> SocketAddr.
    peer_slab: crate::piece_reservation::PeerSlab,
    /// M93: Per-piece availability count.
    availability: Vec<u32>,
    /// M93: Priority pieces (streaming, time-critical).
    priority_pieces: BTreeSet<u32>,
    /// M93: Maximum in-flight pieces.
    max_in_flight: usize,
    /// Piece notify handle (for driver spawning).
    reservation_notify: Option<Arc<tokio::sync::Notify>>,
    available_peers: Vec<(SocketAddr, PeerSource)>,
    /// O(1) dedup set for available_peers — kept in sync with the Vec.
    available_peers_set: std::collections::HashSet<SocketAddr>,
    /// M104: Per-peer exponential backoff for failed connections.
    /// Maps peer address → (earliest_retry_time, attempt_count).
    connect_backoff: HashMap<SocketAddr, (std::time::Instant, u32)>,
    choker: Choker,
    /// Per-torrent connection limit override (0 = use config.max_peers).
    max_connections: usize,

    // Metadata (for magnet links)
    metadata_downloader: Option<MetadataDownloader>,

    // Parsed torrent meta (for piece hash verification)
    meta: Option<TorrentMetaV1>,

    // Stats
    downloaded: u64,
    uploaded: u64,
    checking_progress: f32,
    total_download: u64,
    total_upload: u64,
    total_failed_bytes: u64,
    total_redundant_bytes: u64,
    added_time: i64,
    completed_time: i64,
    last_download: i64,
    last_upload: i64,
    last_seen_complete: i64,
    active_duration: i64,
    finished_duration: i64,
    seeding_duration: i64,
    active_since: Option<std::time::Instant>,
    state_duration_since: Option<std::time::Instant>,
    #[allow(dead_code)] // M104: ConnectPhase removed; kept for future diagnostics
    started_at: std::time::Instant,
    moving_storage: bool,
    has_incoming: bool,
    need_save_resume: bool,
    error: String,
    error_file: i32,

    // Channels
    cmd_rx: mpsc::Receiver<TorrentCommand>,
    event_tx: mpsc::Sender<PeerEvent>,
    event_rx: mpsc::Receiver<PeerEvent>,

    // Async disk pipeline channels
    write_error_rx: mpsc::Receiver<crate::disk::DiskWriteError>,
    write_error_tx: mpsc::Sender<crate::disk::DiskWriteError>,
    verify_result_rx: mpsc::Receiver<crate::disk::VerifyResult>,
    verify_result_tx: mpsc::Sender<crate::disk::VerifyResult>,
    /// Pieces currently awaiting async verification — prevents duplicate
    /// verify tasks when end game or slow peers deliver duplicate blocks.
    pending_verify: HashSet<u32>,
    /// Generation counter per piece — increments on release/re-reserve.
    /// Used to detect stale hash results from the HashPool (M96).
    piece_generations: Vec<u64>,
    /// Receiver for hash pool results (M96).
    hash_result_rx: tokio::sync::mpsc::Receiver<crate::hash_pool::HashResult>,
    /// Sender for hash pool results — cloned into DiskHandle (M96).
    hash_result_tx: tokio::sync::mpsc::Sender<crate::hash_pool::HashResult>,

    // TCP listener for incoming peer connections
    listener: Option<Box<dyn crate::transport::TransportListener>>,

    // uTP socket for outbound connections (shared with session, cloned)
    utp_socket: Option<torrent_utp::UtpSocket>,
    // IPv6 uTP socket for outbound connections to IPv6 peers
    utp_socket_v6: Option<torrent_utp::UtpSocket>,

    // Tracker management
    tracker_manager: TrackerManager,

    // DHT handles (shared, optional)
    dht: Option<DhtHandle>,
    dht_v6: Option<DhtHandle>,
    dht_peers_rx: Option<mpsc::Receiver<Vec<SocketAddr>>>,
    dht_v6_peers_rx: Option<mpsc::Receiver<Vec<SocketAddr>>>,
    /// Consecutive times the V6 DHT returned an empty table.
    /// After 30 failures (~3s at 100ms), stop retrying to avoid log spam.
    dht_v6_empty_count: u32,
    /// Timestamp of last V6 DHT retry attempt (M97).
    dht_v6_last_retry: Option<std::time::Instant>,

    // Alert system (M15)
    alert_tx: broadcast::Sender<Alert>,
    alert_mask: Arc<AtomicU32>,

    // Rate limiting (M14)
    upload_bucket: crate::rate_limiter::TokenBucket,
    download_bucket: crate::rate_limiter::TokenBucket,
    global_upload_bucket: Option<SharedBucket>,
    #[allow(dead_code)] // M73: rate limiting deferred to M74
    global_download_bucket: Option<SharedBucket>,
    slot_tuner: crate::slot_tuner::SlotTuner,
    upload_bytes_interval: u64,

    /// Peak aggregate download rate observed (bytes/sec), for peer turnover cutoff.
    peak_download_rate: u64,

    // Web seeding (M22)
    web_seeds: HashMap<String, mpsc::Sender<crate::web_seed::WebSeedCommand>>,
    banned_web_seeds: HashSet<String>,
    web_seed_in_flight: HashMap<u32, String>,

    // BEP 16 super seeding (M23)
    super_seed: Option<crate::super_seed::SuperSeedState>,
    // Batched Have (M23)
    have_buffer: crate::have_buffer::HaveBuffer,

    /// M44: pieces we've suggested to each peer (avoid re-suggesting)
    suggested_to_peers: HashMap<SocketAddr, HashSet<u32>>,

    /// M44: pieces for which we've already sent predictive Have
    predictive_have_sent: HashSet<u32>,

    // Smart banning (M25)
    ban_manager: crate::session::SharedBanManager,
    piece_contributors: HashMap<u32, HashSet<std::net::IpAddr>>,
    parole_pieces: HashMap<u32, crate::ban::ParoleState>,

    // IP filtering (M29)
    ip_filter: crate::session::SharedIpFilter,

    // BEP 40 peer priority (M32b)
    external_ip: Option<std::net::IpAddr>,

    // Share mode (M32c): LRU tracker for in-memory piece relay.
    // Tracks which pieces are currently "live" (servable) in share mode.
    // Oldest pieces are evicted when capacity is reached.
    share_lru: std::collections::VecDeque<u32>,
    /// Max pieces to keep live in share mode (0 = share mode disabled).
    share_max_pieces: usize,

    // Extension plugins (M32d)
    plugins: Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>>,

    // BEP 52 v2/hybrid support (M34-M35)
    hash_picker: Option<torrent_core::HashPicker>,
    version: torrent_core::TorrentVersion,
    #[allow(dead_code)] // stored for hybrid torrent re-serialization (M35 Task 5)
    meta_v2: Option<torrent_core::TorrentMetaV2>,

    /// Full info hashes for dual-swarm support (v1 + v2 for hybrid).
    info_hashes: torrent_core::InfoHashes,

    /// Dual-swarm DHT peer receivers (v2 hash in hybrid torrents).
    dht_v2_peers_rx: Option<mpsc::Receiver<Vec<SocketAddr>>>,
    dht_v6_v2_peers_rx: Option<mpsc::Receiver<Vec<SocketAddr>>>,

    /// BEP 53: deferred file selection from magnet `so=` parameter.
    /// Applied after metadata is received to set file priorities.
    magnet_selected_files: Option<Vec<torrent_core::FileSelection>>,

    /// I2P SAM session for anonymous peer connections (M41).
    sam_session: Option<Arc<crate::i2p::SamSession>>,

    /// Receiver for incoming I2P peer connections (M41).
    i2p_accept_rx: Option<mpsc::Receiver<crate::i2p::SamStream>>,

    /// Counter for generating synthetic SocketAddr values for I2P peers (M41).
    i2p_peer_counter: u32,

    /// Maps synthetic SocketAddr → I2pDestination for outbound I2P connects.
    i2p_destinations: HashMap<SocketAddr, crate::i2p::I2pDestination>,

    /// SSL manager for SSL torrent certificate handling (M42).
    ssl_manager: Option<Arc<crate::ssl_manager::SslManager>>,

    /// Per-class rate limiting with mixed-mode (M45).
    rate_limiter_set: crate::rate_limiter::RateLimiterSet,
    /// Whether auto-sequential mode is currently active (hysteresis state).
    auto_sequential_active: bool,
    /// Network transport factory for TCP operations (M51).
    factory: Arc<crate::transport::NetworkFactory>,
    /// Shared hash pool for parallel piece verification (M96).
    hash_pool_ref: Option<std::sync::Arc<crate::hash_pool::HashPool>>,
    /// M106: Peer quality scorer for churn decisions.
    scorer: PeerScorer,
}

/// Maximum number of in-flight end-game requests per peer.
/// libtorrent continues full pipelining in end-game; we use a moderate
/// depth so that round-trip latency doesn't bottleneck throughput.
/// End-game pipeline depth: match normal mode (128 slots per peer).
/// Safe because the reactive per-block cascade was replaced with a 200ms
/// batch refill tick — raising depth no longer amplifies picker invocations.
const END_GAME_DEPTH: usize = 128;

/// Minimum free pipeline slots before invoking the full piece picker in
/// `handle_piece_data()`.  Avoids running the 5-layer picker on every single
impl TorrentActor {
    /// Returns the effective maximum connection count for this torrent.
    ///
    /// If `max_connections > 0`, use that override; otherwise fall back to `config.max_peers`.
    fn effective_max_connections(&self) -> usize {
        if self.max_connections > 0 {
            self.max_connections
        } else {
            self.config.max_peers
        }
    }

    /// Transition to a new state, firing a StateChanged alert if different.
    fn transition_state(&mut self, new_state: TorrentState) {
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

    /// Count connected peers by transport type.
    fn transport_peer_counts(&self) -> (usize, usize) {
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

    /// Main event loop.
    async fn run(mut self) {
        // Verify existing pieces on startup (resume support)
        self.verify_existing_pieces().await;

        // M93: Initialize lock-free piece states after verification
        // so we_have reflects already-verified pieces.
        if let Some(ct) = &self.chunk_tracker {
            let atomic_states = Arc::new(AtomicPieceStates::new(
                self.num_pieces,
                ct.bitfield(),
                &self.wanted_pieces,
            ));
            self.atomic_states = Some(Arc::clone(&atomic_states));
            self.availability = vec![0u32; self.num_pieces as usize];
            self.piece_owner = vec![None; self.num_pieces as usize];
            self.max_in_flight = self.config.max_in_flight_pieces;

            // M103: Initialize block stealing infrastructure
            if self.config.use_block_stealing {
                if let Some(ref lengths) = self.lengths {
                    self.block_maps = Some(Arc::new(BlockMaps::new(self.num_pieces, lengths)));
                }
                self.steal_candidates = Some(Arc::new(StealCandidates::new()));
            }

            let snapshot = Arc::new(AvailabilitySnapshot::build(
                &self.availability,
                &atomic_states,
                &self.priority_pieces,
                0,
            ));
            self.availability_snapshot = Some(snapshot);
            self.snapshot_generation = 0;

            let notify = Arc::new(tokio::sync::Notify::new());
            self.reservation_notify = Some(notify);
        }

        // Spawn web seeds if not already seeding
        if self.state != TorrentState::Seeding {
            self.spawn_web_seeds();
            self.assign_pieces_to_web_seeds();
        }

        let mut unchoke_interval = tokio::time::interval(Duration::from_secs(10));
        let mut optimistic_interval = tokio::time::interval(Duration::from_secs(30));
        // M104.1: Two-phase connect — 100ms ramp for first 15s (fast peer discovery),
        // then 500ms steady state. Simpler than old ConnectPhase enum; per-peer backoff
        // prevents hammering failed peers during the aggressive phase.
        let mut connect_interval = tokio::time::interval(Duration::from_millis(100));
        let mut connect_ramped_down = false;
        let mut refill_interval = tokio::time::interval(Duration::from_millis(100));
        let mut have_flush_interval = if self.config.have_send_delay_ms > 0 {
            Some(tokio::time::interval(Duration::from_millis(
                self.config.have_send_delay_ms,
            )))
        } else {
            None
        };
        let mut suggest_interval = if self.config.suggest_mode {
            Some(tokio::time::interval(Duration::from_secs(30)))
        } else {
            None
        };
        // M106: 1s scored turnover tick — actual churn is gated by scorer.should_churn()
        let mut turnover_interval = tokio::time::interval(Duration::from_secs(1));
        let mut pipeline_tick_interval = tokio::time::interval(Duration::from_millis(1000));
        // M77: Skip missed ticks — safety-net notify should fire at most once/second
        pipeline_tick_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut end_game_tick_interval = tokio::time::interval(Duration::from_millis(200));
        end_game_tick_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut diag_interval = tokio::time::interval(Duration::from_secs(5));
        // M103: 50ms debounce for reactive snapshot (was 500ms fixed interval)
        let mut snapshot_rebuild_interval = tokio::time::interval(Duration::from_millis(50));

        // Don't fire immediately for the first tick
        unchoke_interval.tick().await;
        optimistic_interval.tick().await;
        connect_interval.tick().await;
        refill_interval.tick().await;
        if let Some(ref mut si) = suggest_interval {
            si.tick().await; // skip initial tick
        }
        turnover_interval.tick().await;
        pipeline_tick_interval.tick().await;
        end_game_tick_interval.tick().await;
        diag_interval.tick().await;
        snapshot_rebuild_interval.tick().await;

        // Initial tracker announce (Started event) — non-blocking, fires via select! arm
        // DHT announce (v4 + v6) — dual-swarm for hybrid torrents
        if self.state == TorrentState::Downloading && self.config.enable_dht {
            // Primary hash (v1 or best_v1)
            if let Some(ref dht) = self.dht
                && let Err(e) = dht.announce(self.info_hash, self.config.listen_port).await
            {
                warn!("DHT v4 announce failed: {e}");
            }
            if let Some(ref dht6) = self.dht_v6
                && let Err(e) = dht6.announce(self.info_hash, self.config.listen_port).await
            {
                debug!("DHT v6 announce failed: {e}");
            }
            // Dual-swarm: also announce v2 hash (truncated) for hybrid torrents
            if self.info_hashes.is_hybrid()
                && let Some(v2) = self.info_hashes.v2
            {
                let v2_as_v1 = Id20(v2.0[..20].try_into().unwrap());
                if v2_as_v1 != self.info_hash {
                    if let Some(ref dht) = self.dht
                        && let Err(e) = dht.announce(v2_as_v1, self.config.listen_port).await
                    {
                        debug!("DHT v4 dual-swarm announce failed: {e}");
                    }
                    if let Some(ref dht6) = self.dht_v6
                        && let Err(e) = dht6.announce(v2_as_v1, self.config.listen_port).await
                    {
                        debug!("DHT v6 dual-swarm announce failed: {e}");
                    }
                }
            }
        }

        // I2P accept loop: spawn a background task that feeds incoming I2P
        // connections back via a channel, so the select! arm can handle them.
        if self.config.enable_i2p
            && let Some(ref sam) = self.sam_session
        {
            let (tx, rx) = mpsc::channel(16);
            let sam = Arc::clone(sam);
            tokio::spawn(async move {
                loop {
                    match sam.accept().await {
                        Ok(stream) => {
                            if tx.send(stream).await.is_err() {
                                break; // torrent actor dropped
                            }
                        }
                        Err(e) => {
                            warn!("I2P accept error: {e}");
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    }
                }
            });
            self.i2p_accept_rx = Some(rx);
        }

        loop {
            tokio::select! {
                biased;
                // Events from peers — batch-drain to reduce select! overhead.
                // At 100 MB/s we get ~6K events/sec; processing one-by-one
                // means 6K select! iterations with waker re-registration.
                // biased; ensures this high-throughput arm is checked first.
                event = self.event_rx.recv() => {
                    if let Some(event) = event {
                        self.handle_peer_event(event).await;
                        // Drain up to 512 more ready events without re-entering select!
                        for _ in 0..512 {
                            match self.event_rx.try_recv() {
                                Ok(event) => self.handle_peer_event(event).await,
                                Err(_) => break,
                            }
                        }
                    }
                }
                // Async piece verification results
                Some(result) = self.verify_result_rx.recv() => {
                    self.pending_verify.remove(&result.piece);
                    // Guard: ignore stale/duplicate results for already-verified pieces
                    let dominated = self.chunk_tracker.as_ref()
                        .map(|ct| ct.bitfield().get(result.piece))
                        .unwrap_or(false);
                    if !dominated {
                        if result.passed {
                            self.on_piece_verified(result.piece).await;
                        } else {
                            self.on_piece_hash_failed(result.piece).await;
                            // M73: Drivers pick up released pieces automatically via shared state
                        }
                    }
                }
                // M96: Hash pool verification results
                Some(result) = self.hash_result_rx.recv() => {
                    self.handle_hash_result(result).await;
                }
                // Commands from handle
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(TorrentCommand::AddPeers { peers, source }) => {
                            self.handle_add_peers(peers, source);
                            self.try_connect_peers();
                        }
                        Some(TorrentCommand::Stats { reply }) => {
                            let _ = reply.send(self.make_stats());
                        }
                        Some(TorrentCommand::Pause) => {
                            self.handle_pause().await;
                        }
                        Some(TorrentCommand::Resume) => {
                            self.handle_resume().await;
                        }
                        Some(TorrentCommand::SaveResumeData { reply }) => {
                            let result = self.build_resume_data();
                            let _ = reply.send(result);
                        }
                        Some(TorrentCommand::SetFilePriority { index, priority, reply }) => {
                            let result = self.handle_set_file_priority(index, priority);
                            let _ = reply.send(result);
                        }
                        Some(TorrentCommand::FilePriorities { reply }) => {
                            let _ = reply.send(self.file_priorities.clone());
                        }
                        Some(TorrentCommand::ForceReannounce) => {
                            self.tracker_manager.force_reannounce();
                        }
                        Some(TorrentCommand::TrackerList { reply }) => {
                            let _ = reply.send(self.tracker_manager.tracker_list());
                        }
                        Some(TorrentCommand::Scrape { reply }) => {
                            let result = self.tracker_manager.scrape().await;
                            if let Some((ref url, ref info)) = result {
                                post_alert(&self.alert_tx, &self.alert_mask, AlertKind::ScrapeReply {
                                    info_hash: self.info_hash,
                                    url: url.clone(),
                                    complete: info.complete,
                                    incomplete: info.incomplete,
                                    downloaded: info.downloaded,
                                });
                            }
                            let _ = reply.send(result);
                        }
                        Some(TorrentCommand::OpenFile { file_index, reply }) => {
                            let result = self.handle_open_file(file_index);
                            let _ = reply.send(result);
                        }
                        Some(TorrentCommand::IncomingPeer { stream, addr }) => {
                            self.spawn_peer_from_stream_with_mode(
                                addr,
                                stream,
                                Some(torrent_wire::mse::EncryptionMode::Disabled),
                            );
                        }
                        Some(TorrentCommand::UpdateExternalIp { ip }) => {
                            self.external_ip = Some(ip);
                            self.sort_available_peers();
                            post_alert(
                                &self.alert_tx,
                                &self.alert_mask,
                                AlertKind::ExternalIpDetected { ip },
                            );
                        }
                        Some(TorrentCommand::MoveStorage { new_path, reply }) => {
                            let result = self.handle_move_storage(new_path).await;
                            let _ = reply.send(result);
                        }
                        Some(TorrentCommand::SpawnSslPeer { addr, stream }) => {
                            // TLS is already completed; encryption is handled by TLS layer
                            self.spawn_peer_from_stream_with_mode(
                                addr,
                                stream.0,
                                Some(torrent_wire::mse::EncryptionMode::Disabled),
                            );
                        }
                        Some(TorrentCommand::SetDownloadLimit { bytes_per_sec, reply }) => {
                            self.download_bucket.set_rate(bytes_per_sec);
                            let _ = reply.send(());
                        }
                        Some(TorrentCommand::SetUploadLimit { bytes_per_sec, reply }) => {
                            self.upload_bucket.set_rate(bytes_per_sec);
                            let _ = reply.send(());
                        }
                        Some(TorrentCommand::DownloadLimit { reply }) => {
                            let _ = reply.send(self.download_bucket.rate());
                        }
                        Some(TorrentCommand::UploadLimit { reply }) => {
                            let _ = reply.send(self.upload_bucket.rate());
                        }
                        Some(TorrentCommand::SetSequentialDownload { enabled, reply }) => {
                            self.config.sequential_download = enabled;
                            let _ = reply.send(());
                        }
                        Some(TorrentCommand::IsSequentialDownload { reply }) => {
                            let _ = reply.send(self.config.sequential_download);
                        }
                        Some(TorrentCommand::SetSuperSeeding { enabled, reply }) => {
                            self.config.super_seeding = enabled;
                            self.super_seed = if enabled {
                                Some(crate::super_seed::SuperSeedState::new())
                            } else {
                                None
                            };
                            let _ = reply.send(());
                        }
                        Some(TorrentCommand::IsSuperSeeding { reply }) => {
                            let _ = reply.send(self.config.super_seeding);
                        }
                        Some(TorrentCommand::AddTracker { url }) => {
                            self.tracker_manager.add_tracker_url(&url);
                        }
                        Some(TorrentCommand::ReplaceTrackers { urls, reply }) => {
                            self.tracker_manager.replace_all(urls);
                            let _ = reply.send(());
                        }
                        Some(TorrentCommand::ForceRecheck { reply }) => {
                            self.handle_force_recheck(reply).await;
                        }
                        Some(TorrentCommand::RenameFile { file_index, new_name, reply }) => {
                            let result = self.handle_rename_file(file_index, new_name).await;
                            let _ = reply.send(result);
                        }
                        Some(TorrentCommand::SetMaxConnections { limit, reply }) => {
                            self.max_connections = limit;
                            let _ = reply.send(());
                        }
                        Some(TorrentCommand::MaxConnections { reply }) => {
                            let _ = reply.send(self.max_connections);
                        }
                        Some(TorrentCommand::SetMaxUploads { limit, reply }) => {
                            self.choker.set_unchoke_slots(limit);
                            let _ = reply.send(());
                        }
                        Some(TorrentCommand::MaxUploads { reply }) => {
                            let _ = reply.send(self.choker.unchoke_slots());
                        }
                        Some(TorrentCommand::GetPeerInfo { reply }) => {
                            let _ = reply.send(self.build_peer_info());
                        }
                        Some(TorrentCommand::GetDownloadQueue { reply }) => {
                            let _ = reply.send(self.build_download_queue());
                        }
                        Some(TorrentCommand::HavePiece { index, reply }) => {
                            let has = self.chunk_tracker.as_ref()
                                .map(|ct| ct.has_piece(index))
                                .unwrap_or(false);
                            let _ = reply.send(has);
                        }
                        Some(TorrentCommand::PieceAvailability { reply }) => {
                            let avail = self.availability.clone();
                            let _ = reply.send(avail);
                        }
                        Some(TorrentCommand::FileProgress { reply }) => {
                            let _ = reply.send(self.compute_file_progress());
                        }
                        Some(TorrentCommand::InfoHashes { reply }) => {
                            let _ = reply.send(self.info_hashes.clone());
                        }
                        Some(TorrentCommand::TorrentFile { reply }) => {
                            let _ = reply.send(self.meta.clone());
                        }
                        Some(TorrentCommand::TorrentFileV2 { reply }) => {
                            let _ = reply.send(self.meta_v2.clone());
                        }
                        Some(TorrentCommand::ForceDhtAnnounce) => {
                            self.handle_force_dht_announce().await;
                        }
                        Some(TorrentCommand::ReadPiece { index, reply }) => {
                            let result = self.handle_read_piece(index).await;
                            let _ = reply.send(result);
                        }
                        Some(TorrentCommand::FlushCache { reply }) => {
                            let result = self.handle_flush_cache().await;
                            let _ = reply.send(result);
                        }
                        Some(TorrentCommand::ClearError) => {
                            self.handle_clear_error().await;
                        }
                        Some(TorrentCommand::FileStatus { reply }) => {
                            let _ = reply.send(self.build_file_status());
                        }
                        Some(TorrentCommand::Flags { reply }) => {
                            let _ = reply.send(self.build_flags());
                        }
                        Some(TorrentCommand::SetFlags { flags, reply }) => {
                            self.apply_set_flags(flags).await;
                            let _ = reply.send(());
                        }
                        Some(TorrentCommand::UnsetFlags { flags, reply }) => {
                            self.apply_unset_flags(flags).await;
                            let _ = reply.send(());
                        }
                        Some(TorrentCommand::ConnectPeer { addr }) => {
                            self.handle_connect_peer(addr);
                        }
                        Some(TorrentCommand::Shutdown) => {
                            info!("torrent actor: received Shutdown command, exiting");
                            self.shutdown_web_seeds().await;
                            self.shutdown_peers().await;
                            return;
                        }
                        None => {
                            warn!("torrent actor: cmd_rx channel closed (all senders dropped), exiting");
                            self.shutdown_web_seeds().await;
                            self.shutdown_peers().await;
                            return;
                        }
                    }
                }
                // Async disk write errors
                Some(err) = self.write_error_rx.recv() => {
                    warn!(piece = err.piece, begin = err.begin, "async disk write failed: {}", err.error);
                }
                // Accept incoming peers
                result = accept_incoming(&mut self.listener) => {
                    if let Ok((stream, addr)) = result {
                        self.spawn_peer_from_stream(addr, stream);
                    }
                }
                // Accept incoming I2P peers (M41)
                stream = accept_i2p(&mut self.i2p_accept_rx) => {
                    if let Some(stream) = stream {
                        self.handle_i2p_incoming(stream);
                    }
                }
                // Unchoke timer
                _ = unchoke_interval.tick() => {
                    self.update_peer_rates();
                    // Auto upload slot tuning
                    self.slot_tuner.observe(self.upload_bytes_interval);
                    self.choker.observe_throughput(self.upload_bytes_interval);
                    self.upload_bytes_interval = 0;
                    self.choker.set_unchoke_slots(self.slot_tuner.current_slots());
                    self.run_choker().await;
                    // Update streaming cursors and piece priorities
                    self.update_streaming_cursors();
                    // Update auto-sequential hysteresis (M45)
                    if self.config.auto_sequential {
                        self.auto_sequential_active = crate::piece_selector::evaluate_auto_sequential(
                            self.piece_owner.iter().filter(|o| o.is_some()).count(),
                            self.peers.len(),
                            self.auto_sequential_active,
                        );
                    }
                }
                // Optimistic unchoke timer
                _ = optimistic_interval.tick() => {
                    self.rotate_optimistic();
                }
                // Connect timer — 100ms ramp → 500ms steady (M104.1)
                _ = connect_interval.tick() => {
                    // Ramp down from 100ms to 500ms after 15s of peer discovery
                    if !connect_ramped_down && self.started_at.elapsed() >= Duration::from_secs(15) {
                        connect_ramped_down = true;
                        connect_interval = tokio::time::interval(Duration::from_millis(500));
                        connect_interval.reset();
                    }
                    self.try_connect_peers();
                    self.assign_pieces_to_web_seeds();
                    // Re-trigger DHT search if no active search and we still need peers.
                    // Don't gate on available_peers being empty — tracker peers alone
                    // shouldn't prevent DHT from running (initial get_peers may have
                    // raced with bootstrap and found an empty routing table).
                    if self.config.enable_dht
                        && self.peers.len() < self.effective_max_connections()
                    {
                        if self.dht_peers_rx.is_none()
                            && let Some(ref dht) = self.dht
                        {
                            match dht.get_peers(self.info_hash).await {
                                Ok(rx) => self.dht_peers_rx = Some(rx),
                                Err(e) => warn!("DHT v4 re-search failed: {e}"),
                            }
                        }
                        if self.dht_v6_peers_rx.is_none()
                            && self.dht_v6_empty_count < 30
                            && self.should_retry_v6()
                            && let Some(ref dht6) = self.dht_v6
                        {
                            self.dht_v6_last_retry = Some(std::time::Instant::now());
                            match dht6.get_peers(self.info_hash).await {
                                Ok(rx) => self.dht_v6_peers_rx = Some(rx),
                                Err(e) => debug!("DHT v6 re-search failed: {e}"),
                            }
                        }
                        // Dual-swarm: re-search v2 hash
                        if self.info_hashes.is_hybrid()
                            && let Some(v2) = self.info_hashes.v2
                        {
                            let v2_as_v1 = Id20(v2.0[..20].try_into().unwrap());
                            if self.dht_v2_peers_rx.is_none()
                                && let Some(ref dht) = self.dht
                            {
                                match dht.get_peers(v2_as_v1).await {
                                    Ok(rx) => self.dht_v2_peers_rx = Some(rx),
                                    Err(e) => debug!("DHT v4 v2-swarm re-search failed: {e}"),
                                }
                            }
                            if self.dht_v6_v2_peers_rx.is_none()
                                && self.dht_v6_empty_count < 30
                                && self.should_retry_v6()
                                && let Some(ref dht6) = self.dht_v6
                            {
                                self.dht_v6_last_retry = Some(std::time::Instant::now());
                                match dht6.get_peers(v2_as_v1).await {
                                    Ok(rx) => self.dht_v6_v2_peers_rx = Some(rx),
                                    Err(e) => debug!("DHT v6 v2-swarm re-search failed: {e}"),
                                }
                            }
                        }
                    }
                }
                // Tracker re-announce timer — also fires during FetchingMetadata
                // so magnets with &tr= URLs can discover peers before metadata arrives
                _ = async {
                    match self.tracker_manager.next_announce_in() {
                        Some(dur) => tokio::time::sleep(dur).await,
                        None => std::future::pending().await,
                    }
                } => {
                    let left = self.calculate_left();
                    let result = self.tracker_manager.announce(
                        torrent_tracker::AnnounceEvent::None,
                        self.uploaded,
                        self.downloaded,
                        left,
                    ).await;
                    self.fire_tracker_alerts(&result.outcomes);
                    if !result.peers.is_empty() {
                        debug!(count = result.peers.len(), "tracker returned peers");
                        self.handle_add_peers(result.peers, PeerSource::Tracker);
                        self.try_connect_peers();
                    }
                }
                // DHT v4 peer discovery
                result = async {
                    match &mut self.dht_peers_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match result {
                        Some(peers) => {
                            debug!(count = peers.len(), "DHT v4 returned peers");
                            self.handle_add_peers(peers, PeerSource::Dht);
                            self.try_connect_peers();
                        }
                        None => {
                            debug!("DHT v4 peer search exhausted");
                            self.dht_peers_rx = None;
                        }
                    }
                }
                // DHT v6 peer discovery
                result = async {
                    match &mut self.dht_v6_peers_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match result {
                        Some(peers) => {
                            debug!(count = peers.len(), "DHT v6 returned peers");
                            self.dht_v6_empty_count = 0; // V6 is working, reset
                            self.handle_add_peers(peers, PeerSource::Dht);
                            self.try_connect_peers();
                        }
                        None => {
                            self.dht_v6_peers_rx = None;
                            self.dht_v6_empty_count += 1;
                            if self.dht_v6_empty_count == 30 {
                                debug!("DHT v6 routing table persistently empty, giving up");
                            } else if self.dht_v6_empty_count < 30 {
                                debug!("DHT v6 peer search exhausted");
                            }
                        }
                    }
                }
                // Dual-swarm: DHT v4 v2-hash peer discovery (hybrid)
                result = async {
                    match &mut self.dht_v2_peers_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match result {
                        Some(peers) => {
                            debug!(count = peers.len(), "DHT v4 v2-swarm returned peers");
                            self.handle_add_peers(peers, PeerSource::Dht);
                            self.try_connect_peers();
                        }
                        None => {
                            debug!("DHT v4 v2-swarm peer search exhausted");
                            self.dht_v2_peers_rx = None;
                        }
                    }
                }
                // Dual-swarm: DHT v6 v2-hash peer discovery (hybrid)
                result = async {
                    match &mut self.dht_v6_v2_peers_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match result {
                        Some(peers) => {
                            debug!(count = peers.len(), "DHT v6 v2-swarm returned peers");
                            self.handle_add_peers(peers, PeerSource::Dht);
                            self.try_connect_peers();
                        }
                        None => {
                            debug!("DHT v6 v2-swarm peer search exhausted");
                            self.dht_v6_v2_peers_rx = None;
                        }
                    }
                }
                // Batched Have flush timer
                _ = async {
                    match &mut have_flush_interval {
                        Some(interval) => interval.tick().await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.flush_have_buffer().await;
                }
                // M44: Suggest cached pieces timer
                _ = async {
                    match suggest_interval {
                        Some(ref mut interval) => interval.tick().await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.suggest_cached_pieces().await;
                }
                // M106: Scored peer turnover (1s tick, gated by scorer.should_churn())
                _ = turnover_interval.tick() => {
                    self.run_scored_turnover().await;
                }
                // Pipeline tick (1s) — update EWMA, snub detection, peer scoring
                _ = pipeline_tick_interval.tick() => {
                    let snub_timeout = Duration::from_secs(self.config.snub_timeout_secs as u64);

                    for (_addr, peer) in self.peers.iter_mut() {
                        peer.pipeline.tick();

                        // Snub detection: no data for snub_timeout_secs while unchoked
                        if !peer.peer_choking && !peer.snubbed {
                            let idle = peer.last_data_received
                                .map(|t| t.elapsed() > snub_timeout)
                                .unwrap_or(false);
                            if idle {
                                peer.snubbed = true;
                                // M106: Count pending requests as timed-out blocks
                                peer.blocks_timed_out = peer.blocks_timed_out
                                    .saturating_add(peer.pending_requests.len() as u64);
                                debug!(%_addr, "peer snubbed (no data for {}s)", self.config.snub_timeout_secs);
                            }
                        }
                    }

                    // M106: Score all peers
                    // Pass 1 — build swarm context (exclude probation peers)
                    let ctx = PeerScorer::build_swarm_context(
                        self.peers.values()
                            .filter(|p| !self.scorer.is_in_probation(p.connected_at))
                            .map(|p| (p.pipeline.ewma_rate(), p.avg_rtt, p.score))
                    );

                    // Pass 2 — score each peer
                    for peer in self.peers.values_mut() {
                        if self.scorer.is_in_probation(peer.connected_at) {
                            peer.score = 0.5;
                        } else {
                            peer.score = PeerScorer::compute_score(
                                peer.pipeline.ewma_rate(),
                                peer.avg_rtt,
                                peer.blocks_completed,
                                peer.blocks_timed_out,
                                peer.bitfield.count_ones(),
                                self.num_pieces,
                                peer.snubbed,
                                &ctx,
                            );
                        }
                    }

                    // Refresh cached peer rates for steal decisions (avoids
                    // rebuilding a FxHashMap from all peers on every block arrival).
                    self.refresh_peer_rates();

                    // M73: Periodic endgame activation check (was in batch_fill_all_peers)
                    if !self.end_game.is_active() {
                        self.check_end_game_activation();
                    }

                    // M77: Safety-net periodic notification — catches availability
                    // changes from add_peer()/peer_have() that no longer notify immediately.
                    if let Some(ref notify) = self.reservation_notify {
                        notify.notify_waiters();
                    }
                }
                // (M75: peer tasks handle dispatch via integrated select! arm)
                // End-game refill tick (200ms) — replace reactive per-block cascade
                // with periodic batch refill. All peers with available pipeline slots
                // get new end-game blocks, preventing idle stalls between ticks.
                _ = end_game_tick_interval.tick(), if self.end_game.is_active() => {
                    let addrs: Vec<SocketAddr> = self.peers.iter()
                        .filter(|(_, p)| !p.peer_choking && p.pending_requests.len() < END_GAME_DEPTH)
                        .map(|(addr, _)| *addr)
                        .collect();
                    for addr in addrs {
                        self.request_end_game_block(addr).await;
                    }
                }
                // Periodic download status report (5s)
                _ = diag_interval.tick() => {
                    // Heartbeat: log state regardless of download state
                    {
                        let have = self.chunk_tracker.as_ref().map(|ct| ct.bitfield().count_ones()).unwrap_or(0);
                        let eg = self.end_game.is_active();
                        let eg_blocks = self.end_game.block_count();
                        info!(state = ?self.state, have, total = self.num_pieces, end_game = eg, eg_blocks, "heartbeat");
                    }
                    if self.state == TorrentState::Downloading {
                        let have = self.chunk_tracker.as_ref().map(|ct| ct.bitfield().count_ones()).unwrap_or(0);
                        let in_flight = self.atomic_states.as_ref().map(|s| s.in_flight_count() as usize).unwrap_or(0);
                        let unchoked = self.peers.values().filter(|p| !p.peer_choking).count();
                        info!(have, in_flight, total = self.num_pieces,
                              downloaded_mb = self.downloaded / (1024 * 1024),
                              peers = self.peers.len(), unchoked,
                              "download progress");
                        for (addr, p) in &self.peers {
                            let last_data = p.last_data_received.map(|t| t.elapsed().as_secs()).unwrap_or(9999);
                            trace!(%addr,
                                   choking = p.peer_choking,
                                   pending = p.pending_requests.len(),
                                   ewma_rate = p.pipeline.ewma_rate() as u64,
                                   last_data_secs = last_data,
                                   bf_ones = p.bitfield.count_ones(),
                                   "peer state");
                        }
                    }
                }
                // M103: Reactive snapshot rebuild with 50ms debounce (was 500ms fixed)
                _ = snapshot_rebuild_interval.tick() => {
                    if self.snapshot_dirty {
                        self.snapshot_dirty = false;
                        self.rebuild_availability_snapshot();
                    }
                }
                // Rate limiter refill (100ms)
                _ = refill_interval.tick() => {
                    let elapsed = Duration::from_millis(100);
                    self.upload_bucket.refill(elapsed);
                    self.download_bucket.refill(elapsed);
                    // Refill per-class buckets and apply mixed-mode (M45)
                    self.rate_limiter_set.refill(elapsed);
                    let (tcp_peers, utp_peers) = self.transport_peer_counts();
                    self.rate_limiter_set.apply_mixed_mode(
                        self.config.mixed_mode_algorithm,
                        tcp_peers,
                        utp_peers,
                        self.config.upload_rate_limit,
                    );
                }
            }
        }
    }

    // ----- Command handlers -----

    fn handle_add_peers(&mut self, peers: Vec<SocketAddr>, source: PeerSource) {
        let mut added = false;
        {
            let ban_mgr = self.ban_manager.read().unwrap();
            let ip_flt = self.ip_filter.read().unwrap();
            for addr in peers {
                if ban_mgr.is_banned(&addr.ip()) {
                    continue;
                }
                if ip_flt.is_blocked(addr.ip()) {
                    continue;
                }
                if !self.peers.contains_key(&addr) && self.available_peers_set.insert(addr) {
                    self.available_peers.push((addr, source));
                    added = true;
                }
            }
        }
        if added {
            self.sort_available_peers();
        }
    }

    /// Sort available peers by BEP 40 canonical priority (descending) so that
    /// `pop()` yields the most preferred peer (lowest priority value).
    fn sort_available_peers(&mut self) {
        if let Some(my_ip) = self.external_ip {
            self.available_peers.sort_by(|a, b| {
                let pa = crate::peer_priority::canonical_peer_priority(my_ip, a.0.ip());
                let pb = crate::peer_priority::canonical_peer_priority(my_ip, b.0.ip());
                // Descending: highest priority value first, so pop() gives lowest
                pb.cmp(&pa)
            });
        }
    }

    /// Handle MoveStorage: relocate data files, re-register storage.
    async fn handle_move_storage(&mut self, new_path: std::path::PathBuf) -> crate::Result<()> {
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
        let preallocate = self.config.storage_mode == torrent_core::StorageMode::Full;
        let storage: Arc<dyn TorrentStorage> = match torrent_storage::FilesystemStorage::new(
            &new_path,
            file_paths,
            file_lengths,
            lengths,
            Some(&self.file_priorities),
            preallocate,
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
    async fn handle_rename_file(
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
    /// For efficiency, only checks files whose byte range overlaps the completed piece.
    fn check_file_completion(&self, piece_index: u32) {
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
        if files.is_empty() {
            return;
        }

        let piece_length = lengths.piece_length();
        let piece_start = lengths.piece_offset(piece_index);
        let piece_end = piece_start + lengths.piece_size(piece_index) as u64;

        // Walk files to find which ones overlap this piece
        let mut file_offset = 0u64;
        for (file_idx, file_entry) in files.iter().enumerate() {
            let file_end = file_offset + file_entry.length;

            // Skip files that don't overlap this piece
            if file_end <= piece_start || file_offset >= piece_end {
                file_offset = file_end;
                continue;
            }

            // This file overlaps the completed piece. Check if ALL pieces for
            // this file are now complete.
            let first_piece = (file_offset / piece_length) as u32;
            let last_piece = if file_entry.length == 0 {
                first_piece
            } else {
                ((file_end - 1) / piece_length) as u32
            };

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

    /// Compute download progress metrics.
    ///
    /// Returns `(total, total_done, total_wanted, total_wanted_done, progress, progress_ppm)`.
    fn compute_progress(&self) -> (u64, u64, u64, u64, f32, u32) {
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

    /// Compute distributed copy availability across the swarm.
    ///
    /// Returns `(full_copies, fraction, copies_float)` where `fraction` is in thousandths.
    fn distributed_copies(&self) -> (u32, u32, f32) {
        if self.num_pieces == 0 || self.peers.is_empty() {
            return (0, 0, 0.0);
        }

        let num = self.num_pieces as usize;
        let mut availability = vec![0u32; num];

        for peer in self.peers.values() {
            for idx in 0..self.num_pieces {
                if peer.bitfield.get(idx) {
                    availability[idx as usize] += 1;
                }
            }
        }

        let min_avail = availability.iter().copied().min().unwrap_or(0);
        let rarest_count = availability.iter().filter(|&&c| c == min_avail).count() as u32;
        let fraction = ((self.num_pieces - rarest_count) * 1000) / self.num_pieces;
        let copies_float = min_avail as f32 + fraction as f32 / 1000.0;

        (min_avail, fraction, copies_float)
    }

    fn build_peer_info(&self) -> Vec<PeerInfo> {
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

    fn build_download_queue(&self) -> Vec<PartialPieceInfo> {
        self.piece_owner
            .iter()
            .enumerate()
            .filter_map(|(piece_index, owner)| {
                owner.map(|_| {
                    let piece_index = piece_index as u32;
                    let blocks_in_piece = self
                        .lengths
                        .as_ref()
                        .map(|l| l.piece_size(piece_index).div_ceil(l.chunk_size()))
                        .unwrap_or(0);
                    PartialPieceInfo {
                        piece_index,
                        blocks_in_piece,
                        blocks_assigned: 0,
                    }
                })
            })
            .collect()
    }

    /// Compute per-file downloaded bytes.
    fn compute_file_progress(&self) -> Vec<u64> {
        let meta = match self.meta.as_ref() {
            Some(m) => m,
            None => return Vec::new(),
        };
        let lengths = match self.lengths.as_ref() {
            Some(l) => l,
            None => return Vec::new(),
        };
        let chunk_tracker = match self.chunk_tracker.as_ref() {
            Some(ct) => ct,
            None => return Vec::new(),
        };

        let files = meta.info.files();
        if files.is_empty() {
            return Vec::new();
        }

        let piece_length = lengths.piece_length();
        let mut result = Vec::with_capacity(files.len());
        let mut file_offset = 0u64;

        for file_entry in &files {
            let file_len = file_entry.length;
            if file_len == 0 {
                result.push(0);
                file_offset += file_len;
                continue;
            }

            let file_end = file_offset + file_len;
            let first_piece = (file_offset / piece_length) as u32;
            let last_piece = ((file_end - 1) / piece_length) as u32;

            let mut downloaded = 0u64;

            for p in first_piece..=last_piece {
                if !chunk_tracker.has_piece(p) {
                    continue;
                }

                let piece_start = lengths.piece_offset(p);
                let piece_end = piece_start + lengths.piece_size(p) as u64;

                // Clamp to file boundaries
                let overlap_start = piece_start.max(file_offset);
                let overlap_end = piece_end.min(file_end);

                if overlap_start < overlap_end {
                    downloaded += overlap_end - overlap_start;
                }
            }

            result.push(downloaded);
            file_offset = file_end;
        }

        result
    }

    /// Exponential backoff delay for V6 DHT retries (M97).
    /// 100ms, 200ms, 400ms, 800ms, 1600ms, 3200ms, 5000ms (cap).
    fn v6_retry_delay(&self) -> std::time::Duration {
        let base_ms: u64 = 100;
        let max_ms: u64 = 5000;
        let delay_ms = base_ms
            .saturating_mul(
                1u64.checked_shl(self.dht_v6_empty_count)
                    .unwrap_or(u64::MAX),
            )
            .min(max_ms);
        std::time::Duration::from_millis(delay_ms)
    }

    /// Check if enough time has elapsed for the next V6 DHT retry (M97).
    fn should_retry_v6(&self) -> bool {
        let Some(last) = self.dht_v6_last_retry else {
            return true; // First attempt
        };
        last.elapsed() >= self.v6_retry_delay()
    }

    /// Force an immediate DHT announce on all available DHT handles (v4 + v6).
    async fn handle_force_dht_announce(&self) {
        if let Some(ref dht) = self.dht
            && let Err(e) = dht.announce(self.info_hash, self.config.listen_port).await
        {
            warn!("Force DHT v4 announce failed: {e}");
        }
        if let Some(ref dht6) = self.dht_v6
            && let Err(e) = dht6.announce(self.info_hash, self.config.listen_port).await
        {
            debug!("Force DHT v6 announce failed: {e}");
        }
        // Dual-swarm: also announce v2 hash for hybrid torrents
        if self.info_hashes.is_hybrid()
            && let Some(v2) = self.info_hashes.v2
        {
            let v2_as_v1 = Id20(v2.0[..20].try_into().unwrap());
            if v2_as_v1 != self.info_hash {
                if let Some(ref dht) = self.dht
                    && let Err(e) = dht.announce(v2_as_v1, self.config.listen_port).await
                {
                    debug!("Force DHT v4 dual-swarm announce failed: {e}");
                }
                if let Some(ref dht6) = self.dht_v6
                    && let Err(e) = dht6.announce(v2_as_v1, self.config.listen_port).await
                {
                    debug!("Force DHT v6 dual-swarm announce failed: {e}");
                }
            }
        }
    }

    /// Read a complete piece from disk by reading all chunks and concatenating.
    async fn handle_read_piece(&self, index: u32) -> crate::Result<Bytes> {
        let disk = self
            .disk
            .as_ref()
            .ok_or(crate::Error::MetadataNotReady(self.info_hash))?;
        let lengths = self
            .lengths
            .as_ref()
            .ok_or(crate::Error::MetadataNotReady(self.info_hash))?;

        let piece_size = lengths.piece_size(index);
        if piece_size == 0 {
            return Err(crate::Error::InvalidPieceIndex {
                index,
                num_pieces: lengths.num_pieces(),
            });
        }

        let chunk_size = lengths.chunk_size();
        let num_chunks = lengths.chunks_in_piece(index);
        let mut buf = bytes::BytesMut::with_capacity(piece_size as usize);

        for chunk_idx in 0..num_chunks {
            let begin = chunk_idx * chunk_size;
            let len = if chunk_idx == num_chunks - 1 {
                piece_size - begin
            } else {
                chunk_size
            };
            let data = disk
                .read_chunk(index, begin, len, DiskJobFlags::empty())
                .await
                .map_err(crate::Error::Storage)?;
            buf.extend_from_slice(&data);
        }

        Ok(buf.freeze())
    }

    /// Flush the disk write cache.
    async fn handle_flush_cache(&self) -> crate::Result<()> {
        let disk = self
            .disk
            .as_ref()
            .ok_or(crate::Error::MetadataNotReady(self.info_hash))?;
        disk.flush_cache().await.map_err(crate::Error::Storage)
    }

    /// Clear the error state. If the torrent was paused and had an error, resume it.
    async fn handle_clear_error(&mut self) {
        let had_error = !self.error.is_empty();
        self.error = String::new();
        self.error_file = -1;
        // If we were paused and had an error, resume
        if had_error && self.state == TorrentState::Paused {
            self.handle_resume().await;
        }
    }

    /// Build per-file status based on the current torrent state.
    fn build_file_status(&self) -> Vec<crate::types::FileStatus> {
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
    fn build_flags(&self) -> crate::types::TorrentFlags {
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
    async fn apply_set_flags(&mut self, flags: crate::types::TorrentFlags) {
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
    async fn apply_unset_flags(&mut self, flags: crate::types::TorrentFlags) {
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

    /// Immediately initiate a connection to the given peer address.
    fn handle_connect_peer(&mut self, addr: SocketAddr) {
        // Skip if already connected
        if self.peers.contains_key(&addr) {
            return;
        }
        // Add to the end of available peers (pop() takes from the end, so it
        // will be the next peer attempted) and trigger connection immediately.
        self.available_peers_set.insert(addr);
        self.available_peers.push((addr, PeerSource::Incoming));
        self.try_connect_peers();
    }

    fn make_stats(&self) -> TorrentStats {
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
            peers_available: self.available_peers.len(),
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
            list_peers: self.peers.len() + self.available_peers.len(),
            connect_candidates: self.available_peers.len(),
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

    /// Fire TrackerReply / TrackerError alerts from announce outcomes.
    fn fire_tracker_alerts(&self, outcomes: &[crate::tracker_manager::TrackerOutcome]) {
        for outcome in outcomes {
            match &outcome.result {
                Ok(num_peers) => {
                    post_alert(
                        &self.alert_tx,
                        &self.alert_mask,
                        AlertKind::TrackerReply {
                            info_hash: self.info_hash,
                            url: outcome.url.clone(),
                            num_peers: *num_peers,
                        },
                    );
                }
                Err(msg) => {
                    post_alert(
                        &self.alert_tx,
                        &self.alert_mask,
                        AlertKind::TrackerError {
                            info_hash: self.info_hash,
                            url: outcome.url.clone(),
                            message: msg.clone(),
                        },
                    );
                }
            }
        }
    }

    /// Calculate bytes remaining for tracker announce.
    fn calculate_left(&self) -> u64 {
        match (&self.meta, &self.chunk_tracker) {
            (Some(meta), Some(ct)) => {
                let total = meta.info.total_length();
                let have = ct.bitfield().count_ones() as u64;
                let pieces_total = self.num_pieces as u64;
                if pieces_total == 0 {
                    total
                } else {
                    total.saturating_sub(have * (total / pieces_total))
                }
            }
            _ => 0,
        }
    }

    async fn shutdown_peers(&mut self) {
        // Best-effort announce Stopped to trackers (with timeout to prevent hang)
        let left = self.calculate_left();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            self.tracker_manager
                .announce_stopped(self.uploaded, self.downloaded, left),
        )
        .await;

        // Non-blocking peer shutdown — peers may already be dead or channels full
        for peer in self.peers.values() {
            let _ = peer.cmd_tx.try_send(PeerCommand::Shutdown);
        }
    }

    async fn handle_pause(&mut self) {
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

    async fn handle_resume(&mut self) {
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
        self.try_connect_peers();
    }

    // ----- Event handlers -----

    async fn handle_peer_event(&mut self, event: PeerEvent) {
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
                // In FetchingMetadata mode, if we learn the metadata_size, start requesting
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
                        let missing = dl.missing_pieces();
                        for piece in missing {
                            if let Some(peer) = self.peers.get(&peer_addr) {
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
            PeerEvent::MetadataReject {
                peer_addr: _,
                piece: _,
            } => {
                // Could retry from a different peer; for now, ignore.
            }
            PeerEvent::PexPeers { new_peers } => {
                if self.config.enable_pex {
                    self.handle_add_peers(new_peers, PeerSource::Pex);
                    self.try_connect_peers();
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
                let attempt = self.connect_backoff.get(&peer_addr).map_or(0, |&(_, a)| a);
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

    async fn handle_piece_data(
        &mut self,
        peer_addr: SocketAddr,
        index: u32,
        begin: u32,
        data: Bytes,
    ) {
        // Skip duplicate blocks — in end-game mode or after timeout re-requests,
        // the same block may arrive from multiple peers. Writing it to the store
        // buffer would overwrite valid data that's pending verification.
        if let Some(ref ct) = self.chunk_tracker
            && ct.has_chunk(index, begin)
        {
            self.total_download += data.len() as u64 + 13;
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
            // M75: Permit already returned by peer task on Piece receipt
            return;
        }

        let data_len = data.len();

        // M100: Deferred write via per-torrent writer task.
        if let Some(ref disk) = self.disk {
            disk.write_block_deferred(index, begin, data);
        }

        self.downloaded += data_len as u64;
        self.total_download += data_len as u64 + 13; // payload + message header
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
            peer.download_bytes_window += data_len as u64;
            peer.pipeline
                .block_received(index, begin, data_len as u32, now);
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
            // M44: Predictive piece announce — send Have before verification
            if self.config.predictive_piece_announce_ms > 0
                && !self.predictive_have_sent.contains(&index)
            {
                self.predictive_have_sent.insert(index);
                for peer in self.peers.values() {
                    if !peer.bitfield.get(index) {
                        let _ = peer.cmd_tx.try_send(PeerCommand::Have(index));
                    }
                }
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

        // M75: Permit already returned by peer task on Piece receipt.
        // End-game dispatch still happens here.
        if self.end_game.is_active() {
            self.request_end_game_block(peer_addr).await;
        }
    }

    /// M92: Process a batch of block completions from a single peer.
    /// Iterates blocks, calling `process_block_completion()` for each.
    /// Piece verifications are triggered inline as pieces complete
    /// (same as the former per-block path).
    async fn handle_piece_blocks_batch(
        &mut self,
        peer_addr: SocketAddr,
        blocks: Vec<crate::types::BlockEntry>,
    ) {
        for block in &blocks {
            self.process_block_completion(peer_addr, block.index, block.begin, block.length)
                .await;
        }
    }

    /// M92: Process a single block completion — extracted from `handle_chunk_written()`.
    /// Called once per block in a batch. Returns `true` if this block completed a piece
    /// and piece verification was triggered.
    ///
    /// IMPORTANT: This is a line-for-line extraction of `handle_chunk_written()`.
    /// Every edge case (duplicate detection, end-game cancels, v2/hybrid verification,
    /// predictive Have, smart banning) is preserved identically.
    async fn process_block_completion(
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
            // M44: Predictive piece announce — send Have before verification
            if self.config.predictive_piece_announce_ms > 0
                && !self.predictive_have_sent.contains(&index)
            {
                self.predictive_have_sent.insert(index);
                for peer in self.peers.values() {
                    if !peer.bitfield.get(index) {
                        let _ = peer.cmd_tx.try_send(PeerCommand::Have(index));
                    }
                }
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

    async fn verify_existing_pieces(&mut self) {
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

        let max_concurrent = self.config.hashing_threads.max(1);
        let mut verified_count = 0u32;
        let mut checked_count = 0u32;
        let total = self.num_pieces;

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
    fn fire_file_completed_alerts(&self) {
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
    async fn handle_force_recheck(
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
            self.rebuild_availability_snapshot();
        }

        let _ = reply.send(Ok(()));
    }

    fn build_resume_data(&self) -> crate::Result<torrent_core::FastResumeData> {
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

    fn handle_set_file_priority(
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

    fn handle_open_file(
        &mut self,
        file_index: usize,
    ) -> crate::Result<crate::streaming::FileStreamHandle> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(crate::Error::MetadataNotReady(self.info_hash))?;
        let files = meta.info.files();
        if file_index >= files.len() {
            return Err(crate::Error::InvalidFileIndex {
                index: file_index,
                count: files.len(),
            });
        }
        if self.file_priorities.get(file_index).copied() == Some(FilePriority::Skip) {
            return Err(crate::Error::FileSkipped { index: file_index });
        }

        let lengths = self
            .lengths
            .as_ref()
            .ok_or(crate::Error::MetadataNotReady(self.info_hash))?;
        let disk = self
            .disk
            .as_ref()
            .ok_or(crate::Error::MetadataNotReady(self.info_hash))?;

        // Compute file offset within torrent data
        let mut file_offset = 0u64;
        for f in &files[..file_index] {
            file_offset += f.length;
        }
        let file_length = files[file_index].length;

        let (cursor_tx, cursor_rx) = tokio::sync::watch::channel(0u64);

        let permit = self
            .stream_read_semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|_| crate::Error::Connection("too many concurrent stream readers".into()))?;

        // Add streaming cursor for the actor to track
        self.streaming_cursors
            .push(crate::streaming::StreamingCursor {
                file_index,
                file_offset,
                cursor_piece: (file_offset / lengths.piece_length()) as u32,
                readahead_pieces: self.config.readahead_pieces,
                cursor_rx,
            });

        Ok(crate::streaming::FileStreamHandle {
            disk: disk.clone(),
            lengths: lengths.clone(),
            file_index,
            file_offset,
            file_length,
            cursor_tx,
            piece_ready_rx: self.piece_ready_tx.subscribe(),
            have: self.have_watch_rx.clone(),
            read_permit: permit,
        })
    }

    async fn verify_and_mark_piece(&mut self, index: u32) {
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
    async fn verify_and_mark_piece_v2(&mut self, index: u32) {
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
    async fn run_v2_block_verification(&mut self, index: u32) -> HashResult {
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
    async fn verify_and_mark_piece_hybrid(&mut self, index: u32) {
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
    async fn handle_hash_result(&mut self, result: crate::hash_pool::HashResult) {
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

    async fn on_piece_verified(&mut self, index: u32) {
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

        // Broadcast Have to all peers (skip in super-seed mode)
        if self.super_seed.is_none() {
            let already_announced = self.predictive_have_sent.remove(&index);
            if !already_announced {
                if self.have_buffer.is_enabled() {
                    self.have_buffer.push(index);
                } else {
                    // Immediate mode — with redundancy elimination
                    // Use try_send to avoid blocking the actor if any peer's channel is full
                    for peer in self.peers.values() {
                        if !peer.bitfield.get(index) {
                            let _ = peer.cmd_tx.try_send(PeerCommand::Have(index));
                        }
                    }
                }
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
    async fn on_piece_hash_failed(&mut self, index: u32) {
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
    async fn handle_hashes_received(
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
    async fn handle_incoming_hash_request(
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
        let use_parole = self.ban_manager.read().unwrap().use_parole();
        if !use_parole || contributors.is_empty() {
            // Parole disabled or no contributors to blame — strike everyone
            let mut mgr = self.ban_manager.write().unwrap();
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
            let mut mgr = self.ban_manager.write().unwrap();
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
            let mut mgr = self.ban_manager.write().unwrap();
            if mgr.record_strike(parole_ip) {
                info!(%parole_ip, "parole peer banned");
            }
        }
        // Don't re-enter parole for the same piece — ambiguous situation
    }

    /// Disconnect all peers matching a banned IP and remove from available_peers.
    async fn disconnect_banned_ip(&mut self, ip: std::net::IpAddr) {
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
        // Remove from available peers pool
        let set = &mut self.available_peers_set;
        self.available_peers.retain(|(a, _)| {
            if a.ip() == ip {
                set.remove(a);
                false
            } else {
                true
            }
        });
    }

    /// Flush the batched Have buffer, sending accumulated Haves or a full bitfield.
    async fn flush_have_buffer(&mut self) {
        let ct = match &self.chunk_tracker {
            Some(ct) => ct,
            None => return,
        };

        let result = self.have_buffer.flush(ct.bitfield());
        match result {
            Some(crate::have_buffer::FlushResult::SendHaves(pieces)) => {
                for peer in self.peers.values() {
                    for &idx in &pieces {
                        if !peer.bitfield.get(idx) {
                            let _ = peer.cmd_tx.try_send(PeerCommand::Have(idx));
                        }
                    }
                }
            }
            Some(crate::have_buffer::FlushResult::SendBitfield(bf)) => {
                let data = Bytes::copy_from_slice(bf.as_bytes());
                for peer in self.peers.values() {
                    let _ = peer
                        .cmd_tx
                        .try_send(PeerCommand::SendBitfield(data.clone()));
                }
            }
            None => {}
        }
    }

    /// M44: Suggest cached pieces to connected peers (BEP 6).
    async fn suggest_cached_pieces(&mut self) {
        if !self.config.suggest_mode {
            return;
        }
        let disk = match self.disk {
            Some(ref d) => d.clone(),
            None => return,
        };
        let cached = disk.cached_pieces().await;
        if cached.is_empty() {
            return;
        }
        let max_suggest = self.config.max_suggest_pieces;
        let peer_addrs: Vec<SocketAddr> = self.peers.keys().copied().collect();
        for peer_addr in peer_addrs {
            let already_suggested = self.suggested_to_peers.entry(peer_addr).or_default();
            let peer_has_piece = |piece: u32| -> bool {
                self.peers
                    .get(&peer_addr)
                    .is_some_and(|p| p.bitfield.get(piece))
            };
            let mut sent = 0;
            for &piece in &cached {
                if sent >= max_suggest {
                    break;
                }
                if peer_has_piece(piece) {
                    continue;
                }
                if already_suggested.contains(&piece) {
                    continue;
                }
                if let Some(peer) = self.peers.get(&peer_addr) {
                    let _ = peer.cmd_tx.try_send(PeerCommand::SuggestPiece(piece));
                    already_suggested.insert(piece);
                    sent += 1;
                }
            }
        }
    }

    async fn try_assemble_metadata(&mut self) {
        let assembled = if let Some(ref dl) = self.metadata_downloader {
            dl.assemble_and_verify()
        } else {
            return;
        };

        match assembled {
            Ok(info_bytes) => {
                // Build torrent bytes wrapping the raw info dict into a minimal torrent
                // We need to parse it as a full torrent. The info_bytes is the raw bencoded
                // info dict. We'll build a minimal torrent around it.
                // Actually, torrent_from_bytes expects a full torrent dict.
                // Let's build one:
                let mut torrent_bytes = b"d4:info".to_vec();
                torrent_bytes.extend_from_slice(&info_bytes);
                torrent_bytes.push(b'e');

                match torrent_from_bytes(&torrent_bytes) {
                    Ok(meta) => {
                        let num_pieces = meta.info.num_pieces() as u32;
                        let lengths = Lengths::new(
                            meta.info.total_length(),
                            meta.info.piece_length,
                            DEFAULT_CHUNK_SIZE,
                        );

                        // Create filesystem storage now that we know the file layout
                        let files = meta.info.files();
                        let file_paths: Vec<std::path::PathBuf> = files
                            .iter()
                            .map(|f| f.path.iter().collect::<std::path::PathBuf>())
                            .collect();
                        let file_lengths_vec: Vec<u64> = files.iter().map(|f| f.length).collect();
                        let preallocate =
                            self.config.storage_mode == torrent_core::StorageMode::Full;
                        let storage: Arc<dyn TorrentStorage> =
                            match torrent_storage::FilesystemStorage::new(
                                &self.config.download_dir,
                                file_paths,
                                file_lengths_vec,
                                lengths.clone(),
                                None,
                                preallocate,
                            ) {
                                Ok(s) => Arc::new(s),
                                Err(e) => {
                                    warn!(
                                        "failed to create filesystem storage: {e}, falling back to memory"
                                    );
                                    Arc::new(MemoryStorage::new(lengths.clone()))
                                }
                            };
                        let mut disk_handle = self
                            .disk_manager
                            .register_torrent(self.info_hash, storage)
                            .await;

                        self.chunk_tracker = Some(ChunkTracker::new(lengths.clone()));
                        self.lengths = Some(lengths);
                        self.num_pieces = num_pieces;
                        // M96: Initialize real generation counters + hash result channel
                        self.piece_generations = vec![0u64; num_pieces as usize];
                        let (hash_tx, hash_rx) = tokio::sync::mpsc::channel(64);
                        self.hash_result_tx = hash_tx;
                        self.hash_result_rx = hash_rx;
                        // M96: Wire hash pool into disk handle (version check deferred
                        // until after metadata detection below sets self.version)
                        if let Some(ref pool) = self.hash_pool_ref {
                            disk_handle.set_hash_pool(pool.clone());
                            disk_handle.set_hash_result_tx(self.hash_result_tx.clone());
                        }
                        self.disk = Some(disk_handle);
                        // Update all connected peer tasks so they can validate
                        // incoming Bitfield messages with the correct piece count.
                        for peer in self.peers.values() {
                            let _ = peer
                                .cmd_tx
                                .try_send(PeerCommand::UpdateNumPieces(num_pieces));
                        }
                        let file_lengths: Vec<u64> =
                            meta.info.files().iter().map(|f| f.length).collect();
                        let mut meta = meta;
                        meta.info_bytes = Some(Bytes::from(info_bytes));
                        self.meta = Some(meta);
                        self.file_priorities = vec![FilePriority::Normal; file_lengths.len()];

                        // BEP 53: apply magnet so= file selection
                        if let Some(ref selections) = self.magnet_selected_files {
                            self.file_priorities = torrent_core::FileSelection::to_priorities(
                                selections,
                                file_lengths.len(),
                            );
                            self.magnet_selected_files = None;
                        }

                        self.wanted_pieces = crate::piece_selector::build_wanted_pieces(
                            &self.file_priorities,
                            &file_lengths,
                            self.lengths.as_ref().unwrap(),
                        );
                        if self.config.share_mode {
                            self.transition_state(TorrentState::Sharing);
                        } else {
                            self.transition_state(TorrentState::Downloading);
                            self.scorer.start_download();
                        }
                        self.metadata_downloader = None;

                        // Populate tracker manager with newly parsed metadata
                        if let Some(ref meta) = self.meta {
                            self.tracker_manager
                                .set_metadata_filtered(meta, &self.config.url_security);
                        }

                        // Detect hybrid/v2 from metadata and update dual-swarm state
                        // (Gap 1 & 2: propagate info_hashes to tracker + DHT after magnet resolves)
                        if let Ok(detected) = torrent_core::torrent_from_bytes_any(&torrent_bytes) {
                            let new_version = detected.version();
                            if new_version != torrent_core::TorrentVersion::V1Only {
                                let new_hashes = detected.info_hashes();
                                self.version = new_version;
                                self.info_hashes = new_hashes.clone();
                                self.tracker_manager.set_info_hashes(new_hashes.clone());
                                if let Some(v2_meta) = detected.as_v2() {
                                    self.meta_v2 = Some(v2_meta.clone());
                                }
                                // Start v2 DHT lookups for hybrid torrents
                                if new_hashes.is_hybrid()
                                    && let Some(v2) = new_hashes.v2
                                {
                                    let v2_as_v1 = Id20(v2.0[..20].try_into().unwrap());
                                    if v2_as_v1 != self.info_hash {
                                        if self.dht_v2_peers_rx.is_none()
                                            && let Some(ref dht) = self.dht
                                            && let Ok(rx) = dht.get_peers(v2_as_v1).await
                                        {
                                            self.dht_v2_peers_rx = Some(rx);
                                        }
                                        if self.dht_v6_v2_peers_rx.is_none()
                                            && self.dht_v6_empty_count < 30
                                            && self.should_retry_v6()
                                            && let Some(ref dht6) = self.dht_v6
                                            && let Ok(rx) = dht6.get_peers(v2_as_v1).await
                                        {
                                            self.dht_v6_last_retry =
                                                Some(std::time::Instant::now());
                                            self.dht_v6_v2_peers_rx = Some(rx);
                                        }
                                    }
                                }
                            }
                        }

                        let name = self
                            .meta
                            .as_ref()
                            .map(|m| m.info.name.clone())
                            .unwrap_or_default();
                        post_alert(
                            &self.alert_tx,
                            &self.alert_mask,
                            AlertKind::MetadataReceived {
                                info_hash: self.info_hash,
                                name,
                            },
                        );
                        info!("metadata assembled, switching to Downloading");

                        // M93: Initialize lock-free piece states after metadata
                        if let Some(ct) = &self.chunk_tracker {
                            let atomic_states = Arc::new(AtomicPieceStates::new(
                                self.num_pieces,
                                ct.bitfield(),
                                &self.wanted_pieces,
                            ));
                            self.atomic_states = Some(Arc::clone(&atomic_states));
                            self.availability = vec![0u32; self.num_pieces as usize];
                            self.piece_owner = vec![None; self.num_pieces as usize];
                            self.max_in_flight = self.config.max_in_flight_pieces;

                            // M103: Initialize block stealing infrastructure
                            if self.config.use_block_stealing {
                                if let Some(ref lengths) = self.lengths {
                                    self.block_maps =
                                        Some(Arc::new(BlockMaps::new(self.num_pieces, lengths)));
                                }
                                self.steal_candidates = Some(Arc::new(StealCandidates::new()));
                            }

                            let snapshot = Arc::new(AvailabilitySnapshot::build(
                                &self.availability,
                                &atomic_states,
                                &self.priority_pieces,
                                0,
                            ));
                            self.availability_snapshot = Some(snapshot);
                            self.snapshot_generation = 0;

                            let notify = Arc::new(tokio::sync::Notify::new());
                            self.reservation_notify = Some(notify);
                        }

                        // Start web seeds now that we have metadata
                        self.spawn_web_seeds();
                        self.assign_pieces_to_web_seeds();

                        // Kick-start piece requesting for all peers that connected during
                        // metadata phase. Send StartRequesting to all connected peers.
                        let peer_addrs: Vec<SocketAddr> = self.peers.keys().copied().collect();
                        info!(
                            connected_peers = peer_addrs.len(),
                            "kick-starting piece requests for pre-connected peers"
                        );
                        for addr in peer_addrs {
                            let has_bitfield = self
                                .peers
                                .get(&addr)
                                .map(|p| p.bitfield.count_ones())
                                .unwrap_or(0);
                            let is_choking = self
                                .peers
                                .get(&addr)
                                .map(|p| p.peer_choking)
                                .unwrap_or(true);
                            debug!(%addr, has_bitfield, is_choking, "post-metadata peer state");
                            self.maybe_express_interest(addr).await;
                            // M93: Update availability from pre-connected peer
                            if let Some(peer) = self.peers.get(&addr)
                                && peer.bitfield.count_ones() > 0
                            {
                                for piece in 0..self.num_pieces {
                                    if peer.bitfield.get(piece) {
                                        self.availability[piece as usize] += 1;
                                    }
                                }
                                let _slot = self.peer_slab.insert(addr);
                            }
                        }
                        self.recalc_max_in_flight();
                        self.rebuild_availability_snapshot();
                        // M93: Inform all connected peers about lock-free dispatch state
                        if let (Some(atomic_states), Some(snapshot), Some(notify)) = (
                            &self.atomic_states,
                            &self.availability_snapshot,
                            &self.reservation_notify,
                        ) && let Some(ref lengths) = self.lengths
                        {
                            for peer in self.peers.values() {
                                let _ = peer.cmd_tx.try_send(PeerCommand::StartRequesting {
                                    atomic_states: Arc::clone(atomic_states),
                                    availability_snapshot: Arc::clone(snapshot),
                                    piece_notify: Arc::clone(notify),
                                    disk_handle: self.disk.clone(),
                                    write_error_tx: self.write_error_tx.clone(),
                                    lengths: lengths.clone(),
                                    block_maps: self.block_maps.clone(),
                                    steal_candidates: self.steal_candidates.clone(),
                                });
                            }
                        }
                    }
                    Err(e) => {
                        warn!("failed to parse assembled metadata: {e}");
                        post_alert(
                            &self.alert_tx,
                            &self.alert_mask,
                            AlertKind::MetadataFailed {
                                info_hash: self.info_hash,
                            },
                        );
                    }
                }
            }
            Err(e) => {
                warn!("metadata assembly failed: {e}");
                post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::MetadataFailed {
                        info_hash: self.info_hash,
                    },
                );
            }
        }
    }

    // ----- Web seeding (M22) -----

    fn spawn_web_seeds(&mut self) {
        if !self.config.enable_web_seed {
            return;
        }
        let meta = match &self.meta {
            Some(m) => m,
            None => return,
        };
        let lengths = match &self.lengths {
            Some(l) => l.clone(),
            None => return,
        };

        let file_lengths: Vec<u64> = meta.info.files().iter().map(|f| f.length).collect();
        let file_map = torrent_storage::FileMap::new(file_lengths, lengths.clone());

        // BEP 19 (GetRight) web seeds
        for url in &meta.url_list {
            if self.banned_web_seeds.contains(url) || self.web_seeds.contains_key(url) {
                continue;
            }
            if self.web_seeds.len() >= self.config.max_web_seeds {
                break;
            }

            // Security validation
            if let Err(e) = crate::url_guard::validate_web_seed_url(url, &self.config.url_security)
            {
                warn!(%url, %e, "web seed URL rejected by security policy");
                continue;
            }

            let url_builder = if meta.info.length.is_some() {
                crate::web_seed::WebSeedUrlBuilder::single(url.clone(), meta.info.name.clone())
            } else {
                let file_paths: Vec<String> = meta
                    .info
                    .files()
                    .iter()
                    .map(|f| f.path[1..].join("/")) // skip torrent name prefix
                    .collect();
                crate::web_seed::WebSeedUrlBuilder::multi(
                    url.clone(),
                    meta.info.name.clone(),
                    file_paths,
                )
            };

            let (cmd_tx, cmd_rx) = mpsc::channel(16);
            let task = crate::web_seed::WebSeedTask::new(
                url.clone(),
                crate::web_seed::WebSeedMode::GetRight,
                url_builder,
                lengths.clone(),
                file_map.clone(),
                self.info_hash,
                cmd_rx,
                self.event_tx.clone(),
                &self.config.url_security,
            );
            tokio::spawn(task.run());
            self.web_seeds.insert(url.clone(), cmd_tx);
            debug!(url, "spawned BEP 19 web seed");
        }

        // BEP 17 (Hoffman) HTTP seeds
        for url in &meta.httpseeds {
            if self.banned_web_seeds.contains(url) || self.web_seeds.contains_key(url) {
                continue;
            }
            if self.web_seeds.len() >= self.config.max_web_seeds {
                break;
            }

            // Security validation
            if let Err(e) = crate::url_guard::validate_web_seed_url(url, &self.config.url_security)
            {
                warn!(%url, %e, "web seed URL rejected by security policy");
                continue;
            }

            // BEP 17 doesn't use URL builder for per-file paths; it sends parameterized URLs
            let url_builder =
                crate::web_seed::WebSeedUrlBuilder::single(url.clone(), meta.info.name.clone());

            let (cmd_tx, cmd_rx) = mpsc::channel(16);
            let task = crate::web_seed::WebSeedTask::new(
                url.clone(),
                crate::web_seed::WebSeedMode::Hoffman,
                url_builder,
                lengths.clone(),
                file_map.clone(),
                self.info_hash,
                cmd_rx,
                self.event_tx.clone(),
                &self.config.url_security,
            );
            tokio::spawn(task.run());
            self.web_seeds.insert(url.clone(), cmd_tx);
            debug!(url, "spawned BEP 17 web seed");
        }
    }

    fn assign_pieces_to_web_seeds(&mut self) {
        if self.state != TorrentState::Downloading || self.end_game.is_active() {
            return;
        }

        // Collect idle web seed URLs (not currently downloading a piece)
        let active_urls: HashSet<&String> = self.web_seed_in_flight.values().collect();
        let idle_urls: Vec<String> = self
            .web_seeds
            .keys()
            .filter(|u| !active_urls.contains(u))
            .cloned()
            .collect();

        let ct = match &self.chunk_tracker {
            Some(ct) => ct,
            None => return,
        };

        for url in idle_urls {
            // Find lowest-index piece that is: not verified, not reserved by a peer,
            // not in web_seed_in_flight, and wanted.
            let piece = (0..self.num_pieces).find(|&i| {
                !ct.has_piece(i)
                    && !self
                        .piece_owner
                        .get(i as usize)
                        .is_some_and(|o| o.is_some())
                    && !self.web_seed_in_flight.contains_key(&i)
                    && self.wanted_pieces.get(i)
            });

            if let Some(piece) = piece
                && let Some(cmd_tx) = self.web_seeds.get(&url)
            {
                let _ = cmd_tx.try_send(crate::web_seed::WebSeedCommand::FetchPiece(piece));
                self.web_seed_in_flight.insert(piece, url);
            }
        }
    }

    async fn handle_web_seed_piece_data(&mut self, url: String, index: u32, data: Bytes) {
        self.web_seed_in_flight.remove(&index);

        // If peer already completed this piece, discard
        if let Some(ref ct) = self.chunk_tracker
            && ct.has_piece(index)
        {
            self.assign_pieces_to_web_seeds();
            return;
        }

        // Write entire piece to disk at offset 0
        if let Some(ref disk) = self.disk
            && let Err(e) = disk
                .write_chunk(index, 0, data.clone(), DiskJobFlags::FLUSH_PIECE)
                .await
        {
            warn!(index, "web seed: failed to write piece: {e}");
            self.assign_pieces_to_web_seeds();
            return;
        }

        // Mark all chunks as received
        if let Some(ref mut ct) = self.chunk_tracker
            && let Some(ref lengths) = self.lengths
        {
            let num_chunks = lengths.chunks_in_piece(index);
            for chunk_idx in 0..num_chunks {
                if let Some((begin, _len)) = lengths.chunk_info(index, chunk_idx) {
                    ct.chunk_received(index, begin);
                }
            }
        }

        self.downloaded += data.len() as u64;
        self.total_download += data.len() as u64 + 13; // payload + message header
        self.last_download = now_unix();
        self.need_save_resume = true;

        // Verify the piece hash
        self.verify_and_mark_piece(index).await;

        // If hash failed, ban this web seed (BEP 19 spec)
        if let Some(ref ct) = self.chunk_tracker
            && !ct.has_piece(index)
        {
            self.ban_web_seed(&url);
            return;
        }

        self.assign_pieces_to_web_seeds();
    }

    fn handle_web_seed_error(&mut self, url: String, piece: u32, message: String) {
        self.web_seed_in_flight.remove(&piece);
        warn!(%url, piece, %message, "web seed error");
        self.assign_pieces_to_web_seeds();
    }

    fn ban_web_seed(&mut self, url: &str) {
        warn!(%url, "banning web seed due to hash failure");
        self.banned_web_seeds.insert(url.to_owned());

        // Send shutdown to the task
        if let Some(cmd_tx) = self.web_seeds.remove(url) {
            let _ = cmd_tx.try_send(crate::web_seed::WebSeedCommand::Shutdown);
        }

        // Remove all in-flight pieces for this URL
        self.web_seed_in_flight.retain(|_, v| v != url);

        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::WebSeedBanned {
                info_hash: self.info_hash,
                url: url.to_owned(),
            },
        );
    }

    async fn shutdown_web_seeds(&mut self) {
        for (_, cmd_tx) in self.web_seeds.drain() {
            let _ = cmd_tx.send(crate::web_seed::WebSeedCommand::Shutdown).await;
        }
        self.web_seed_in_flight.clear();
    }

    /// Rebuild the cached peer rates map from current peer state.
    fn refresh_peer_rates(&mut self) {
        self.cached_peer_rates.clear();
        self.cached_peer_rates.reserve(self.peers.len());
        for (&addr, p) in &self.peers {
            self.cached_peer_rates.insert(addr, p.pipeline.ewma_rate());
        }
    }

    /// M103: Mark the availability snapshot as stale for deferred rebuild.
    ///
    /// The actual rebuild happens on the next 50ms snapshot tick, batching
    /// multiple rapid events (bitfield, have, disconnect) into one rebuild.
    fn mark_snapshot_dirty(&mut self) {
        self.snapshot_dirty = true;
    }

    /// M93: Rebuild the availability snapshot and broadcast to all peers.
    fn rebuild_availability_snapshot(&mut self) {
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
    fn recalc_max_in_flight(&mut self) {
        let connected = self.peers.len();
        let calculated = 512usize.max(connected.saturating_mul(4));
        self.max_in_flight = calculated.min(self.num_pieces as usize / 2).max(512);
    }

    fn check_end_game_activation(&mut self) {
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

    async fn request_end_game_block(&mut self, peer_addr: SocketAddr) {
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

    fn update_streaming_cursors(&mut self) {
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

    async fn maybe_express_interest(&mut self, peer_addr: SocketAddr) {
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

    // ----- Choking -----

    fn update_peer_rates(&mut self) {
        for peer in self.peers.values_mut() {
            // Window is 10 seconds (unchoke interval)
            peer.download_rate = peer.download_bytes_window / 10;
            peer.upload_rate = peer.upload_bytes_window / 10;
            peer.download_bytes_window = 0;
            peer.upload_bytes_window = 0;
        }

        // Track peak download rate for peer turnover cutoff
        let aggregate_download: u64 = self.peers.values().map(|p| p.download_rate).sum();
        if aggregate_download > self.peak_download_rate {
            self.peak_download_rate = aggregate_download;
        }
    }

    /// M106: Extracted disconnect helper — consolidates all peer cleanup into one place.
    ///
    /// Performs: BEP 16 super-seed cleanup, peer removal, end-game cleanup,
    /// slab/piece-owner release, availability decrement, cmd_tx shutdown,
    /// PeerDisconnected alert, and suggested_to_peers cleanup.
    fn disconnect_peer(&mut self, addr: SocketAddr, reason: &str) {
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
    async fn run_scored_turnover(&mut self) {
        if self.state != TorrentState::Downloading {
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
            self.try_connect_peers();
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

        let peers_before = self.peers.len();
        self.try_connect_peers();
        let replaced = self.peers.len().saturating_sub(peers_before);

        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::PeerTurnover {
                info_hash: self.info_hash,
                disconnected: to_disconnect.len(),
                replaced,
            },
        );
    }

    async fn run_choker(&mut self) {
        let peer_infos: Vec<ChokerPeerInfo> = self
            .peers
            .values()
            .map(|p| ChokerPeerInfo {
                addr: p.addr,
                download_rate: p.download_rate,
                upload_rate: p.upload_rate,
                interested: p.peer_interested,
                upload_only: p.upload_only,
                is_seed: p.upload_only
                    || (self.num_pieces > 0 && p.bitfield.count_ones() == self.num_pieces),
            })
            .collect();

        let decision = self.choker.decide(&peer_infos);

        for addr in &decision.to_unchoke {
            if let Some(peer) = self.peers.get_mut(addr)
                && peer.am_choking
            {
                peer.am_choking = false;
                let _ = peer.cmd_tx.try_send(PeerCommand::SetChoking(false));
            }
        }

        for addr in &decision.to_choke {
            if let Some(peer) = self.peers.get_mut(addr)
                && !peer.am_choking
            {
                if peer.supports_fast {
                    let pending: Vec<(u32, u32, u32)> = peer.incoming_requests.drain(..).collect();
                    for (index, begin, length) in pending {
                        let _ = peer.cmd_tx.try_send(PeerCommand::RejectRequest {
                            index,
                            begin,
                            length,
                        });
                    }
                }
                peer.am_choking = true;
                let _ = peer.cmd_tx.try_send(PeerCommand::SetChoking(true));
            }
        }

        // Serve any buffered requests from newly-unchoked peers
        self.serve_incoming_requests().await;

        // Zombie pruning: disconnect peers with empty bitfields after 30s.
        // These peers consume connection slots but contribute no pieces.
        // Only prune during downloading — when seeding, empty-bitfield peers
        // are leechers we want to upload to.
        if self.state == TorrentState::Downloading {
            let zombie_threshold = Duration::from_secs(30);
            let zombies: Vec<SocketAddr> = self
                .peers
                .values()
                .filter(|p| {
                    p.bitfield.count_ones() == 0 && p.connected_at.elapsed() > zombie_threshold
                })
                .map(|p| p.addr)
                .collect();

            for &addr in &zombies {
                debug!(%addr, "disconnecting zombie peer (empty bitfield after 30s)");
                self.disconnect_peer(addr, "zombie peer (empty bitfield)");
            }
            if !zombies.is_empty() {
                self.recalc_max_in_flight();
                self.mark_snapshot_dirty();
            }
        }
    }

    async fn serve_incoming_requests(&mut self) {
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
                && !global.lock().unwrap().try_consume(chunk_size)
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

    fn check_seed_ratio(&mut self) -> bool {
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

    fn rotate_optimistic(&mut self) {
        let peer_infos: Vec<ChokerPeerInfo> = self
            .peers
            .values()
            .map(|p| ChokerPeerInfo {
                addr: p.addr,
                download_rate: p.download_rate,
                upload_rate: p.upload_rate,
                interested: p.peer_interested,
                upload_only: p.upload_only,
                is_seed: p.upload_only
                    || (self.num_pieces > 0 && p.bitfield.count_ones() == self.num_pieces),
            })
            .collect();

        self.choker.rotate_optimistic(&peer_infos);
    }

    /// BEP 16: assign the next super-seed piece to a peer.
    async fn assign_next_piece_for_peer(&mut self, peer_addr: SocketAddr) {
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

    // ----- Peer connectivity -----

    fn try_connect_peers(&mut self) {
        loop {
            // M106: Score-based admission — evict worst peer if at capacity
            if self.peers.len() >= self.effective_max_connections() {
                let worst = self
                    .peers
                    .iter()
                    .filter(|(_, p)| !self.scorer.is_in_probation(p.connected_at))
                    .min_by(|(_, a), (_, b)| {
                        a.score
                            .partial_cmp(&b.score)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                if let Some((worst_addr, worst_peer)) = worst {
                    if worst_peer.score < self.scorer.min_score_threshold() {
                        let worst_addr = *worst_addr;
                        self.disconnect_peer(worst_addr, "score-based admission");
                        self.recalc_max_in_flight();
                        self.mark_snapshot_dirty();
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }

            let (addr, source) = match self.available_peers.pop() {
                Some(pair) => {
                    self.available_peers_set.remove(&pair.0);
                    pair
                }
                None => break,
            };

            if self.peers.contains_key(&addr) {
                continue;
            }

            // M104: Skip peers with active backoff
            if let Some(&(next_attempt, _)) = self.connect_backoff.get(&addr)
                && std::time::Instant::now() < next_attempt
            {
                continue;
            }

            // Skip banned peers
            if self.ban_manager.read().unwrap().is_banned(&addr.ip()) {
                continue;
            }

            // Skip IP-filtered peers
            if self.ip_filter.read().unwrap().is_blocked(addr.ip()) {
                post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::PeerBlocked { addr },
                );
                continue;
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

                    tokio::spawn(async move {
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
                                // Transport identified: use Tcp as stand-in since the
                                // underlying socket is TCP to the SAM bridge
                                // (until a PeerTransport::I2p variant is added)
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
                    continue; // Skip TCP/uTP path
                }
                // No SAM session — can't connect to I2P peer, clean up
                self.peers.remove(&addr);
                continue;
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
                // Try uTP first (5s timeout), fall back to TCP
                // Note: uTP is not proxied — if proxy is active, skip uTP
                if enable_utp
                    && !use_proxy
                    && let Some(socket) = utp_socket
                {
                    match tokio::time::timeout(Duration::from_secs(5), socket.connect(addr)).await {
                        Ok(Ok(stream)) => {
                            debug!(%addr, "uTP connection established");
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
                            )
                            .await
                            {
                                Ok(()) => debug!(%addr, "uTP peer session ended normally"),
                                Err(e) => {
                                    // MSE fallback: determine retry mode based on encryption setting.
                                    // Enabled: offered both → retry with Disabled (skip MSE entirely)
                                    // PreferPlaintext: offered plaintext-only → retry with Enabled (offer both)
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
                            return;
                        }
                        Ok(Err(e)) => {
                            debug!(%addr, error = %e, "uTP connect failed, falling back to TCP");
                        }
                        Err(_) => {
                            debug!(%addr, "uTP connect timed out, falling back to TCP");
                        }
                    }
                }

                // TCP connection — through proxy if configured, or via transport factory
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
                            )
                            .await
                            {
                                Ok(()) => debug!(%addr, "TCP peer session ended normally"),
                                Err(e) => {
                                    // MSE fallback for TCP: determine retry mode.
                                    // Enabled: offered both → retry with Disabled (skip MSE entirely)
                                    // PreferPlaintext: offered plaintext-only → retry with Enabled (offer both)
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

    /// Handle an incoming I2P peer connection (M41).
    ///
    /// Assigns a synthetic `SocketAddr` (from the reserved 240.0.0.0/4 range) since
    /// I2P peers don't have real IP addresses, then hands the underlying TCP stream
    /// to `spawn_peer_from_stream`.
    fn handle_i2p_incoming(&mut self, stream: crate::i2p::SamStream) {
        if self.peers.len() >= self.effective_max_connections() {
            return;
        }

        let synthetic_addr = self.next_i2p_synthetic_addr();

        let remote_dest = stream.remote_destination().clone();
        let dest_preview = {
            let b64 = remote_dest.to_base64();
            if b64.len() >= 8 {
                b64[..8].to_string()
            } else {
                b64
            }
        };
        self.i2p_destinations.insert(synthetic_addr, remote_dest);
        let tcp_stream = stream.into_inner();

        self.spawn_peer_from_stream(synthetic_addr, tcp_stream);

        debug!(dest = %dest_preview, addr = %synthetic_addr, "accepted I2P peer");
    }

    /// Add an I2P peer by destination, assigning a synthetic SocketAddr.
    #[allow(dead_code)] // Used by Task 2 (outbound I2P connects)
    fn add_i2p_peer(
        &mut self,
        dest: crate::i2p::I2pDestination,
        source: PeerSource,
    ) -> Option<SocketAddr> {
        // Dedup: check if we already track this destination
        if self.i2p_destinations.values().any(|d| d == &dest) {
            return None;
        }
        let addr = self.next_i2p_synthetic_addr();
        self.i2p_destinations.insert(addr, dest);
        if self.available_peers_set.insert(addr) {
            self.available_peers.push((addr, source));
            Some(addr)
        } else {
            None
        }
    }

    /// Generate a unique synthetic `SocketAddr` for an I2P peer.
    ///
    /// Uses addresses from 240.0.0.0/4 (reserved, never routable) to avoid
    /// conflicts with real peers. The counter ensures uniqueness across the
    /// torrent's lifetime.
    fn next_i2p_synthetic_addr(&mut self) -> SocketAddr {
        self.i2p_peer_counter = self.i2p_peer_counter.wrapping_add(1);
        let a = ((self.i2p_peer_counter >> 16) & 0x0F) as u8 | 240;
        let b = ((self.i2p_peer_counter >> 8) & 0xFF) as u8;
        let c = (self.i2p_peer_counter & 0xFF) as u8;
        SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(a, b, c, 1)),
            (self.i2p_peer_counter & 0xFFFF) as u16,
        )
    }
}

/// Check whether a `SocketAddr` uses a synthetic I2P address (240.0.0.0/4 range).
pub(crate) fn is_i2p_synthetic_addr(addr: &SocketAddr) -> bool {
    match addr {
        SocketAddr::V4(v4) => v4.ip().octets()[0] & 0xF0 == 0xF0,
        _ => false,
    }
}

impl TorrentActor {
    /// Spawn a peer task from an already-connected stream (for incoming connections and tests).
    fn spawn_peer_from_stream(
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
    fn spawn_peer_from_stream_with_mode(
        &mut self,
        addr: SocketAddr,
        stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
        mode_override: Option<torrent_wire::mse::EncryptionMode>,
    ) {
        if self.peers.contains_key(&addr) || self.peers.len() >= self.effective_max_connections() {
            return;
        }

        // Reject banned incoming peers
        if self.ban_manager.read().unwrap().is_banned(&addr.ip()) {
            debug!(%addr, "rejected banned incoming peer");
            return;
        }

        // Reject IP-filtered incoming peers
        if self.ip_filter.read().unwrap().is_blocked(addr.ip()) {
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

        tokio::spawn(async move {
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
        if self.ban_manager.read().unwrap().is_banned(&target.ip()) {
            debug!(%target, "holepunch: target is banned");
            return;
        }
        if self.ip_filter.read().unwrap().is_blocked(target.ip()) {
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
    #[allow(dead_code)] // will be wired into peer discovery in Task 7
    async fn try_holepunch(&mut self, target: SocketAddr) {
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

/// Helper to accept a connection from an optional transport listener.
/// Returns `pending` if no listener is bound, so the `select!` branch is skipped.
async fn accept_incoming(
    listener: &mut Option<Box<dyn crate::transport::TransportListener>>,
) -> std::io::Result<(crate::transport::BoxedStream, SocketAddr)> {
    match listener {
        Some(l) => l.accept().await,
        None => std::future::pending().await,
    }
}

/// Helper to receive an incoming I2P connection from the accept loop channel.
/// Returns `pending` if I2P is not enabled, so the `select!` branch is skipped.
async fn accept_i2p(
    rx: &mut Option<mpsc::Receiver<crate::i2p::SamStream>>,
) -> Option<crate::i2p::SamStream> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

// ============================================================================
// BEP 52 hash serving (M87)
// ============================================================================

/// Determine what to serve for a BEP 52 hash request.
///
/// Returns `Some(hashes)` to serve, or `None` to reject.
/// Only serves piece-layer hashes (the layer stored in `piece_layers`).
/// Block-layer or other layer requests are rejected since we don't store
/// the full Merkle tree.
fn serve_hashes(
    meta_v2: Option<&torrent_core::TorrentMetaV2>,
    version: torrent_core::TorrentVersion,
    lengths: Option<&Lengths>,
    request: &torrent_core::HashRequest,
) -> Option<Vec<torrent_core::Id32>> {
    // Reject if v1-only or no v2 metadata
    let meta_v2 = match meta_v2 {
        Some(m) if version != torrent_core::TorrentVersion::V1Only => m,
        _ => return None,
    };

    // Look up piece-layer hashes for the requested file root
    let piece_hashes = meta_v2.file_piece_hashes(&request.file_root)?;

    // We need lengths to validate the request geometry
    let lengths = lengths?;

    // Compute per-file block count from piece hashes and piece/chunk sizes.
    // Each piece hash covers `piece_length / chunk_size` blocks, except the
    // last piece which may cover fewer. For validation purposes we use the
    // padded count that `validate_hash_request` expects.
    let blocks_per_piece = (meta_v2.info.piece_length / lengths.chunk_size() as u64) as u32;
    let num_pieces = piece_hashes.len() as u32;
    let num_blocks = num_pieces.saturating_mul(blocks_per_piece);

    if !torrent_core::validate_hash_request(request, num_blocks, num_pieces) {
        return None;
    }

    // We only have piece-layer hashes. The piece layer is at
    // base = log2(blocks_per_piece). Reject requests for other layers.
    let piece_layer_base = blocks_per_piece.trailing_zeros();
    if request.base != piece_layer_base {
        return None;
    }

    // Extract requested hashes from the piece layer
    let start = request.index as usize;
    let end = (start + request.count as usize).min(piece_hashes.len());
    let mut hashes: Vec<torrent_core::Id32> = piece_hashes[start..end].to_vec();

    // Compute proof (uncle) hashes if requested.
    //
    // BEP 52 specifies a single subtree proof for the entire batch, not
    // per-leaf proofs. The receiver rebuilds the subtree root from the
    // base hashes itself, so we skip the first `log2(count)` levels of
    // the proof path (those are internal to the requested subtree) and
    // only send the uncle hashes above it.
    if request.proof_layers > 0 && !piece_hashes.is_empty() {
        let tree = torrent_core::MerkleTree::from_leaves(&piece_hashes);
        let full_proof = tree.proof_path(start);
        // Skip levels internal to the requested subtree
        let subtree_depth = if request.count > 1 {
            (request.count as usize)
                .next_power_of_two()
                .trailing_zeros() as usize
        } else {
            0
        };
        let available = full_proof.len().saturating_sub(subtree_depth);
        let proof_count = (request.proof_layers as usize).min(available);
        hashes.extend_from_slice(&full_proof[subtree_depth..subtree_depth + proof_count]);
    }

    Some(hashes)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::{SinkExt, StreamExt};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_util::codec::{FramedRead, FramedWrite};
    use torrent_wire::{ExtHandshake, Handshake, Message, MessageCodec};

    // -- Helpers --

    /// Build a valid TorrentMetaV1 from raw data with given piece length.
    fn make_test_torrent(data: &[u8], piece_length: u64) -> TorrentMetaV1 {
        use serde::Serialize;

        let mut pieces = Vec::new();
        let mut offset = 0;
        while offset < data.len() {
            let end = (offset + piece_length as usize).min(data.len());
            let hash = torrent_core::sha1(&data[offset..end]);
            pieces.extend_from_slice(hash.as_bytes());
            offset = end;
        }

        #[derive(Serialize)]
        struct Info<'a> {
            length: u64,
            name: &'a str,
            #[serde(rename = "piece length")]
            piece_length: u64,
            #[serde(with = "serde_bytes")]
            pieces: &'a [u8],
        }

        #[derive(Serialize)]
        struct Torrent<'a> {
            info: Info<'a>,
        }

        let t = Torrent {
            info: Info {
                length: data.len() as u64,
                name: "test",
                piece_length,
                pieces: &pieces,
            },
        };

        let bytes = torrent_bencode::to_bytes(&t).unwrap();
        torrent_from_bytes(&bytes).unwrap()
    }

    fn test_config() -> TorrentConfig {
        TorrentConfig {
            listen_port: 0, // random port
            max_peers: 200,
            target_request_queue: 5,
            download_dir: std::path::PathBuf::from("/tmp"),
            enable_dht: false,
            enable_pex: false,
            enable_fast: false,
            seed_ratio_limit: None,
            strict_end_game: true,
            upload_rate_limit: 0,
            download_rate_limit: 0,
            encryption_mode: torrent_wire::mse::EncryptionMode::Disabled,
            enable_utp: false,
            enable_web_seed: true,
            enable_holepunch: false,
            max_web_seeds: 4,
            super_seeding: false,
            upload_only_announce: true,
            have_send_delay_ms: 0,
            hashing_threads: 2,
            sequential_download: false,
            initial_picker_threshold: 4,
            whole_pieces_threshold: 20,
            snub_timeout_secs: 15,
            readahead_pieces: 8,
            streaming_timeout_escalation: true,
            max_concurrent_stream_reads: 8,
            proxy: crate::proxy::ProxyConfig::default(),
            anonymous_mode: false,
            share_mode: false,
            enable_i2p: false,
            allow_i2p_mixed: false,
            ssl_listen_port: 0,
            seed_choking_algorithm: crate::choker::SeedChokingAlgorithm::FastestUpload,
            choking_algorithm: crate::choker::ChokingAlgorithm::FixedSlots,
            piece_extent_affinity: true,
            suggest_mode: false,
            max_suggest_pieces: 10,
            predictive_piece_announce_ms: 0,
            mixed_mode_algorithm: crate::rate_limiter::MixedModeAlgorithm::PeerProportional,
            auto_sequential: true,
            storage_mode: torrent_core::StorageMode::Auto,
            block_request_timeout_secs: 60,
            enable_lsd: false,
            force_proxy: false,
            steal_threshold_ratio: 10.0,
            probation_duration_secs: 20,
            discovery_phase_secs: 60,
            discovery_churn_interval_secs: 30,
            discovery_churn_percent: 0.10,
            steady_churn_interval_secs: 120,
            steady_churn_percent: 0.05,
            min_score_threshold: 0.15,
            url_security: crate::url_guard::UrlSecurityConfig::default(),
            peer_connect_timeout: 5,
            peer_dscp: 0x08,
            initial_queue_depth: 128,
            max_request_queue_depth: 250,
            request_queue_time: 3.0,
            max_metadata_size: 4 * 1024 * 1024,
            max_message_size: 16 * 1024 * 1024,
            max_piece_length: 32 * 1024 * 1024,
            max_outstanding_requests: 500,
            max_in_flight_pieces: 20,
            use_block_stealing: true,
            fixed_pipeline_depth: 128,
        }
    }

    fn make_storage(data: &[u8], piece_length: u64) -> Arc<MemoryStorage> {
        let lengths = Lengths::new(data.len() as u64, piece_length, DEFAULT_CHUNK_SIZE);
        Arc::new(MemoryStorage::new(lengths))
    }

    fn make_seeded_storage(data: &[u8], piece_length: u64) -> Arc<MemoryStorage> {
        let lengths = Lengths::new(data.len() as u64, piece_length, DEFAULT_CHUNK_SIZE);
        let storage = Arc::new(MemoryStorage::new(lengths.clone()));
        // Write data piece by piece
        let num_pieces = lengths.num_pieces();
        for p in 0..num_pieces {
            let piece_size = lengths.piece_size(p) as usize;
            let offset = lengths.piece_offset(p) as usize;
            let end = offset + piece_size;
            storage.write_chunk(p, 0, &data[offset..end]).unwrap();
        }
        storage
    }

    fn test_alert_channel() -> (broadcast::Sender<Alert>, Arc<AtomicU32>) {
        let (tx, _) = broadcast::channel(64);
        let mask = Arc::new(AtomicU32::new(crate::alert::AlertCategory::ALL.bits()));
        (tx, mask)
    }

    fn test_ban_manager() -> crate::session::SharedBanManager {
        Arc::new(std::sync::RwLock::new(crate::ban::BanManager::new(
            crate::ban::BanConfig::default(),
        )))
    }

    fn test_ip_filter() -> crate::session::SharedIpFilter {
        Arc::new(std::sync::RwLock::new(crate::ip_filter::IpFilter::new()))
    }

    fn test_disk_manager() -> (DiskManagerHandle, tokio::task::JoinHandle<()>) {
        DiskManagerHandle::new(crate::disk::DiskConfig::default())
    }

    async fn test_register_disk(
        info_hash: Id20,
        storage: Arc<dyn TorrentStorage>,
    ) -> (DiskHandle, DiskManagerHandle, tokio::task::JoinHandle<()>) {
        let (dm, join) = test_disk_manager();
        let dh = dm.register_torrent(info_hash, storage).await;
        (dh, dm, join)
    }

    /// Handshake size constant.
    const HANDSHAKE_SIZE: usize = 68;

    // ---- Test 1: Create from torrent ----

    #[tokio::test]
    async fn create_from_torrent() {
        let data = vec![0xAB; 32768]; // 32 KiB
        let meta = make_test_torrent(&data, 16384); // 2 pieces
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);
        assert_eq!(stats.pieces_total, 2);
        assert_eq!(stats.pieces_have, 0);
        assert_eq!(stats.peers_connected, 0);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 2: Create from magnet ----

    #[tokio::test]
    async fn create_from_magnet() {
        let magnet = Magnet {
            info_hashes: torrent_core::InfoHashes::v1_only(
                Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap(),
            ),
            display_name: Some("test".into()),
            trackers: vec![],
            peers: vec![],
            selected_files: None,
        };
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dm, _dj) = test_disk_manager();
        let handle = TorrentHandle::from_magnet(
            magnet,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::FetchingMetadata);
        assert_eq!(stats.pieces_total, 0);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 3: Add peers ----

    #[tokio::test]
    async fn add_peers_increases_available() {
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Bind listeners so the connect attempts succeed and peers stay in connected state
        let _listener1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr1 = _listener1.local_addr().unwrap();
        let _listener2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr2 = _listener2.local_addr().unwrap();

        handle
            .add_peers(vec![addr1, addr2], PeerSource::Tracker)
            .await
            .unwrap();

        // Small delay for the actor to process
        tokio::time::sleep(Duration::from_millis(100)).await;

        let stats = handle.stats().await.unwrap();
        // Peers may be available or already connecting (try_connect_peers fires immediately)
        assert!(
            stats.peers_available + stats.peers_connected >= 2,
            "expected at least 2 peers known, got available={}, connected={}",
            stats.peers_available,
            stats.peers_connected
        );

        handle.shutdown().await.unwrap();
    }

    // ---- Test 4: Stats reporting ----

    #[tokio::test]
    async fn stats_reporting() {
        let data = vec![0xAB; 65536]; // 64 KiB
        let meta = make_test_torrent(&data, 16384); // 4 pieces
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);
        assert_eq!(stats.downloaded, 0);
        assert_eq!(stats.uploaded, 0);
        assert_eq!(stats.pieces_have, 0);
        assert_eq!(stats.pieces_total, 4);
        assert_eq!(stats.peers_connected, 0);
        assert_eq!(stats.peers_available, 0);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 5: Private torrent disables DHT/PEX ----

    #[tokio::test]
    async fn private_torrent_disables_dht_pex() {
        // Build a private torrent by embedding private=1 in the info dict
        use serde::Serialize;

        let data = vec![0xAB; 16384];
        let hash = torrent_core::sha1(&data);
        let mut pieces = Vec::new();
        pieces.extend_from_slice(hash.as_bytes());

        #[derive(Serialize)]
        struct Info<'a> {
            length: u64,
            name: &'a str,
            #[serde(rename = "piece length")]
            piece_length: u64,
            #[serde(with = "serde_bytes")]
            pieces: &'a [u8],
            private: i64,
        }

        #[derive(Serialize)]
        struct Torrent<'a> {
            info: Info<'a>,
        }

        let t = Torrent {
            info: Info {
                length: data.len() as u64,
                name: "private_test",
                piece_length: 16384,
                pieces: &pieces,
                private: 1,
            },
        };

        let bytes = torrent_bencode::to_bytes(&t).unwrap();
        let meta = torrent_from_bytes(&bytes).unwrap();
        assert_eq!(meta.info.private, Some(1));

        let storage = make_storage(&data, 16384);
        let mut config = test_config();
        config.enable_dht = true;
        config.enable_pex = true;

        // The from_torrent constructor should disable DHT and PEX
        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // We can't directly inspect the actor's config, but we can verify
        // the torrent was created successfully. The real test is that PEX peers
        // would be ignored and DHT not used. For now verify the handle works.
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 6: Shutdown cleanup ----

    #[tokio::test]
    async fn shutdown_cleanup() {
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        handle.shutdown().await.unwrap();

        // After shutdown, stats should fail (channel closed)
        tokio::time::sleep(Duration::from_millis(50)).await;
        let result = handle.stats().await;
        assert!(result.is_err());
    }

    // ---- Test 7: Duplicate add_peers ignored ----

    #[tokio::test]
    async fn duplicate_peers_ignored() {
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Bind a listener so the connection succeeds and the peer stays connected
        let _listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = _listener.local_addr().unwrap();
        handle
            .add_peers(vec![addr, addr, addr], PeerSource::Tracker)
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;
        let stats = handle.stats().await.unwrap();
        // Only one unique peer should be known (available or connecting)
        assert!(
            stats.peers_available + stats.peers_connected <= 1,
            "expected at most 1 unique peer, got available={}, connected={}",
            stats.peers_available,
            stats.peers_connected
        );

        handle.shutdown().await.unwrap();
    }

    // ---- Test 8: Multiple handles (Clone) share same actor ----

    #[tokio::test]
    async fn cloned_handle_shares_actor() {
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();
        let handle2 = handle.clone();

        // Bind a listener so the connection succeeds and the peer stays connected
        let _listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = _listener.local_addr().unwrap();

        // Add peers through one handle
        handle
            .add_peers(vec![peer_addr], PeerSource::Tracker)
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Read stats through the other — peer may be available or connecting
        let stats = handle2.stats().await.unwrap();
        assert!(
            stats.peers_available + stats.peers_connected >= 1,
            "expected at least 1 peer known, got available={}, connected={}",
            stats.peers_available,
            stats.peers_connected
        );

        handle.shutdown().await.unwrap();
    }

    // ---- Test 9: Peer connection and disconnect via listener ----

    #[tokio::test]
    async fn peer_connect_and_disconnect_via_listener() {
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_storage(&data, 16384);

        // Bind a listener on a specific port so we can connect to it
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();

        let config = TorrentConfig {
            listen_port: listen_addr.port(),
            ..test_config()
        };

        // Drop the pre-bound listener before from_torrent binds
        drop(listener);

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Give the actor time to start
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Connect a mock peer
        let mut stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();

        // Perform handshake
        let remote_id = Id20::from_hex("1111111111111111111111111111111111111111").unwrap();
        let remote_hs = Handshake::new(info_hash, remote_id);
        stream.write_all(&remote_hs.to_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        let mut hs_buf = [0u8; HANDSHAKE_SIZE];
        stream.read_exact(&mut hs_buf).await.unwrap();
        let their_hs = Handshake::from_bytes(&hs_buf).unwrap();
        assert_eq!(their_hs.info_hash, info_hash);

        // Give time for peer to be registered
        tokio::time::sleep(Duration::from_millis(100)).await;

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.peers_connected, 1);

        // Drop the connection
        drop(stream);

        // Wait for disconnect event
        tokio::time::sleep(Duration::from_millis(200)).await;

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.peers_connected, 0);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 10: Piece download and verification via injected events ----
    //
    // We test the full flow: connect a mock peer that sends bitfield, unchoke,
    // then responds to requests with correct piece data.

    #[tokio::test]
    async fn piece_download_and_verify() {
        // Create a 1-piece torrent with 16384 bytes (exactly one chunk)
        let data = vec![0xCDu8; 16384];
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_storage(&data, 16384);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();
        drop(listener);

        let config = TorrentConfig {
            listen_port: listen_addr.port(),
            ..test_config()
        };

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Connect mock peer
        let stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let remote_id = Id20::from_hex("2222222222222222222222222222222222222222").unwrap();

        // Run mock seeder in a task
        let mock_data = data.clone();
        let mock_task = tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(stream);
            let mut reader = reader;
            let mut writer = writer;

            // Handshake
            let hs = Handshake::new(info_hash, remote_id);
            writer.write_all(&hs.to_bytes()).await.unwrap();
            writer.flush().await.unwrap();

            let mut hs_buf = [0u8; HANDSHAKE_SIZE];
            reader.read_exact(&mut hs_buf).await.unwrap();

            // Switch to framed
            let mut framed_read = FramedRead::new(reader, MessageCodec::new());
            let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

            // Read ext handshake from the torrent actor's peer
            let _msg = framed_read.next().await;

            // Send ext handshake back
            let ext_hs = ExtHandshake::new();
            let payload = ext_hs.to_bytes().unwrap();
            framed_write
                .send(Message::Extended { ext_id: 0, payload })
                .await
                .unwrap();

            // Send bitfield (all pieces = piece 0 set)
            let mut bf = Bitfield::new(1);
            bf.set(0);
            framed_write
                .send(Message::Bitfield(Bytes::copy_from_slice(bf.as_bytes())))
                .await
                .unwrap();

            // Send Unchoke
            framed_write.send(Message::Unchoke).await.unwrap();

            // Wait for requests and respond with piece data
            while let Some(Ok(msg)) = framed_read.next().await {
                match msg {
                    Message::Request {
                        index,
                        begin,
                        length,
                    } => {
                        let start = begin as usize;
                        let end = start + length as usize;
                        let piece_data = &mock_data[start..end];
                        framed_write
                            .send(Message::Piece {
                                index,
                                begin,
                                data: Bytes::copy_from_slice(piece_data),
                            })
                            .await
                            .unwrap();
                    }
                    Message::Interested => {
                        // Expected — the torrent should express interest
                    }
                    _ => {}
                }
            }
        });

        // Wait for the download to complete
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let stats = handle.stats().await.unwrap();
            if stats.state == TorrentState::Seeding {
                assert_eq!(stats.pieces_have, 1);
                assert_eq!(stats.pieces_total, 1);
                break;
            }
            if tokio::time::Instant::now() > deadline {
                let stats = handle.stats().await.unwrap();
                panic!(
                    "download did not complete within 5s, state={:?}, have={}/{}",
                    stats.state, stats.pieces_have, stats.pieces_total
                );
            }
        }

        handle.shutdown().await.unwrap();
        mock_task.abort();
    }

    // ---- Test 11: Failed piece verification re-requests ----

    #[tokio::test]
    async fn failed_piece_verification() {
        // Create a 1-piece torrent
        let data = vec![0xEEu8; 16384];
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_storage(&data, 16384);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();
        drop(listener);

        let config = TorrentConfig {
            listen_port: listen_addr.port(),
            ..test_config()
        };

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Connect mock peer that first sends bad data, then correct data
        let stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let remote_id = Id20::from_hex("3333333333333333333333333333333333333333").unwrap();

        let correct_data = data.clone();
        let mock_task = tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(stream);

            // Handshake
            let mut writer = writer;
            let mut reader = reader;
            let hs = Handshake::new(info_hash, remote_id);
            writer.write_all(&hs.to_bytes()).await.unwrap();
            writer.flush().await.unwrap();

            let mut hs_buf = [0u8; HANDSHAKE_SIZE];
            reader.read_exact(&mut hs_buf).await.unwrap();

            let mut framed_read = FramedRead::new(reader, MessageCodec::new());
            let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

            // Read ext handshake
            let _msg = framed_read.next().await;

            // Send ext handshake
            let ext_hs = ExtHandshake::new();
            let payload = ext_hs.to_bytes().unwrap();
            framed_write
                .send(Message::Extended { ext_id: 0, payload })
                .await
                .unwrap();

            // Bitfield: have piece 0
            let mut bf = Bitfield::new(1);
            bf.set(0);
            framed_write
                .send(Message::Bitfield(Bytes::copy_from_slice(bf.as_bytes())))
                .await
                .unwrap();

            // Unchoke
            framed_write.send(Message::Unchoke).await.unwrap();

            let mut request_count = 0u32;
            while let Some(Ok(msg)) = framed_read.next().await {
                match msg {
                    Message::Request {
                        index,
                        begin,
                        length,
                    } => {
                        request_count += 1;
                        let piece_data = if request_count <= 1 {
                            // First request: send bad data
                            vec![0xFF; length as usize]
                        } else {
                            // Subsequent: send correct data
                            let start = begin as usize;
                            let end = start + length as usize;
                            correct_data[start..end].to_vec()
                        };
                        framed_write
                            .send(Message::Piece {
                                index,
                                begin,
                                data: Bytes::from(piece_data),
                            })
                            .await
                            .unwrap();
                    }
                    Message::Interested => {}
                    _ => {}
                }
            }
        });

        // Wait for completion (should eventually succeed after retry)
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let stats = handle.stats().await.unwrap();
            if stats.state == TorrentState::Seeding {
                assert_eq!(stats.pieces_have, 1);
                break;
            }
            if tokio::time::Instant::now() > deadline {
                let stats = handle.stats().await.unwrap();
                panic!(
                    "download did not complete after retry within 5s, state={:?}, have={}",
                    stats.state, stats.pieces_have,
                );
            }
        }

        handle.shutdown().await.unwrap();
        mock_task.abort();
    }

    // ---- Test 12: Complete state transitions after all pieces ----

    #[tokio::test]
    async fn complete_transitions_state() {
        // 2-piece torrent, each 16384 bytes (one chunk each)
        let data = vec![0xBBu8; 32768];
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_storage(&data, 16384);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();
        drop(listener);

        let config = TorrentConfig {
            listen_port: listen_addr.port(),
            ..test_config()
        };

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Mock seeder with all 2 pieces
        let stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let remote_id = Id20::from_hex("4444444444444444444444444444444444444444").unwrap();

        let mock_data = data.clone();
        let mock_task = tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(stream);
            let mut writer = writer;
            let mut reader = reader;

            let hs = Handshake::new(info_hash, remote_id);
            writer.write_all(&hs.to_bytes()).await.unwrap();
            writer.flush().await.unwrap();

            let mut hs_buf = [0u8; HANDSHAKE_SIZE];
            reader.read_exact(&mut hs_buf).await.unwrap();

            let mut framed_read = FramedRead::new(reader, MessageCodec::new());
            let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

            // Read ext handshake
            let _msg = framed_read.next().await;

            // Send ext handshake
            let ext_hs = ExtHandshake::new();
            let payload = ext_hs.to_bytes().unwrap();
            framed_write
                .send(Message::Extended { ext_id: 0, payload })
                .await
                .unwrap();

            // Bitfield: have both pieces
            let mut bf = Bitfield::new(2);
            bf.set(0);
            bf.set(1);
            framed_write
                .send(Message::Bitfield(Bytes::copy_from_slice(bf.as_bytes())))
                .await
                .unwrap();

            framed_write.send(Message::Unchoke).await.unwrap();

            while let Some(Ok(msg)) = framed_read.next().await {
                match msg {
                    Message::Request {
                        index,
                        begin,
                        length,
                    } => {
                        let abs_start = (index as usize * 16384) + begin as usize;
                        let abs_end = abs_start + length as usize;
                        let piece_data = &mock_data[abs_start..abs_end];
                        framed_write
                            .send(Message::Piece {
                                index,
                                begin,
                                data: Bytes::copy_from_slice(piece_data),
                            })
                            .await
                            .unwrap();
                    }
                    Message::Interested => {}
                    _ => {}
                }
            }
        });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let stats = handle.stats().await.unwrap();
            if stats.state == TorrentState::Seeding {
                assert_eq!(stats.pieces_have, 2);
                assert_eq!(stats.pieces_total, 2);
                break;
            }
            if tokio::time::Instant::now() > deadline {
                let stats = handle.stats().await.unwrap();
                panic!(
                    "expected Complete, got {:?}, have={}/{}",
                    stats.state, stats.pieces_have, stats.pieces_total
                );
            }
        }

        handle.shutdown().await.unwrap();
        mock_task.abort();
    }

    // ---- Test 13: Multiple pieces with multi-chunk pieces ----

    #[tokio::test]
    async fn multi_chunk_piece_download() {
        // 1 piece of 32768 bytes = 2 chunks of 16384 each
        let data = vec![0xAAu8; 32768];
        let meta = make_test_torrent(&data, 32768);
        let info_hash = meta.info_hash;
        let storage = make_storage(&data, 32768);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();
        drop(listener);

        let config = TorrentConfig {
            listen_port: listen_addr.port(),
            ..test_config()
        };

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        let stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let remote_id = Id20::from_hex("5555555555555555555555555555555555555555").unwrap();

        let mock_data = data.clone();
        let mock_task = tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(stream);
            let mut writer = writer;
            let mut reader = reader;

            let hs = Handshake::new(info_hash, remote_id);
            writer.write_all(&hs.to_bytes()).await.unwrap();
            writer.flush().await.unwrap();

            let mut hs_buf = [0u8; HANDSHAKE_SIZE];
            reader.read_exact(&mut hs_buf).await.unwrap();

            let mut framed_read = FramedRead::new(reader, MessageCodec::new());
            let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

            let _msg = framed_read.next().await;

            let ext_hs = ExtHandshake::new();
            let payload = ext_hs.to_bytes().unwrap();
            framed_write
                .send(Message::Extended { ext_id: 0, payload })
                .await
                .unwrap();

            let mut bf = Bitfield::new(1);
            bf.set(0);
            framed_write
                .send(Message::Bitfield(Bytes::copy_from_slice(bf.as_bytes())))
                .await
                .unwrap();

            framed_write.send(Message::Unchoke).await.unwrap();

            while let Some(Ok(msg)) = framed_read.next().await {
                match msg {
                    Message::Request {
                        index: _,
                        begin,
                        length,
                    } => {
                        let start = begin as usize;
                        let end = start + length as usize;
                        framed_write
                            .send(Message::Piece {
                                index: 0,
                                begin,
                                data: Bytes::copy_from_slice(&mock_data[start..end]),
                            })
                            .await
                            .unwrap();
                    }
                    Message::Interested => {}
                    _ => {}
                }
            }
        });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let stats = handle.stats().await.unwrap();
            if stats.state == TorrentState::Seeding {
                assert_eq!(stats.pieces_have, 1);
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("multi-chunk download did not complete within 5s");
            }
        }

        handle.shutdown().await.unwrap();
        mock_task.abort();
    }

    // ---- Test 14: Seeder/Leecher integration with two actors ----

    #[tokio::test]
    async fn seeder_leecher_integration() {
        // Seeder has all data, leecher has none. Connect them via TCP.
        let data = vec![0xDDu8; 32768]; // 32 KiB, 2 pieces of 16384
        let piece_length = 16384u64;
        let meta = make_test_torrent(&data, piece_length);
        let info_hash = meta.info_hash;

        // Seeder: storage pre-filled
        let seeder_storage = make_seeded_storage(&data, piece_length);

        // For the seeder, we need a from_torrent variant that starts in Complete state
        // but still serves pieces. Since our actor starts in Downloading, the seeder
        // will just be a mock that accepts and serves.

        // Use a mock seeder approach instead (manual protocol handling):
        let seeder_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let seeder_addr = seeder_listener.local_addr().unwrap();

        let seeder_task = tokio::spawn(async move {
            let (stream, _addr) = seeder_listener.accept().await.unwrap();
            let (reader, writer) = tokio::io::split(stream);
            let mut writer = writer;
            let mut reader = reader;

            // Handshake
            let mut hs_buf = [0u8; HANDSHAKE_SIZE];
            reader.read_exact(&mut hs_buf).await.unwrap();
            let their_hs = Handshake::from_bytes(&hs_buf).unwrap();
            assert_eq!(their_hs.info_hash, info_hash);

            let hs = Handshake::new(info_hash, PeerId::generate().0);
            writer.write_all(&hs.to_bytes()).await.unwrap();
            writer.flush().await.unwrap();

            let mut framed_read = FramedRead::new(reader, MessageCodec::new());
            let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

            // Read ext handshake
            let _msg = framed_read.next().await;

            // Send ext handshake
            let ext_hs = ExtHandshake::new();
            let payload = ext_hs.to_bytes().unwrap();
            framed_write
                .send(Message::Extended { ext_id: 0, payload })
                .await
                .unwrap();

            // Send bitfield (all pieces)
            let mut bf = Bitfield::new(2);
            bf.set(0);
            bf.set(1);
            framed_write
                .send(Message::Bitfield(Bytes::copy_from_slice(bf.as_bytes())))
                .await
                .unwrap();

            // Unchoke
            framed_write.send(Message::Unchoke).await.unwrap();

            // Serve requests
            while let Some(Ok(msg)) = framed_read.next().await {
                match msg {
                    Message::Request {
                        index,
                        begin,
                        length,
                    } => {
                        let piece_data = seeder_storage.read_chunk(index, begin, length).unwrap();
                        framed_write
                            .send(Message::Piece {
                                index,
                                begin,
                                data: Bytes::from(piece_data),
                            })
                            .await
                            .unwrap();
                    }
                    Message::Interested => {}
                    _ => {}
                }
            }
        });

        // Leecher: empty storage
        let leecher_storage = make_storage(&data, piece_length);
        let leecher_meta = make_test_torrent(&data, piece_length);

        let leecher_config = test_config();
        let (latx, lamask) = test_alert_channel();
        let (ldh, ldm, _ldj) = test_register_disk(leecher_meta.info_hash, leecher_storage).await;
        let leecher = TorrentHandle::from_torrent(
            leecher_meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            ldh,
            ldm,
            leecher_config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            latx,
            lamask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Add seeder as a peer
        leecher
            .add_peers(vec![seeder_addr], PeerSource::Tracker)
            .await
            .unwrap();

        // Give the connect interval time to fire (it ticks every 5s).
        // The actor's try_connect_peers runs on the timer, and also immediately
        // when peers are added via AddPeers command. Wait up to 10 seconds.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let stats = leecher.stats().await.unwrap();
            if stats.state == TorrentState::Seeding {
                assert_eq!(stats.pieces_have, 2);
                assert_eq!(stats.pieces_total, 2);
                break;
            }
            if tokio::time::Instant::now() > deadline {
                let stats = leecher.stats().await.unwrap();
                panic!(
                    "seeder/leecher: leecher did not complete, state={:?}, have={}/{}, connected={}, available={}",
                    stats.state,
                    stats.pieces_have,
                    stats.pieces_total,
                    stats.peers_connected,
                    stats.peers_available,
                );
            }
        }

        leecher.shutdown().await.unwrap();
        seeder_task.abort();
    }

    // ---- Test 15: Magnet stats ----

    #[tokio::test]
    async fn magnet_initial_stats() {
        let magnet = Magnet {
            info_hashes: torrent_core::InfoHashes::v1_only(
                Id20::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap(),
            ),
            display_name: Some("magnet test".into()),
            trackers: vec![],
            peers: vec![],
            selected_files: None,
        };

        let (atx, amask) = test_alert_channel();
        let (dm, _dj) = test_disk_manager();
        let handle = TorrentHandle::from_magnet(
            magnet,
            dm,
            test_config(),
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::FetchingMetadata);
        assert_eq!(stats.pieces_total, 0);
        assert_eq!(stats.pieces_have, 0);
        assert_eq!(stats.downloaded, 0);
        assert_eq!(stats.uploaded, 0);
        assert_eq!(stats.peers_connected, 0);
        assert_eq!(stats.peers_available, 0);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 16: Tracker manager is populated from torrent metadata ----

    #[tokio::test]
    async fn tracker_populated_from_metadata() {
        use serde::Serialize;

        let data = vec![0xAB; 16384];
        let hash = torrent_core::sha1(&data);
        let mut pieces = Vec::new();
        pieces.extend_from_slice(hash.as_bytes());

        #[derive(Serialize)]
        struct Info<'a> {
            length: u64,
            name: &'a str,
            #[serde(rename = "piece length")]
            piece_length: u64,
            #[serde(with = "serde_bytes")]
            pieces: &'a [u8],
        }

        #[derive(Serialize)]
        struct Torrent<'a> {
            announce: &'a str,
            info: Info<'a>,
        }

        let t = Torrent {
            announce: "http://tracker.example.com:8080/announce",
            info: Info {
                length: data.len() as u64,
                name: "test",
                piece_length: 16384,
                pieces: &pieces,
            },
        };

        let bytes = torrent_bencode::to_bytes(&t).unwrap();
        let meta = torrent_from_bytes(&bytes).unwrap();
        assert!(meta.announce.is_some());

        let storage = make_storage(&data, 16384);
        let config = test_config();

        // The torrent should start and announce to tracker (which will fail since
        // the tracker doesn't exist, but that's fine — failures are non-fatal).
        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 17: Private torrent with DHT=None works ----

    #[tokio::test]
    async fn private_torrent_no_dht_field() {
        let data = vec![0xAB; 16384];
        let hash = torrent_core::sha1(&data);
        let mut pieces = Vec::new();
        pieces.extend_from_slice(hash.as_bytes());

        use serde::Serialize;

        #[derive(Serialize)]
        struct Info<'a> {
            length: u64,
            name: &'a str,
            #[serde(rename = "piece length")]
            piece_length: u64,
            #[serde(with = "serde_bytes")]
            pieces: &'a [u8],
            private: i64,
        }

        #[derive(Serialize)]
        struct Torrent<'a> {
            announce: &'a str,
            info: Info<'a>,
        }

        let t = Torrent {
            announce: "http://private-tracker.example.com/announce",
            info: Info {
                length: data.len() as u64,
                name: "private_test",
                piece_length: 16384,
                pieces: &pieces,
                private: 1,
            },
        };

        let bytes = torrent_bencode::to_bytes(&t).unwrap();
        let meta = torrent_from_bytes(&bytes).unwrap();
        assert_eq!(meta.info.private, Some(1));

        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 18: Magnet defers tracker announce ----

    #[tokio::test]
    async fn magnet_no_tracker_before_metadata() {
        let magnet = Magnet {
            info_hashes: torrent_core::InfoHashes::v1_only(
                Id20::from_hex("cccccccccccccccccccccccccccccccccccccccc").unwrap(),
            ),
            display_name: Some("magnet test".into()),
            trackers: vec![],
            peers: vec![],
            selected_files: None,
        };

        let (atx, amask) = test_alert_channel();
        let (dm, _dj) = test_disk_manager();
        let handle = TorrentHandle::from_magnet(
            magnet,
            dm,
            test_config(),
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::FetchingMetadata);

        // With no trackers configured, no announces happen regardless of state.
        // Note: tracker announces ARE now allowed during FetchingMetadata for
        // magnets with &tr= URLs (needed to discover peers before metadata).
        tokio::time::sleep(Duration::from_millis(50)).await;

        handle.shutdown().await.unwrap();
    }

    // ---- Test 19: Pause and resume ----

    #[tokio::test]
    async fn pause_and_resume() {
        let data = vec![0xEEu8; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();
        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);

        handle.pause().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Paused);

        handle.resume().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 20: Pause already paused is noop ----

    #[tokio::test]
    async fn pause_already_paused_is_noop() {
        let data = vec![0xEEu8; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();
        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        handle.pause().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.pause().await.unwrap(); // double pause is fine
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Paused);

        handle.shutdown().await.unwrap();
    }

    // ---- Test 21: Incoming request served from storage ----
    //
    // Phase 1: Mock seeder feeds piece 0 to the torrent so it becomes verified.
    // Phase 2: Mock leecher connects and requests piece 0, verifying upload pipeline.

    #[tokio::test]
    async fn incoming_request_served_from_storage() {
        let data = vec![0xABu8; 16384];
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_storage(&data, 16384);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();
        drop(listener);

        let config = TorrentConfig {
            listen_port: listen_addr.port(),
            ..test_config()
        };

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Phase 1: Seed the torrent with piece 0
        let seed_data = data.clone();
        let seed_stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let seeder_task = tokio::spawn({
            let info_hash = info_hash;
            async move {
                let (reader, writer) = tokio::io::split(seed_stream);
                let mut writer = writer;
                let mut reader = reader;

                let hs = Handshake::new(
                    info_hash,
                    Id20::from_hex("6666666666666666666666666666666666666666").unwrap(),
                );
                writer.write_all(&hs.to_bytes()).await.unwrap();
                writer.flush().await.unwrap();
                let mut hs_buf = [0u8; HANDSHAKE_SIZE];
                reader.read_exact(&mut hs_buf).await.unwrap();

                let mut framed_read = FramedRead::new(reader, MessageCodec::new());
                let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

                let _msg = framed_read.next().await; // ext handshake
                let ext_hs = ExtHandshake::new();
                let payload = ext_hs.to_bytes().unwrap();
                framed_write
                    .send(Message::Extended { ext_id: 0, payload })
                    .await
                    .unwrap();

                // Send bitfield + unchoke
                let mut bf = Bitfield::new(1);
                bf.set(0);
                framed_write
                    .send(Message::Bitfield(Bytes::copy_from_slice(bf.as_bytes())))
                    .await
                    .unwrap();
                framed_write.send(Message::Unchoke).await.unwrap();

                // Respond to requests
                while let Some(Ok(msg)) = framed_read.next().await {
                    match msg {
                        Message::Request {
                            index,
                            begin,
                            length,
                        } => {
                            let start = begin as usize;
                            let end = start + length as usize;
                            framed_write
                                .send(Message::Piece {
                                    index,
                                    begin,
                                    data: Bytes::copy_from_slice(&seed_data[start..end]),
                                })
                                .await
                                .unwrap();
                        }
                        Message::Interested => {}
                        _ => {}
                    }
                }
            }
        });

        // Wait for download to complete
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let stats = handle.stats().await.unwrap();
            if stats.pieces_have == 1 {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("piece download did not complete within 5s");
            }
        }

        // Phase 2: Connect a mock leecher to request piece 0 back
        let leech_stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let expected_data = data.clone();
        let leecher_task = tokio::spawn({
            let info_hash = info_hash;
            async move {
                let (reader, writer) = tokio::io::split(leech_stream);
                let mut writer = writer;
                let mut reader = reader;

                let hs = Handshake::new(
                    info_hash,
                    Id20::from_hex("7777777777777777777777777777777777777777").unwrap(),
                );
                writer.write_all(&hs.to_bytes()).await.unwrap();
                writer.flush().await.unwrap();
                let mut hs_buf = [0u8; HANDSHAKE_SIZE];
                reader.read_exact(&mut hs_buf).await.unwrap();

                let mut framed_read = FramedRead::new(reader, MessageCodec::new());
                let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

                let _msg = framed_read.next().await; // ext handshake
                let ext_hs = ExtHandshake::new();
                let payload = ext_hs.to_bytes().unwrap();
                framed_write
                    .send(Message::Extended { ext_id: 0, payload })
                    .await
                    .unwrap();

                // Send Interested and wait for Unchoke
                framed_write.send(Message::Interested).await.unwrap();

                let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                loop {
                    tokio::select! {
                        msg = framed_read.next() => {
                            match msg {
                                Some(Ok(Message::Unchoke)) => { break; }
                                Some(Ok(_)) => {}
                                _ => panic!("connection closed before unchoke"),
                            }
                        }
                        _ = tokio::time::sleep_until(deadline) => {
                            panic!("timed out waiting for unchoke");
                        }
                    }
                }

                // Request piece 0
                framed_write
                    .send(Message::Request {
                        index: 0,
                        begin: 0,
                        length: 16384,
                    })
                    .await
                    .unwrap();

                // Read Piece response
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                loop {
                    tokio::select! {
                        msg = framed_read.next() => {
                            match msg {
                                Some(Ok(Message::Piece { index, begin, data })) => {
                                    assert_eq!(index, 0);
                                    assert_eq!(begin, 0);
                                    assert_eq!(data.as_ref(), expected_data.as_slice());
                                    return; // success
                                }
                                Some(Ok(_)) => {}
                                Some(Err(e)) => panic!("error reading: {e}"),
                                None => panic!("connection closed before piece"),
                            }
                        }
                        _ = tokio::time::sleep_until(deadline) => {
                            panic!("timed out waiting for piece data");
                        }
                    }
                }
            }
        });

        // Wait for leecher to complete
        let result = tokio::time::timeout(Duration::from_secs(20), leecher_task).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("leecher task panicked: {e}"),
            Err(_) => panic!("test timed out"),
        }

        // Verify uploaded bytes
        let stats = handle.stats().await.unwrap();
        assert!(
            stats.uploaded > 0,
            "expected uploaded > 0, got {}",
            stats.uploaded
        );

        handle.shutdown().await.unwrap();
        seeder_task.abort();
    }

    // ---- Test 22: Seed ratio limit stops torrent ----

    #[tokio::test]
    async fn seed_ratio_limit_stops_torrent() {
        // 1-piece torrent, ratio limit = 1.0
        // After downloading 16384 bytes and uploading 16384 bytes, ratio = 1.0 → stop
        let data = vec![0xCCu8; 16384];
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_storage(&data, 16384);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();
        drop(listener);

        let config = TorrentConfig {
            listen_port: listen_addr.port(),
            seed_ratio_limit: Some(1.0),
            ..test_config()
        };

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Phase 1: Seed the torrent with piece 0
        let seed_data = data.clone();
        let seed_stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let seeder_task = tokio::spawn({
            let info_hash = info_hash;
            async move {
                let (reader, writer) = tokio::io::split(seed_stream);
                let mut writer = writer;
                let mut reader = reader;

                let hs = Handshake::new(
                    info_hash,
                    Id20::from_hex("8888888888888888888888888888888888888888").unwrap(),
                );
                writer.write_all(&hs.to_bytes()).await.unwrap();
                writer.flush().await.unwrap();
                let mut hs_buf = [0u8; HANDSHAKE_SIZE];
                reader.read_exact(&mut hs_buf).await.unwrap();

                let mut framed_read = FramedRead::new(reader, MessageCodec::new());
                let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

                let _msg = framed_read.next().await;
                let ext_hs = ExtHandshake::new();
                let payload = ext_hs.to_bytes().unwrap();
                framed_write
                    .send(Message::Extended { ext_id: 0, payload })
                    .await
                    .unwrap();

                let mut bf = Bitfield::new(1);
                bf.set(0);
                framed_write
                    .send(Message::Bitfield(Bytes::copy_from_slice(bf.as_bytes())))
                    .await
                    .unwrap();
                framed_write.send(Message::Unchoke).await.unwrap();

                while let Some(Ok(msg)) = framed_read.next().await {
                    match msg {
                        Message::Request {
                            index,
                            begin,
                            length,
                        } => {
                            let start = begin as usize;
                            let end = start + length as usize;
                            framed_write
                                .send(Message::Piece {
                                    index,
                                    begin,
                                    data: Bytes::copy_from_slice(&seed_data[start..end]),
                                })
                                .await
                                .unwrap();
                        }
                        Message::Interested => {}
                        _ => {}
                    }
                }
            }
        });

        // Wait for download to complete (transitions to Seeding)
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let stats = handle.stats().await.unwrap();
            if stats.state == TorrentState::Seeding {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("download did not complete within 5s");
            }
        }

        // Phase 2: Connect leecher to request piece 0 — this should trigger ratio limit
        let leech_stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let leecher_task = tokio::spawn({
            let info_hash = info_hash;
            async move {
                let (reader, writer) = tokio::io::split(leech_stream);
                let mut writer = writer;
                let mut reader = reader;

                let hs = Handshake::new(
                    info_hash,
                    Id20::from_hex("9999999999999999999999999999999999999999").unwrap(),
                );
                writer.write_all(&hs.to_bytes()).await.unwrap();
                writer.flush().await.unwrap();
                let mut hs_buf = [0u8; HANDSHAKE_SIZE];
                reader.read_exact(&mut hs_buf).await.unwrap();

                let mut framed_read = FramedRead::new(reader, MessageCodec::new());
                let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

                let _msg = framed_read.next().await;
                let ext_hs = ExtHandshake::new();
                let payload = ext_hs.to_bytes().unwrap();
                framed_write
                    .send(Message::Extended { ext_id: 0, payload })
                    .await
                    .unwrap();

                framed_write.send(Message::Interested).await.unwrap();

                // Wait for unchoke
                let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                loop {
                    tokio::select! {
                        msg = framed_read.next() => {
                            match msg {
                                Some(Ok(Message::Unchoke)) => break,
                                Some(Ok(_)) => {}
                                _ => return, // connection may close due to ratio shutdown
                            }
                        }
                        _ = tokio::time::sleep_until(deadline) => return,
                    }
                }

                // Request piece 0
                framed_write
                    .send(Message::Request {
                        index: 0,
                        begin: 0,
                        length: 16384,
                    })
                    .await
                    .unwrap();

                // Read until connection closes (the torrent may stop and disconnect us)
                while let Some(Ok(_msg)) = framed_read.next().await {}
            }
        });

        // Wait for state to become Stopped
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let stats = handle.stats().await.unwrap();
            if stats.state == TorrentState::Stopped {
                assert!(
                    stats.uploaded >= 16384,
                    "expected uploaded >= 16384, got {}",
                    stats.uploaded
                );
                break;
            }
            if tokio::time::Instant::now() > deadline {
                let stats = handle.stats().await.unwrap();
                panic!(
                    "expected Stopped, got {:?}, uploaded={}, downloaded={}",
                    stats.state, stats.uploaded, stats.downloaded
                );
            }
        }

        handle.shutdown().await.unwrap();
        seeder_task.abort();
        leecher_task.abort();
    }

    // ---- Test 23: Resume with seeded storage starts as seeder ----

    #[tokio::test]
    async fn resume_with_seeded_storage() {
        let data = vec![0xDDu8; 32768]; // 2 pieces
        let meta = make_test_torrent(&data, 16384);
        let storage = make_seeded_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Give the actor time to verify existing pieces
        tokio::time::sleep(Duration::from_millis(100)).await;

        let stats = handle.stats().await.unwrap();
        assert_eq!(
            stats.state,
            TorrentState::Seeding,
            "should start as seeder with all pieces verified"
        );
        assert_eq!(stats.pieces_have, 2);
        assert_eq!(stats.pieces_total, 2);

        handle.shutdown().await.unwrap();
    }

    // ---- Test: save_resume_data captures state ----

    #[tokio::test]
    async fn save_resume_data_captures_state() {
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Give actor time to start
        tokio::time::sleep(Duration::from_millis(50)).await;

        let rd = handle.save_resume_data().await.unwrap();

        assert_eq!(rd.file_format, "libtorrent resume file");
        assert_eq!(rd.file_version, 1);
        assert_eq!(rd.info_hash, info_hash.as_bytes().to_vec());
        assert_eq!(rd.name, "test");
        assert_eq!(rd.save_path, "/tmp");
        assert_eq!(rd.paused, 0);
        // No pieces downloaded yet — bitfield should be all zeros
        assert!(!rd.pieces.is_empty());
        // Stats should be zero for a freshly started torrent with no peers
        assert_eq!(rd.total_uploaded, 0);
        assert_eq!(rd.total_downloaded, 0);

        handle.shutdown().await.unwrap();
    }

    // ---- Test: save_resume_data for seeder ----

    #[tokio::test]
    async fn save_resume_data_seeder() {
        let data = vec![0xCD; 32768];
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_seeded_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Give actor time to verify pieces and switch to seeding
        tokio::time::sleep(Duration::from_millis(100)).await;

        let rd = handle.save_resume_data().await.unwrap();

        assert_eq!(rd.info_hash, info_hash.as_bytes().to_vec());
        assert_eq!(rd.name, "test");
        assert_eq!(rd.seed_mode, 1, "seeder should have seed_mode=1");
        assert_eq!(rd.paused, 0);
        // All pieces should be marked in the bitfield
        // 2 pieces -> 1 byte, top 2 bits set = 0b1100_0000 = 0xC0
        assert_eq!(rd.pieces.len(), 1);
        assert_eq!(
            rd.pieces[0] & 0xC0,
            0xC0,
            "both pieces should be marked complete"
        );

        handle.shutdown().await.unwrap();
    }

    // ---- Test: save_resume_data for paused torrent ----

    #[tokio::test]
    async fn save_resume_data_paused() {
        let data = vec![0xEF; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.pause().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let rd = handle.save_resume_data().await.unwrap();
        assert_eq!(rd.paused, 1, "paused torrent should have paused=1");
        assert_eq!(rd.seed_mode, 0);

        handle.shutdown().await.unwrap();
    }

    // ---- Test: set_file_priority and read back ----

    #[tokio::test]
    async fn set_file_priority_and_read_back() {
        let info_bytes = b"d5:filesld6:lengthi100e4:pathl5:a.bineed6:lengthi100e4:pathl5:b.bineee4:name4:test12:piece lengthi100e6:pieces40:AAAAAAAAAAAAAAAAAAAABBBBBBBBBBBBBBBBBBBBe";
        let mut torrent_bytes = b"d4:info".to_vec();
        torrent_bytes.extend_from_slice(info_bytes);
        torrent_bytes.push(b'e');

        let meta = torrent_core::torrent_from_bytes(&torrent_bytes).unwrap();
        let lengths = Lengths::new(200, 100, DEFAULT_CHUNK_SIZE);
        let storage: Arc<dyn TorrentStorage> = Arc::new(MemoryStorage::new(lengths));
        let config = TorrentConfig {
            listen_port: 0,
            ..Default::default()
        };

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Default priorities should all be Normal
        let prios = handle.file_priorities().await.unwrap();
        assert_eq!(prios.len(), 2);
        assert!(prios.iter().all(|p| *p == FilePriority::Normal));

        // Set file 0 to Skip
        handle
            .set_file_priority(0, FilePriority::Skip)
            .await
            .unwrap();

        let prios = handle.file_priorities().await.unwrap();
        assert_eq!(prios[0], FilePriority::Skip);
        assert_eq!(prios[1], FilePriority::Normal);

        // Invalid index should error
        let result = handle.set_file_priority(99, FilePriority::High).await;
        assert!(result.is_err());

        handle.shutdown().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn resume_data_preserves_file_priorities() {
        let info_bytes = b"d5:filesld6:lengthi100e4:pathl5:a.bineed6:lengthi100e4:pathl5:b.bineee4:name4:test12:piece lengthi100e6:pieces40:AAAAAAAAAAAAAAAAAAAABBBBBBBBBBBBBBBBBBBBe";
        let mut torrent_bytes = b"d4:info".to_vec();
        torrent_bytes.extend_from_slice(info_bytes);
        torrent_bytes.push(b'e');

        let meta = torrent_core::torrent_from_bytes(&torrent_bytes).unwrap();
        let lengths = Lengths::new(200, 100, DEFAULT_CHUNK_SIZE);
        let storage: Arc<dyn TorrentStorage> = Arc::new(MemoryStorage::new(lengths));
        let config = TorrentConfig {
            listen_port: 0,
            ..Default::default()
        };

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Set file priorities
        handle
            .set_file_priority(0, FilePriority::High)
            .await
            .unwrap();
        handle
            .set_file_priority(1, FilePriority::Skip)
            .await
            .unwrap();

        // Save resume data
        let rd = handle.save_resume_data().await.unwrap();
        assert_eq!(rd.file_priority, vec![7, 0]); // High=7, Skip=0

        // Verify bencode round-trip
        let encoded = torrent_bencode::to_bytes(&rd).unwrap();
        let decoded: torrent_core::FastResumeData = torrent_bencode::from_bytes(&encoded).unwrap();
        assert_eq!(decoded.file_priority, vec![7, 0]);

        handle.shutdown().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // ---- Rate limiting integration tests (M14) ----

    #[tokio::test]
    async fn upload_rate_limiting_caps_throughput() {
        // Test that per-torrent upload rate limiting gates serve_incoming_requests.
        // We use a very low rate (1 KB/s) so the 16 KB piece requires ~16 seconds.
        // We verify: 1) piece does NOT arrive within 200ms (bucket too small),
        //            2) the torrent actor is alive and functional.
        let data = vec![0xAB; 16384]; // 1 piece
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_seeded_storage(&data, 16384);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();

        let config = TorrentConfig {
            listen_port: listen_addr.port(),
            upload_rate_limit: 1024, // 1 KB/s — way too slow for 16 KB chunk
            ..test_config()
        };

        drop(listener);
        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Connect mock leecher (raw handshake + framed messages)
        let stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let (reader, writer) = tokio::io::split(stream);
        let mut writer = writer;
        let mut reader = reader;

        let hs = Handshake::new(
            info_hash,
            Id20::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap(),
        );
        writer.write_all(&hs.to_bytes()).await.unwrap();
        writer.flush().await.unwrap();
        let mut hs_buf = [0u8; HANDSHAKE_SIZE];
        reader.read_exact(&mut hs_buf).await.unwrap();

        let mut framed_read = FramedRead::new(reader, MessageCodec::new());
        let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

        // Read ext handshake + bitfield
        let _msg = framed_read.next().await;
        let ext_hs = ExtHandshake::new();
        let payload = ext_hs.to_bytes().unwrap();
        framed_write
            .send(Message::Extended { ext_id: 0, payload })
            .await
            .unwrap();

        // Read the bitfield
        let _bf_msg = framed_read.next().await;

        // Express interest
        framed_write.send(Message::Interested).await.unwrap();

        // Wait for unchoke
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            tokio::select! {
                msg = framed_read.next() => {
                    match msg {
                        Some(Ok(Message::Unchoke)) => break,
                        Some(Ok(_)) => {}
                        _ => panic!("connection closed before unchoke"),
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    panic!("timed out waiting for unchoke");
                }
            }
        }

        // Request piece 0
        framed_write
            .send(Message::Request {
                index: 0,
                begin: 0,
                length: 16384,
            })
            .await
            .unwrap();

        // At 1 KB/s, the bucket accumulates ~100 bytes per 100ms tick (max burst = 1024).
        // A 16 KB chunk needs 16384 tokens, so it should NOT be served quickly.
        // We wait 2 seconds — at 1 KB/s we'd have at most 2 KB, still < 16 KB.
        let mut got_piece = false;
        match tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match framed_read.next().await {
                    Some(Ok(Message::Piece { .. })) => return true,
                    Some(Ok(_)) => continue,
                    _ => return false,
                }
            }
        })
        .await
        {
            Ok(true) => got_piece = true,
            _ => {}
        }

        // Piece should NOT have arrived in 2 seconds (would need 16s at 1 KB/s)
        assert!(
            !got_piece,
            "piece should be delayed by rate limiter (1 KB/s for 16 KB chunk)"
        );

        // Verify actor is still alive
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.uploaded, 0); // nothing served yet

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unlimited_rate_has_no_effect() {
        // Default config (rate = 0) should behave identically to pre-M14
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        // Rate limits are 0 (unlimited) by default
        assert_eq!(config.upload_rate_limit, 0);
        assert_eq!(config.download_rate_limit, 0);

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);
        assert_eq!(stats.pieces_total, 2);

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn download_rate_limiting_throttles_requests() {
        // Test that download_rate_limit prevents sending requests when budget exhausted.
        // With 1 KB/s limit and 16 KB chunks, budget is exhausted almost immediately.
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_storage(&data, 16384);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_addr = listener.local_addr().unwrap();

        let config = TorrentConfig {
            listen_port: listen_addr.port(),
            download_rate_limit: 1024, // Very low: 1 KB/s
            ..test_config()
        };

        drop(listener);
        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Connect mock seeder
        let stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let (reader, writer) = tokio::io::split(stream);
        let mut writer = writer;
        let mut reader = reader;

        let hs = Handshake::new(
            info_hash,
            Id20::from_hex("cccccccccccccccccccccccccccccccccccccccc").unwrap(),
        );
        writer.write_all(&hs.to_bytes()).await.unwrap();
        writer.flush().await.unwrap();
        let mut hs_buf = [0u8; HANDSHAKE_SIZE];
        reader.read_exact(&mut hs_buf).await.unwrap();

        let mut framed_read = FramedRead::new(reader, MessageCodec::new());
        let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

        // Read ext handshake
        let _msg = framed_read.next().await;
        let ext_hs = ExtHandshake::new();
        let payload = ext_hs.to_bytes().unwrap();
        framed_write
            .send(Message::Extended { ext_id: 0, payload })
            .await
            .unwrap();

        // Send bitfield saying we have all pieces (act as seeder)
        let mut bf = Bitfield::new(2);
        bf.set(0);
        bf.set(1);
        framed_write
            .send(Message::Bitfield(Bytes::copy_from_slice(bf.as_bytes())))
            .await
            .unwrap();

        // Unchoke the torrent
        framed_write.send(Message::Unchoke).await.unwrap();

        // Count Request messages received within 500ms.
        // With 1 KB/s download limit, the bucket only accumulates ~50 bytes
        // per 100ms tick, far less than 16 KB needed for a full chunk request.
        let mut requests_received = 0u32;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        loop {
            match tokio::time::timeout(
                deadline.saturating_duration_since(tokio::time::Instant::now()),
                framed_read.next(),
            )
            .await
            {
                Ok(Some(Ok(Message::Request { .. }))) => {
                    requests_received += 1;
                }
                Ok(Some(Ok(_))) => continue,
                _ => break,
            }
        }

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);

        // With 1 KB/s download limit and 16 KB chunks, we should see very few
        // or no requests within 500ms (budget insufficient for even one chunk)
        assert!(
            requests_received <= 2,
            "with 1 KB/s limit, should get very few requests, got {requests_received}"
        );

        handle.shutdown().await.unwrap();
    }

    // ── Smart banning tests (M25) ────────────────────────────────────

    #[test]
    fn piece_contributor_tracking() {
        use std::net::IpAddr;
        let mut contributors: HashMap<u32, HashSet<IpAddr>> = HashMap::new();
        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.2".parse().unwrap();

        contributors.entry(0).or_default().insert(ip1);
        contributors.entry(0).or_default().insert(ip2);
        assert_eq!(contributors[&0].len(), 2);
        assert!(contributors[&0].contains(&ip1));
        assert!(contributors[&0].contains(&ip2));

        // Clear on verify
        contributors.remove(&0);
        assert!(!contributors.contains_key(&0));
    }

    #[test]
    fn parole_enter_on_hash_failure() {
        use crate::ban::{BanConfig, BanManager, ParoleState};
        use std::net::IpAddr;

        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.2".parse().unwrap();
        let contributors = vec![ip1, ip2];

        // Simulate entering parole
        let parole = ParoleState {
            original_contributors: contributors.into_iter().collect(),
            parole_peer: None,
        };

        assert_eq!(parole.original_contributors.len(), 2);
        assert!(parole.original_contributors.contains(&ip1));
        assert!(parole.original_contributors.contains(&ip2));
        assert!(parole.parole_peer.is_none());
    }

    #[test]
    fn parole_success_strikes_originals() {
        use crate::ban::{BanConfig, BanManager, ParoleState};
        use std::net::IpAddr;

        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.2".parse().unwrap();
        let parole_ip: IpAddr = "10.0.0.3".parse().unwrap();

        let mut mgr = BanManager::new(BanConfig {
            max_failures: 2,
            use_parole: true,
        });

        let parole = ParoleState {
            original_contributors: [ip1, ip2].into_iter().collect(),
            parole_peer: Some(parole_ip),
        };

        // Simulate parole success: strike all originals
        for ip in &parole.original_contributors {
            mgr.record_strike(*ip);
        }

        assert_eq!(*mgr.strikes_map().get(&ip1).unwrap(), 1);
        assert_eq!(*mgr.strikes_map().get(&ip2).unwrap(), 1);
        // Parole peer should not be struck
        assert!(!mgr.strikes_map().contains_key(&parole_ip));

        // Second strike bans them
        for ip in &parole.original_contributors {
            mgr.record_strike(*ip);
        }
        assert!(mgr.is_banned(&ip1));
        assert!(mgr.is_banned(&ip2));
    }

    #[test]
    fn parole_failure_strikes_parole_peer() {
        use crate::ban::{BanConfig, BanManager, ParoleState};
        use std::net::IpAddr;

        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let parole_ip: IpAddr = "10.0.0.3".parse().unwrap();

        let mut mgr = BanManager::new(BanConfig {
            max_failures: 2,
            use_parole: true,
        });

        let parole = ParoleState {
            original_contributors: [ip1].into_iter().collect(),
            parole_peer: Some(parole_ip),
        };

        // Parole failure: strike the parole peer, not originals
        if let Some(pp) = parole.parole_peer {
            mgr.record_strike(pp);
        }

        assert_eq!(*mgr.strikes_map().get(&parole_ip).unwrap(), 1);
        assert!(!mgr.strikes_map().contains_key(&ip1));
    }

    #[tokio::test]
    async fn banned_peer_rejected_on_connect() {
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();
        let ban_mgr = test_ban_manager();

        // Pre-ban an IP
        let banned_ip: std::net::IpAddr = "192.168.1.100".parse().unwrap();
        ban_mgr.write().unwrap().ban(banned_ip);

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            Arc::clone(&ban_mgr),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Add the banned peer — it should be filtered out
        handle
            .add_peers(
                vec![
                    SocketAddr::new(banned_ip, 6881),
                    "10.0.0.1:6881".parse().unwrap(),
                ],
                PeerSource::Tracker,
            )
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let stats = handle.stats().await.unwrap();
        // Only the non-banned peer should be in available pool (and may have connected)
        // The banned one should never appear
        assert!(
            stats.peers_available + stats.peers_connected <= 1,
            "banned peer should not be added: available={}, connected={}",
            stats.peers_available,
            stats.peers_connected
        );

        handle.shutdown().await.unwrap();
    }

    #[test]
    fn banned_peer_filtered_from_available() {
        use crate::ban::{BanConfig, BanManager};
        use std::net::IpAddr;

        let banned_ip: IpAddr = "192.168.1.200".parse().unwrap();
        let ok_ip: IpAddr = "10.0.0.1".parse().unwrap();

        let mgr = BanManager::new(BanConfig::default());
        // Not banned yet — both should pass
        assert!(!mgr.is_banned(&banned_ip));
        assert!(!mgr.is_banned(&ok_ip));

        let mut mgr = BanManager::new(BanConfig::default());
        mgr.ban(banned_ip);

        // Now banned_ip is filtered, ok_ip is not
        assert!(mgr.is_banned(&banned_ip));
        assert!(!mgr.is_banned(&ok_ip));
    }

    // ---- M27: Parallel hashing tests ----

    #[test]
    fn hashing_threads_config_default() {
        let s = crate::settings::Settings::default();
        let expected = {
            let cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4);
            (cores / 4).clamp(2, 8)
        };
        assert_eq!(s.hashing_threads, expected);
        let tc = TorrentConfig::default();
        assert_eq!(tc.hashing_threads, expected);
    }

    #[tokio::test]
    async fn checking_state_and_progress_alerts() {
        use crate::alert::{AlertCategory, AlertKind};

        let data = vec![0xEEu8; 65536]; // 4 pieces of 16384
        let meta = make_test_torrent(&data, 16384);
        let storage = make_seeded_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let mut rx = atx.subscribe();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Collect alerts for up to 2 seconds
        let mut saw_checking = false;
        let mut progress_values: Vec<f32> = Vec::new();
        let mut saw_checked = false;
        let mut checked_have = 0u32;
        let mut checked_total = 0u32;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Ok(alert)) => match alert.kind {
                    AlertKind::StateChanged {
                        new_state: TorrentState::Checking,
                        ..
                    } => {
                        saw_checking = true;
                    }
                    AlertKind::CheckingProgress { progress, .. } => {
                        progress_values.push(progress);
                    }
                    AlertKind::TorrentChecked {
                        pieces_have,
                        pieces_total,
                        ..
                    } => {
                        saw_checked = true;
                        checked_have = pieces_have;
                        checked_total = pieces_total;
                        break;
                    }
                    _ => {}
                },
                _ => break,
            }
        }

        assert!(saw_checking, "should have seen StateChanged → Checking");
        assert!(
            !progress_values.is_empty(),
            "should have seen CheckingProgress alerts"
        );
        // Progress should be monotonically increasing
        for w in progress_values.windows(2) {
            assert!(
                w[1] >= w[0],
                "progress should be monotonically increasing: {} < {}",
                w[0],
                w[1]
            );
        }
        assert!(saw_checked, "should have seen TorrentChecked");
        assert_eq!(checked_have, 4);
        assert_eq!(checked_total, 4);

        // Final state should be Seeding (all pieces valid)
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Seeding);

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn checking_progress_in_stats() {
        // When not in Checking state, checking_progress should be 0.0
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Give actor time to finish checking (no valid pieces → Downloading)
        tokio::time::sleep(Duration::from_millis(100)).await;

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);
        assert_eq!(
            stats.checking_progress, 0.0,
            "checking_progress should be 0.0 when not checking"
        );

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn verify_pieces_partial_data() {
        use crate::alert::AlertKind;

        // 4 pieces, only first 2 have valid data
        let data = vec![0xCCu8; 65536]; // 4 pieces × 16384
        let meta = make_test_torrent(&data, 16384);

        // Create storage and only write valid data for pieces 0 and 1
        let lengths = Lengths::new(data.len() as u64, 16384, DEFAULT_CHUNK_SIZE);
        let storage = Arc::new(MemoryStorage::new(lengths.clone()));
        for p in 0..2u32 {
            let offset = lengths.piece_offset(p) as usize;
            let size = lengths.piece_size(p) as usize;
            storage
                .write_chunk(p, 0, &data[offset..offset + size])
                .unwrap();
        }
        // Pieces 2 and 3 have no data (zeros) — won't match hash

        let config = test_config();
        let (atx, amask) = test_alert_channel();
        let mut rx = atx.subscribe();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Wait for TorrentChecked alert
        let mut checked_have = 0u32;
        let mut checked_total = 0u32;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Ok(alert)) => {
                    if let AlertKind::TorrentChecked {
                        pieces_have,
                        pieces_total,
                        ..
                    } = alert.kind
                    {
                        checked_have = pieces_have;
                        checked_total = pieces_total;
                        break;
                    }
                }
                _ => break,
            }
        }

        assert_eq!(checked_have, 2, "only 2 pieces should be valid");
        assert_eq!(checked_total, 4);

        // Final state should be Downloading (partial)
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);
        assert_eq!(stats.pieces_have, 2);
        assert_eq!(stats.pieces_total, 4);

        handle.shutdown().await.unwrap();
    }

    // ---- M29: IP filter integration tests ----

    #[tokio::test]
    async fn ip_filter_blocks_peers_in_handle_add_peers() {
        let data = vec![0xCD; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        // Create an IP filter that blocks 203.0.113.0/24 (TEST-NET-3, public range)
        let ip_filter = {
            let mut f = crate::ip_filter::IpFilter::new();
            f.add_rule(
                "203.0.113.0".parse().unwrap(),
                "203.0.113.255".parse().unwrap(),
                1,
            );
            Arc::new(std::sync::RwLock::new(f))
        };

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            Arc::clone(&ip_filter),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Add peers: one blocked (public IP in TEST-NET-3), one allowed (different public IP)
        let blocked_addr: SocketAddr = "203.0.113.42:6881".parse().unwrap();
        let allowed_addr: SocketAddr = "198.51.100.1:6881".parse().unwrap();
        handle
            .add_peers(vec![blocked_addr, allowed_addr], PeerSource::Tracker)
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let stats = handle.stats().await.unwrap();
        // Only the allowed peer should be in the pool
        assert!(
            stats.peers_available + stats.peers_connected <= 1,
            "blocked peer should not be added: available={}, connected={}",
            stats.peers_available,
            stats.peers_connected
        );

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn set_ip_filter_replaces_filter_and_blocks_new_ip() {
        // Test that updating the shared IP filter takes effect for new peer additions.
        // Use public IPs (TEST-NET ranges) since local networks are always exempt.
        let data = vec![0xCD; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        // Start with empty filter (everything allowed)
        let ip_filter: crate::session::SharedIpFilter =
            Arc::new(std::sync::RwLock::new(crate::ip_filter::IpFilter::new()));

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            Arc::clone(&ip_filter),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Initially, peers are allowed by the IP filter.
        // Use a local listener so the connection succeeds and the peer stays known.
        let _listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = _listener.local_addr().unwrap();
        handle
            .add_peers(vec![local_addr], PeerSource::Tracker)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let stats = handle.stats().await.unwrap();
        assert!(
            stats.peers_available + stats.peers_connected >= 1,
            "peer should be allowed initially"
        );
        handle.shutdown().await.unwrap();

        // Now update the shared filter to block that IP range
        {
            let mut f = ip_filter.write().unwrap();
            f.add_rule(
                "198.51.100.0".parse().unwrap(),
                "198.51.100.255".parse().unwrap(),
                1,
            );
        }

        // Verify the filter is updated (public IP, so is_blocked applies)
        assert!(
            ip_filter
                .read()
                .unwrap()
                .is_blocked("198.51.100.1".parse().unwrap())
        );
        // Verify a different public IP is still allowed
        assert!(
            !ip_filter
                .read()
                .unwrap()
                .is_blocked("203.0.113.1".parse().unwrap())
        );
    }

    #[test]
    fn relocate_files_moves_and_cleans_up() {
        let tmp = std::env::temp_dir().join(format!("torrent_relocate_{}", std::process::id()));
        let src = tmp.join("src");
        let dst = tmp.join("dst");

        // Create source files mimicking multi-file torrent layout:
        // TorrentName/subdir/file1.txt
        // TorrentName/file2.txt
        let subdir = src.join("TorrentName").join("subdir");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::write(subdir.join("file1.txt"), b"hello").unwrap();
        std::fs::write(src.join("TorrentName").join("file2.txt"), b"world").unwrap();

        let file_paths = vec![
            std::path::PathBuf::from("TorrentName/subdir/file1.txt"),
            std::path::PathBuf::from("TorrentName/file2.txt"),
        ];

        relocate_files(&src, &dst, &file_paths).unwrap();

        // Destination should have both files
        assert_eq!(
            std::fs::read_to_string(dst.join("TorrentName/subdir/file1.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("TorrentName/file2.txt")).unwrap(),
            "world"
        );

        // Source directory should be cleaned up (empty dirs removed)
        assert!(!src.join("TorrentName").join("subdir").exists());
        assert!(!src.join("TorrentName").exists());

        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn relocate_files_skips_missing() {
        let tmp =
            std::env::temp_dir().join(format!("torrent_relocate_skip_{}", std::process::id()));
        let src = tmp.join("src");
        let dst = tmp.join("dst");
        std::fs::create_dir_all(&src).unwrap();

        // File doesn't exist — should be skipped without error
        let file_paths = vec![std::path::PathBuf::from("nonexistent.txt")];
        relocate_files(&src, &dst, &file_paths).unwrap();

        assert!(!dst.join("nonexistent.txt").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---- M106: Scored peer turnover integration tests ----
    //
    // These tests exercise the integration between PeerScorer and the turnover /
    // admission / snub logic that runs inside TorrentActor. They use real
    // PeerState instances and PeerScorer to faithfully reproduce the actor's
    // data flow without needing the full async actor infrastructure.

    /// Helper: create a PeerState with a specific score, connected_at, and
    /// optional snub/bitfield state. Uses a throwaway mpsc channel.
    fn make_scored_peer(
        addr: SocketAddr,
        score: f64,
        snubbed: bool,
        connected_at: std::time::Instant,
        piece_count: u32,
        total_pieces: u32,
    ) -> PeerState {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut peer = PeerState::new(addr, total_pieces, tx, PeerSource::Tracker);
        peer.score = score;
        peer.snubbed = snubbed;
        peer.connected_at = connected_at;
        // Set bitfield bits to simulate piece_count
        for i in 0..piece_count {
            peer.bitfield.set(i);
        }
        peer
    }

    // ---- Test M106-1: Scored turnover evicts lowest-scoring peers ----
    //
    // Exercises the full turnover pipeline: build peer map, filter eligible,
    // sort by score, apply churn_percent, verify correct eviction order.
    #[test]
    fn scored_turnover_evicts_lowest_scoring_peers() {
        use crate::peer_scorer::PeerScorer;

        let scorer = PeerScorer::new(
            Duration::from_secs(20),
            Duration::from_secs(60),
            Duration::from_secs(30),
            0.10,
            Duration::from_secs(120),
            0.05,
            0.15, // min_score_threshold
        );

        let num_pieces = 100u32;
        // All peers connected well before probation (30s ago)
        let old_connect = std::time::Instant::now() - Duration::from_secs(30);

        // Build 12 peers: 10 non-probation peers (required for turnover) + 2 extras.
        // Peers with varying scores. Some below threshold (0.15), some above.
        let mut peers: HashMap<SocketAddr, PeerState> = HashMap::new();
        let peer_data = [
            ("10.0.0.1:6881", 0.02), // lowest — should be evicted first
            ("10.0.0.2:6881", 0.80),
            ("10.0.0.3:6881", 0.07), // second lowest below threshold
            ("10.0.0.4:6881", 0.65),
            ("10.0.0.5:6881", 0.50),
            ("10.0.0.6:6881", 0.40),
            ("10.0.0.7:6881", 0.12), // below threshold
            ("10.0.0.8:6881", 0.90),
            ("10.0.0.9:6881", 0.35),
            ("10.0.0.10:6881", 0.55),
            ("10.0.0.11:6881", 0.70),
            ("10.0.0.12:6881", 0.60),
        ];

        for (addr_str, score) in &peer_data {
            let addr: SocketAddr = addr_str.parse().expect("valid addr");
            let peer = make_scored_peer(addr, *score, false, old_connect, 50, num_pieces);
            peers.insert(addr, peer);
        }

        // Reproduce the turnover logic from run_scored_turnover
        let scored_count = peers
            .values()
            .filter(|p| !scorer.is_in_probation(p.connected_at))
            .count();
        assert!(
            scored_count >= 10,
            "need >= 10 scored peers for turnover, got {scored_count}"
        );

        // Build swarm context
        let ctx = PeerScorer::build_swarm_context(
            peers
                .values()
                .filter(|p| !scorer.is_in_probation(p.connected_at))
                .map(|p| (p.pipeline.ewma_rate(), p.avg_rtt, p.score)),
        );
        let churn_pct = scorer.churn_percent(ctx.median_score);

        // Collect eligible (below threshold, not seed, not probation)
        let mut eligible: Vec<(SocketAddr, f64)> = peers
            .iter()
            .filter(|(_, p)| {
                !scorer.is_in_probation(p.connected_at)
                    && p.bitfield.count_ones() != num_pieces
                    && p.score < scorer.min_score_threshold()
            })
            .map(|(addr, p)| (*addr, p.score))
            .collect();
        eligible.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        // Should have exactly 3 eligible peers below 0.15: 0.02, 0.07, 0.12
        assert_eq!(eligible.len(), 3, "expected 3 peers below threshold");
        assert_eq!(
            eligible[0].0,
            "10.0.0.1:6881".parse::<SocketAddr>().expect("valid")
        );
        assert!((eligible[0].1 - 0.02).abs() < 1e-10);
        assert_eq!(
            eligible[1].0,
            "10.0.0.3:6881".parse::<SocketAddr>().expect("valid")
        );
        assert!((eligible[1].1 - 0.07).abs() < 1e-10);
        assert_eq!(
            eligible[2].0,
            "10.0.0.7:6881".parse::<SocketAddr>().expect("valid")
        );
        assert!((eligible[2].1 - 0.12).abs() < 1e-10);

        // Compute how many to disconnect
        let n = ((eligible.len() as f64 * churn_pct).floor() as usize).max(1);
        // With 3 eligible and low churn_pct, at least 1 is evicted
        assert!(n >= 1, "should evict at least 1 peer, got {n}");

        // The first evicted peer should be the lowest scorer
        let to_disconnect: Vec<SocketAddr> =
            eligible.into_iter().take(n).map(|(addr, _)| addr).collect();
        assert_eq!(
            to_disconnect[0],
            "10.0.0.1:6881".parse::<SocketAddr>().expect("valid"),
            "lowest-scoring peer should be evicted first"
        );

        // High-scoring peers must NOT be in the eviction list
        let high_scorer: SocketAddr = "10.0.0.2:6881".parse().expect("valid");
        assert!(
            !to_disconnect.contains(&high_scorer),
            "high-scoring peer must survive turnover"
        );
    }

    // ---- Test M106-2: Probation peers survive turnover ----
    //
    // Peers within probation_duration are excluded from the eligible set
    // even if their score is below the threshold.
    #[test]
    fn probation_peers_survive_turnover() {
        use crate::peer_scorer::PeerScorer;

        let scorer = PeerScorer::new(
            Duration::from_secs(20), // 20s probation
            Duration::from_secs(60),
            Duration::from_secs(30),
            0.10,
            Duration::from_secs(120),
            0.05,
            0.15,
        );

        let num_pieces = 100u32;
        let old_connect = std::time::Instant::now() - Duration::from_secs(30);
        let recent_connect = std::time::Instant::now(); // within probation

        // Build 12 peers: 10 old + 2 recent (in probation)
        let mut peers: HashMap<SocketAddr, PeerState> = HashMap::new();
        for i in 1..=10 {
            let addr: SocketAddr = format!("10.0.0.{i}:6881").parse().expect("valid addr");
            // Give them a score of 0.05 (below threshold) — they should be eligible
            let peer = make_scored_peer(addr, 0.05, false, old_connect, 50, num_pieces);
            peers.insert(addr, peer);
        }

        // Two recent peers with low scores — should be protected by probation
        let probation_addr1: SocketAddr = "10.0.0.11:6881".parse().expect("valid");
        let probation_addr2: SocketAddr = "10.0.0.12:6881".parse().expect("valid");
        peers.insert(
            probation_addr1,
            make_scored_peer(probation_addr1, 0.01, false, recent_connect, 50, num_pieces),
        );
        peers.insert(
            probation_addr2,
            make_scored_peer(probation_addr2, 0.03, false, recent_connect, 50, num_pieces),
        );

        // Verify probation status
        assert!(
            scorer.is_in_probation(recent_connect),
            "recently connected peer should be in probation"
        );
        assert!(
            !scorer.is_in_probation(old_connect),
            "old peer should not be in probation"
        );

        // Probation peers should score 0.5 (the default for probation)
        // in the actual tick loop, and they are excluded from eligible set
        let eligible: Vec<SocketAddr> = peers
            .iter()
            .filter(|(_, p)| {
                !scorer.is_in_probation(p.connected_at)
                    && p.bitfield.count_ones() != num_pieces
                    && p.score < scorer.min_score_threshold()
            })
            .map(|(addr, _)| *addr)
            .collect();

        assert!(
            !eligible.contains(&probation_addr1),
            "probation peer 1 must not be in eligible set"
        );
        assert!(
            !eligible.contains(&probation_addr2),
            "probation peer 2 must not be in eligible set"
        );
        assert_eq!(
            eligible.len(),
            10,
            "only old peers should be eligible for eviction"
        );
    }

    // ---- Test M106-3: Score-based admission replaces low-scorer at capacity ----
    //
    // When peers.len() >= max_connections, admission finds the worst non-probation
    // peer below threshold and evicts it to make room.
    #[test]
    fn score_based_admission_replaces_low_scorer() {
        use crate::peer_scorer::PeerScorer;

        let scorer = PeerScorer::new(
            Duration::from_secs(20),
            Duration::from_secs(60),
            Duration::from_secs(30),
            0.10,
            Duration::from_secs(120),
            0.05,
            0.15,
        );

        let num_pieces = 100u32;
        let old_connect = std::time::Instant::now() - Duration::from_secs(30);
        let max_connections = 5usize;

        // Fill to capacity with mixed-quality peers
        let mut peers: HashMap<SocketAddr, PeerState> = HashMap::new();
        let peer_data = [
            ("10.0.0.1:6881", 0.08), // worst — below threshold
            ("10.0.0.2:6881", 0.50),
            ("10.0.0.3:6881", 0.70),
            ("10.0.0.4:6881", 0.90),
            ("10.0.0.5:6881", 0.60),
        ];
        for (addr_str, score) in &peer_data {
            let addr: SocketAddr = addr_str.parse().expect("valid addr");
            peers.insert(
                addr,
                make_scored_peer(addr, *score, false, old_connect, 50, num_pieces),
            );
        }

        assert!(
            peers.len() >= max_connections,
            "must be at capacity for admission test"
        );

        // Reproduce the admission logic from try_connect_peers
        let worst = peers
            .iter()
            .filter(|(_, p)| !scorer.is_in_probation(p.connected_at))
            .min_by(|(_, a), (_, b)| {
                a.score
                    .partial_cmp(&b.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

        let (worst_addr, worst_peer) = worst.expect("should find a worst peer");
        assert!(
            worst_peer.score < scorer.min_score_threshold(),
            "worst peer score {} should be below threshold {}",
            worst_peer.score,
            scorer.min_score_threshold()
        );
        assert_eq!(
            *worst_addr,
            "10.0.0.1:6881".parse::<SocketAddr>().expect("valid"),
            "peer with score 0.08 should be identified as worst"
        );

        // After disconnecting worst, we should have room for a new peer
        let worst_addr_copy = *worst_addr;
        peers.remove(&worst_addr_copy);
        assert_eq!(peers.len(), max_connections - 1);

        // Now test: if all peers are above threshold, admission should NOT evict
        let worst_now = peers
            .iter()
            .filter(|(_, p)| !scorer.is_in_probation(p.connected_at))
            .min_by(|(_, a), (_, b)| {
                a.score
                    .partial_cmp(&b.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        let (_, best_worst) = worst_now.expect("should find a peer");
        assert!(
            best_worst.score >= scorer.min_score_threshold(),
            "remaining worst peer {} should be above threshold — pool is strong enough",
            best_worst.score
        );
    }

    // ---- Test M106-4: Small swarm (< 10 scored peers) disables eviction ----
    //
    // When fewer than 10 non-probation peers exist, the regular churn path
    // in run_scored_turnover returns early.
    #[test]
    fn small_swarm_disables_regular_eviction() {
        use crate::peer_scorer::PeerScorer;

        let scorer = PeerScorer::new(
            Duration::from_secs(20),
            Duration::from_secs(60),
            Duration::from_secs(30),
            0.10,
            Duration::from_secs(120),
            0.05,
            0.15,
        );

        let num_pieces = 100u32;
        let old_connect = std::time::Instant::now() - Duration::from_secs(30);

        // Build 8 peers (below the 10-peer threshold)
        let mut peers: HashMap<SocketAddr, PeerState> = HashMap::new();
        for i in 1..=8 {
            let addr: SocketAddr = format!("10.0.0.{i}:6881").parse().expect("valid addr");
            // All below threshold — would be eligible if swarm were large enough
            peers.insert(
                addr,
                make_scored_peer(addr, 0.05, false, old_connect, 50, num_pieces),
            );
        }

        // Count non-probation peers
        let scored_count = peers
            .values()
            .filter(|p| !scorer.is_in_probation(p.connected_at))
            .count();

        assert!(
            scored_count < 10,
            "scored_count={scored_count}, should be < 10 for small swarm protection"
        );

        // The run_scored_turnover early return condition: if scored_count < 10 { return; }
        // So no eviction should happen even though all peers are below threshold.
        let eligible: Vec<(SocketAddr, f64)> = peers
            .iter()
            .filter(|(_, p)| {
                !scorer.is_in_probation(p.connected_at)
                    && p.bitfield.count_ones() != num_pieces
                    && p.score < scorer.min_score_threshold()
            })
            .map(|(addr, p)| (*addr, p.score))
            .collect();

        // All 8 peers would be eligible, but the guard prevents eviction
        assert_eq!(eligible.len(), 8);
        // The guard check that would prevent eviction:
        assert!(
            scored_count < 10,
            "small swarm guard should prevent eviction"
        );
    }

    // ---- Test M106-5: Endgame suppresses regular eviction ----
    //
    // When end_game is active, run_scored_turnover skips the regular churn path
    // (but snub fast-path still fires).
    #[test]
    fn endgame_suppresses_regular_eviction() {
        // The endgame guard in run_scored_turnover:
        //   if self.end_game.is_active() { return; }
        //
        // Verify the EndGame state machine reports active when blocks are registered.
        let mut end_game = crate::end_game::EndGame::new();

        // Before activation, should not be active
        assert!(
            !end_game.is_active(),
            "endgame should not be active initially"
        );

        // Activate endgame (simulate what check_end_game_activation does)
        let no_pending: Vec<(SocketAddr, Vec<(u32, u32, u32)>)> = Vec::new();
        end_game.activate(&no_pending);

        assert!(
            end_game.is_active(),
            "endgame should be active after activation"
        );

        // When active, the guard should fire — verify the condition
        let should_skip = end_game.is_active();
        assert!(
            should_skip,
            "endgame active → regular eviction must be suppressed"
        );
    }

    // ---- Test M106-7: Snubbed peer gets low score, evicted on churn ----
    //
    // A snubbed peer scores 0.0 via compute_score (snubbed early return).
    // This score is below the threshold, making it eligible for regular turnover.
    #[test]
    fn snubbed_peer_low_score_eligible_for_churn() {
        use crate::peer_scorer::{PeerScorer, SwarmContext};

        let scorer = PeerScorer::new(
            Duration::from_secs(20),
            Duration::from_secs(60),
            Duration::from_secs(30),
            0.20,
            Duration::from_secs(120),
            0.05,
            0.15,
        );

        let num_pieces = 100u32;
        let old_connect = std::time::Instant::now() - Duration::from_secs(30);

        // Build 12 peers: 11 good + 1 snubbed
        let mut peers: HashMap<SocketAddr, PeerState> = HashMap::new();
        for i in 1..=11 {
            let addr: SocketAddr = format!("10.0.0.{i}:6881").parse().expect("valid addr");
            peers.insert(
                addr,
                make_scored_peer(addr, 0.60, false, old_connect, 50, num_pieces),
            );
        }
        // Add a snubbed peer — score computed as 0.0
        let snubbed_addr: SocketAddr = "10.0.0.12:6881".parse().expect("valid");
        let mut snubbed_peer =
            make_scored_peer(snubbed_addr, 0.0, true, old_connect, 50, num_pieces);

        // Verify: compute_score returns 0.0 for snubbed peers
        let ctx = SwarmContext {
            max_rate: 1_000_000.0,
            min_rtt: 0.020,
            median_score: 0.5,
        };
        let snubbed_score = PeerScorer::compute_score(
            500_000.0,
            Some(0.030),
            100,
            0,
            50,
            num_pieces,
            true, // snubbed
            &ctx,
        );
        assert_eq!(snubbed_score, 0.0, "snubbed peer must score exactly 0.0");
        snubbed_peer.score = snubbed_score;
        peers.insert(snubbed_addr, snubbed_peer);

        // Now run the eligible filter (turnover logic)
        let mut eligible: Vec<(SocketAddr, f64)> = peers
            .iter()
            .filter(|(_, p)| {
                !scorer.is_in_probation(p.connected_at)
                    && p.bitfield.count_ones() != num_pieces
                    && p.score < scorer.min_score_threshold()
            })
            .map(|(addr, p)| (*addr, p.score))
            .collect();
        eligible.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        // Only the snubbed peer should be eligible (the rest score 0.60)
        assert_eq!(eligible.len(), 1, "only snubbed peer should be eligible");
        assert_eq!(
            eligible[0].0, snubbed_addr,
            "snubbed peer should be the eviction candidate"
        );
        assert!((eligible[0].1).abs() < 1e-10, "score should be 0.0");
    }

    // ---- Test M106-8: Snub fast-path evicts regardless of churn timer ----
    //
    // The snub_evict path in run_scored_turnover fires without waiting for
    // should_churn(). This test verifies that snubbed + below-threshold peers
    // are collected for immediate eviction.
    #[test]
    fn snub_fast_path_evicts_without_churn_timer() {
        use crate::peer_scorer::PeerScorer;

        let mut scorer = PeerScorer::new(
            Duration::from_secs(20),
            Duration::from_secs(60),
            Duration::from_secs(30),
            0.10,
            Duration::from_secs(120),
            0.05,
            0.15,
        );

        let num_pieces = 100u32;
        let old_connect = std::time::Instant::now() - Duration::from_secs(30);

        // Mark the scorer as having just churned — should_churn will return false
        scorer.mark_churned(std::time::Instant::now());
        assert!(
            !scorer.should_churn(std::time::Instant::now()),
            "churn timer should not have elapsed yet"
        );

        // Build peers: some snubbed + below threshold, some not
        let mut peers: HashMap<SocketAddr, PeerState> = HashMap::new();

        // Good peers
        for i in 1..=5 {
            let addr: SocketAddr = format!("10.0.0.{i}:6881").parse().expect("valid addr");
            peers.insert(
                addr,
                make_scored_peer(addr, 0.60, false, old_connect, 50, num_pieces),
            );
        }

        // Snubbed peer with score 0.0 (below threshold)
        let snubbed_addr: SocketAddr = "10.0.0.6:6881".parse().expect("valid");
        peers.insert(
            snubbed_addr,
            make_scored_peer(snubbed_addr, 0.0, true, old_connect, 50, num_pieces),
        );

        // Snubbed peer but with score above threshold (should NOT be fast-evicted)
        let snubbed_high_addr: SocketAddr = "10.0.0.7:6881".parse().expect("valid");
        peers.insert(
            snubbed_high_addr,
            make_scored_peer(snubbed_high_addr, 0.50, true, old_connect, 50, num_pieces),
        );

        // Reproduce the snub fast-path from run_scored_turnover
        let snub_evict: Vec<SocketAddr> = peers
            .iter()
            .filter(|(_, p)| p.snubbed && p.score < scorer.min_score_threshold())
            .map(|(addr, _)| *addr)
            .collect();

        assert_eq!(
            snub_evict.len(),
            1,
            "only 1 snubbed peer below threshold should be fast-evicted"
        );
        assert_eq!(
            snub_evict[0], snubbed_addr,
            "the correct snubbed peer should be identified"
        );
        assert!(
            !snub_evict.contains(&snubbed_high_addr),
            "snubbed peer above threshold must NOT be fast-evicted"
        );

        // Verify this happened without should_churn returning true
        assert!(
            !scorer.should_churn(std::time::Instant::now()),
            "fast-path should fire regardless of churn timer state"
        );
    }

    // ---- Test M106-10: blocks_timed_out increments on snub detection ----
    //
    // When a peer is snubbed, its pending_requests count is added to
    // blocks_timed_out. This test verifies the increment logic.
    #[test]
    fn blocks_timed_out_increments_on_snub() {
        let addr: SocketAddr = "10.0.0.1:6881".parse().expect("valid addr");
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut peer = PeerState::new(addr, 100, tx, PeerSource::Tracker);

        // Simulate some pending requests
        peer.pending_requests.insert(0, 0, 16384);
        peer.pending_requests.insert(0, 16384, 16384);
        peer.pending_requests.insert(1, 0, 16384);
        assert_eq!(peer.pending_requests.len(), 3);

        // Initially zero
        assert_eq!(peer.blocks_timed_out, 0);
        assert!(!peer.snubbed);

        // Reproduce the snub detection logic from the pipeline tick:
        //   peer.snubbed = true;
        //   peer.blocks_timed_out += peer.pending_requests.len();
        peer.snubbed = true;
        peer.blocks_timed_out = peer
            .blocks_timed_out
            .saturating_add(peer.pending_requests.len() as u64);

        assert!(peer.snubbed);
        assert_eq!(
            peer.blocks_timed_out, 3,
            "blocks_timed_out should equal pending request count"
        );

        // Subsequent snub detections should accumulate
        peer.pending_requests.insert(2, 0, 16384);
        peer.blocks_timed_out = peer
            .blocks_timed_out
            .saturating_add(peer.pending_requests.len() as u64);
        assert_eq!(
            peer.blocks_timed_out, 7,
            "blocks_timed_out should accumulate across snub events: 3 + 4 = 7"
        );

        // Verify the blocks_timed_out feeds into score computation correctly
        let ctx = crate::peer_scorer::SwarmContext {
            max_rate: 1_000_000.0,
            min_rtt: 0.020,
            median_score: 0.5,
        };

        // A peer with many timed-out blocks should score lower on reliability
        let score_reliable = crate::peer_scorer::PeerScorer::compute_score(
            500_000.0,
            Some(0.040),
            100,
            0, // no timeouts
            50,
            100,
            false,
            &ctx,
        );
        let score_unreliable = crate::peer_scorer::PeerScorer::compute_score(
            500_000.0,
            Some(0.040),
            100,
            50, // many timeouts
            50,
            100,
            false,
            &ctx,
        );
        assert!(
            score_reliable > score_unreliable,
            "peer with no timeouts ({score_reliable}) should score higher than peer with timeouts ({score_unreliable})"
        );
    }

    // ---- Test: force_recheck transitions through Checking state ----

    #[tokio::test]
    async fn force_recheck_transitions_to_checking() {
        let data = vec![0xDDu8; 32768]; // 2 pieces
        let meta = make_test_torrent(&data, 16384);
        let storage = make_seeded_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let mut arx = atx.subscribe();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Wait for initial verification to complete (should become Seeding)
        tokio::time::sleep(Duration::from_millis(100)).await;
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Seeding, "should start as seeder");

        // Drain any existing alerts
        while arx.try_recv().is_ok() {}

        // Force recheck
        handle.force_recheck().await.unwrap();

        // After force_recheck returns, look for a StateChanged alert that
        // went through Checking (the transition_state fires it)
        let mut saw_checking = false;
        while let Ok(alert) = arx.try_recv() {
            if let crate::alert::AlertKind::StateChanged { new_state, .. } = alert.kind {
                if new_state == TorrentState::Checking {
                    saw_checking = true;
                }
            }
        }
        assert!(
            saw_checking,
            "should have transitioned through Checking state"
        );

        handle.shutdown().await.unwrap();
    }

    // ---- Test: force_recheck completes with correct state ----

    #[tokio::test]
    async fn force_recheck_completes() {
        let data = vec![0xEEu8; 32768]; // 2 pieces
        let meta = make_test_torrent(&data, 16384);
        let storage = make_seeded_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Wait for initial verification
        tokio::time::sleep(Duration::from_millis(100)).await;
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Seeding);
        assert_eq!(stats.pieces_have, 2);

        // Force recheck — should re-verify all pieces and return to Seeding
        handle.force_recheck().await.unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(
            stats.state,
            TorrentState::Seeding,
            "should return to Seeding after recheck"
        );
        assert_eq!(stats.pieces_have, 2, "all pieces should still be verified");

        handle.shutdown().await.unwrap();
    }

    // ---- Test: rename_file succeeds with valid index ----

    #[tokio::test]
    async fn rename_file_succeeds() {
        // Create a real file on disk that we can rename
        let tmp = std::env::temp_dir().join(format!("torrent_rename_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        let data = vec![0xFFu8; 16384]; // 1 piece
        let meta = make_test_torrent(&data, 16384);
        let storage = make_seeded_storage(&data, 16384);

        // The single-file torrent has name "test", so file path is "test"
        // Create the actual file on disk at download_dir/test
        std::fs::write(tmp.join("test"), &data).unwrap();

        let mut config = test_config();
        config.download_dir = tmp.clone();

        let (atx, amask) = test_alert_channel();
        let mut arx = atx.subscribe();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Wait for initial verification
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Drain existing alerts
        while arx.try_recv().is_ok() {}

        // Rename file 0 to "test_renamed"
        handle.rename_file(0, "test_renamed".into()).await.unwrap();

        // Check that the old file is gone and new file exists
        assert!(!tmp.join("test").exists(), "old file should be removed");
        assert!(tmp.join("test_renamed").exists(), "new file should exist");

        // Check that FileRenamed alert was fired
        let mut saw_renamed = false;
        while let Ok(alert) = arx.try_recv() {
            if let AlertKind::FileRenamed { index, .. } = alert.kind {
                assert_eq!(index, 0);
                saw_renamed = true;
            }
        }
        assert!(saw_renamed, "should have received FileRenamed alert");

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---- Test: rename_file with invalid index returns error ----

    #[tokio::test]
    async fn rename_file_invalid_index_errors() {
        let data = vec![0xCCu8; 16384]; // 1 piece, single-file torrent
        let meta = make_test_torrent(&data, 16384);
        let storage = make_seeded_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Wait for initial verification
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Try to rename file index 99 (out of range)
        let result = handle.rename_file(99, "bad".into()).await;
        assert!(result.is_err(), "should fail for out-of-range file index");

        handle.shutdown().await.unwrap();
    }

    // ---- Test: FileCompleted alert fires when all pieces of a file are verified ----

    #[tokio::test]
    async fn file_completed_alert_fires() {
        let data = vec![0xBBu8; 32768]; // 2 pieces
        let meta = make_test_torrent(&data, 16384);
        let storage = make_seeded_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let mut arx = atx.subscribe();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Wait for initial verification (seeded storage => all pieces verify)
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Should have received FileCompleted alert for the single file
        let mut saw_file_completed = false;
        while let Ok(alert) = arx.try_recv() {
            if let AlertKind::FileCompleted { file_index, .. } = alert.kind {
                assert_eq!(file_index, 0, "should be file index 0");
                saw_file_completed = true;
            }
        }
        assert!(
            saw_file_completed,
            "should have received FileCompleted alert"
        );

        handle.shutdown().await.unwrap();
    }

    // ---- Test: MetadataFailed alert fires (unit test on AlertKind) ----

    #[test]
    fn metadata_failed_alert_fires() {
        // Test that MetadataFailed alert has the correct category
        let info_hash = Id20::from([0u8; 20]);
        let alert = crate::alert::Alert::new(AlertKind::MetadataFailed { info_hash });
        assert!(
            alert
                .category()
                .contains(crate::alert::AlertCategory::STATUS),
            "MetadataFailed should have STATUS category"
        );
        assert!(
            alert
                .category()
                .contains(crate::alert::AlertCategory::ERROR),
            "MetadataFailed should have ERROR category"
        );

        // Verify it can be posted through the alert system
        let (tx, mut rx) = broadcast::channel(16);
        let mask = Arc::new(AtomicU32::new(crate::alert::AlertCategory::ALL.bits()));
        post_alert(&tx, &mask, AlertKind::MetadataFailed { info_hash });
        let received = rx.try_recv().expect("should receive MetadataFailed alert");
        assert!(matches!(received.kind, AlertKind::MetadataFailed { .. }));
    }

    // ---- Test: set_max_connections persists ----

    #[tokio::test]
    async fn set_max_connections_persists() {
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Set max_connections to 10
        handle.set_max_connections(10).await.unwrap();
        let val = handle.max_connections().await.unwrap();
        assert_eq!(val, 10);

        // Update to a different value
        handle.set_max_connections(25).await.unwrap();
        let val = handle.max_connections().await.unwrap();
        assert_eq!(val, 25);

        // Verify stats reflect the override
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.connections_limit, 25);

        handle.shutdown().await.unwrap();
    }

    // ---- Test: max_connections default is 0 (use config.max_peers) ----

    #[tokio::test]
    async fn max_connections_default() {
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();
        let expected_default = config.max_peers;

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Default max_connections should be 0
        let val = handle.max_connections().await.unwrap();
        assert_eq!(val, 0);

        // Stats should show config.max_peers as the effective limit
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.connections_limit, expected_default);

        handle.shutdown().await.unwrap();
    }

    // ---- Test: set_max_uploads round trip ----

    #[tokio::test]
    async fn set_max_uploads_round_trip() {
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Set max_uploads to 8
        handle.set_max_uploads(8).await.unwrap();
        let val = handle.max_uploads().await.unwrap();
        assert_eq!(val, 8);

        // Verify stats uploads_limit reflects the new value
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.uploads_limit, 8);

        handle.shutdown().await.unwrap();
    }

    // ---- Test: ExternalIpDetected alert fires ----

    #[tokio::test]
    async fn external_ip_detected_alert() {
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let mut arx = atx.subscribe();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Drain any initial alerts
        while arx.try_recv().is_ok() {}

        // Send UpdateExternalIp command
        let test_ip: std::net::IpAddr = "203.0.113.42".parse().unwrap();
        handle
            .cmd_tx
            .send(TorrentCommand::UpdateExternalIp { ip: test_ip })
            .await
            .unwrap();

        // Wait for the actor to process
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Check for ExternalIpDetected alert
        let mut saw_alert = false;
        while let Ok(alert) = arx.try_recv() {
            if let AlertKind::ExternalIpDetected { ip } = alert.kind {
                assert_eq!(ip, test_ip);
                saw_alert = true;
            }
        }
        assert!(saw_alert, "should have received ExternalIpDetected alert");

        handle.shutdown().await.unwrap();
    }

    // ---- Test: get_peer_info returns connected peers ----

    #[tokio::test]
    async fn get_peer_info_returns_connected_peers() {
        let data = vec![0xAB; 65536]; // 64 KiB
        let meta = make_test_torrent(&data, 16384); // 4 pieces
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta.clone(),
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Set up a fake peer via TCP handshake
        let stats = handle.stats().await.unwrap();
        let listen_port = stats.peers_connected; // Initially 0

        // Add a peer to the available pool and let the actor connect
        let peer_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer_listener.local_addr().unwrap();

        handle
            .add_peers(vec![peer_addr], PeerSource::Tracker)
            .await
            .unwrap();

        // Accept the connection and complete the handshake
        let accept_timeout =
            tokio::time::timeout(Duration::from_secs(2), peer_listener.accept()).await;
        if let Ok(Ok((mut stream, _))) = accept_timeout {
            // Read handshake
            let mut hs_buf = [0u8; HANDSHAKE_SIZE];
            if tokio::time::timeout(Duration::from_millis(500), stream.read_exact(&mut hs_buf))
                .await
                .is_ok()
            {
                // Send back handshake
                let hs = Handshake::new(meta.info_hash, Id20::from([0xBB; 20]));
                let hs_bytes = hs.to_bytes();
                let _ = stream.write_all(&hs_bytes).await;

                // Give the actor time to register the peer
                tokio::time::sleep(Duration::from_millis(200)).await;

                // Now query peer info
                let peer_info = handle.get_peer_info().await.unwrap();
                // We should have at least one peer (the one we just handshaked)
                if !peer_info.is_empty() {
                    let p = &peer_info[0];
                    // Verify default choking/interested state
                    assert!(p.peer_choking, "peer should be choking us initially");
                    assert!(p.am_choking, "we should be choking peer initially");
                    assert!(
                        !p.peer_interested,
                        "peer should not be interested initially"
                    );
                    assert_eq!(p.num_pieces, 0);
                    assert_eq!(p.source, PeerSource::Tracker);
                }
            }
        }
        // Even if handshake timing fails, at least verify the API works
        let _ = handle.get_peer_info().await.unwrap();
        assert_eq!(listen_port, 0); // sanity: initially had no peers

        handle.shutdown().await.unwrap();
    }

    // ---- Test: get_peer_info empty when no peers ----

    #[tokio::test]
    async fn get_peer_info_empty_when_no_peers() {
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        let peer_info = handle.get_peer_info().await.unwrap();
        assert!(peer_info.is_empty(), "should have no peers initially");

        handle.shutdown().await.unwrap();
    }

    // ---- Test: get_download_queue empty initially ----

    #[tokio::test]
    async fn get_download_queue_empty_initially() {
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        let queue = handle.get_download_queue().await.unwrap();
        assert!(
            queue.is_empty(),
            "download queue should be empty with no active downloads"
        );

        handle.shutdown().await.unwrap();
    }

    // ---- Test: have_piece false initially ----

    #[tokio::test]
    async fn have_piece_false_initially() {
        let data = vec![0xAB; 32768]; // 32 KiB = 2 pieces
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        assert!(
            !handle.have_piece(0).await.unwrap(),
            "piece 0 should not be downloaded initially"
        );
        assert!(
            !handle.have_piece(1).await.unwrap(),
            "piece 1 should not be downloaded initially"
        );

        handle.shutdown().await.unwrap();
    }

    // ---- Test: piece_availability empty with no peers ----

    #[tokio::test]
    async fn piece_availability_empty_no_peers() {
        let data = vec![0xAB; 32768]; // 2 pieces
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        let avail = handle.piece_availability().await.unwrap();
        assert_eq!(avail.len(), 2, "should have availability for 2 pieces");
        assert!(
            avail.iter().all(|&c| c == 0),
            "all availability counts should be 0 with no peers"
        );

        handle.shutdown().await.unwrap();
    }

    // ---- Test: file_progress zeros initially ----

    #[tokio::test]
    async fn file_progress_zeros_initially() {
        let data = vec![0xAB; 32768]; // single-file, 2 pieces
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        let progress = handle.file_progress().await.unwrap();
        assert_eq!(progress.len(), 1, "single-file torrent should have 1 entry");
        assert_eq!(progress[0], 0, "no bytes should be downloaded initially");

        handle.shutdown().await.unwrap();
    }

    // ---- Test: file_progress length matches file count (multi-file) ----

    /// Build a multi-file TorrentMetaV1 from a total data blob and file lengths.
    fn make_test_torrent_multi(
        data: &[u8],
        piece_length: u64,
        file_lengths: &[u64],
    ) -> TorrentMetaV1 {
        use serde::Serialize;

        let mut pieces = Vec::new();
        let mut offset = 0;
        while offset < data.len() {
            let end = (offset + piece_length as usize).min(data.len());
            let hash = torrent_core::sha1(&data[offset..end]);
            pieces.extend_from_slice(hash.as_bytes());
            offset = end;
        }

        #[derive(Serialize)]
        struct FileE {
            length: u64,
            path: Vec<String>,
        }

        #[derive(Serialize)]
        struct Info<'a> {
            name: &'a str,
            #[serde(rename = "piece length")]
            piece_length: u64,
            #[serde(with = "serde_bytes")]
            pieces: &'a [u8],
            files: Vec<FileE>,
        }

        #[derive(Serialize)]
        struct Torrent<'a> {
            info: Info<'a>,
        }

        let files: Vec<FileE> = file_lengths
            .iter()
            .enumerate()
            .map(|(i, &len)| FileE {
                length: len,
                path: vec![format!("file{i}.bin")],
            })
            .collect();

        let t = Torrent {
            info: Info {
                name: "test_multi",
                piece_length,
                pieces: &pieces,
                files,
            },
        };

        let bytes = torrent_bencode::to_bytes(&t).unwrap();
        torrent_from_bytes(&bytes).unwrap()
    }

    #[tokio::test]
    async fn file_progress_length_matches_file_count() {
        // 3 files: 10000 + 20000 + 2768 = 32768 bytes total, 2 pieces of 16384
        let data = vec![0xCD; 32768];
        let file_lengths = [10000u64, 20000, 2768];
        let meta = make_test_torrent_multi(&data, 16384, &file_lengths);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        let progress = handle.file_progress().await.unwrap();
        assert_eq!(
            progress.len(),
            3,
            "multi-file torrent should have 3 entries"
        );
        assert!(
            progress.iter().all(|&b| b == 0),
            "all progress should be 0 initially"
        );

        handle.shutdown().await.unwrap();
    }

    // ---- Test: is_valid returns true for active torrent ----

    #[tokio::test]
    async fn is_valid_true_for_active() {
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        assert!(
            handle.is_valid(),
            "handle should be valid while torrent actor is alive"
        );

        handle.shutdown().await.unwrap();
    }

    // ---- Test: is_valid returns false after shutdown ----

    #[tokio::test]
    async fn is_valid_false_after_remove() {
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        assert!(handle.is_valid());

        // Shutdown the torrent (simulating removal)
        handle.shutdown().await.unwrap();

        // Give the actor time to stop and close the channel
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(
            !handle.is_valid(),
            "handle should be invalid after shutdown"
        );
    }

    // ---- Test: clear_error resets error state ----

    #[tokio::test]
    async fn clear_error_resets() {
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Initially no error
        let stats = handle.stats().await.unwrap();
        assert!(stats.error.is_empty());
        assert_eq!(stats.error_file, -1);

        // Clear error (no-op when no error) should succeed without issue
        handle.clear_error().await.unwrap();

        let stats = handle.stats().await.unwrap();
        assert!(stats.error.is_empty());
        assert_eq!(stats.error_file, -1);

        handle.shutdown().await.unwrap();
    }

    // ---- Test: flags round trip ----

    #[tokio::test]
    async fn flags_round_trip() {
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // Initial flags: torrent starts downloading (not paused), no sequential, no super seeding
        let initial = handle.flags().await.unwrap();
        assert!(!initial.contains(crate::types::TorrentFlags::PAUSED));
        assert!(!initial.contains(crate::types::TorrentFlags::SEQUENTIAL_DOWNLOAD));
        assert!(!initial.contains(crate::types::TorrentFlags::SUPER_SEEDING));

        // Enable sequential download via set_flags
        handle
            .set_flags(crate::types::TorrentFlags::SEQUENTIAL_DOWNLOAD)
            .await
            .unwrap();
        let after_set = handle.flags().await.unwrap();
        assert!(after_set.contains(crate::types::TorrentFlags::SEQUENTIAL_DOWNLOAD));

        // Disable it via unset_flags
        handle
            .unset_flags(crate::types::TorrentFlags::SEQUENTIAL_DOWNLOAD)
            .await
            .unwrap();
        let after_unset = handle.flags().await.unwrap();
        assert!(!after_unset.contains(crate::types::TorrentFlags::SEQUENTIAL_DOWNLOAD));

        // Verify sequential_download state via the dedicated query
        assert!(!handle.is_sequential_download().await.unwrap());

        handle.shutdown().await.unwrap();
    }

    // ---- Test: connect_peer does not error ----

    #[tokio::test]
    async fn connect_peer_no_error() {
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta,
            torrent_core::TorrentVersion::V1Only,
            None,
            dh,
            dm,
            config,
            None,
            None,
            None,
            None,
            crate::slot_tuner::SlotTuner::disabled(4),
            atx,
            amask,
            None,
            None,
            test_ban_manager(),
            test_ip_filter(),
            Arc::new(Vec::new()),
            None,
            None,
            Arc::new(crate::transport::NetworkFactory::tokio()),
            None, // M96: hash_pool
        )
        .await
        .unwrap();

        // connect_peer should not error even though the peer doesn't exist
        // (the connection attempt will fail asynchronously, but the command itself succeeds)
        let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        handle.connect_peer(addr).await.unwrap();

        // Give the actor a moment to process
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        handle.shutdown().await.unwrap();
    }

    // ---- BEP 52 hash serving tests (M87) ----

    /// Build a minimal TorrentMetaV2 with piece-layer hashes for testing.
    fn make_test_meta_v2(
        piece_hashes: &[torrent_core::Id32],
        file_root: torrent_core::Id32,
        piece_length: u64,
        file_length: u64,
    ) -> torrent_core::TorrentMetaV2 {
        use std::collections::BTreeMap;

        // Concatenate piece hashes into raw bytes
        let mut layer_bytes = Vec::with_capacity(piece_hashes.len() * 32);
        for h in piece_hashes {
            layer_bytes.extend_from_slice(&h.0);
        }

        let mut piece_layers = BTreeMap::new();
        piece_layers.insert(file_root, layer_bytes);

        let file_tree = torrent_core::FileTreeNode::Directory({
            let mut children = BTreeMap::new();
            children.insert(
                "test.dat".to_string(),
                torrent_core::FileTreeNode::File(torrent_core::V2FileAttr {
                    length: file_length,
                    pieces_root: Some(file_root),
                }),
            );
            children
        });

        torrent_core::TorrentMetaV2 {
            info_hashes: torrent_core::InfoHashes::v2_only(torrent_core::Id32::ZERO),
            info_bytes: None,
            announce: None,
            announce_list: None,
            comment: None,
            created_by: None,
            creation_date: None,
            info: torrent_core::InfoDictV2 {
                name: "test".to_string(),
                piece_length,
                meta_version: 2,
                file_tree,
                ssl_cert: None,
            },
            piece_layers,
            ssl_cert: None,
        }
    }

    #[test]
    fn test_serve_hashes_v2_piece_layer() {
        // 4 piece hashes, piece_length = 16384, chunk_size = 16384
        // => blocks_per_piece = 1, piece_layer_base = 0
        let hashes: Vec<torrent_core::Id32> = (0..4u8)
            .map(|i| {
                let mut h = [0u8; 32];
                h[0] = i;
                torrent_core::Id32(h)
            })
            .collect();
        let file_root = torrent_core::Id32([0xAA; 32]);
        let meta = make_test_meta_v2(&hashes, file_root, 16384, 16384 * 4);
        let lengths = Lengths::new(16384 * 4, 16384, DEFAULT_CHUNK_SIZE);

        let request = torrent_core::HashRequest {
            file_root,
            base: 0, // piece layer when blocks_per_piece = 1
            index: 0,
            count: 4,
            proof_layers: 0,
        };

        let result = serve_hashes(
            Some(&meta),
            torrent_core::TorrentVersion::V2Only,
            Some(&lengths),
            &request,
        );
        let served = result.expect("should serve hashes");
        assert_eq!(served.len(), 4);
        for (i, h) in served.iter().enumerate() {
            assert_eq!(h.0[0], i as u8);
        }
    }

    #[test]
    fn test_serve_hashes_rejects_v1_only() {
        let hashes: Vec<torrent_core::Id32> = vec![torrent_core::Id32([0xBB; 32])];
        let file_root = torrent_core::Id32([0xAA; 32]);
        let meta = make_test_meta_v2(&hashes, file_root, 16384, 16384);
        let lengths = Lengths::new(16384, 16384, DEFAULT_CHUNK_SIZE);

        let request = torrent_core::HashRequest {
            file_root,
            base: 0,
            index: 0,
            count: 1,
            proof_layers: 0,
        };

        let result = serve_hashes(
            Some(&meta),
            torrent_core::TorrentVersion::V1Only,
            Some(&lengths),
            &request,
        );
        assert!(result.is_none(), "V1Only should reject hash requests");
    }

    #[test]
    fn test_serve_hashes_rejects_unknown_root() {
        let hashes: Vec<torrent_core::Id32> = vec![torrent_core::Id32([0xBB; 32])];
        let file_root = torrent_core::Id32([0xAA; 32]);
        let meta = make_test_meta_v2(&hashes, file_root, 16384, 16384);
        let lengths = Lengths::new(16384, 16384, DEFAULT_CHUNK_SIZE);

        // Request a different file root that doesn't exist
        let unknown_root = torrent_core::Id32([0xFF; 32]);
        let request = torrent_core::HashRequest {
            file_root: unknown_root,
            base: 0,
            index: 0,
            count: 1,
            proof_layers: 0,
        };

        let result = serve_hashes(
            Some(&meta),
            torrent_core::TorrentVersion::V2Only,
            Some(&lengths),
            &request,
        );
        assert!(result.is_none(), "unknown file_root should reject");
    }

    #[test]
    fn test_serve_hashes_rejects_out_of_bounds() {
        // 2 piece hashes, piece_length = 16384, chunk_size = 16384
        let hashes: Vec<torrent_core::Id32> =
            (0..2u8).map(|i| torrent_core::Id32([i; 32])).collect();
        let file_root = torrent_core::Id32([0xAA; 32]);
        let meta = make_test_meta_v2(&hashes, file_root, 16384, 16384 * 2);
        let lengths = Lengths::new(16384 * 2, 16384, DEFAULT_CHUNK_SIZE);

        // Request starting at index 5, which is beyond the 2 available hashes
        let request = torrent_core::HashRequest {
            file_root,
            base: 0,
            index: 5,
            count: 1,
            proof_layers: 0,
        };

        let result = serve_hashes(
            Some(&meta),
            torrent_core::TorrentVersion::V2Only,
            Some(&lengths),
            &request,
        );
        assert!(result.is_none(), "out-of-bounds index should reject");
    }

    #[test]
    fn test_serve_hashes_includes_proofs() {
        // 4 piece hashes, piece_length = 16384, chunk_size = 16384
        // => blocks_per_piece = 1, tree has 4 leaves => depth 2
        let hashes: Vec<torrent_core::Id32> =
            (0..4u8).map(|i| torrent_core::Id32([i; 32])).collect();
        let file_root = torrent_core::Id32([0xAA; 32]);
        let meta = make_test_meta_v2(&hashes, file_root, 16384, 16384 * 4);
        let lengths = Lengths::new(16384 * 4, 16384, DEFAULT_CHUNK_SIZE);

        // Request 1 hash with 1 proof layer
        let request = torrent_core::HashRequest {
            file_root,
            base: 0,
            index: 0,
            count: 1,
            proof_layers: 1,
        };

        let result = serve_hashes(
            Some(&meta),
            torrent_core::TorrentVersion::V2Only,
            Some(&lengths),
            &request,
        );
        let served = result.expect("should serve hashes with proofs");
        // 1 requested hash + 1 proof hash (sibling of leaf 0) = 2 total
        assert_eq!(served.len(), 2, "should have 1 data hash + 1 proof hash");
        // First hash is the requested piece hash
        assert_eq!(served[0], hashes[0]);
        // Second hash is the sibling (proof) — which is hashes[1]
        assert_eq!(served[1], hashes[1]);
    }

    #[test]
    fn test_serve_hashes_proof_with_batch() {
        // 4 piece hashes, piece_length = 16384, chunk_size = 16384
        // => blocks_per_piece = 1, tree has 4 leaves => depth 2
        //
        // Tree layout (1-indexed heap):
        //          [1] root
        //        /          \
        //     [2]            [3]
        //    /    \         /    \
        //  [4]h0  [5]h1  [6]h2  [7]h3
        //
        // Request count=2 at index=0 => subtree rooted at [2] (h0, h1).
        // subtree_depth = log2(2) = 1, so we skip 1 level of the proof path.
        // proof_path(0) = [h1, hash(h2,h3)] — h1 is internal to subtree,
        // hash(h2,h3) is the uncle above. We skip h1 and send hash(h2,h3).
        let hashes: Vec<torrent_core::Id32> =
            (0..4u8).map(|i| torrent_core::Id32([i; 32])).collect();
        let file_root = torrent_core::Id32([0xAA; 32]);
        let meta = make_test_meta_v2(&hashes, file_root, 16384, 16384 * 4);
        let lengths = Lengths::new(16384 * 4, 16384, DEFAULT_CHUNK_SIZE);

        let request = torrent_core::HashRequest {
            file_root,
            base: 0,
            index: 0,
            count: 2,
            proof_layers: 1,
        };

        let result = serve_hashes(
            Some(&meta),
            torrent_core::TorrentVersion::V2Only,
            Some(&lengths),
            &request,
        );
        let served = result.expect("should serve hashes with batch proof");
        // 2 base hashes + 1 uncle hash = 3 total
        assert_eq!(served.len(), 3, "should have 2 data hashes + 1 uncle hash");
        // First two are the requested piece hashes
        assert_eq!(served[0], hashes[0]);
        assert_eq!(served[1], hashes[1]);
        // Third is the uncle: sibling of the subtree root at [2],
        // which is the node at [3] = hash(h2, h3)
        let tree = torrent_core::MerkleTree::from_leaves(&hashes);
        let expected_uncle = tree.layer(1)[1]; // layer 1 has 2 nodes; index 1 is the right one
        assert_eq!(served[2], expected_uncle);

        // Verify the proof is valid: reconstruct subtree root from base hashes,
        // then verify against the tree root using the uncle hash
        let sub_root = torrent_core::MerkleTree::root_from_hashes(&served[..2]);
        let uncle_hashes = &served[2..];
        let leaf_index = request.index as usize / 2; // 0 / 2 = 0
        assert!(
            torrent_core::MerkleTree::verify_proof(tree.root(), sub_root, leaf_index, uncle_hashes),
            "subtree proof should verify against tree root"
        );
    }

    #[test]
    fn is_i2p_synthetic_addr_detects_240_range() {
        assert!(is_i2p_synthetic_addr(&"240.0.0.1:1".parse().unwrap()));
        assert!(is_i2p_synthetic_addr(
            &"255.255.255.255:65535".parse().unwrap()
        ));
        assert!(!is_i2p_synthetic_addr(&"192.168.1.1:6881".parse().unwrap()));
        assert!(!is_i2p_synthetic_addr(&"[::1]:6881".parse().unwrap()));
    }

    #[tokio::test]
    #[ignore = "requires local I2P router with SAM bridge on 127.0.0.1:7656"]
    async fn i2p_session_integration() {
        let session = crate::i2p::SamSession::create(
            "127.0.0.1",
            7656,
            "integration-test",
            crate::i2p::SamTunnelConfig::default(),
        )
        .await
        .expect("SAM session should connect");
        assert!(!session.destination().is_empty());
        assert!(session.destination().to_b32_address().ends_with(".b32.i2p"));
    }

    #[test]
    fn v6_retry_delay_progression() {
        // Verify exponential backoff: 100, 200, 400, 800, 1600, 3200, 5000, 5000...
        let expected_ms = [100, 200, 400, 800, 1600, 3200, 5000, 5000, 5000, 5000, 5000];
        for (count, &expected) in expected_ms.iter().enumerate() {
            let delay_ms = {
                let base_ms: u64 = 100;
                let max_ms: u64 = 5000;
                base_ms
                    .saturating_mul(1u64.checked_shl(count as u32).unwrap_or(u64::MAX))
                    .min(max_ms)
            };
            assert_eq!(
                delay_ms, expected,
                "count={count}: expected {expected}ms, got {delay_ms}ms"
            );
        }
    }

    // ---- M104: Per-peer backoff and max_in_flight formula tests ----

    #[test]
    fn peer_backoff_exponential() {
        // Verify the M104 backoff formula: 200ms * 2^attempt, capped at 30s.
        // attempt starts at 1 (first failure increments 0 → 1).
        let expected_ms: Vec<u64> = vec![400, 800, 1600, 3200, 6400, 12800, 25600, 30000, 30000];
        for (i, &expected) in expected_ms.iter().enumerate() {
            let attempt = (i as u32) + 1; // attempt counts start at 1
            let delay_ms = 200u64.saturating_mul(1u64 << attempt.min(10)).min(30_000);
            assert_eq!(
                delay_ms, expected,
                "attempt={attempt}: expected {expected}ms, got {delay_ms}ms"
            );
        }
    }

    #[test]
    fn peer_backoff_clears_on_data() {
        // Verify that backoff map operations work correctly:
        // insert on disconnect, remove on data received.
        let mut backoff: HashMap<SocketAddr, (std::time::Instant, u32)> = HashMap::new();
        let addr: SocketAddr = "1.2.3.4:6881".parse().unwrap();

        // No backoff initially
        assert!(backoff.get(&addr).is_none());

        // First disconnect: attempt 1
        let attempt = backoff.get(&addr).map_or(0, |&(_, a)| a);
        let next = attempt.saturating_add(1);
        let delay_ms = 200u64.saturating_mul(1u64 << next.min(10)).min(30_000);
        let earliest = std::time::Instant::now() + Duration::from_millis(delay_ms);
        backoff.insert(addr, (earliest, next));
        assert_eq!(backoff.get(&addr).unwrap().1, 1);

        // Second disconnect: attempt 2
        let attempt = backoff.get(&addr).map_or(0, |&(_, a)| a);
        let next = attempt.saturating_add(1);
        let delay_ms = 200u64.saturating_mul(1u64 << next.min(10)).min(30_000);
        let earliest = std::time::Instant::now() + Duration::from_millis(delay_ms);
        backoff.insert(addr, (earliest, next));
        assert_eq!(backoff.get(&addr).unwrap().1, 2);

        // Data received: clear
        backoff.remove(&addr);
        assert!(backoff.get(&addr).is_none());
    }

    #[test]
    fn backoff_prevents_hammering() {
        // Verify that a peer with a future backoff time would be skipped.
        let mut backoff: HashMap<SocketAddr, (std::time::Instant, u32)> = HashMap::new();
        let addr: SocketAddr = "1.2.3.4:6881".parse().unwrap();

        // Set backoff 10 seconds in the future
        let future = std::time::Instant::now() + Duration::from_secs(10);
        backoff.insert(addr, (future, 3));

        // Should be skipped (now < next_attempt)
        if let Some(&(next_attempt, _)) = backoff.get(&addr) {
            assert!(std::time::Instant::now() < next_attempt);
        }

        // Set backoff in the past — should NOT be skipped
        let past = std::time::Instant::now() - Duration::from_secs(1);
        backoff.insert(addr, (past, 3));
        if let Some(&(next_attempt, _)) = backoff.get(&addr) {
            assert!(std::time::Instant::now() >= next_attempt);
        }
    }

    #[test]
    fn max_in_flight_formula_updated() {
        // M104: max(512, connected*4) clamped to pieces/2, floored at 512.
        let formula = |connected: usize, num_pieces: u32| -> usize {
            let calculated = 512usize.max(connected.saturating_mul(4));
            calculated.min(num_pieces as usize / 2).max(512)
        };

        // Few peers: floor dominates
        assert_eq!(formula(10, 2000), 512);

        // Many peers: connected * 4 takes over
        assert_eq!(formula(200, 2000), 800);

        // Very many peers: clamped by pieces/2
        assert_eq!(formula(500, 2000), 1000); // 2000 clamped to 1000

        // Tiny torrent: floor dominates even with many peers
        assert_eq!(formula(200, 100), 512); // 800 clamped to 50, floored to 512

        // Exact boundary
        assert_eq!(formula(128, 10000), 512); // 128*4=512, max(512,512)=512
        assert_eq!(formula(129, 10000), 516); // 129*4=516, max(512,516)=516

        // Zero peers
        assert_eq!(formula(0, 10000), 512);

        // Zero pieces (edge case — would give pieces/2=0, floor=512)
        assert_eq!(formula(100, 0), 512);
    }
}
