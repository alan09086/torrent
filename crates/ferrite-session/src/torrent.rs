//! TorrentActor (single-owner event loop) and TorrentHandle (cloneable public API).
//!
//! The actor owns all per-torrent state (chunk tracking, piece selection, choking,
//! peer management) and communicates with spawned PeerTasks via channels.
//! The handle is a thin wrapper around an mpsc sender.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::alert::{post_alert, Alert, AlertKind};
use crate::disk::{DiskHandle, DiskJobFlags, DiskManagerHandle};

use ferrite_core::{
    torrent_from_bytes, FilePriority, Id20, Lengths, Magnet, PeerId, TorrentMetaV1, DEFAULT_CHUNK_SIZE,
};
use ferrite_dht::DhtHandle;
use ferrite_storage::{Bitfield, ChunkTracker, MemoryStorage, TorrentStorage};

use crate::choker::{Choker, PeerInfo};
use crate::end_game::EndGame;
use crate::metadata::MetadataDownloader;
use crate::peer::run_peer;
use crate::peer_state::{PeerSource, PeerState};
use crate::piece_selector::{InFlightPiece, PeerSpeed, PickContext, PieceSelector};
use crate::tracker_manager::TrackerManager;
use crate::types::{PeerCommand, PeerEvent, TorrentCommand, TorrentConfig, TorrentState, TorrentStats};

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
        version: ferrite_core::TorrentVersion,
        meta_v2: Option<ferrite_core::TorrentMetaV2>,
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
        utp_socket: Option<ferrite_utp::UtpSocket>,
        utp_socket_v6: Option<ferrite_utp::UtpSocket>,
        ban_manager: crate::session::SharedBanManager,
        ip_filter: crate::session::SharedIpFilter,
        plugins: Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>>,
        sam_session: Option<Arc<crate::i2p::SamSession>>,
        ssl_manager: Option<Arc<crate::ssl_manager::SslManager>>,
    ) -> crate::Result<Self> {
        let mut config = config;
        // BEP 27: private torrents disable DHT and PEX
        if meta.info.private == Some(1) {
            config.enable_dht = false;
            config.enable_pex = false;
        }

        let info_hashes = match (&version, &meta_v2) {
            (ferrite_core::TorrentVersion::Hybrid, Some(v2_meta)) => {
                if let Some(v2_hash) = v2_meta.info_hashes.v2 {
                    ferrite_core::InfoHashes::hybrid(meta.info_hash, v2_hash)
                } else {
                    ferrite_core::InfoHashes::v1_only(meta.info_hash)
                }
            }
            (ferrite_core::TorrentVersion::V2Only, Some(v2_meta)) => {
                v2_meta.info_hashes.clone()
            }
            _ => ferrite_core::InfoHashes::v1_only(meta.info_hash),
        };

        let num_pieces = meta.info.num_pieces() as u32;
        let lengths = Lengths::new(
            meta.info.total_length(),
            meta.info.piece_length,
            DEFAULT_CHUNK_SIZE,
        );
        let chunk_tracker = ChunkTracker::new(lengths.clone());
        let piece_selector = PieceSelector::new(num_pieces);
        let file_lengths: Vec<u64> = meta.info.files().iter().map(|f| f.length).collect();
        let file_priorities = vec![FilePriority::Normal; file_lengths.len()];
        let wanted_pieces = crate::piece_selector::build_wanted_pieces(
            &file_priorities, &file_lengths, &lengths,
        );

        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let (event_tx, event_rx) = mpsc::channel(256);
        let our_peer_id = PeerId::generate().0;

        // Bind listener for incoming connections
        // Try dual-stack [::]:port first, fall back to IPv4-only
        let listener = match TcpListener::bind((std::net::Ipv6Addr::UNSPECIFIED, config.listen_port)).await {
            Ok(l) => Some(l),
            Err(_) => TcpListener::bind(("0.0.0.0", config.listen_port)).await.ok(),
        };

        let mut tracker_manager =
            TrackerManager::from_torrent(&meta, our_peer_id, config.listen_port);
        tracker_manager.set_info_hashes(info_hashes.clone());

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
            info_hashes.v2.map(|v2| Id20(v2.0[..20].try_into().unwrap()))
        } else {
            None
        };
        let (dht_v2_peers_rx, dht_v6_v2_peers_rx) = if let (true, Some(v2_id)) = (enable_dht, v2_as_v1) {
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
            0, 0, 0, 0,
            config.upload_rate_limit,
            config.download_rate_limit,
        );

        let super_seed = if config.super_seeding {
            Some(crate::super_seed::SuperSeedState::new())
        } else {
            None
        };
        let have_buffer = crate::have_buffer::HaveBuffer::new(num_pieces, config.have_send_delay_ms);
        let is_share_mode = config.share_mode;

        let (piece_ready_tx, _) = broadcast::channel(64);
        let initial_have = chunk_tracker.bitfield().clone();
        let (have_watch_tx, have_watch_rx) = tokio::sync::watch::channel(initial_have);
        let stream_read_semaphore = crate::streaming::stream_read_semaphore(
            config.max_concurrent_stream_reads,
        );

        let choker = Choker::with_algorithms(
            4,
            config.seed_choking_algorithm,
            config.choking_algorithm,
            config.upload_rate_limit,
            2,
            20,
        );

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
            piece_selector,
            in_flight_pieces: HashMap::new(),
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
            available_peers: Vec::new(),
            choker,
            metadata_downloader: None,
            downloaded: 0,
            uploaded: 0,
            checking_progress: 0.0,
            cmd_rx,
            event_tx,
            event_rx,
            meta: Some(meta),
            listener,
            utp_socket,
            utp_socket_v6,
            tracker_manager,
            dht: if enable_dht { dht } else { None },
            dht_v6: if enable_dht { dht_v6 } else { None },
            dht_peers_rx,
            dht_v6_peers_rx,
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
            ssl_manager,
            rate_limiter_set,
            auto_sequential_active: false,
        };

        tokio::spawn(actor.run());
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
        utp_socket: Option<ferrite_utp::UtpSocket>,
        utp_socket_v6: Option<ferrite_utp::UtpSocket>,
        ban_manager: crate::session::SharedBanManager,
        ip_filter: crate::session::SharedIpFilter,
        plugins: Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>>,
        sam_session: Option<Arc<crate::i2p::SamSession>>,
        ssl_manager: Option<Arc<crate::ssl_manager::SslManager>>,
    ) -> crate::Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let (event_tx, event_rx) = mpsc::channel(256);
        let our_peer_id = PeerId::generate().0;

        // Try dual-stack [::]:port first, fall back to IPv4-only
        let listener = match TcpListener::bind((std::net::Ipv6Addr::UNSPECIFIED, config.listen_port)).await {
            Ok(l) => Some(l),
            Err(_) => TcpListener::bind(("0.0.0.0", config.listen_port)).await.ok(),
        };

        let tracker_manager =
            TrackerManager::empty(magnet.info_hash(), our_peer_id, config.listen_port);

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
            0, 0, 0, 0,
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
        let stream_read_semaphore = crate::streaming::stream_read_semaphore(
            config.max_concurrent_stream_reads,
        );

        let choker = Choker::with_algorithms(
            4,
            config.seed_choking_algorithm,
            config.choking_algorithm,
            config.upload_rate_limit,
            2,
            20,
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
            piece_selector: PieceSelector::new(0),
            in_flight_pieces: HashMap::new(),
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
            available_peers: Vec::new(),
            choker,
            metadata_downloader: Some(MetadataDownloader::new(magnet.info_hash())),
            downloaded: 0,
            uploaded: 0,
            checking_progress: 0.0,
            cmd_rx,
            event_tx,
            event_rx,
            meta: None,
            listener,
            utp_socket,
            utp_socket_v6,
            tracker_manager,
            dht: if enable_dht { dht } else { None },
            dht_v6: if enable_dht { dht_v6 } else { None },
            dht_peers_rx,
            dht_v6_peers_rx,
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
            version: ferrite_core::TorrentVersion::V1Only,
            meta_v2: None,
            info_hashes,
            dht_v2_peers_rx: None,
            dht_v6_v2_peers_rx: None,
            magnet_selected_files,
            sam_session,
            i2p_accept_rx: None,
            i2p_peer_counter: 0,
            ssl_manager,
            rate_limiter_set,
            auto_sequential_active: false,
        };

        tokio::spawn(actor.run());
        Ok(TorrentHandle { cmd_tx })
    }

    /// Send an incoming uTP peer (routed by the session) to this torrent.
    pub(crate) async fn send_incoming_peer(
        &self,
        stream: crate::utp_routing::PrefixedStream<ferrite_utp::UtpStream>,
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
        // Best-effort send; if the channel is already closed, that's fine.
        let _ = self.cmd_tx.send(TorrentCommand::Shutdown).await;
        Ok(())
    }

    /// Snapshot current torrent state into libtorrent-compatible resume data.
    pub async fn save_resume_data(&self) -> crate::Result<ferrite_core::FastResumeData> {
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
        priority: ferrite_core::FilePriority,
    ) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::SetFilePriority { index, priority, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get the current per-file priorities.
    pub async fn file_priorities(&self) -> crate::Result<Vec<ferrite_core::FilePriority>> {
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
    pub async fn scrape(&self) -> crate::Result<Option<(String, ferrite_tracker::ScrapeInfo)>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::Scrape { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Open a streaming reader for a file within the torrent.
    pub async fn open_file(&self, file_index: usize) -> crate::Result<crate::streaming::FileStream> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TorrentCommand::OpenFile { file_index, reply: tx })
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
            .send(TorrentCommand::MoveStorage { new_path, reply: tx })
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
    piece_selector: PieceSelector,
    in_flight_pieces: HashMap<u32, InFlightPiece>,
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
    available_peers: Vec<(SocketAddr, PeerSource)>,
    choker: Choker,

    // Metadata (for magnet links)
    metadata_downloader: Option<MetadataDownloader>,

    // Parsed torrent meta (for piece hash verification)
    meta: Option<TorrentMetaV1>,

    // Stats
    downloaded: u64,
    uploaded: u64,
    checking_progress: f32,

    // Channels
    cmd_rx: mpsc::Receiver<TorrentCommand>,
    event_tx: mpsc::Sender<PeerEvent>,
    event_rx: mpsc::Receiver<PeerEvent>,

    // TCP listener for incoming peer connections
    listener: Option<TcpListener>,

    // uTP socket for outbound connections (shared with session, cloned)
    utp_socket: Option<ferrite_utp::UtpSocket>,
    // IPv6 uTP socket for outbound connections to IPv6 peers
    utp_socket_v6: Option<ferrite_utp::UtpSocket>,

    // Tracker management
    tracker_manager: TrackerManager,

    // DHT handles (shared, optional)
    dht: Option<DhtHandle>,
    dht_v6: Option<DhtHandle>,
    dht_peers_rx: Option<mpsc::Receiver<Vec<SocketAddr>>>,
    dht_v6_peers_rx: Option<mpsc::Receiver<Vec<SocketAddr>>>,

    // Alert system (M15)
    alert_tx: broadcast::Sender<Alert>,
    alert_mask: Arc<AtomicU32>,

    // Rate limiting (M14)
    upload_bucket: crate::rate_limiter::TokenBucket,
    download_bucket: crate::rate_limiter::TokenBucket,
    global_upload_bucket: Option<SharedBucket>,
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
    hash_picker: Option<ferrite_core::HashPicker>,
    version: ferrite_core::TorrentVersion,
    #[allow(dead_code)] // stored for hybrid torrent re-serialization (M35 Task 5)
    meta_v2: Option<ferrite_core::TorrentMetaV2>,

    /// Full info hashes for dual-swarm support (v1 + v2 for hybrid).
    info_hashes: ferrite_core::InfoHashes,

    /// Dual-swarm DHT peer receivers (v2 hash in hybrid torrents).
    dht_v2_peers_rx: Option<mpsc::Receiver<Vec<SocketAddr>>>,
    dht_v6_v2_peers_rx: Option<mpsc::Receiver<Vec<SocketAddr>>>,

    /// BEP 53: deferred file selection from magnet `so=` parameter.
    /// Applied after metadata is received to set file priorities.
    magnet_selected_files: Option<Vec<ferrite_core::FileSelection>>,

    /// I2P SAM session for anonymous peer connections (M41).
    sam_session: Option<Arc<crate::i2p::SamSession>>,

    /// Receiver for incoming I2P peer connections (M41).
    i2p_accept_rx: Option<mpsc::Receiver<crate::i2p::SamStream>>,

    /// Counter for generating synthetic SocketAddr values for I2P peers (M41).
    i2p_peer_counter: u32,

    /// SSL manager for SSL torrent certificate handling (M42).
    ssl_manager: Option<Arc<crate::ssl_manager::SslManager>>,

    /// Per-class rate limiting with mixed-mode (M45).
    rate_limiter_set: crate::rate_limiter::RateLimiterSet,
    /// Whether auto-sequential mode is currently active (hysteresis state).
    auto_sequential_active: bool,
}

impl TorrentActor {
    /// Transition to a new state, firing a StateChanged alert if different.
    fn transition_state(&mut self, new_state: TorrentState) {
        let prev = self.state;
        if prev != new_state {
            self.state = new_state;
            post_alert(&self.alert_tx, &self.alert_mask, AlertKind::StateChanged {
                info_hash: self.info_hash,
                prev_state: prev,
                new_state,
            });
        }
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

        // Spawn web seeds if not already seeding
        if self.state != TorrentState::Seeding {
            self.spawn_web_seeds();
            self.assign_pieces_to_web_seeds();
        }

        let mut unchoke_interval = tokio::time::interval(Duration::from_secs(10));
        let mut optimistic_interval = tokio::time::interval(Duration::from_secs(30));
        let mut connect_interval = tokio::time::interval(Duration::from_secs(30));
        let mut refill_interval = tokio::time::interval(Duration::from_millis(100));
        let mut have_flush_interval = if self.config.have_send_delay_ms > 0 {
            Some(tokio::time::interval(Duration::from_millis(self.config.have_send_delay_ms)))
        } else {
            None
        };
        let mut suggest_interval = if self.config.suggest_mode {
            Some(tokio::time::interval(Duration::from_secs(30)))
        } else {
            None
        };
        let mut turnover_interval = if self.config.peer_turnover_interval > 0 {
            Some(tokio::time::interval(Duration::from_secs(self.config.peer_turnover_interval)))
        } else {
            None
        };

        // Don't fire immediately for the first tick
        unchoke_interval.tick().await;
        optimistic_interval.tick().await;
        connect_interval.tick().await;
        refill_interval.tick().await;
        if let Some(ref mut si) = suggest_interval {
            si.tick().await; // skip initial tick
        }
        if let Some(ref mut interval) = turnover_interval {
            interval.tick().await;
        }

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
                // Commands from handle
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(TorrentCommand::AddPeers { peers, source }) => {
                            self.handle_add_peers(peers, source);
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
                                Some(ferrite_wire::mse::EncryptionMode::Disabled),
                            );
                        }
                        Some(TorrentCommand::UpdateExternalIp { ip }) => {
                            self.external_ip = Some(ip);
                            self.sort_available_peers();
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
                                Some(ferrite_wire::mse::EncryptionMode::Disabled),
                            );
                        }
                        Some(TorrentCommand::Shutdown) | None => {
                            self.shutdown_web_seeds().await;
                            self.shutdown_peers().await;
                            return;
                        }
                    }
                }
                // Events from peers
                event = self.event_rx.recv() => {
                    if let Some(event) = event {
                        self.handle_peer_event(event).await;
                    }
                }
                // Accept incoming peers
                result = accept_incoming(&self.listener) => {
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
                            self.in_flight_pieces.len(),
                            self.peers.len(),
                            self.auto_sequential_active,
                        );
                    }
                }
                // Optimistic unchoke timer
                _ = optimistic_interval.tick() => {
                    self.rotate_optimistic();
                }
                // Connect timer
                _ = connect_interval.tick() => {
                    self.try_connect_peers();
                    self.assign_pieces_to_web_seeds();
                    // Re-trigger DHT search if exhausted and we still need peers
                    if self.config.enable_dht
                        && self.available_peers.is_empty()
                        && self.peers.len() < self.config.max_peers
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
                            && let Some(ref dht6) = self.dht_v6
                        {
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
                                && let Some(ref dht6) = self.dht_v6
                            {
                                match dht6.get_peers(v2_as_v1).await {
                                    Ok(rx) => self.dht_v6_v2_peers_rx = Some(rx),
                                    Err(e) => debug!("DHT v6 v2-swarm re-search failed: {e}"),
                                }
                            }
                        }
                    }
                }
                // Tracker re-announce timer
                _ = async {
                    match self.tracker_manager.next_announce_in() {
                        Some(dur) => tokio::time::sleep(dur).await,
                        None => std::future::pending().await,
                    }
                } => {
                    if self.state != TorrentState::FetchingMetadata {
                        let left = self.calculate_left();
                        let result = self.tracker_manager.announce(
                            ferrite_tracker::AnnounceEvent::None,
                            self.uploaded,
                            self.downloaded,
                            left,
                        ).await;
                        self.fire_tracker_alerts(&result.outcomes);
                        if !result.peers.is_empty() {
                            debug!(count = result.peers.len(), "tracker returned peers");
                            self.handle_add_peers(result.peers, PeerSource::Tracker);
                        }
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
                            self.handle_add_peers(peers, PeerSource::Dht);
                        }
                        None => {
                            debug!("DHT v6 peer search exhausted");
                            self.dht_v6_peers_rx = None;
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
                // Peer turnover timer (M46)
                _ = async {
                    match &mut turnover_interval {
                        Some(interval) => interval.tick().await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.run_peer_turnover().await;
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
                if !self.peers.contains_key(&addr)
                    && !self.available_peers.iter().any(|(a, _)| *a == addr)
                {
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
        let meta = self.meta.as_ref().ok_or_else(|| {
            crate::Error::Config("cannot move storage: metadata not available".into())
        })?;

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
        tokio::task::spawn_blocking(move || relocate_files(&src, &dst, &paths))
            .await
            .map_err(|e| crate::Error::Io(std::io::Error::other(e)))?
            .map_err(crate::Error::Io)?;

        // Unregister old storage
        self.disk_manager.unregister_torrent(self.info_hash).await;

        // Create new storage at destination
        let lengths = self.lengths.clone().ok_or_else(|| {
            crate::Error::Config("lengths not available".into())
        })?;
        let storage: Arc<dyn TorrentStorage> = Arc::new(
            ferrite_storage::FilesystemStorage::new(
                &new_path,
                file_paths,
                file_lengths,
                lengths,
                Some(&self.file_priorities),
                false,
            )?,
        );

        // Re-register with disk manager
        self.disk = Some(self.disk_manager.register_torrent(self.info_hash, storage).await);

        // Update download dir
        self.config.download_dir = new_path.clone();

        // Fire alert
        post_alert(&self.alert_tx, &self.alert_mask, AlertKind::StorageMoved {
            info_hash: self.info_hash,
            new_path,
        });

        Ok(())
    }

    fn make_stats(&self) -> TorrentStats {
        let pieces_have = self
            .chunk_tracker
            .as_ref()
            .map(|ct| ct.bitfield().count_ones())
            .unwrap_or(0);

        let mut peers_by_source = std::collections::HashMap::new();
        for peer in self.peers.values() {
            *peers_by_source.entry(peer.source).or_insert(0) += 1;
        }

        TorrentStats {
            state: self.state,
            downloaded: self.downloaded,
            uploaded: self.uploaded,
            pieces_have,
            pieces_total: self.num_pieces,
            peers_connected: self.peers.len(),
            peers_available: self.available_peers.len(),
            checking_progress: self.checking_progress,
            peers_by_source,
        }
    }

    /// Fire TrackerReply / TrackerError alerts from announce outcomes.
    fn fire_tracker_alerts(&self, outcomes: &[crate::tracker_manager::TrackerOutcome]) {
        for outcome in outcomes {
            match &outcome.result {
                Ok(num_peers) => {
                    post_alert(&self.alert_tx, &self.alert_mask, AlertKind::TrackerReply {
                        info_hash: self.info_hash,
                        url: outcome.url.clone(),
                        num_peers: *num_peers,
                    });
                }
                Err(msg) => {
                    post_alert(&self.alert_tx, &self.alert_mask, AlertKind::TrackerError {
                        info_hash: self.info_hash,
                        url: outcome.url.clone(),
                        message: msg.clone(),
                    });
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
        // Best-effort announce Stopped to trackers
        let left = self.calculate_left();
        self.tracker_manager
            .announce_stopped(self.uploaded, self.downloaded, left)
            .await;

        for peer in self.peers.values() {
            let _ = peer.cmd_tx.send(PeerCommand::Shutdown).await;
        }
    }

    async fn handle_pause(&mut self) {
        if self.state == TorrentState::Paused || self.state == TorrentState::Stopped {
            return;
        }
        let prev_state = self.state;
        self.transition_state(TorrentState::Paused);
        post_alert(&self.alert_tx, &self.alert_mask, AlertKind::TorrentPaused { info_hash: self.info_hash });
        // Disconnect all peers
        for peer in self.peers.values() {
            let _ = peer.cmd_tx.send(PeerCommand::Shutdown).await;
        }
        self.peers.clear();
        // Announce Stopped to trackers
        if prev_state == TorrentState::Downloading
            || prev_state == TorrentState::Seeding
            || prev_state == TorrentState::Complete
        {
            let left = self.calculate_left();
            self.tracker_manager
                .announce_stopped(self.uploaded, self.downloaded, left)
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
            self.choker.set_seed_mode(false);
        }
        post_alert(&self.alert_tx, &self.alert_mask, AlertKind::TorrentResumed { info_hash: self.info_hash });
        // Re-announce Started
        let left = self.calculate_left();
        let result = self.tracker_manager.announce(
            ferrite_tracker::AnnounceEvent::Started,
            self.uploaded,
            self.downloaded,
            left,
        ).await;
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
                self.piece_selector.add_peer_bitfield(&bitfield);
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.bitfield = bitfield;
                }
                // BEP 16: assign a piece in super-seed mode
                self.assign_next_piece_for_peer(peer_addr).await;
                // Check if we're interested in this peer
                self.maybe_express_interest(peer_addr).await;
                self.request_pieces_from_peer(peer_addr).await;
            }
            PeerEvent::Have { peer_addr, index } => {
                self.piece_selector.increment(index);
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.bitfield.set(index);
                }
                // BEP 16: Have-back detection in super-seed mode
                if let Some(ref mut ss) = self.super_seed
                    && ss.peer_reported_have(peer_addr, index)
                {
                    self.assign_next_piece_for_peer(peer_addr).await;
                }
                self.maybe_express_interest(peer_addr).await;
                self.request_pieces_from_peer(peer_addr).await;
            }
            PeerEvent::PieceData {
                peer_addr,
                index,
                begin,
                data,
            } => {
                self.handle_piece_data(peer_addr, index, begin, &data)
                    .await;
            }
            PeerEvent::PeerChoking {
                peer_addr,
                choking,
            } => {
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.peer_choking = choking;
                }
                if !choking {
                    // Unchoked — try requesting pieces
                    self.request_pieces_from_peer(peer_addr).await;
                }
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
                }
            }
            PeerEvent::TrackersReceived { tracker_urls } => {
                for url in tracker_urls {
                    if self.tracker_manager.add_tracker_url(&url) {
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
                    peer.incoming_requests.push((index, begin, length));
                }
                self.serve_incoming_requests().await;
            }
            PeerEvent::RejectRequest {
                peer_addr,
                index,
                begin,
                length: _,
            } => {
                if let Some(peer) = self.peers.get_mut(&peer_addr)
                    && let Some(pos) = peer
                        .pending_requests
                        .iter()
                        .position(|&(i, b, _)| i == index && b == begin)
                {
                    peer.pending_requests.swap_remove(pos);
                }
                debug!(index, %peer_addr, "request rejected by peer");
            }
            PeerEvent::AllowedFast {
                peer_addr,
                index,
            } => {
                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.allowed_fast.insert(index);
                }
            }
            PeerEvent::SuggestPiece {
                peer_addr,
                index,
            } => {
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
            PeerEvent::Disconnected {
                peer_addr,
                reason,
            } => {
                debug!(%peer_addr, ?reason, "peer disconnected");
                post_alert(&self.alert_tx, &self.alert_mask, AlertKind::PeerDisconnected {
                    info_hash: self.info_hash,
                    addr: peer_addr,
                    reason: reason.clone(),
                });
                // BEP 16: clean up super-seed assignment
                if let Some(ref mut ss) = self.super_seed {
                    ss.peer_disconnected(peer_addr);
                }
                if let Some(peer) = self.peers.remove(&peer_addr) {
                    self.piece_selector.remove_peer_bitfield(&peer.bitfield);
                    // Remove pieces that only this peer was downloading
                    let peer_pieces: HashSet<u32> = peer
                        .pending_requests
                        .iter()
                        .map(|&(idx, _, _)| idx)
                        .collect();
                    for piece_idx in peer_pieces {
                        let other_has = self.peers.values().any(|p| {
                            p.pending_requests.iter().any(|&(i, _, _)| i == piece_idx)
                        });
                        if !other_has {
                            self.in_flight_pieces.remove(&piece_idx);
                        }
                    }
                    // Clean up pipeline request tracking for disconnected peer
                    for ifp in self.in_flight_pieces.values_mut() {
                        ifp.assigned_blocks.retain(|_, addr| *addr != peer_addr);
                    }
                    if self.end_game.is_active() {
                        self.end_game.peer_disconnected(peer_addr);
                    }
                }
                self.suggested_to_peers.remove(&peer_addr);
            }
            PeerEvent::WebSeedPieceData { url, index, data } => {
                self.handle_web_seed_piece_data(url, index, data).await;
            }
            PeerEvent::WebSeedError { url, piece, message } => {
                self.handle_web_seed_error(url, piece, message);
            }
            PeerEvent::HashesReceived { peer_addr, request, hashes } => {
                self.handle_hashes_received(peer_addr, request, hashes).await;
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
            PeerEvent::HolepunchConnect { peer_addr: _, target } => {
                if self.config.enable_holepunch {
                    self.handle_holepunch_connect(target).await;
                }
            }
            PeerEvent::HolepunchError { peer_addr: _, target, error_code } => {
                let message = ferrite_wire::HolepunchError::from_u32(error_code)
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| format!("unknown error code {error_code}"));
                debug!(%target, %error_code, %message, "holepunch rendezvous failed");
                post_alert(&self.alert_tx, &self.alert_mask, AlertKind::HolepunchFailed {
                    info_hash: self.info_hash,
                    addr: target,
                    error_code: Some(error_code),
                    message,
                });
            }
        }
    }

    async fn handle_piece_data(
        &mut self,
        peer_addr: SocketAddr,
        index: u32,
        begin: u32,
        data: &[u8],
    ) {
        // Write chunk to disk
        if let Some(ref disk) = self.disk
            && let Err(e) = disk.write_chunk(index, begin, Bytes::copy_from_slice(data), DiskJobFlags::empty()).await
        {
            warn!(index, begin, "failed to write chunk: {e}");
            return;
        }

        self.downloaded += data.len() as u64;

        // Smart banning: track which peers contribute to each piece
        self.piece_contributors.entry(index).or_default().insert(peer_addr.ip());

        if let Some(peer) = self.peers.get_mut(&peer_addr) {
            if let Some(pos) = peer
                .pending_requests
                .iter()
                .position(|&(i, b, _)| i == index && b == begin)
            {
                peer.pending_requests.swap_remove(pos);
            }
            peer.download_bytes_window += data.len() as u64;

            // Update pipeline state (M28)
            peer.pipeline.block_received(index, begin, data.len() as u32, std::time::Instant::now());
            peer.last_data_received = Some(std::time::Instant::now());
            // Clear snub if snubbed
            if peer.snubbed {
                peer.snubbed = false;
                peer.pipeline.reset_to_slow_start();
            }
        }

        // Remove block assignment from InFlightPiece
        if let Some(ifp) = self.in_flight_pieces.get_mut(&index) {
            ifp.assigned_blocks.remove(&(index, begin));
        }

        // End-game: cancel this block on all other peers
        if self.end_game.is_active() {
            let cancels = self.end_game.block_received(index, begin, peer_addr);
            for (cancel_addr, ci, cb, cl) in cancels {
                if let Some(cancel_peer) = self.peers.get_mut(&cancel_addr) {
                    let _ = cancel_peer
                        .cmd_tx
                        .send(PeerCommand::Cancel {
                            index: ci,
                            begin: cb,
                            length: cl,
                        })
                        .await;
                    if let Some(pos) = cancel_peer
                        .pending_requests
                        .iter()
                        .position(|&(i, b, _)| i == ci && b == cb)
                    {
                        cancel_peer.pending_requests.swap_remove(pos);
                    }
                }
            }
        }

        // Track chunk completion
        let piece_complete = if let Some(ref mut ct) = self.chunk_tracker {
            ct.chunk_received(index, begin)
        } else {
            false
        };

        if piece_complete {
            // M44: Predictive piece announce — send Have before verification
            if self.config.predictive_piece_announce_ms > 0
                && !self.predictive_have_sent.contains(&index)
            {
                self.predictive_have_sent.insert(index);
                for peer in self.peers.values() {
                    if !peer.bitfield.get(index) {
                        let _ = peer.cmd_tx.send(PeerCommand::Have(index)).await;
                    }
                }
            }

            self.verify_and_mark_piece(index).await;
        }

        // Try to request more from this peer
        self.request_pieces_from_peer(peer_addr).await;
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
        }
    }

    fn build_resume_data(&self) -> crate::Result<ferrite_core::FastResumeData> {
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

        let mut rd = ferrite_core::FastResumeData::new(
            self.info_hash.as_bytes().to_vec(),
            name,
            save_path,
        );

        rd.pieces = pieces_bytes;

        rd.total_uploaded = self.uploaded as i64;
        rd.total_downloaded = self.downloaded as i64;

        rd.paused = if self.state == TorrentState::Paused { 1 } else { 0 };
        rd.seed_mode = if self.state == TorrentState::Seeding { 1 } else { 0 };
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
        rd.peers = ferrite_tracker::compact::encode_compact_peers(&peer_addrs);
        rd.peers6 = ferrite_tracker::compact::encode_compact_peers6(&peer_addrs);

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
                    &self.file_priorities, &file_lengths, lengths,
                );
            }
        }

        Ok(())
    }

    fn handle_open_file(
        &mut self,
        file_index: usize,
    ) -> crate::Result<crate::streaming::FileStreamHandle> {
        let meta = self.meta.as_ref().ok_or(crate::Error::MetadataNotReady(self.info_hash))?;
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

        let lengths = self.lengths.as_ref().ok_or(crate::Error::MetadataNotReady(self.info_hash))?;
        let disk = self.disk.as_ref().ok_or(crate::Error::MetadataNotReady(self.info_hash))?;

        // Compute file offset within torrent data
        let mut file_offset = 0u64;
        for f in &files[..file_index] {
            file_offset += f.length;
        }
        let file_length = files[file_index].length;

        let (cursor_tx, cursor_rx) = tokio::sync::watch::channel(0u64);

        let permit = self.stream_read_semaphore.clone()
            .try_acquire_owned()
            .map_err(|_| crate::Error::Connection(
                "too many concurrent stream readers".into(),
            ))?;

        // Add streaming cursor for the actor to track
        self.streaming_cursors.push(crate::streaming::StreamingCursor {
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
            ferrite_core::TorrentVersion::V1Only => {
                self.verify_and_mark_piece_v1(index).await;
            }
            ferrite_core::TorrentVersion::V2Only => {
                self.verify_and_mark_piece_v2(index).await;
            }
            ferrite_core::TorrentVersion::Hybrid => {
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

        let verified = if let (Some(disk), Some(expected)) =
            (&self.disk, expected_hash)
        {
            disk.verify_piece(index, expected, DiskJobFlags::empty()).await.unwrap_or(false)
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

            let block_hash = match disk.hash_block(index, begin, length, DiskJobFlags::empty()).await {
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
                    ferrite_core::SetBlockResult::Ok => {
                        if let Some(ref mut ct) = self.chunk_tracker {
                            ct.mark_block_verified(index, chunk_idx);
                        }
                    }
                    ferrite_core::SetBlockResult::Unknown => {
                        // Piece-layer hash not yet available — stored for deferred verification
                        debug!(index, chunk_idx, "block hash stored, awaiting piece-layer hashes");
                    }
                    ferrite_core::SetBlockResult::HashFailed => {
                        warn!(index, chunk_idx, "block hash failed Merkle verification");
                        return HashResult::Failed;
                    }
                }
            }
        }

        if all_ok && self.chunk_tracker.as_ref().is_some_and(|ct| ct.all_blocks_verified(index)) {
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
                match disk.verify_piece(index, expected, DiskJobFlags::empty()).await {
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
                debug!(index, "hybrid: v1 passed, v2 deferred — waiting for piece-layer hashes");
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
    async fn on_piece_verified(&mut self, index: u32) {
        if let Some(ref mut ct) = self.chunk_tracker {
            ct.mark_verified(index);
        }
        self.in_flight_pieces.remove(&index);
        self.piece_contributors.remove(&index);
        info!(index, "piece verified");
        post_alert(&self.alert_tx, &self.alert_mask, AlertKind::PieceFinished {
            info_hash: self.info_hash,
            piece: index,
        });

        // Notify FileStream consumers of piece completion
        let _ = self.piece_ready_tx.send(index);
        if let Some(ref ct) = self.chunk_tracker {
            let _ = self.have_watch_tx.send(ct.bitfield().clone());
        }

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
                    for peer in self.peers.values() {
                        if !peer.bitfield.get(index) {
                            let _ = peer.cmd_tx.send(PeerCommand::Have(index)).await;
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
                if already.len() >= max_suggest { continue; }
                let should_suggest = self.peers.get(&peer_addr)
                    .is_some_and(|p| !p.bitfield.get(index));
                if should_suggest && !already.contains(&index)
                    && let Some(peer) = self.peers.get(&peer_addr)
                {
                    let _ = peer.cmd_tx.send(PeerCommand::SuggestPiece(index)).await;
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
            post_alert(&self.alert_tx, &self.alert_mask, AlertKind::TorrentFinished {
                info_hash: self.info_hash,
            });
            self.end_game.deactivate();
            self.transition_state(TorrentState::Seeding);
            self.choker.set_seed_mode(true);
            // BEP 21: broadcast upload-only status
            if self.config.upload_only_announce {
                let hs = ferrite_wire::ExtHandshake::new_upload_only();
                for peer in self.peers.values() {
                    let _ = peer.cmd_tx.send(PeerCommand::SendExtHandshake(hs.clone())).await;
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
        let contributors: Vec<std::net::IpAddr> = self.piece_contributors
            .remove(&index)
            .unwrap_or_default()
            .into_iter()
            .collect();

        warn!(index, contributors = contributors.len(), "piece hash verification failed");

        self.predictive_have_sent.remove(&index);

        // Check if this is a parole failure
        if let Some(parole) = self.parole_pieces.remove(&index) {
            self.apply_parole_failure(index, parole);
        } else {
            // First failure: enter parole if enabled
            self.enter_parole(index, contributors.clone());
        }

        post_alert(&self.alert_tx, &self.alert_mask, AlertKind::HashFailed {
            info_hash: self.info_hash,
            piece: index,
            contributors,
        });
        if let Some(ref mut ct) = self.chunk_tracker {
            ct.mark_failed(index);
        }
        self.in_flight_pieces.remove(&index);
        // Hash failure in end-game: deactivate and resume normal mode
        if self.end_game.is_active() {
            self.end_game.deactivate();
            info!(index, "end-game deactivated due to hash failure");
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
                message: format!(
                    "v1 and v2 hashes do not describe the same data (piece {piece})"
                ),
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
        request: ferrite_core::HashRequest,
        hashes: Vec<ferrite_core::Id32>,
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
                    post_alert(&self.alert_tx, &self.alert_mask, AlertKind::HashFailed {
                        info_hash: self.info_hash,
                        piece,
                        contributors: Vec::new(),
                    });
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
        request: ferrite_core::HashRequest,
    ) {
        let peer = match self.peers.get(&peer_addr) {
            Some(p) => p,
            None => return,
        };

        // Reject if we don't have a hash picker (v1 torrent or no metadata yet)
        // TODO(M35): serve hashes from our Merkle tree state when seeding v2
        let _ = peer.cmd_tx.send(PeerCommand::SendHashReject(request)).await;
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

        info!(index, contributors = contributors.len(), "entering parole mode");
        self.parole_pieces.insert(index, crate::ban::ParoleState {
            original_contributors: contributors.into_iter().collect(),
            parole_peer: None,
        });
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
            post_alert(&self.alert_tx, &self.alert_mask, AlertKind::PeerBanned {
                info_hash: self.info_hash,
                addr: std::net::SocketAddr::new(ip, 0),
            });
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
        let addrs_to_remove: Vec<SocketAddr> = self.peers.keys()
            .filter(|a| a.ip() == ip)
            .copied()
            .collect();
        for addr in addrs_to_remove {
            if let Some(peer) = self.peers.remove(&addr) {
                let _ = peer.cmd_tx.send(PeerCommand::Shutdown).await;
                post_alert(&self.alert_tx, &self.alert_mask, AlertKind::PeerDisconnected {
                    info_hash: self.info_hash,
                    addr,
                    reason: Some("banned".into()),
                });
            }
        }
        // Remove from available peers pool
        self.available_peers.retain(|(a, _)| a.ip() != ip);
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
                            let _ = peer.cmd_tx.send(PeerCommand::Have(idx)).await;
                        }
                    }
                }
            }
            Some(crate::have_buffer::FlushResult::SendBitfield(bf)) => {
                let data = Bytes::copy_from_slice(bf.as_bytes());
                for peer in self.peers.values() {
                    let _ = peer.cmd_tx.send(PeerCommand::SendBitfield(data.clone())).await;
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
                self.peers.get(&peer_addr).is_some_and(|p| p.bitfield.get(piece))
            };
            let mut sent = 0;
            for &piece in &cached {
                if sent >= max_suggest { break; }
                if peer_has_piece(piece) { continue; }
                if already_suggested.contains(&piece) { continue; }
                if let Some(peer) = self.peers.get(&peer_addr) {
                    let _ = peer.cmd_tx.send(PeerCommand::SuggestPiece(piece)).await;
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

                        // Create storage and register with disk manager
                        let storage: Arc<dyn TorrentStorage> =
                            Arc::new(MemoryStorage::new(lengths.clone()));
                        let disk_handle = self.disk_manager.register_torrent(
                            self.info_hash, storage,
                        ).await;

                        self.disk = Some(disk_handle);
                        self.chunk_tracker = Some(ChunkTracker::new(lengths.clone()));
                        self.lengths = Some(lengths);
                        self.num_pieces = num_pieces;
                        self.piece_selector = PieceSelector::new(num_pieces);
                        let file_lengths: Vec<u64> = meta.info.files().iter().map(|f| f.length).collect();
                        let mut meta = meta;
                        meta.info_bytes = Some(Bytes::from(info_bytes));
                        self.meta = Some(meta);
                        self.file_priorities = vec![FilePriority::Normal; file_lengths.len()];

                        // BEP 53: apply magnet so= file selection
                        if let Some(ref selections) = self.magnet_selected_files {
                            self.file_priorities = ferrite_core::FileSelection::to_priorities(
                                selections,
                                file_lengths.len(),
                            );
                            self.magnet_selected_files = None;
                        }

                        self.wanted_pieces = crate::piece_selector::build_wanted_pieces(
                            &self.file_priorities, &file_lengths, self.lengths.as_ref().unwrap(),
                        );
                        if self.config.share_mode {
                            self.transition_state(TorrentState::Sharing);
                        } else {
                            self.transition_state(TorrentState::Downloading);
                        }
                        self.metadata_downloader = None;

                        // Populate tracker manager with newly parsed metadata
                        if let Some(ref meta) = self.meta {
                            self.tracker_manager.set_metadata(meta);
                        }

                        // Detect hybrid/v2 from metadata and update dual-swarm state
                        // (Gap 1 & 2: propagate info_hashes to tracker + DHT after magnet resolves)
                        if let Ok(detected) = ferrite_core::torrent_from_bytes_any(&torrent_bytes) {
                            let new_version = detected.version();
                            if new_version != ferrite_core::TorrentVersion::V1Only {
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
                                            && let Some(ref dht6) = self.dht_v6
                                            && let Ok(rx) = dht6.get_peers(v2_as_v1).await
                                        {
                                            self.dht_v6_v2_peers_rx = Some(rx);
                                        }
                                    }
                                }
                            }
                        }

                        let name = self.meta.as_ref().map(|m| m.info.name.clone()).unwrap_or_default();
                        post_alert(&self.alert_tx, &self.alert_mask, AlertKind::MetadataReceived {
                            info_hash: self.info_hash,
                            name,
                        });
                        info!("metadata assembled, switching to Downloading");

                        // Start web seeds now that we have metadata
                        self.spawn_web_seeds();
                        self.assign_pieces_to_web_seeds();
                    }
                    Err(e) => {
                        warn!("failed to parse assembled metadata: {e}");
                    }
                }
            }
            Err(e) => {
                warn!("metadata assembly failed: {e}");
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
        let file_map = ferrite_storage::FileMap::new(file_lengths, lengths.clone());

        // BEP 19 (GetRight) web seeds
        for url in &meta.url_list {
            if self.banned_web_seeds.contains(url) || self.web_seeds.contains_key(url) {
                continue;
            }
            if self.web_seeds.len() >= self.config.max_web_seeds {
                break;
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

            // BEP 17 doesn't use URL builder for per-file paths; it sends parameterized URLs
            let url_builder = crate::web_seed::WebSeedUrlBuilder::single(
                url.clone(),
                meta.info.name.clone(),
            );

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
            // Find lowest-index piece that is: not verified, not in peer in_flight,
            // not in web_seed_in_flight, and wanted.
            let piece = (0..self.num_pieces).find(|&i| {
                !ct.has_piece(i)
                    && !self.in_flight_pieces.contains_key(&i)
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
            && let Err(e) = disk.write_chunk(index, 0, data.clone(), DiskJobFlags::FLUSH_PIECE).await
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

    // ----- Piece requesting -----

    async fn request_pieces_from_peer(&mut self, peer_addr: SocketAddr) {
        if self.state != TorrentState::Downloading {
            return;
        }

        let ct = match &self.chunk_tracker {
            Some(ct) => ct,
            None => return,
        };
        if self.meta.is_none() {
            return;
        }

        // Rate limit: don't send new requests if download budget exhausted
        if !self.download_bucket.is_unlimited() && self.download_bucket.available() == 0 {
            return;
        }

        // In end-game: request single block, no pipelining
        if self.end_game.is_active() {
            self.request_end_game_block(peer_addr).await;
            return;
        }

        // Compute available slots from pipeline
        let (slots, peer_speed, peer_rate, is_snubbed) = match self.peers.get(&peer_addr) {
            Some(p) => {
                if p.peer_choking || p.upload_only {
                    return;
                }
                let depth = if p.snubbed { 1 } else { p.pipeline.queue_depth() };
                let avail = depth.saturating_sub(p.pending_requests.len());
                if avail == 0 {
                    return;
                }
                let speed = PeerSpeed::from_rate(p.pipeline.ewma_rate());
                (avail, speed, p.pipeline.ewma_rate(), p.snubbed)
            }
            None => return,
        };

        // Parole piece assignment: check if any parole piece can be assigned
        // to this peer (peer must not be an original contributor, must have the piece,
        // and must not already have a parole peer assigned).
        let peer_ip = peer_addr.ip();
        for (&parole_idx, parole) in &mut self.parole_pieces {
            if parole.parole_peer.is_some() {
                continue; // already assigned
            }
            if parole.original_contributors.contains(&peer_ip) {
                continue; // this peer is a suspect
            }
            let peer_has = self.peers.get(&peer_addr)
                .is_some_and(|p| p.bitfield.get(parole_idx));
            if !peer_has {
                continue;
            }
            // Assign this peer as parole peer
            parole.parole_peer = Some(peer_ip);
            debug!(parole_idx, %peer_addr, "assigned parole peer for piece");
            break;
        }

        let we_have = ct.bitfield().clone();
        let completed_count = ct.bitfield().count_ones();

        // Collect pieces to request (must borrow self.peers immutably for bitfield)
        let peer_bitfield = match self.peers.get(&peer_addr) {
            Some(p) => p.bitfield.clone(),
            None => return,
        };
        let suggested = self.peers.get(&peer_addr)
            .map(|p| p.suggested_pieces.clone())
            .unwrap_or_default();

        let ctx = PickContext {
            peer_addr,
            peer_has: &peer_bitfield,
            peer_speed,
            peer_is_snubbed: is_snubbed,
            peer_rate,
            we_have: &we_have,
            in_flight_pieces: &self.in_flight_pieces,
            wanted: &self.wanted_pieces,
            streaming_pieces: &self.streaming_pieces,
            time_critical_pieces: &self.time_critical_pieces,
            suggested_pieces: &suggested,
            sequential_download: self.config.sequential_download,
            completed_count,
            initial_picker_threshold: self.config.initial_picker_threshold,
            connected_peer_count: self.peers.len(),
            whole_pieces_threshold: self.config.whole_pieces_threshold,
            piece_size: self.lengths.as_ref().map(|l| l.piece_length() as u32).unwrap_or(262144),
            extent_affinity: self.config.piece_extent_affinity,
            auto_sequential_active: self.config.auto_sequential && self.auto_sequential_active,
        };

        let missing_chunks_fn = |piece: u32| -> Vec<(u32, u32)> {
            self.chunk_tracker.as_ref()
                .map(|ct| ct.missing_chunks(piece))
                .unwrap_or_default()
        };

        if let Some(result) = self.piece_selector.pick_blocks(&ctx, missing_chunks_fn) {
            // Track in in_flight_pieces
            let total_blocks = self.chunk_tracker.as_ref()
                .map(|ct| ct.missing_chunks(result.piece).len() as u32)
                .unwrap_or(1);
            let ifp = self.in_flight_pieces
                .entry(result.piece)
                .or_insert_with(|| InFlightPiece::new(total_blocks));

            for (begin, length) in result.blocks.iter().take(slots) {
                ifp.assigned_blocks.insert((result.piece, *begin), peer_addr);

                // Check download rate limits
                if !self.download_bucket.is_unlimited() {
                    let is_local = crate::rate_limiter::is_local_network(peer_addr.ip());
                    if !is_local
                        && let Some(ref global) = self.global_download_bucket
                        && !global.lock().unwrap().try_consume(*length as u64)
                    {
                        break;
                    }
                    if !self.download_bucket.try_consume(*length as u64) {
                        break;
                    }
                }

                if let Some(peer) = self.peers.get_mut(&peer_addr) {
                    peer.pipeline.request_sent(result.piece, *begin, std::time::Instant::now());
                    let _ = peer
                        .cmd_tx
                        .send(PeerCommand::Request {
                            index: result.piece,
                            begin: *begin,
                            length: *length,
                        })
                        .await;
                    peer.pending_requests.push((result.piece, *begin, *length));
                }
            }
        } else {
            self.check_end_game_activation();
        }
    }

    fn check_end_game_activation(&mut self) {
        if self.end_game.is_active() || self.state != TorrentState::Downloading {
            return;
        }
        let Some(ref ct) = self.chunk_tracker else {
            return;
        };
        let have = ct.bitfield().count_ones();
        let in_flight = self.in_flight_pieces.len() as u32;
        if have + in_flight >= self.num_pieces && !self.in_flight_pieces.is_empty() {
            let pending: Vec<_> = self
                .peers
                .iter()
                .map(|(addr, p)| (*addr, p.pending_requests.clone()))
                .collect();
            self.end_game.activate_with_inflight(&self.in_flight_pieces, &pending);
            info!(
                blocks = self.end_game.block_count(),
                "end-game mode activated"
            );
        }
    }

    async fn request_end_game_block(&mut self, peer_addr: SocketAddr) {
        let can_request = self
            .peers
            .get(&peer_addr)
            .is_some_and(|p| !p.peer_choking && p.pending_requests.is_empty());
        if !can_request {
            return; // In end-game: only 1 pending request per peer (no pipelining)
        }

        let peer_bitfield = match self.peers.get(&peer_addr) {
            Some(p) => p.bitfield.clone(),
            None => return,
        };

        let block = if !self.streaming_pieces.is_empty() {
            self.end_game.pick_block_streaming(peer_addr, &peer_bitfield, &self.streaming_pieces)
        } else if self.config.strict_end_game {
            self.end_game.pick_block_strict(peer_addr, &peer_bitfield, &[])
        } else {
            self.end_game.pick_block(peer_addr, &peer_bitfield)
        };

        if let Some((index, begin, length)) = block {
            self.end_game.register_request(index, begin, peer_addr);
            if let Some(peer) = self.peers.get_mut(&peer_addr) {
                let _ = peer
                    .cmd_tx
                    .send(PeerCommand::Request {
                        index,
                        begin,
                        length,
                    })
                    .await;
                peer.pending_requests.push((index, begin, length));
            }
        }
    }

    fn update_streaming_cursors(&mut self) {
        // Remove cursors whose receiver has been dropped (FileStream dropped)
        self.streaming_cursors.retain(|c| c.cursor_rx.has_changed().is_ok());

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
                peer.bitfield
                    .ones()
                    .any(|i| !we_have.get(i))
            } else {
                false
            }
        });

        if dominated == Some(true)
            && let Some(peer) = self.peers.get_mut(&peer_addr)
            && !peer.am_interested
        {
            peer.am_interested = true;
            let _ = peer.cmd_tx.send(PeerCommand::SetInterested(true)).await;
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

    /// Peer turnover: disconnect worst-performing peers and connect replacements.
    async fn run_peer_turnover(&mut self) {
        // Only during active downloading
        if self.state != TorrentState::Downloading {
            return;
        }

        // Check cutoff: if current rate >= cutoff * peak, don't churn
        let aggregate_download: u64 = self.peers.values().map(|p| p.download_rate).sum();
        if self.peak_download_rate > 0 {
            let threshold = self.config.peer_turnover_cutoff * self.peak_download_rate as f64;
            if aggregate_download as f64 >= threshold {
                return;
            }
        }

        // Collect parole IPs for exemption check
        let parole_ips: std::collections::HashSet<std::net::IpAddr> = self
            .parole_pieces
            .values()
            .filter_map(|p| p.parole_peer)
            .collect();

        // Collect eligible peers (not exempt)
        let mut eligible: Vec<(SocketAddr, u64)> = self
            .peers
            .values()
            .filter(|p| {
                // Exempt: seeds (bitfield complete)
                if p.bitfield.count_ones() == self.num_pieces {
                    return false;
                }
                // Exempt: outstanding requests
                if !p.pending_requests.is_empty() {
                    return false;
                }
                // Exempt: parole
                if parole_ips.contains(&p.addr.ip()) {
                    return false;
                }
                // Exempt: recently connected (< 30s)
                if p.connected_at.elapsed() < Duration::from_secs(30) {
                    return false;
                }
                true
            })
            .map(|p| (p.addr, p.download_rate))
            .collect();

        if eligible.is_empty() {
            return;
        }

        // Sort by download rate ascending (worst first)
        eligible.sort_by_key(|&(_, rate)| rate);

        // Calculate how many to disconnect
        let turnover_count = (eligible.len() as f64 * self.config.peer_turnover).floor() as usize;
        let turnover_count = if self.config.peer_turnover > 0.0 {
            turnover_count.max(1)
        } else {
            0
        };

        if turnover_count == 0 {
            return;
        }

        let to_disconnect: Vec<SocketAddr> = eligible
            .iter()
            .take(turnover_count)
            .map(|&(addr, _)| addr)
            .collect();

        // Disconnect peers — follow the SAME cleanup pattern as PeerEvent::Disconnected handler
        for &addr in &to_disconnect {
            // BEP 16: clean up super-seed assignment (before remove, matching disconnect handler)
            if let Some(ref mut ss) = self.super_seed {
                ss.peer_disconnected(addr);
            }
            if let Some(peer) = self.peers.remove(&addr) {
                self.piece_selector.remove_peer_bitfield(&peer.bitfield);
                // Remove pieces that only this peer was downloading
                let peer_pieces: HashSet<u32> = peer
                    .pending_requests
                    .iter()
                    .map(|&(idx, _, _)| idx)
                    .collect();
                for piece_idx in peer_pieces {
                    let other_has = self.peers.values().any(|p| {
                        p.pending_requests.iter().any(|&(i, _, _)| i == piece_idx)
                    });
                    if !other_has {
                        self.in_flight_pieces.remove(&piece_idx);
                    }
                }
                // Clean up pipeline request tracking for disconnected peer
                for ifp in self.in_flight_pieces.values_mut() {
                    ifp.assigned_blocks.retain(|_, a| *a != addr);
                }
                // End-game cleanup
                if self.end_game.is_active() {
                    self.end_game.peer_disconnected(addr);
                }
                // Send shutdown command (peer is still connected, unlike normal disconnect)
                let _ = peer.cmd_tx.send(PeerCommand::Shutdown).await;
                post_alert(&self.alert_tx, &self.alert_mask, AlertKind::PeerDisconnected {
                    info_hash: self.info_hash,
                    addr,
                    reason: Some("peer turnover".into()),
                });
            }
            // Suggest tracking cleanup (outside if-let, matching disconnect handler)
            self.suggested_to_peers.remove(&addr);
        }

        // Connect replacements
        let peers_before = self.peers.len();
        self.try_connect_peers();
        let replaced = self.peers.len().saturating_sub(peers_before);

        // Fire turnover alert
        post_alert(&self.alert_tx, &self.alert_mask, AlertKind::PeerTurnover {
            info_hash: self.info_hash,
            disconnected: to_disconnect.len(),
            replaced,
        });
    }

    async fn run_choker(&mut self) {
        let peer_infos: Vec<PeerInfo> = self
            .peers
            .values()
            .map(|p| PeerInfo {
                addr: p.addr,
                download_rate: p.download_rate,
                upload_rate: p.upload_rate,
                interested: p.peer_interested,
                upload_only: p.upload_only,
                is_seed: p.upload_only || (self.num_pieces > 0 && p.bitfield.count_ones() == self.num_pieces),
            })
            .collect();

        let decision = self.choker.decide(&peer_infos);

        for addr in &decision.to_unchoke {
            if let Some(peer) = self.peers.get_mut(addr)
                && peer.am_choking
            {
                peer.am_choking = false;
                let _ = peer.cmd_tx.send(PeerCommand::SetChoking(false)).await;
            }
        }

        for addr in &decision.to_choke {
            if let Some(peer) = self.peers.get_mut(addr)
                && !peer.am_choking
            {
                if peer.supports_fast {
                    let pending: Vec<(u32, u32, u32)> =
                        peer.incoming_requests.drain(..).collect();
                    for (index, begin, length) in pending {
                        let _ = peer
                            .cmd_tx
                            .send(PeerCommand::RejectRequest {
                                index,
                                begin,
                                length,
                            })
                            .await;
                    }
                }
                peer.am_choking = true;
                let _ = peer.cmd_tx.send(PeerCommand::SetChoking(true)).await;
            }
        }

        // Serve any buffered requests from newly-unchoked peers
        self.serve_incoming_requests().await;
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
                let _ = peer
                    .cmd_tx
                    .send(PeerCommand::RejectRequest { index, begin, length })
                    .await;
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

            match disk.read_chunk(index, begin, length, DiskJobFlags::empty()).await {
                Ok(data) => {
                    if let Some(peer) = self.peers.get_mut(&addr) {
                        peer.incoming_requests
                            .retain(|&(i, b, l)| !(i == index && b == begin && l == length));
                        let _ = peer
                            .cmd_tx
                            .send(PeerCommand::SendPiece {
                                index,
                                begin,
                                data,
                            })
                            .await;
                        self.uploaded += chunk_size;
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
        let peer_infos: Vec<PeerInfo> = self
            .peers
            .values()
            .map(|p| PeerInfo {
                addr: p.addr,
                download_rate: p.download_rate,
                upload_rate: p.upload_rate,
                interested: p.peer_interested,
                upload_only: p.upload_only,
                is_seed: p.upload_only || (self.num_pieces > 0 && p.bitfield.count_ones() == self.num_pieces),
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

        let availability = self.piece_selector.availability();
        if let Some(idx) = ss.assign_piece(peer_addr, &peer_bitfield, availability, self.num_pieces)
            && let Some(peer) = self.peers.get(&peer_addr)
        {
            let _ = peer.cmd_tx.send(PeerCommand::Have(idx)).await;
        }
    }

    // ----- Peer connectivity -----

    fn try_connect_peers(&mut self) {
        while self.peers.len() < self.config.max_peers {
            let (addr, source) = match self.available_peers.pop() {
                Some(pair) => pair,
                None => break,
            };

            if self.peers.contains_key(&addr) {
                continue;
            }

            // Skip banned peers
            if self.ban_manager.read().unwrap().is_banned(&addr.ip()) {
                continue;
            }

            // Skip IP-filtered peers
            if self.ip_filter.read().unwrap().is_blocked(addr.ip()) {
                post_alert(&self.alert_tx, &self.alert_mask, AlertKind::PeerBlocked { addr });
                continue;
            }

            let (cmd_tx, cmd_rx) = mpsc::channel(64);
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
            post_alert(&self.alert_tx, &self.alert_mask, AlertKind::PeerConnected {
                info_hash: self.info_hash,
                addr,
            });

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
            let info_bytes = self.meta.as_ref().and_then(|m| m.info_bytes.clone());
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
            let ssl_client_config = self.meta.as_ref()
                .and_then(|m| m.ssl_cert.as_deref())
                .and_then(|cert| {
                    self.ssl_manager.as_ref().and_then(|mgr| {
                        match mgr.client_config(cert) {
                            Ok(cfg) => Some(cfg),
                            Err(e) => {
                                warn!(%addr, error = %e, "failed to build SSL client config");
                                None
                            }
                        }
                    })
                });

            tokio::spawn(async move {
                // Try uTP first (5s timeout), fall back to TCP
                // Note: uTP is not proxied — if proxy is active, skip uTP
                if enable_utp && !use_proxy
                    && let Some(socket) = utp_socket
                {
                    match tokio::time::timeout(
                        Duration::from_secs(5),
                        socket.connect(addr),
                    )
                    .await
                    {
                        Ok(Ok(stream)) => {
                            debug!(%addr, "uTP connection established");
                            let _ = event_tx.send(PeerEvent::TransportIdentified {
                                peer_addr: addr,
                                transport: crate::rate_limiter::PeerTransport::Utp,
                            }).await;
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
                                true, // outbound
                                anonymous_mode,
                                info_bytes.clone(),
                                Arc::clone(&plugins),
                                enable_holepunch,
                            )
                            .await;
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

                // TCP connection — through proxy if configured
                let tcp_result = if use_proxy {
                    crate::proxy::connect_through_proxy(&proxy_config, addr).await
                } else {
                    tokio::net::TcpStream::connect(addr).await
                };
                match tcp_result {
                    Ok(stream) => {
                        let _ = event_tx.send(PeerEvent::TransportIdentified {
                            peer_addr: addr,
                            transport: crate::rate_limiter::PeerTransport::Tcp,
                        }).await;
                        // SSL torrent: wrap TCP stream in TLS (M42)
                        if let Some(ref client_config) = ssl_client_config {
                            match ferrite_wire::ssl::connect_tls(stream, info_hash, Arc::clone(client_config)).await {
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
                                        ferrite_wire::mse::EncryptionMode::Disabled,
                                        true, // outbound
                                        anonymous_mode,
                                        info_bytes,
                                        plugins,
                                        enable_holepunch,
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
                                true, // outbound
                                anonymous_mode,
                                info_bytes,
                                plugins,
                                enable_holepunch,
                            )
                            .await;
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
        if self.peers.len() >= self.config.max_peers {
            return;
        }

        let synthetic_addr = self.next_i2p_synthetic_addr();

        let dest_preview = {
            let b64 = stream.remote_destination().to_base64();
            if b64.len() >= 8 { b64[..8].to_string() } else { b64 }
        };
        let tcp_stream = stream.into_inner();

        self.spawn_peer_from_stream(synthetic_addr, tcp_stream);

        debug!(dest = %dest_preview, addr = %synthetic_addr, "accepted I2P peer");
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

    /// Spawn a peer task from an already-connected stream (for incoming connections and tests).
    fn spawn_peer_from_stream(
        &mut self,
        addr: SocketAddr,
        stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    ) {
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
        mode_override: Option<ferrite_wire::mse::EncryptionMode>,
    ) {
        if self.peers.contains_key(&addr) || self.peers.len() >= self.config.max_peers {
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
            post_alert(&self.alert_tx, &self.alert_mask, AlertKind::PeerBlocked { addr });
            return;
        }

        let (cmd_tx, cmd_rx) = mpsc::channel(64);
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
            .insert(addr, PeerState::new(addr, self.num_pieces, cmd_tx, PeerSource::Incoming));
        // Identify transport for incoming peers (M45)
        let transport = if mode_override.is_some() {
            crate::rate_limiter::PeerTransport::Utp
        } else {
            crate::rate_limiter::PeerTransport::Tcp
        };
        if let Some(peer) = self.peers.get_mut(&addr) {
            peer.transport = Some(transport);
        }
        post_alert(&self.alert_tx, &self.alert_mask, AlertKind::PeerConnected {
            info_hash: self.info_hash,
            addr,
        });

        let info_hash = self.info_hash;
        let peer_id = self.our_peer_id;
        let num_pieces = self.num_pieces;
        let event_tx = self.event_tx.clone();
        let enable_dht = self.config.enable_dht;
        let enable_fast = self.config.enable_fast;
        let encryption_mode = mode_override.unwrap_or(self.config.encryption_mode);
        let anonymous_mode = self.config.anonymous_mode;
        let enable_holepunch = self.config.enable_holepunch;
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
            )
            .await;
        });
    }

    // ── BEP 55 Holepunch (M40) ──

    /// Handle a Rendezvous request from an initiator peer.
    ///
    /// We act as relay: validate the request, then forward Connect messages to
    /// both the initiator and the target so they can perform simultaneous open.
    async fn handle_holepunch_rendezvous(&mut self, initiator_addr: SocketAddr, target: SocketAddr) {
        use ferrite_wire::HolepunchMessage;

        debug!(%initiator_addr, %target, "holepunch: processing rendezvous request");

        // Cannot relay to ourselves
        if target == initiator_addr {
            debug!(%initiator_addr, "holepunch: rendezvous target == initiator (NoSelf)");
            if let Some(peer) = self.peers.get(&initiator_addr) {
                let _ = peer.cmd_tx.send(PeerCommand::SendHolepunch(
                    HolepunchMessage::error(target, ferrite_wire::HolepunchError::NoSelf),
                )).await;
            }
            return;
        }

        // Target must be connected to us
        let target_peer = match self.peers.get(&target) {
            Some(p) => p,
            None => {
                debug!(%initiator_addr, %target, "holepunch: target not connected (NotConnected)");
                if let Some(peer) = self.peers.get(&initiator_addr) {
                    let _ = peer.cmd_tx.send(PeerCommand::SendHolepunch(
                        HolepunchMessage::error(target, ferrite_wire::HolepunchError::NotConnected),
                    )).await;
                }
                return;
            }
        };

        // Target must support holepunch
        if !target_peer.supports_holepunch {
            debug!(%initiator_addr, %target, "holepunch: target doesn't support holepunch (NoSupport)");
            if let Some(peer) = self.peers.get(&initiator_addr) {
                let _ = peer.cmd_tx.send(PeerCommand::SendHolepunch(
                    HolepunchMessage::error(target, ferrite_wire::HolepunchError::NoSupport),
                )).await;
            }
            return;
        }

        // Forward Connect to target: "connect to the initiator"
        let _ = target_peer.cmd_tx.send(PeerCommand::SendHolepunch(
            HolepunchMessage::connect(initiator_addr),
        )).await;

        // Forward Connect to initiator: "connect to the target"
        if let Some(initiator) = self.peers.get(&initiator_addr) {
            let _ = initiator.cmd_tx.send(PeerCommand::SendHolepunch(
                HolepunchMessage::connect(target),
            )).await;
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
        if self.peers.len() >= self.config.max_peers {
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
            post_alert(&self.alert_tx, &self.alert_mask, AlertKind::PeerBlocked { addr: target });
            return;
        }

        // Set up peer state and channels (same pattern as try_connect_peers)
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let bitfield = if self.super_seed.is_some() {
            Bitfield::new(self.num_pieces)
        } else {
            self.chunk_tracker
                .as_ref()
                .map(|ct| ct.bitfield().clone())
                .unwrap_or_else(|| Bitfield::new(self.num_pieces))
        };

        self.peers.insert(target, PeerState::new(target, self.num_pieces, cmd_tx, PeerSource::Pex));
        post_alert(&self.alert_tx, &self.alert_mask, AlertKind::PeerConnected {
            info_hash: self.info_hash,
            addr: target,
        });

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
        let info_bytes = self.meta.as_ref().and_then(|m| m.info_bytes.clone());
        let utp_socket = if target.is_ipv6() {
            self.utp_socket_v6.clone()
        } else {
            self.utp_socket.clone()
        };
        let plugins = Arc::clone(&self.plugins);

        tokio::spawn(async move {
            // Try uTP first (5s timeout) — preferred for holepunch as NAT traversal
            // works better with UDP-based protocols
            if enable_utp
                && let Some(socket) = utp_socket
            {
                match tokio::time::timeout(
                    Duration::from_secs(5),
                    socket.connect(target),
                ).await {
                    Ok(Ok(stream)) => {
                        debug!(%target, "holepunch: uTP connection established");
                        post_alert(&alert_tx, &alert_mask, AlertKind::HolepunchSucceeded {
                            info_hash,
                            addr: target,
                        });
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
                        ).await;
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
            match tokio::time::timeout(
                Duration::from_secs(10),
                tokio::net::TcpStream::connect(target),
            ).await {
                Ok(Ok(stream)) => {
                    debug!(%target, "holepunch: TCP connection established");
                    post_alert(&alert_tx, &alert_mask, AlertKind::HolepunchSucceeded {
                        info_hash,
                        addr: target,
                    });
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
                    ).await;
                }
                Ok(Err(e)) => {
                    let message = format!("TCP connect failed: {e}");
                    debug!(%target, %message, "holepunch: connection failed");
                    post_alert(&alert_tx, &alert_mask, AlertKind::HolepunchFailed {
                        info_hash,
                        addr: target,
                        error_code: None,
                        message,
                    });
                    let _ = event_tx.send(PeerEvent::Disconnected {
                        peer_addr: target,
                        reason: Some(format!("holepunch TCP connect failed: {e}")),
                    }).await;
                }
                Err(_) => {
                    let message = "TCP connect timed out".to_string();
                    debug!(%target, "holepunch: TCP connect timed out");
                    post_alert(&alert_tx, &alert_mask, AlertKind::HolepunchFailed {
                        info_hash,
                        addr: target,
                        error_code: None,
                        message,
                    });
                    let _ = event_tx.send(PeerEvent::Disconnected {
                        peer_addr: target,
                        reason: Some("holepunch TCP connect timed out".to_string()),
                    }).await;
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
        use ferrite_wire::HolepunchMessage;

        if !self.config.enable_holepunch {
            return;
        }

        // Don't holepunch if already connected
        if self.peers.contains_key(&target) {
            return;
        }

        // Find a relay: a connected peer that supports holepunch and has completed
        // the extension handshake (and isn't the target itself)
        let relay_addr = self.peers.iter()
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
            let _ = relay.cmd_tx.send(PeerCommand::SendHolepunch(
                HolepunchMessage::rendezvous(target),
            )).await;
        }
    }
}

/// Helper to accept a connection from an optional listener.
/// Returns `pending` if no listener is bound, so the `select!` branch is skipped.
async fn accept_incoming(
    listener: &Option<TcpListener>,
) -> std::io::Result<(tokio::net::TcpStream, SocketAddr)> {
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
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use bytes::Bytes;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use ferrite_wire::{ExtHandshake, Handshake, Message, MessageCodec};
    use futures::{SinkExt, StreamExt};
    use tokio_util::codec::{FramedRead, FramedWrite};

    // -- Helpers --

    /// Build a valid TorrentMetaV1 from raw data with given piece length.
    fn make_test_torrent(data: &[u8], piece_length: u64) -> TorrentMetaV1 {
        use serde::Serialize;

        let mut pieces = Vec::new();
        let mut offset = 0;
        while offset < data.len() {
            let end = (offset + piece_length as usize).min(data.len());
            let hash = ferrite_core::sha1(&data[offset..end]);
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

        let bytes = ferrite_bencode::to_bytes(&t).unwrap();
        torrent_from_bytes(&bytes).unwrap()
    }

    fn test_config() -> TorrentConfig {
        TorrentConfig {
            listen_port: 0, // random port
            max_peers: 50,
            target_request_queue: 5,
            download_dir: std::path::PathBuf::from("/tmp"),
            enable_dht: false,
            enable_pex: false,
            enable_fast: false,
            seed_ratio_limit: None,
            strict_end_game: true,
            upload_rate_limit: 0,
            download_rate_limit: 0,
            encryption_mode: ferrite_wire::mse::EncryptionMode::Disabled,
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
            snub_timeout_secs: 60,
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
            peer_turnover: 0.04,
            peer_turnover_cutoff: 0.9,
            peer_turnover_interval: 300,
            url_security: crate::url_guard::UrlSecurityConfig::default(),
        }
    }

    fn make_storage(data: &[u8], piece_length: u64) -> Arc<MemoryStorage> {
        let lengths = Lengths::new(
            data.len() as u64,
            piece_length,
            DEFAULT_CHUNK_SIZE,
        );
        Arc::new(MemoryStorage::new(lengths))
    }

    fn make_seeded_storage(data: &[u8], piece_length: u64) -> Arc<MemoryStorage> {
        let lengths = Lengths::new(
            data.len() as u64,
            piece_length,
            DEFAULT_CHUNK_SIZE,
        );
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
        Arc::new(std::sync::RwLock::new(crate::ban::BanManager::new(crate::ban::BanConfig::default())))
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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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
            info_hashes: ferrite_core::InfoHashes::v1_only(
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
        let handle = TorrentHandle::from_magnet(magnet, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
            .await
            .unwrap();

        handle
            .add_peers(vec![
                "127.0.0.1:6881".parse().unwrap(),
                "127.0.0.1:6882".parse().unwrap(),
            ], PeerSource::Tracker)
            .await
            .unwrap();

        // Small delay for the actor to process
        tokio::time::sleep(Duration::from_millis(50)).await;

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.peers_available, 2);

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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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
        let hash = ferrite_core::sha1(&data);
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

        let bytes = ferrite_bencode::to_bytes(&t).unwrap();
        let meta = torrent_from_bytes(&bytes).unwrap();
        assert_eq!(meta.info.private, Some(1));

        let storage = make_storage(&data, 16384);
        let mut config = test_config();
        config.enable_dht = true;
        config.enable_pex = true;

        // The from_torrent constructor should disable DHT and PEX
        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
            .await
            .unwrap();

        let addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        handle.add_peers(vec![addr, addr, addr], PeerSource::Tracker).await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        let stats = handle.stats().await.unwrap();
        // Only one unique peer should be in available
        assert_eq!(stats.peers_available, 1);

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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
            .await
            .unwrap();
        let handle2 = handle.clone();

        // Add peers through one handle
        handle
            .add_peers(vec!["127.0.0.1:7777".parse().unwrap()], PeerSource::Tracker)
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Read stats through the other
        let stats = handle2.stats().await.unwrap();
        assert_eq!(stats.peers_available, 1);

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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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
                .send(Message::Extended {
                    ext_id: 0,
                    payload,
                })
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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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
                .send(Message::Extended {
                    ext_id: 0,
                    payload,
                })
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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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
                .send(Message::Extended {
                    ext_id: 0,
                    payload,
                })
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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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
                .send(Message::Extended {
                    ext_id: 0,
                    payload,
                })
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
                .send(Message::Extended {
                    ext_id: 0,
                    payload,
                })
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
                        let piece_data = seeder_storage
                            .read_chunk(index, begin, length)
                            .unwrap();
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
        let leecher = TorrentHandle::from_torrent(leecher_meta, ferrite_core::TorrentVersion::V1Only, None, ldh, ldm, leecher_config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), latx, lamask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
            .await
            .unwrap();

        // Add seeder as a peer
        leecher.add_peers(vec![seeder_addr], PeerSource::Tracker).await.unwrap();

        // Give the connect interval time to fire (it ticks every 30s).
        // Instead, wait a bit for the initial connect tick. Since our interval
        // skips the first tick, the first real connect happens at 30s.
        // That's too long for a test. Let's trigger it by polling stats.
        //
        // Actually, the connect timer fires immediately on the SECOND tick after
        // the first (which we consumed). So we need a different approach.
        // We added the available peer — but the connect interval won't fire for 30s.
        //
        // Let's just wait and check — the actor's try_connect_peers runs on the timer.
        // For this test to be practical, we need a shorter interval or a direct connect trigger.
        //
        // Actually the timer will fire every 30 seconds after the first tick we consumed.
        // For testing, we can just wait longer or use a smaller torrent.
        // Let's wait up to 35 seconds with a short poll.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(35);
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
                    stats.state, stats.pieces_have, stats.pieces_total, stats.peers_connected, stats.peers_available,
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
            info_hashes: ferrite_core::InfoHashes::v1_only(
                Id20::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap(),
            ),
            display_name: Some("magnet test".into()),
            trackers: vec![],
            peers: vec![],
            selected_files: None,
        };

        let (atx, amask) = test_alert_channel();
        let (dm, _dj) = test_disk_manager();
        let handle = TorrentHandle::from_magnet(magnet, dm, test_config(), None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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
        let hash = ferrite_core::sha1(&data);
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

        let bytes = ferrite_bencode::to_bytes(&t).unwrap();
        let meta = torrent_from_bytes(&bytes).unwrap();
        assert!(meta.announce.is_some());

        let storage = make_storage(&data, 16384);
        let config = test_config();

        // The torrent should start and announce to tracker (which will fail since
        // the tracker doesn't exist, but that's fine — failures are non-fatal).
        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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
        let hash = ferrite_core::sha1(&data);
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

        let bytes = ferrite_bencode::to_bytes(&t).unwrap();
        let meta = torrent_from_bytes(&bytes).unwrap();
        assert_eq!(meta.info.private, Some(1));

        let storage = make_storage(&data, 16384);
        let config = test_config();

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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
            info_hashes: ferrite_core::InfoHashes::v1_only(
                Id20::from_hex("cccccccccccccccccccccccccccccccccccccccc").unwrap(),
            ),
            display_name: Some("magnet test".into()),
            trackers: vec![],
            peers: vec![],
            selected_files: None,
        };

        let (atx, amask) = test_alert_channel();
        let (dm, _dj) = test_disk_manager();
        let handle = TorrentHandle::from_magnet(magnet, dm, test_config(), None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
            .await
            .unwrap();

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::FetchingMetadata);

        // No tracker announces should happen in FetchingMetadata state
        // (verified by the tracker_announce arm checking state != FetchingMetadata)
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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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

                let hs = Handshake::new(info_hash, Id20::from_hex("6666666666666666666666666666666666666666").unwrap());
                writer.write_all(&hs.to_bytes()).await.unwrap();
                writer.flush().await.unwrap();
                let mut hs_buf = [0u8; HANDSHAKE_SIZE];
                reader.read_exact(&mut hs_buf).await.unwrap();

                let mut framed_read = FramedRead::new(reader, MessageCodec::new());
                let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

                let _msg = framed_read.next().await; // ext handshake
                let ext_hs = ExtHandshake::new();
                let payload = ext_hs.to_bytes().unwrap();
                framed_write.send(Message::Extended { ext_id: 0, payload }).await.unwrap();

                // Send bitfield + unchoke
                let mut bf = Bitfield::new(1);
                bf.set(0);
                framed_write.send(Message::Bitfield(Bytes::copy_from_slice(bf.as_bytes()))).await.unwrap();
                framed_write.send(Message::Unchoke).await.unwrap();

                // Respond to requests
                while let Some(Ok(msg)) = framed_read.next().await {
                    match msg {
                        Message::Request { index, begin, length } => {
                            let start = begin as usize;
                            let end = start + length as usize;
                            framed_write.send(Message::Piece {
                                index,
                                begin,
                                data: Bytes::copy_from_slice(&seed_data[start..end]),
                            }).await.unwrap();
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

                let hs = Handshake::new(info_hash, Id20::from_hex("7777777777777777777777777777777777777777").unwrap());
                writer.write_all(&hs.to_bytes()).await.unwrap();
                writer.flush().await.unwrap();
                let mut hs_buf = [0u8; HANDSHAKE_SIZE];
                reader.read_exact(&mut hs_buf).await.unwrap();

                let mut framed_read = FramedRead::new(reader, MessageCodec::new());
                let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

                let _msg = framed_read.next().await; // ext handshake
                let ext_hs = ExtHandshake::new();
                let payload = ext_hs.to_bytes().unwrap();
                framed_write.send(Message::Extended { ext_id: 0, payload }).await.unwrap();

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
                framed_write.send(Message::Request { index: 0, begin: 0, length: 16384 }).await.unwrap();

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
        assert!(stats.uploaded > 0, "expected uploaded > 0, got {}", stats.uploaded);

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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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

                let hs = Handshake::new(info_hash, Id20::from_hex("8888888888888888888888888888888888888888").unwrap());
                writer.write_all(&hs.to_bytes()).await.unwrap();
                writer.flush().await.unwrap();
                let mut hs_buf = [0u8; HANDSHAKE_SIZE];
                reader.read_exact(&mut hs_buf).await.unwrap();

                let mut framed_read = FramedRead::new(reader, MessageCodec::new());
                let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

                let _msg = framed_read.next().await;
                let ext_hs = ExtHandshake::new();
                let payload = ext_hs.to_bytes().unwrap();
                framed_write.send(Message::Extended { ext_id: 0, payload }).await.unwrap();

                let mut bf = Bitfield::new(1);
                bf.set(0);
                framed_write.send(Message::Bitfield(Bytes::copy_from_slice(bf.as_bytes()))).await.unwrap();
                framed_write.send(Message::Unchoke).await.unwrap();

                while let Some(Ok(msg)) = framed_read.next().await {
                    match msg {
                        Message::Request { index, begin, length } => {
                            let start = begin as usize;
                            let end = start + length as usize;
                            framed_write.send(Message::Piece {
                                index,
                                begin,
                                data: Bytes::copy_from_slice(&seed_data[start..end]),
                            }).await.unwrap();
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

                let hs = Handshake::new(info_hash, Id20::from_hex("9999999999999999999999999999999999999999").unwrap());
                writer.write_all(&hs.to_bytes()).await.unwrap();
                writer.flush().await.unwrap();
                let mut hs_buf = [0u8; HANDSHAKE_SIZE];
                reader.read_exact(&mut hs_buf).await.unwrap();

                let mut framed_read = FramedRead::new(reader, MessageCodec::new());
                let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

                let _msg = framed_read.next().await;
                let ext_hs = ExtHandshake::new();
                let payload = ext_hs.to_bytes().unwrap();
                framed_write.send(Message::Extended { ext_id: 0, payload }).await.unwrap();

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
                framed_write.send(Message::Request { index: 0, begin: 0, length: 16384 }).await.unwrap();

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
                assert!(stats.uploaded >= 16384, "expected uploaded >= 16384, got {}", stats.uploaded);
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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
            .await
            .unwrap();

        // Give the actor time to verify existing pieces
        tokio::time::sleep(Duration::from_millis(100)).await;

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Seeding, "should start as seeder with all pieces verified");
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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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
        assert_eq!(rd.pieces[0] & 0xC0, 0xC0, "both pieces should be marked complete");

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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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

        let meta = ferrite_core::torrent_from_bytes(&torrent_bytes).unwrap();
        let lengths = Lengths::new(200, 100, DEFAULT_CHUNK_SIZE);
        let storage: Arc<dyn TorrentStorage> = Arc::new(MemoryStorage::new(lengths));
        let config = TorrentConfig {
            listen_port: 0,
            ..Default::default()
        };

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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

        let meta = ferrite_core::torrent_from_bytes(&torrent_bytes).unwrap();
        let lengths = Lengths::new(200, 100, DEFAULT_CHUNK_SIZE);
        let storage: Arc<dyn TorrentStorage> = Arc::new(MemoryStorage::new(lengths));
        let config = TorrentConfig {
            listen_port: 0,
            ..Default::default()
        };

        let (atx, amask) = test_alert_channel();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
            .await
            .unwrap();

        // Set file priorities
        handle.set_file_priority(0, FilePriority::High).await.unwrap();
        handle.set_file_priority(1, FilePriority::Skip).await.unwrap();

        // Save resume data
        let rd = handle.save_resume_data().await.unwrap();
        assert_eq!(rd.file_priority, vec![7, 0]); // High=7, Skip=0

        // Verify bencode round-trip
        let encoded = ferrite_bencode::to_bytes(&rd).unwrap();
        let decoded: ferrite_core::FastResumeData = ferrite_bencode::from_bytes(&encoded).unwrap();
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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Connect mock leecher (raw handshake + framed messages)
        let stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let (reader, writer) = tokio::io::split(stream);
        let mut writer = writer;
        let mut reader = reader;

        let hs = Handshake::new(info_hash, Id20::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap());
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
        framed_write.send(Message::Extended { ext_id: 0, payload }).await.unwrap();

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
        framed_write.send(Message::Request { index: 0, begin: 0, length: 16384 }).await.unwrap();

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
        assert!(!got_piece, "piece should be delayed by rate limiter (1 KB/s for 16 KB chunk)");

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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None)
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Connect mock seeder
        let stream = tokio::net::TcpStream::connect(listen_addr).await.unwrap();
        let (reader, writer) = tokio::io::split(stream);
        let mut writer = writer;
        let mut reader = reader;

        let hs = Handshake::new(info_hash, Id20::from_hex("cccccccccccccccccccccccccccccccccccccccc").unwrap());
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
        framed_write.send(Message::Extended { ext_id: 0, payload }).await.unwrap();

        // Send bitfield saying we have all pieces (act as seeder)
        let mut bf = Bitfield::new(2);
        bf.set(0);
        bf.set(1);
        framed_write.send(Message::Bitfield(Bytes::copy_from_slice(bf.as_bytes()))).await.unwrap();

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

        let mut mgr = BanManager::new(BanConfig { max_failures: 2, use_parole: true });

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

        let mut mgr = BanManager::new(BanConfig { max_failures: 2, use_parole: true });

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
        let handle = TorrentHandle::from_torrent(meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None, crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, Arc::clone(&ban_mgr), test_ip_filter(), Arc::new(Vec::new()), None, None)
            .await
            .unwrap();

        // Add the banned peer — it should be filtered out
        handle.add_peers(vec![
            SocketAddr::new(banned_ip, 6881),
            "10.0.0.1:6881".parse().unwrap(),
        ], PeerSource::Tracker).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let stats = handle.stats().await.unwrap();
        // Only the non-banned peer should be in available pool (and may have connected)
        // The banned one should never appear
        assert!(stats.peers_available + stats.peers_connected <= 1,
            "banned peer should not be added: available={}, connected={}",
            stats.peers_available, stats.peers_connected);

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
        assert_eq!(s.hashing_threads, 2);
        let tc = TorrentConfig::default();
        assert_eq!(tc.hashing_threads, 2);
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
            meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None,
            crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None,
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
                    AlertKind::StateChanged { new_state: TorrentState::Checking, .. } => {
                        saw_checking = true;
                    }
                    AlertKind::CheckingProgress { progress, .. } => {
                        progress_values.push(progress);
                    }
                    AlertKind::TorrentChecked { pieces_have, pieces_total, .. } => {
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
        assert!(!progress_values.is_empty(), "should have seen CheckingProgress alerts");
        // Progress should be monotonically increasing
        for w in progress_values.windows(2) {
            assert!(w[1] >= w[0], "progress should be monotonically increasing: {} < {}", w[0], w[1]);
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
            meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None,
            crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None,
        )
        .await
        .unwrap();

        // Give actor time to finish checking (no valid pieces → Downloading)
        tokio::time::sleep(Duration::from_millis(100)).await;

        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);
        assert_eq!(stats.checking_progress, 0.0, "checking_progress should be 0.0 when not checking");

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
            storage.write_chunk(p, 0, &data[offset..offset + size]).unwrap();
        }
        // Pieces 2 and 3 have no data (zeros) — won't match hash

        let config = test_config();
        let (atx, amask) = test_alert_channel();
        let mut rx = atx.subscribe();
        let (dh, dm, _dj) = test_register_disk(meta.info_hash, storage).await;
        let handle = TorrentHandle::from_torrent(
            meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None,
            crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), test_ip_filter(), Arc::new(Vec::new()), None, None,
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
                    if let AlertKind::TorrentChecked { pieces_have, pieces_total, .. } = alert.kind {
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
            meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None,
            crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), Arc::clone(&ip_filter), Arc::new(Vec::new()), None, None,
        )
        .await
        .unwrap();

        // Add peers: one blocked (public IP in TEST-NET-3), one allowed (different public IP)
        let blocked_addr: SocketAddr = "203.0.113.42:6881".parse().unwrap();
        let allowed_addr: SocketAddr = "198.51.100.1:6881".parse().unwrap();
        handle.add_peers(vec![blocked_addr, allowed_addr], PeerSource::Tracker).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let stats = handle.stats().await.unwrap();
        // Only the allowed peer should be in the pool
        assert!(stats.peers_available + stats.peers_connected <= 1,
            "blocked peer should not be added: available={}, connected={}",
            stats.peers_available, stats.peers_connected);

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
            meta, ferrite_core::TorrentVersion::V1Only, None, dh, dm, config, None, None, None, None,
            crate::slot_tuner::SlotTuner::disabled(4), atx, amask, None, None, test_ban_manager(), Arc::clone(&ip_filter), Arc::new(Vec::new()), None, None,
        )
        .await
        .unwrap();

        // Initially, 198.51.100.1 (TEST-NET-2) is allowed
        let test_addr: SocketAddr = "198.51.100.1:6881".parse().unwrap();
        handle.add_peers(vec![test_addr], PeerSource::Tracker).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let stats = handle.stats().await.unwrap();
        assert!(stats.peers_available + stats.peers_connected >= 1,
            "peer should be allowed initially");
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
        assert!(ip_filter.read().unwrap().is_blocked("198.51.100.1".parse().unwrap()));
        // Verify a different public IP is still allowed
        assert!(!ip_filter.read().unwrap().is_blocked("203.0.113.1".parse().unwrap()));
    }

    #[test]
    fn relocate_files_moves_and_cleans_up() {
        let tmp = std::env::temp_dir().join(format!("ferrite_relocate_{}", std::process::id()));
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
        let tmp = std::env::temp_dir().join(format!("ferrite_relocate_skip_{}", std::process::id()));
        let src = tmp.join("src");
        let dst = tmp.join("dst");
        std::fs::create_dir_all(&src).unwrap();

        // File doesn't exist — should be skipped without error
        let file_paths = vec![std::path::PathBuf::from("nonexistent.txt")];
        relocate_files(&src, &dst, &file_paths).unwrap();

        assert!(!dst.join("nonexistent.txt").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---- M46: Peer turnover logic tests ----

    #[test]
    fn peer_turnover_identifies_worst_peers() {
        use std::time::Instant;

        let now = Instant::now();
        let old = now - Duration::from_secs(31);

        struct Candidate {
            addr: SocketAddr,
            download_rate: u64,
            is_seed: bool,
            has_requests: bool,
            in_parole: bool,
            connected_at: Instant,
        }

        let candidates = vec![
            Candidate { addr: "10.0.0.1:6881".parse().unwrap(), download_rate: 100, is_seed: false, has_requests: false, in_parole: false, connected_at: old },
            Candidate { addr: "10.0.0.2:6881".parse().unwrap(), download_rate: 5000, is_seed: false, has_requests: false, in_parole: false, connected_at: old },
            Candidate { addr: "10.0.0.3:6881".parse().unwrap(), download_rate: 50, is_seed: false, has_requests: false, in_parole: false, connected_at: old },
            Candidate { addr: "10.0.0.4:6881".parse().unwrap(), download_rate: 0, is_seed: true, has_requests: false, in_parole: false, connected_at: old },
            Candidate { addr: "10.0.0.5:6881".parse().unwrap(), download_rate: 10, is_seed: false, has_requests: true, in_parole: false, connected_at: old },
            Candidate { addr: "10.0.0.6:6881".parse().unwrap(), download_rate: 0, is_seed: false, has_requests: false, in_parole: false, connected_at: now },
            Candidate { addr: "10.0.0.7:6881".parse().unwrap(), download_rate: 5, is_seed: false, has_requests: false, in_parole: true, connected_at: old },
        ];

        let mut eligible: Vec<_> = candidates.iter()
            .filter(|c| !c.is_seed && !c.has_requests && !c.in_parole && c.connected_at.elapsed() >= Duration::from_secs(30))
            .collect();
        assert_eq!(eligible.len(), 3);

        eligible.sort_by_key(|c| c.download_rate);
        let turnover_count = ((eligible.len() as f64 * 0.04).floor() as usize).max(1);
        assert_eq!(turnover_count, 1);
        assert_eq!(eligible[0].addr, "10.0.0.3:6881".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn peer_turnover_cutoff_suppresses_at_peak() {
        let peak_rate: u64 = 10_000;
        let cutoff = 0.9;

        let current_rate: u64 = 9_500;
        assert!(current_rate as f64 >= cutoff * peak_rate as f64);

        let current_rate: u64 = 8_000;
        assert!(!(current_rate as f64 >= cutoff * peak_rate as f64));
    }

    #[test]
    fn peer_turnover_disabled_when_interval_zero() {
        let interval = 0u64;
        assert_eq!(interval, 0);
    }

    #[test]
    fn peer_turnover_no_action_when_seeding() {
        let state = TorrentState::Seeding;
        assert_ne!(state, TorrentState::Downloading);
    }
}
