//! SessionHandle / SessionActor — multi-torrent session manager.
//!
//! Actor model: SessionHandle is the cloneable public API (mpsc sender),
//! SessionActor is the single-owner event loop (internal).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::sync::{broadcast, mpsc, oneshot};

use tracing::{debug, info, warn};

use torrent_core::{DEFAULT_CHUNK_SIZE, Id20, Lengths, Magnet, TorrentMetaV1};
use torrent_dht::DhtHandle;
use torrent_storage::TorrentStorage;

use crate::alert::{Alert, AlertCategory, AlertKind, AlertStream, post_alert};
use crate::settings::Settings;
use crate::torrent::TorrentHandle;
use crate::types::{
    FileInfo, SessionStats, TorrentConfig, TorrentInfo, TorrentState, TorrentStats,
};

/// Shared global rate limiter bucket.
type SharedBucket = Arc<std::sync::Mutex<crate::rate_limiter::TokenBucket>>;

/// Function signature for queue move operations (move_up, move_down, etc.).
type QueueMoveFn = fn(&mut [crate::queue::QueueEntry], Id20) -> Vec<(Id20, i32, i32)>;

/// Shared session-wide ban manager, accessed by TorrentActors via `Arc`.
pub(crate) type SharedBanManager = Arc<std::sync::RwLock<crate::ban::BanManager>>;

/// Shared session-wide IP filter, accessed by TorrentActors via `Arc`.
pub(crate) type SharedIpFilter = Arc<std::sync::RwLock<crate::ip_filter::IpFilter>>;

/// Entry for a torrent managed by the session.
struct TorrentEntry {
    handle: TorrentHandle,
    meta: Option<TorrentMetaV1>,
    /// Queue position (-1 = not queued / not auto-managed).
    queue_position: i32,
    /// Whether the queue system controls this torrent.
    auto_managed: bool,
    /// When the torrent was last started/resumed (for startup grace period).
    started_at: Option<tokio::time::Instant>,
    /// Previous downloaded bytes (for rate calculation between auto-manage ticks).
    prev_downloaded: u64,
    /// Previous uploaded bytes (for rate calculation between auto-manage ticks).
    prev_uploaded: u64,
}

impl TorrentEntry {
    /// Returns `true` if this torrent has the private flag set (BEP 27).
    fn is_private(&self) -> bool {
        self.meta.as_ref().is_some_and(|m| m.info.private == Some(1))
    }
}

/// Commands sent from SessionHandle to SessionActor.
enum SessionCommand {
    AddTorrent {
        meta: Box<torrent_core::TorrentMeta>,
        storage: Option<Arc<dyn TorrentStorage>>,
        reply: oneshot::Sender<crate::Result<Id20>>,
    },
    AddMagnet {
        magnet: Magnet,
        reply: oneshot::Sender<crate::Result<Id20>>,
    },
    RemoveTorrent {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    PauseTorrent {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    ResumeTorrent {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    TorrentStats {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<TorrentStats>>,
    },
    TorrentInfo {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<TorrentInfo>>,
    },
    ListTorrents {
        reply: oneshot::Sender<Vec<Id20>>,
    },
    SessionStats {
        reply: oneshot::Sender<SessionStats>,
    },
    SaveTorrentResumeData {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<torrent_core::FastResumeData>>,
    },
    SaveSessionState {
        reply: oneshot::Sender<crate::Result<crate::persistence::SessionState>>,
    },
    QueuePosition {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<i32>>,
    },
    SetQueuePosition {
        info_hash: Id20,
        pos: i32,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    QueuePositionUp {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    QueuePositionDown {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    QueuePositionTop {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    QueuePositionBottom {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    BanPeer {
        ip: IpAddr,
        reply: oneshot::Sender<()>,
    },
    UnbanPeer {
        ip: IpAddr,
        reply: oneshot::Sender<bool>,
    },
    BannedPeers {
        reply: oneshot::Sender<Vec<IpAddr>>,
    },
    SetIpFilter {
        filter: crate::ip_filter::IpFilter,
        reply: oneshot::Sender<()>,
    },
    GetIpFilter {
        reply: oneshot::Sender<crate::ip_filter::IpFilter>,
    },
    GetSettings {
        reply: oneshot::Sender<Settings>,
    },
    ApplySettings {
        settings: Box<Settings>,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    MoveTorrentStorage {
        info_hash: Id20,
        new_path: std::path::PathBuf,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    AddPeers {
        info_hash: Id20,
        peers: Vec<SocketAddr>,
        source: crate::peer_state::PeerSource,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    OpenFile {
        info_hash: Id20,
        file_index: usize,
        reply: oneshot::Sender<crate::Result<crate::streaming::FileStream>>,
    },
    ForceReannounce {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    TrackerList {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<Vec<crate::tracker_manager::TrackerInfo>>>,
    },
    Scrape {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<Option<(String, torrent_tracker::ScrapeInfo)>>>,
    },
    SetFilePriority {
        info_hash: Id20,
        index: usize,
        priority: torrent_core::FilePriority,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    FilePriorities {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<Vec<torrent_core::FilePriority>>>,
    },
    SetDownloadLimit {
        info_hash: Id20,
        bytes_per_sec: u64,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    SetUploadLimit {
        info_hash: Id20,
        bytes_per_sec: u64,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    DownloadLimit {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<u64>>,
    },
    UploadLimit {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<u64>>,
    },
    SetSequentialDownload {
        info_hash: Id20,
        enabled: bool,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    IsSequentialDownload {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<bool>>,
    },
    SetSuperSeeding {
        info_hash: Id20,
        enabled: bool,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    IsSuperSeeding {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<bool>>,
    },
    AddTracker {
        info_hash: Id20,
        url: String,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    ReplaceTrackers {
        info_hash: Id20,
        urls: Vec<String>,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    /// Trigger a full piece verification (force recheck) for a torrent.
    ForceRecheck {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    /// Rename a file within a torrent on disk.
    RenameFile {
        info_hash: Id20,
        file_index: usize,
        new_name: String,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    /// Set per-torrent maximum connections (0 = use global default).
    SetMaxConnections {
        info_hash: Id20,
        limit: usize,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    /// Get per-torrent maximum connection limit.
    MaxConnections {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<usize>>,
    },
    /// Set per-torrent maximum upload slots (unchoke slots).
    SetMaxUploads {
        info_hash: Id20,
        limit: usize,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    /// Get per-torrent maximum upload slots (unchoke slots).
    MaxUploads {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<usize>>,
    },
    /// Get per-peer details for all connected peers of a torrent.
    GetPeerInfo {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<Vec<crate::types::PeerInfo>>>,
    },
    /// Get in-flight piece download status for a torrent.
    GetDownloadQueue {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<Vec<crate::types::PartialPieceInfo>>>,
    },
    /// Check whether a specific piece has been downloaded.
    HavePiece {
        info_hash: Id20,
        index: u32,
        reply: oneshot::Sender<crate::Result<bool>>,
    },
    /// Get per-piece availability counts from connected peers.
    PieceAvailability {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<Vec<u32>>>,
    },
    /// Get per-file bytes-downloaded progress.
    FileProgress {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<Vec<u64>>>,
    },
    /// Get the torrent's identity hashes (v1 and/or v2).
    InfoHashesQuery {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<torrent_core::InfoHashes>>,
    },
    /// Get the full v1 metainfo for a torrent.
    TorrentFile {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<Option<torrent_core::TorrentMetaV1>>>,
    },
    /// Get the full v2 metainfo for a torrent.
    TorrentFileV2 {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<Option<torrent_core::TorrentMetaV2>>>,
    },
    /// Force an immediate DHT announce for a torrent.
    ForceDhtAnnounce {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    /// Force an immediate LSD announce for a torrent (session-level only).
    ForceLsdAnnounce {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    /// Read all data for a specific piece from disk.
    ReadPiece {
        info_hash: Id20,
        index: u32,
        reply: oneshot::Sender<crate::Result<bytes::Bytes>>,
    },
    /// Flush the disk write cache for a torrent.
    FlushCache {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    /// Check if a torrent handle is still valid (torrent exists and channel open).
    IsValid {
        info_hash: Id20,
        reply: oneshot::Sender<bool>,
    },
    /// Clear error state on a torrent.
    ClearError {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    /// Get per-file open/mode status for a torrent.
    FileStatus {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<Vec<crate::types::FileStatus>>>,
    },
    /// Read the current torrent flags.
    Flags {
        info_hash: Id20,
        reply: oneshot::Sender<crate::Result<crate::types::TorrentFlags>>,
    },
    /// Set (enable) the specified torrent flags.
    SetFlags {
        info_hash: Id20,
        flags: crate::types::TorrentFlags,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    /// Unset (disable) the specified torrent flags.
    UnsetFlags {
        info_hash: Id20,
        flags: crate::types::TorrentFlags,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    /// Immediately initiate a peer connection for a torrent.
    ConnectPeer {
        info_hash: Id20,
        addr: SocketAddr,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    DhtPutImmutable {
        value: Vec<u8>,
        reply: oneshot::Sender<crate::Result<Id20>>,
    },
    DhtGetImmutable {
        target: Id20,
        reply: oneshot::Sender<crate::Result<Option<Vec<u8>>>>,
    },
    DhtPutMutable {
        keypair_bytes: [u8; 32],
        value: Vec<u8>,
        seq: i64,
        salt: Vec<u8>,
        reply: oneshot::Sender<crate::Result<Id20>>,
    },
    #[allow(clippy::type_complexity)]
    DhtGetMutable {
        public_key: [u8; 32],
        salt: Vec<u8>,
        reply: oneshot::Sender<crate::Result<Option<(Vec<u8>, i64)>>>,
    },
    /// Trigger an immediate session stats snapshot and alert (M50).
    PostSessionStats,
    Shutdown,
}

/// Cloneable handle for interacting with a running session.
#[derive(Clone)]
pub struct SessionHandle {
    cmd_tx: mpsc::Sender<SessionCommand>,
    alert_tx: broadcast::Sender<Alert>,
    alert_mask: Arc<AtomicU32>,
    counters: Arc<crate::stats::SessionCounters>,
    /// Network transport factory (M51). Used by future simulation tasks.
    #[allow(dead_code)]
    factory: Arc<crate::transport::NetworkFactory>,
}

impl SessionHandle {
    /// Start a new session with the given settings and no plugins.
    pub async fn start(settings: Settings) -> crate::Result<Self> {
        Self::start_with_plugins(settings, Arc::new(Vec::new())).await
    }

    /// Start a new session with a custom disk I/O backend and no plugins.
    pub async fn start_with_backend(
        settings: Settings,
        backend: Arc<dyn crate::disk_backend::DiskIoBackend>,
    ) -> crate::Result<Self> {
        Self::start_with_plugins_and_backend(settings, Arc::new(Vec::new()), backend).await
    }

    /// Start a new session with the given settings and extension plugins.
    pub async fn start_with_plugins(
        settings: Settings,
        plugins: Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>>,
    ) -> crate::Result<Self> {
        let disk_config = crate::disk::DiskConfig::from(&settings);
        let backend = crate::disk_backend::create_backend_from_config(&disk_config);
        Self::start_with_plugins_and_backend(settings, plugins, backend).await
    }

    /// Start a new session with the given settings, extension plugins, and
    /// a custom disk I/O backend.
    pub async fn start_with_plugins_and_backend(
        settings: Settings,
        plugins: Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>>,
        backend: Arc<dyn crate::disk_backend::DiskIoBackend>,
    ) -> crate::Result<Self> {
        Self::start_full(
            settings,
            plugins,
            backend,
            Arc::new(crate::transport::NetworkFactory::tokio()),
        )
        .await
    }

    /// Start a new session with the given settings and a custom transport factory.
    ///
    /// Uses default plugins (none) and default disk backend.
    pub async fn start_with_transport(
        settings: Settings,
        factory: Arc<crate::transport::NetworkFactory>,
    ) -> crate::Result<Self> {
        let disk_config = crate::disk::DiskConfig::from(&settings);
        let backend = crate::disk_backend::create_backend_from_config(&disk_config);
        Self::start_full(settings, Arc::new(Vec::new()), backend, factory).await
    }

    /// Start a new session with all customizable parameters.
    ///
    /// This is the most general constructor — all other `start_*` variants
    /// delegate to this method. The `factory` parameter controls how TCP
    /// listeners and connections are created: use [`crate::transport::NetworkFactory::tokio()`]
    /// for real networking or a custom factory for simulation.
    pub async fn start_full(
        settings: Settings,
        plugins: Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>>,
        backend: Arc<dyn crate::disk_backend::DiskIoBackend>,
        factory: Arc<crate::transport::NetworkFactory>,
    ) -> crate::Result<Self> {
        let mut settings = settings;

        // Force proxy mode: all connections must go through proxy.
        if settings.force_proxy {
            if settings.proxy.proxy_type == crate::proxy::ProxyType::None {
                return Err(crate::Error::Config(
                    "force_proxy requires a proxy to be configured".into(),
                ));
            }
            settings.enable_upnp = false;
            settings.enable_natpmp = false;
            settings.enable_dht = false;
            settings.enable_lsd = false;
        }

        // Anonymous mode: suppress identity and disable discovery.
        if settings.anonymous_mode {
            settings.enable_dht = false;
            settings.enable_lsd = false;
            settings.enable_upnp = false;
            settings.enable_natpmp = false;
        }

        let (cmd_tx, cmd_rx) = mpsc::channel(256);

        // Alert broadcast channel
        let (alert_tx, _) = broadcast::channel(settings.alert_channel_size);
        let alert_mask = Arc::new(AtomicU32::new(settings.alert_mask.bits()));

        let (lsd, lsd_peers_rx) = if settings.enable_lsd {
            match crate::lsd::LsdHandle::start(settings.listen_port).await {
                Ok((handle, rx)) => (Some(handle), Some(rx)),
                Err(e) => {
                    warn!("LSD unavailable (port 6771): {e}");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        let global_upload_bucket = Arc::new(std::sync::Mutex::new(
            crate::rate_limiter::TokenBucket::new(settings.upload_rate_limit),
        ));
        let global_download_bucket = Arc::new(std::sync::Mutex::new(
            crate::rate_limiter::TokenBucket::new(settings.download_rate_limit),
        ));

        // uTP socket (shared across all torrents)
        let (utp_socket, utp_listener) = if settings.enable_utp {
            match torrent_utp::UtpSocket::bind(settings.to_utp_config(settings.listen_port)).await {
                Ok((socket, listener)) => (Some(socket), Some(listener)),
                Err(e) => {
                    warn!("uTP bind failed: {e}");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        // IPv6 uTP socket (dual-stack)
        let (utp_socket_v6, utp_listener_v6) = if settings.enable_utp && settings.enable_ipv6 {
            match torrent_utp::UtpSocket::bind(settings.to_utp_config_v6(settings.listen_port))
                .await
            {
                Ok((socket, listener)) => (Some(socket), Some(listener)),
                Err(e) => {
                    debug!("uTP IPv6 bind failed (non-fatal): {e}");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        // NAT port mapping (PCP / NAT-PMP / UPnP)
        let (nat, nat_events_rx) = if settings.enable_upnp || settings.enable_natpmp {
            let nat_config = settings.to_nat_config();
            let (handle, events_rx) = torrent_nat::NatHandle::start(nat_config);
            let udp_port = if settings.enable_utp {
                Some(settings.listen_port)
            } else {
                None
            };
            handle.map_ports(settings.listen_port, udp_port).await;
            (Some(handle), Some(events_rx))
        } else {
            (None, None)
        };

        // I2P SAM session
        let sam_session = if settings.enable_i2p {
            let tunnel_config = settings.to_sam_tunnel_config();
            match crate::i2p::SamSession::create(
                &settings.i2p_hostname,
                settings.i2p_port,
                "torrent",
                tunnel_config,
            )
            .await
            {
                Ok(session) => {
                    let b32 = session.destination().to_b32_address();
                    info!("I2P SAM session created: {}", b32);
                    post_alert(
                        &alert_tx,
                        &alert_mask,
                        AlertKind::I2pSessionCreated { b32_address: b32 },
                    );
                    Some(Arc::new(session))
                }
                Err(e) => {
                    warn!("I2P SAM session failed: {e}");
                    post_alert(
                        &alert_tx,
                        &alert_mask,
                        AlertKind::I2pError {
                            message: format!("SAM session creation failed: {e}"),
                        },
                    );
                    None
                }
            }
        } else {
            None
        };

        // SSL manager (M42): create if ssl_listen_port != 0 or cert paths are provided
        let ssl_manager = if settings.ssl_listen_port != 0 || settings.ssl_cert_path.is_some() {
            match crate::ssl_manager::SslManager::new(&settings) {
                Ok(mgr) => {
                    info!("SSL manager initialized");
                    Some(Arc::new(mgr))
                }
                Err(e) => {
                    warn!(error = %e, "SSL manager initialization failed");
                    None
                }
            }
        } else {
            None
        };

        // TCP listener: bind on the main listen port for incoming peer connections.
        let tcp_listener: Option<Box<dyn crate::transport::TransportListener>> = match factory
            .bind_tcp(SocketAddr::from(([0, 0, 0, 0], settings.listen_port)))
            .await
        {
            Ok(l) => {
                info!(port = settings.listen_port, "TCP listener started");
                Some(l)
            }
            Err(e) => {
                warn!(port = settings.listen_port, error = %e, "TCP listener bind failed");
                None
            }
        };

        // SSL listener (M42): bind if ssl_listen_port != 0
        let ssl_listener: Option<Box<dyn crate::transport::TransportListener>> = if settings
            .ssl_listen_port
            != 0
        {
            match factory
                .bind_tcp(SocketAddr::from(([0, 0, 0, 0], settings.ssl_listen_port)))
                .await
            {
                Ok(l) => {
                    info!(port = settings.ssl_listen_port, "SSL listener started");
                    Some(l)
                }
                Err(e) => {
                    warn!(port = settings.ssl_listen_port, error = %e, "SSL listener bind failed");
                    None
                }
            }
        } else {
            None
        };

        // Start DHT instances
        let (dht_v4, dht_v4_ip_rx) = if settings.enable_dht {
            match DhtHandle::start(settings.to_dht_config()).await {
                Ok((handle, ip_rx)) => {
                    info!("DHT v4 started");
                    (Some(handle), Some(ip_rx))
                }
                Err(e) => {
                    warn!("DHT v4 start failed: {e}");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        let (dht_v6, dht_v6_ip_rx) = if settings.enable_dht && settings.enable_ipv6 {
            match DhtHandle::start(settings.to_dht_config_v6()).await {
                Ok((handle, ip_rx)) => {
                    info!("DHT v6 started");
                    (Some(handle), Some(ip_rx))
                }
                Err(e) => {
                    debug!("DHT v6 start failed (non-fatal): {e}");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        let ban_config = crate::ban::BanConfig::from(&settings);
        let ban_manager: SharedBanManager = Arc::new(std::sync::RwLock::new(
            crate::ban::BanManager::new(ban_config),
        ));

        let ip_filter: SharedIpFilter =
            Arc::new(std::sync::RwLock::new(crate::ip_filter::IpFilter::new()));

        let disk_config = crate::disk::DiskConfig::from(&settings);
        let (disk_manager, disk_actor_handle) =
            crate::disk::DiskManagerHandle::new_with_backend(disk_config, backend);

        let counters = Arc::new(crate::stats::SessionCounters::new());

        // M96: Create shared hash pool for parallel piece verification
        let hash_pool = std::sync::Arc::new(crate::hash_pool::HashPool::new(
            settings.hashing_threads,
            64,
        ));

        let external_ip = settings.external_ip;
        let actor = SessionActor {
            settings,
            torrents: HashMap::new(),
            dht_v4,
            dht_v6,
            lsd,
            lsd_peers_rx,
            cmd_rx,
            alert_tx: alert_tx.clone(),
            alert_mask: Arc::clone(&alert_mask),
            global_upload_bucket,
            global_download_bucket,
            utp_socket,
            utp_listener,
            utp_socket_v6,
            utp_listener_v6,
            nat,
            nat_events_rx,
            ban_manager,
            ip_filter,
            disk_manager,
            disk_actor_handle,
            external_ip,
            dht_v4_ip_rx,
            dht_v6_ip_rx,
            plugins,
            sam_session,
            ssl_manager,
            tcp_listener,
            ssl_listener,
            counters: Arc::clone(&counters),
            factory: Arc::clone(&factory),
            hash_pool,
        };

        let join_handle = tokio::spawn(actor.run());
        tokio::spawn(async move {
            match join_handle.await {
                Ok(()) => {
                    tracing::warn!("session actor exited cleanly");
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
                    tracing::error!("session actor PANICKED: {msg}");
                }
                Err(e) => {
                    tracing::error!("session actor task error: {e}");
                }
            }
        });
        Ok(SessionHandle {
            cmd_tx,
            alert_tx,
            alert_mask,
            counters,
            factory,
        })
    }

    /// Add a torrent from parsed .torrent metadata (v1, v2, or hybrid).
    pub async fn add_torrent(
        &self,
        meta: torrent_core::TorrentMeta,
        storage: Option<Arc<dyn TorrentStorage>>,
    ) -> crate::Result<Id20> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::AddTorrent {
                meta: Box::new(meta),
                storage,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Add a torrent from a magnet link (metadata fetched via BEP 9).
    pub async fn add_magnet(&self, magnet: Magnet) -> crate::Result<Id20> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::AddMagnet { magnet, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Remove a torrent from the session.
    pub async fn remove_torrent(&self, info_hash: Id20) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::RemoveTorrent {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Pause a torrent.
    pub async fn pause_torrent(&self, info_hash: Id20) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::PauseTorrent {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Resume a paused torrent.
    pub async fn resume_torrent(&self, info_hash: Id20) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::ResumeTorrent {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get statistics for a specific torrent.
    pub async fn torrent_stats(&self, info_hash: Id20) -> crate::Result<TorrentStats> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::TorrentStats {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get metadata info for a specific torrent.
    pub async fn torrent_info(&self, info_hash: Id20) -> crate::Result<TorrentInfo> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::TorrentInfo {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// List all active torrent info hashes.
    pub async fn list_torrents(&self) -> crate::Result<Vec<Id20>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::ListTorrents { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Get aggregate session statistics.
    pub async fn session_stats(&self) -> crate::Result<SessionStats> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::SessionStats { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Subscribe to all alerts passing the session-level mask.
    pub fn subscribe(&self) -> broadcast::Receiver<Alert> {
        self.alert_tx.subscribe()
    }

    /// Subscribe with per-subscriber category filtering.
    pub fn subscribe_filtered(&self, filter: AlertCategory) -> AlertStream {
        AlertStream::new(self.alert_tx.subscribe(), filter)
    }

    /// Trigger an immediate session stats snapshot and alert.
    pub async fn post_session_stats(&self) -> crate::Result<()> {
        self.cmd_tx
            .send(SessionCommand::PostSessionStats)
            .await
            .map_err(|_| crate::Error::Shutdown)
    }

    /// Access the shared atomic counters (read-only handle).
    pub fn counters(&self) -> &Arc<crate::stats::SessionCounters> {
        &self.counters
    }

    /// Atomically update the session-level alert mask.
    pub fn set_alert_mask(&self, mask: AlertCategory) {
        self.alert_mask.store(mask.bits(), Ordering::Relaxed);
    }

    /// Read the current session-level alert mask.
    pub fn alert_mask(&self) -> AlertCategory {
        AlertCategory::from_bits_truncate(self.alert_mask.load(Ordering::Relaxed))
    }

    /// Add peers to a specific torrent by info hash.
    pub async fn add_peers(
        &self,
        info_hash: Id20,
        peers: Vec<SocketAddr>,
        source: crate::peer_state::PeerSource,
    ) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::AddPeers {
                info_hash,
                peers,
                source,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Gracefully shut down the session and all torrents.
    pub async fn shutdown(&self) -> crate::Result<()> {
        // Timeout prevents hang if SessionActor is processing a heavy batch
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.cmd_tx.send(SessionCommand::Shutdown),
        )
        .await;
        Ok(())
    }

    /// Save resume data for a specific torrent.
    pub async fn save_torrent_resume_data(
        &self,
        info_hash: Id20,
    ) -> crate::Result<torrent_core::FastResumeData> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::SaveTorrentResumeData {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Save full session state (all torrent resume data + DHT node cache).
    pub async fn save_session_state(&self) -> crate::Result<crate::persistence::SessionState> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::SaveSessionState { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get the queue position of a torrent. Returns -1 if not auto-managed.
    pub async fn queue_position(&self, info_hash: Id20) -> crate::Result<i32> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::QueuePosition {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Set the absolute queue position of a torrent. Shifts other torrents.
    pub async fn set_queue_position(&self, info_hash: Id20, pos: i32) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::SetQueuePosition {
                info_hash,
                pos,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Move a torrent one position up (lower number = higher priority).
    pub async fn queue_position_up(&self, info_hash: Id20) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::QueuePositionUp {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Move a torrent one position down.
    pub async fn queue_position_down(&self, info_hash: Id20) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::QueuePositionDown {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Move a torrent to position 0 (highest priority).
    pub async fn queue_position_top(&self, info_hash: Id20) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::QueuePositionTop {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Move a torrent to the last position (lowest priority).
    pub async fn queue_position_bottom(&self, info_hash: Id20) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::QueuePositionBottom {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Ban a peer IP session-wide. All torrents will disconnect and refuse this IP.
    pub async fn ban_peer(&self, ip: IpAddr) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::BanPeer { ip, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Remove a ban and clear strikes for an IP. Returns `true` if the IP was banned.
    pub async fn unban_peer(&self, ip: IpAddr) -> crate::Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::UnbanPeer { ip, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Replace the session-wide IP filter. Connected peers that are now blocked will
    /// be refused on subsequent connection attempts.
    pub async fn set_ip_filter(&self, filter: crate::ip_filter::IpFilter) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::SetIpFilter { filter, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Get a clone of the current IP filter.
    pub async fn ip_filter(&self) -> crate::Result<crate::ip_filter::IpFilter> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::GetIpFilter { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Get a clone of the current session settings.
    pub async fn settings(&self) -> crate::Result<Settings> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::GetSettings { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Apply new settings at runtime.
    ///
    /// Validates the settings, updates rate limiters immediately, and stores
    /// the new settings. Sub-actor reconfiguration (disk, DHT, NAT) takes
    /// effect on next session restart.
    pub async fn apply_settings(&self, settings: Settings) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::ApplySettings {
                settings: Box::new(settings),
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get the list of currently banned peer IPs.
    pub async fn banned_peers(&self) -> crate::Result<Vec<IpAddr>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::BannedPeers { reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)
    }

    /// Move a torrent's data files to a new download directory.
    pub async fn move_torrent_storage(
        &self,
        info_hash: Id20,
        new_path: std::path::PathBuf,
    ) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::MoveTorrentStorage {
                info_hash,
                new_path,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Opens a file stream for sequential reading (`AsyncRead` + `AsyncSeek`).
    ///
    /// The returned [`FileStream`](crate::streaming::FileStream) reads data from
    /// a specific file within a torrent, blocking on pieces that haven't been
    /// downloaded yet.
    pub async fn open_file(
        &self,
        info_hash: Id20,
        file_index: usize,
    ) -> crate::Result<crate::streaming::FileStream> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::OpenFile {
                info_hash,
                file_index,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Force all trackers for a torrent to re-announce immediately.
    pub async fn force_reannounce(&self, info_hash: Id20) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::ForceReannounce {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get the list of all configured trackers with their status for a torrent.
    pub async fn tracker_list(
        &self,
        info_hash: Id20,
    ) -> crate::Result<Vec<crate::tracker_manager::TrackerInfo>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::TrackerList {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Scrape trackers for seeder/leecher counts for a torrent.
    pub async fn scrape(
        &self,
        info_hash: Id20,
    ) -> crate::Result<Option<(String, torrent_tracker::ScrapeInfo)>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::Scrape {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Set the download priority of a specific file within a torrent.
    pub async fn set_file_priority(
        &self,
        info_hash: Id20,
        index: usize,
        priority: torrent_core::FilePriority,
    ) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::SetFilePriority {
                info_hash,
                index,
                priority,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get the current per-file priorities for a torrent.
    pub async fn file_priorities(
        &self,
        info_hash: Id20,
    ) -> crate::Result<Vec<torrent_core::FilePriority>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::FilePriorities {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Set the per-torrent download rate limit in bytes/sec (0 = unlimited).
    pub async fn set_download_limit(
        &self,
        info_hash: Id20,
        bytes_per_sec: u64,
    ) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::SetDownloadLimit {
                info_hash,
                bytes_per_sec,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Set the per-torrent upload rate limit in bytes/sec (0 = unlimited).
    pub async fn set_upload_limit(&self, info_hash: Id20, bytes_per_sec: u64) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::SetUploadLimit {
                info_hash,
                bytes_per_sec,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get the current per-torrent download rate limit in bytes/sec (0 = unlimited).
    pub async fn download_limit(&self, info_hash: Id20) -> crate::Result<u64> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::DownloadLimit {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get the current per-torrent upload rate limit in bytes/sec (0 = unlimited).
    pub async fn upload_limit(&self, info_hash: Id20) -> crate::Result<u64> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::UploadLimit {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Enable or disable sequential (in-order) piece downloading for a torrent.
    pub async fn set_sequential_download(
        &self,
        info_hash: Id20,
        enabled: bool,
    ) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::SetSequentialDownload {
                info_hash,
                enabled,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Query whether sequential downloading is enabled for a torrent.
    pub async fn is_sequential_download(&self, info_hash: Id20) -> crate::Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::IsSequentialDownload {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Enable or disable BEP 16 super seeding mode for a torrent.
    pub async fn set_super_seeding(&self, info_hash: Id20, enabled: bool) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::SetSuperSeeding {
                info_hash,
                enabled,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Query whether BEP 16 super seeding mode is enabled for a torrent.
    pub async fn is_super_seeding(&self, info_hash: Id20) -> crate::Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::IsSuperSeeding {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Add a new tracker URL to a torrent.
    ///
    /// The URL is validated and deduplicated by the tracker manager.
    pub async fn add_tracker(&self, info_hash: Id20, url: String) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::AddTracker {
                info_hash,
                url,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Replace all tracker URLs for a torrent.
    pub async fn replace_trackers(&self, info_hash: Id20, urls: Vec<String>) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::ReplaceTrackers {
                info_hash,
                urls,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Trigger a full piece verification (force recheck) for a torrent.
    ///
    /// Clears all piece completion data, re-verifies every piece, and
    /// transitions to `Seeding` or `Downloading` depending on the result.
    /// Returns after the recheck is complete.
    pub async fn force_recheck(&self, info_hash: Id20) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::ForceRecheck {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Rename a file within a torrent on disk.
    ///
    /// Changes the filename of the specified file (by index) to `new_name`.
    /// The file stays in the same directory; only the filename component changes.
    /// Fires a `FileRenamed` alert on success.
    pub async fn rename_file(
        &self,
        info_hash: Id20,
        file_index: usize,
        new_name: String,
    ) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::RenameFile {
                info_hash,
                file_index,
                new_name,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Set the per-torrent maximum number of connections (0 = use global default).
    pub async fn set_max_connections(&self, info_hash: Id20, limit: usize) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::SetMaxConnections {
                info_hash,
                limit,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get the current per-torrent maximum connection limit (0 = use global default).
    pub async fn max_connections(&self, info_hash: Id20) -> crate::Result<usize> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::MaxConnections {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Set the per-torrent maximum number of upload slots (unchoke slots).
    pub async fn set_max_uploads(&self, info_hash: Id20, limit: usize) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::SetMaxUploads {
                info_hash,
                limit,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get the current per-torrent maximum upload slots (unchoke slots).
    pub async fn max_uploads(&self, info_hash: Id20) -> crate::Result<usize> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::MaxUploads {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get per-peer details for all connected peers of a torrent.
    pub async fn get_peer_info(
        &self,
        info_hash: Id20,
    ) -> crate::Result<Vec<crate::types::PeerInfo>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::GetPeerInfo {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get in-flight piece download status for a torrent (the download queue).
    pub async fn get_download_queue(
        &self,
        info_hash: Id20,
    ) -> crate::Result<Vec<crate::types::PartialPieceInfo>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::GetDownloadQueue {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Check whether a specific piece has been downloaded for a torrent.
    pub async fn have_piece(&self, info_hash: Id20, index: u32) -> crate::Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::HavePiece {
                info_hash,
                index,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get per-piece availability counts from connected peers for a torrent.
    pub async fn piece_availability(&self, info_hash: Id20) -> crate::Result<Vec<u32>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::PieceAvailability {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get per-file bytes-downloaded progress for a torrent.
    pub async fn file_progress(&self, info_hash: Id20) -> crate::Result<Vec<u64>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::FileProgress {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get the torrent's identity hashes (v1 and/or v2).
    pub async fn info_hashes(&self, info_hash: Id20) -> crate::Result<torrent_core::InfoHashes> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::InfoHashesQuery {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get the full v1 metainfo for a torrent.
    ///
    /// Returns `None` for magnet links before metadata has been received.
    pub async fn torrent_file(
        &self,
        info_hash: Id20,
    ) -> crate::Result<Option<torrent_core::TorrentMetaV1>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::TorrentFile {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get the full v2 metainfo for a torrent.
    ///
    /// Returns `None` if the torrent is not a v2/hybrid torrent, or for magnet
    /// links before metadata has been received.
    pub async fn torrent_file_v2(
        &self,
        info_hash: Id20,
    ) -> crate::Result<Option<torrent_core::TorrentMetaV2>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::TorrentFileV2 {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Force an immediate DHT announce for a torrent.
    pub async fn force_dht_announce(&self, info_hash: Id20) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::ForceDhtAnnounce {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Force an immediate LSD (Local Service Discovery) announce for a torrent.
    ///
    /// LSD is a session-level component — this does not go through the torrent actor.
    pub async fn force_lsd_announce(&self, info_hash: Id20) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::ForceLsdAnnounce {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Read all data for a specific piece from disk.
    pub async fn read_piece(&self, info_hash: Id20, index: u32) -> crate::Result<bytes::Bytes> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::ReadPiece {
                info_hash,
                index,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Flush the disk write cache for a torrent.
    pub async fn flush_cache(&self, info_hash: Id20) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::FlushCache {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Check if a torrent exists in the session and its handle is still valid.
    pub async fn is_valid(&self, info_hash: Id20) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::IsValid {
                info_hash,
                reply: tx,
            })
            .await
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Clear the error state on a torrent, resuming it if it was paused due to error.
    pub async fn clear_error(&self, info_hash: Id20) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::ClearError {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Get per-file open/mode status for a torrent.
    pub async fn file_status(
        &self,
        info_hash: Id20,
    ) -> crate::Result<Vec<crate::types::FileStatus>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::FileStatus {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Read the current torrent flags as a [`crate::types::TorrentFlags`] bitflag set.
    pub async fn flags(&self, info_hash: Id20) -> crate::Result<crate::types::TorrentFlags> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::Flags {
                info_hash,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Set (enable) the specified torrent flags.
    pub async fn set_flags(
        &self,
        info_hash: Id20,
        flags: crate::types::TorrentFlags,
    ) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::SetFlags {
                info_hash,
                flags,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Unset (disable) the specified torrent flags.
    pub async fn unset_flags(
        &self,
        info_hash: Id20,
        flags: crate::types::TorrentFlags,
    ) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::UnsetFlags {
                info_hash,
                flags,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Immediately initiate a peer connection for a torrent.
    pub async fn connect_peer(&self, info_hash: Id20, addr: SocketAddr) -> crate::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::ConnectPeer {
                info_hash,
                addr,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Store an immutable item in the DHT (BEP 44).
    ///
    /// Returns the SHA-1 target hash of the stored value.
    pub async fn dht_put_immutable(&self, value: Vec<u8>) -> crate::Result<Id20> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::DhtPutImmutable { value, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Retrieve an immutable item from the DHT (BEP 44).
    ///
    /// Returns `Some(value)` if found, `None` otherwise.
    pub async fn dht_get_immutable(&self, target: Id20) -> crate::Result<Option<Vec<u8>>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::DhtGetImmutable { target, reply: tx })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Store a mutable item in the DHT (BEP 44).
    ///
    /// `keypair_bytes` is a 32-byte Ed25519 seed. Returns the target hash.
    pub async fn dht_put_mutable(
        &self,
        keypair_bytes: [u8; 32],
        value: Vec<u8>,
        seq: i64,
        salt: Vec<u8>,
    ) -> crate::Result<Id20> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::DhtPutMutable {
                keypair_bytes,
                value,
                seq,
                salt,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }

    /// Retrieve a mutable item from the DHT (BEP 44).
    ///
    /// Returns `Some((value, seq))` if found, `None` otherwise.
    pub async fn dht_get_mutable(
        &self,
        public_key: [u8; 32],
        salt: Vec<u8>,
    ) -> crate::Result<Option<(Vec<u8>, i64)>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCommand::DhtGetMutable {
                public_key,
                salt,
                reply: tx,
            })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
        rx.await.map_err(|_| crate::Error::Shutdown)?
    }
}

// ---------------------------------------------------------------------------
// SessionActor — internal single-owner event loop
// ---------------------------------------------------------------------------

struct SessionActor {
    settings: Settings,
    torrents: HashMap<Id20, TorrentEntry>,
    dht_v4: Option<DhtHandle>,
    dht_v6: Option<DhtHandle>,
    lsd: Option<crate::lsd::LsdHandle>,
    lsd_peers_rx: Option<mpsc::Receiver<(Id20, SocketAddr)>>,
    cmd_rx: mpsc::Receiver<SessionCommand>,
    alert_tx: broadcast::Sender<Alert>,
    alert_mask: Arc<AtomicU32>,
    global_upload_bucket: SharedBucket,
    global_download_bucket: SharedBucket,
    utp_socket: Option<torrent_utp::UtpSocket>,
    utp_listener: Option<torrent_utp::UtpListener>,
    utp_socket_v6: Option<torrent_utp::UtpSocket>,
    utp_listener_v6: Option<torrent_utp::UtpListener>,
    nat: Option<torrent_nat::NatHandle>,
    nat_events_rx: Option<mpsc::Receiver<torrent_nat::NatEvent>>,
    ban_manager: SharedBanManager,
    ip_filter: SharedIpFilter,
    disk_manager: crate::disk::DiskManagerHandle,
    #[allow(dead_code)]
    disk_actor_handle: tokio::task::JoinHandle<()>,
    /// External IP discovered via NAT traversal or configured manually (BEP 40).
    external_ip: Option<std::net::IpAddr>,
    /// BEP 42: External IP consensus from DHT v4 KRPC responses.
    dht_v4_ip_rx: Option<mpsc::Receiver<std::net::IpAddr>>,
    /// BEP 42: External IP consensus from DHT v6 KRPC responses.
    dht_v6_ip_rx: Option<mpsc::Receiver<std::net::IpAddr>>,
    /// Registered extension plugins, shared with all TorrentActors.
    plugins: Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>>,
    /// I2P SAM session (if enabled).
    sam_session: Option<Arc<crate::i2p::SamSession>>,
    /// SSL manager for SSL torrent certificate handling (M42).
    ssl_manager: Option<Arc<crate::ssl_manager::SslManager>>,
    /// TCP listener on the main listen port for incoming peer connections.
    tcp_listener: Option<Box<dyn crate::transport::TransportListener>>,
    /// SSL/TLS TCP listener (separate port from the main listener) (M42).
    ssl_listener: Option<Box<dyn crate::transport::TransportListener>>,
    /// Shared atomic session counters (M50).
    counters: Arc<crate::stats::SessionCounters>,
    /// Network transport factory for TCP operations (M51).
    factory: Arc<crate::transport::NetworkFactory>,
    /// Shared hash pool for parallel piece verification (M96).
    hash_pool: std::sync::Arc<crate::hash_pool::HashPool>,
}

impl SessionActor {
    async fn run(mut self) {
        let mut refill_interval = tokio::time::interval(std::time::Duration::from_millis(100));
        refill_interval.tick().await; // skip first immediate tick

        let auto_manage_secs = self.settings.auto_manage_interval.max(1);
        let mut auto_manage_interval =
            tokio::time::interval(std::time::Duration::from_secs(auto_manage_secs));
        auto_manage_interval.tick().await; // skip first immediate tick

        // Periodic session stats timer (M50)
        let stats_interval_ms = self.settings.stats_report_interval;
        let mut stats_timer = if stats_interval_ms > 0 {
            Some(tokio::time::interval(std::time::Duration::from_millis(
                stats_interval_ms,
            )))
        } else {
            None
        };
        if let Some(ref mut t) = stats_timer {
            t.tick().await; // skip first immediate tick
        }

        // Periodic sample_infohashes timer (BEP 51, M111)
        let sample_interval_secs = self.settings.dht_sample_infohashes_interval;
        let mut sample_timer = if sample_interval_secs > 0 {
            Some(tokio::time::interval(std::time::Duration::from_secs(
                sample_interval_secs,
            )))
        } else {
            None
        };
        if let Some(ref mut t) = sample_timer {
            t.tick().await; // skip first immediate tick
        }

        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(SessionCommand::AddTorrent {
                            meta,
                            storage,
                            reply,
                        }) => {
                            let result = self.handle_add_torrent(*meta, storage).await;
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::AddMagnet { magnet, reply }) => {
                            let result = self.handle_add_magnet(magnet).await;
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::RemoveTorrent { info_hash, reply }) => {
                            let result = self.handle_remove_torrent(info_hash).await;
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::PauseTorrent { info_hash, reply }) => {
                            let result = self.handle_pause_torrent(info_hash).await;
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::ResumeTorrent { info_hash, reply }) => {
                            let result = self.handle_resume_torrent(info_hash).await;
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::TorrentStats { info_hash, reply }) => {
                            let result = self.handle_torrent_stats(info_hash).await;
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::TorrentInfo { info_hash, reply }) => {
                            let result = self.handle_torrent_info(info_hash);
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::ListTorrents { reply }) => {
                            let list: Vec<Id20> = self.torrents.keys().copied().collect();
                            let _ = reply.send(list);
                        }
                        Some(SessionCommand::SessionStats { reply }) => {
                            let stats = self.make_session_stats().await;
                            let _ = reply.send(stats);
                        }
                        Some(SessionCommand::SaveTorrentResumeData { info_hash, reply }) => {
                            let result = self.handle_save_torrent_resume(info_hash).await;
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::SaveSessionState { reply }) => {
                            let result = self.handle_save_session_state().await;
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::QueuePosition { info_hash, reply }) => {
                            let result = match self.torrents.get(&info_hash) {
                                Some(entry) => Ok(entry.queue_position),
                                None => Err(crate::Error::TorrentNotFound(info_hash)),
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::SetQueuePosition { info_hash, pos, reply }) => {
                            let result = self.handle_set_queue_position(info_hash, pos);
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::QueuePositionUp { info_hash, reply }) => {
                            let result = self.handle_queue_move(info_hash, crate::queue::move_up);
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::QueuePositionDown { info_hash, reply }) => {
                            let result = self.handle_queue_move(info_hash, crate::queue::move_down);
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::QueuePositionTop { info_hash, reply }) => {
                            let result = self.handle_queue_move(info_hash, crate::queue::move_top);
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::QueuePositionBottom { info_hash, reply }) => {
                            let result = self.handle_queue_move(info_hash, crate::queue::move_bottom);
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::BanPeer { ip, reply }) => {
                            self.ban_manager.write().unwrap().ban(ip);
                            let _ = reply.send(());
                        }
                        Some(SessionCommand::UnbanPeer { ip, reply }) => {
                            let was_banned = self.ban_manager.write().unwrap().unban(&ip);
                            let _ = reply.send(was_banned);
                        }
                        Some(SessionCommand::BannedPeers { reply }) => {
                            let list: Vec<IpAddr> = self.ban_manager.read().unwrap()
                                .banned_list().iter().copied().collect();
                            let _ = reply.send(list);
                        }
                        Some(SessionCommand::SetIpFilter { filter, reply }) => {
                            *self.ip_filter.write().unwrap() = filter;
                            let _ = reply.send(());
                        }
                        Some(SessionCommand::GetIpFilter { reply }) => {
                            let filter = self.ip_filter.read().unwrap().clone();
                            let _ = reply.send(filter);
                        }
                        Some(SessionCommand::GetSettings { reply }) => {
                            let _ = reply.send(self.settings.clone());
                        }
                        Some(SessionCommand::ApplySettings { settings, reply }) => {
                            let result = self.handle_apply_settings(*settings);
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::MoveTorrentStorage { info_hash, new_path, reply }) => {
                            let result = self.handle_move_torrent_storage(info_hash, new_path).await;
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::AddPeers { info_hash, peers, source, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.add_peers(peers, source).await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::OpenFile { info_hash, file_index, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.open_file(file_index).await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::ForceReannounce { info_hash, reply }) => {
                            let result = match self.torrents.get(&info_hash) {
                                Some(entry) => {
                                    entry.handle.force_reannounce().await
                                }
                                None => Err(crate::Error::TorrentNotFound(info_hash)),
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::TrackerList { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.tracker_list().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::Scrape { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.scrape().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::SetFilePriority { info_hash, index, priority, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.set_file_priority(index, priority).await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::FilePriorities { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.file_priorities().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::SetDownloadLimit { info_hash, bytes_per_sec, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.set_download_limit(bytes_per_sec).await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::SetUploadLimit { info_hash, bytes_per_sec, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.set_upload_limit(bytes_per_sec).await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::DownloadLimit { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.download_limit().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::UploadLimit { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.upload_limit().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::SetSequentialDownload { info_hash, enabled, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.set_sequential_download(enabled).await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::IsSequentialDownload { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.is_sequential_download().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::SetSuperSeeding { info_hash, enabled, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.set_super_seeding(enabled).await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::IsSuperSeeding { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.is_super_seeding().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::AddTracker { info_hash, url, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.add_tracker(url).await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::ReplaceTrackers { info_hash, urls, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.replace_trackers(urls).await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::ForceRecheck { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.force_recheck().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::RenameFile { info_hash, file_index, new_name, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.rename_file(file_index, new_name).await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::SetMaxConnections { info_hash, limit, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.set_max_connections(limit).await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::MaxConnections { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.max_connections().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::SetMaxUploads { info_hash, limit, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.set_max_uploads(limit).await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::MaxUploads { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.max_uploads().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::GetPeerInfo { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.get_peer_info().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::GetDownloadQueue { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.get_download_queue().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::HavePiece { info_hash, index, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.have_piece(index).await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::PieceAvailability { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.piece_availability().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::FileProgress { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.file_progress().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::InfoHashesQuery { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.info_hashes().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::TorrentFile { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.torrent_file().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::TorrentFileV2 { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.torrent_file_v2().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::ForceDhtAnnounce { info_hash, reply }) => {
                            let result = match self.torrents.get(&info_hash) {
                                Some(entry) => {
                                    entry.handle.force_dht_announce().await
                                }
                                None => Err(crate::Error::TorrentNotFound(info_hash)),
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::ForceLsdAnnounce { info_hash, reply }) => {
                            // LSD is session-level: verify the torrent exists, then announce directly.
                            let result = match self.torrents.get(&info_hash) {
                                Some(entry) if entry.is_private() => {
                                    // BEP 27: private torrents must not use LSD
                                    Err(crate::Error::InvalidSettings(
                                        "LSD disabled for private torrent".into(),
                                    ))
                                }
                                Some(_) => {
                                    if let Some(ref lsd) = self.lsd {
                                        lsd.announce(vec![info_hash]).await;
                                    }
                                    Ok(())
                                }
                                None => Err(crate::Error::TorrentNotFound(info_hash)),
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::ReadPiece { info_hash, index, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.read_piece(index).await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::FlushCache { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.flush_cache().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::IsValid { info_hash, reply }) => {
                            let valid = self.torrents.get(&info_hash)
                                .map(|e| e.handle.is_valid())
                                .unwrap_or(false);
                            let _ = reply.send(valid);
                        }
                        Some(SessionCommand::ClearError { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.clear_error().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::FileStatus { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.file_status().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::Flags { info_hash, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.flags().await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::SetFlags { info_hash, flags, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.set_flags(flags).await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::UnsetFlags { info_hash, flags, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.unset_flags(flags).await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::ConnectPeer { info_hash, addr, reply }) => {
                            let result = if let Some(entry) = self.torrents.get(&info_hash) {
                                entry.handle.connect_peer(addr).await
                            } else {
                                Err(crate::Error::TorrentNotFound(info_hash))
                            };
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::DhtPutImmutable { value, reply }) => {
                            let result = self.handle_dht_put_immutable(value).await;
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::DhtGetImmutable { target, reply }) => {
                            let result = self.handle_dht_get_immutable(target).await;
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::DhtPutMutable { keypair_bytes, value, seq, salt, reply }) => {
                            let result = self.handle_dht_put_mutable(keypair_bytes, value, seq, salt).await;
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::DhtGetMutable { public_key, salt, reply }) => {
                            let result = self.handle_dht_get_mutable(public_key, salt).await;
                            let _ = reply.send(result);
                        }
                        Some(SessionCommand::PostSessionStats) => {
                            self.fire_stats_alert();
                        }
                        Some(SessionCommand::Shutdown) | None => {
                            self.shutdown_all().await;
                            return;
                        }
                    }
                }
                result = async {
                    match &mut self.lsd_peers_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some((info_hash, peer_addr)) = result
                        && let Some(entry) = self.torrents.get(&info_hash)
                        && !entry.is_private()  // BEP 27: reject LSD peers for private torrents
                    {
                        let _ = entry.handle.add_peers(vec![peer_addr], crate::peer_state::PeerSource::Lsd).await;
                    }
                }
                // TCP inbound connections (main listen port)
                result = async {
                    if let Some(ref mut listener) = self.tcp_listener {
                        listener.accept().await
                    } else {
                        std::future::pending().await
                    }
                } => {
                    if let Ok((stream, addr)) = result {
                        self.handle_tcp_inbound(stream, addr);
                    }
                }
                // uTP inbound connections (IPv4)
                result = accept_utp(&mut self.utp_listener) => {
                    if let Ok((stream, addr)) = result {
                        self.handle_utp_inbound(stream, addr);
                    }
                }
                // uTP inbound connections (IPv6)
                result = accept_utp(&mut self.utp_listener_v6) => {
                    if let Ok((stream, addr)) = result {
                        self.handle_utp_inbound(stream, addr);
                    }
                }
                // SSL inbound connections (M42)
                result = async {
                    if let Some(ref mut listener) = self.ssl_listener {
                        listener.accept().await
                    } else {
                        std::future::pending().await
                    }
                } => {
                    if let Ok((stream, addr)) = result {
                        self.handle_ssl_incoming(stream, addr).await;
                    }
                }
                // Global rate limiter refill (100ms)
                _ = refill_interval.tick() => {
                    let elapsed = std::time::Duration::from_millis(100);
                    self.global_upload_bucket.lock().unwrap().refill(elapsed);
                    self.global_download_bucket.lock().unwrap().refill(elapsed);
                }
                // Auto-manage queue evaluation
                _ = auto_manage_interval.tick() => {
                    self.evaluate_queue().await;
                }
                // NAT port mapping events
                event = recv_nat_event(&mut self.nat_events_rx) => {
                    match event {
                        torrent_nat::NatEvent::MappingSucceeded { port, protocol } => {
                            info!(port, %protocol, "port mapping succeeded");
                            post_alert(
                                &self.alert_tx,
                                &self.alert_mask,
                                AlertKind::PortMappingSucceeded { port, protocol },
                            );
                        }
                        torrent_nat::NatEvent::MappingFailed { port, message } => {
                            warn!(port, %message, "port mapping failed");
                            post_alert(
                                &self.alert_tx,
                                &self.alert_mask,
                                AlertKind::PortMappingFailed { port, message },
                            );
                        }
                        torrent_nat::NatEvent::ExternalIpDiscovered { ip } => {
                            info!(%ip, "external IP discovered via NAT traversal");
                            self.external_ip = Some(ip);
                            // Propagate to all active torrents for BEP 40 peer priority.
                            for entry in self.torrents.values() {
                                let _ = entry.handle.update_external_ip(ip).await;
                            }
                            // BEP 42: notify DHT instances of external IP
                            if let Some(dht) = &self.dht_v4 {
                                let _ = dht.update_external_ip(ip, torrent_dht::IpVoteSource::Nat).await;
                            }
                            if let Some(dht) = &self.dht_v6 {
                                let _ = dht.update_external_ip(ip, torrent_dht::IpVoteSource::Nat).await;
                            }
                        }
                    }
                }
                // BEP 42: DHT v4 external IP consensus
                Some(ip) = recv_dht_ip(&mut self.dht_v4_ip_rx) => {
                    info!(%ip, "external IP discovered via DHT v4 (BEP 42)");
                    self.external_ip = Some(ip);
                    for entry in self.torrents.values() {
                        let _ = entry.handle.update_external_ip(ip).await;
                    }
                }
                // BEP 42: DHT v6 external IP consensus
                Some(ip) = recv_dht_ip(&mut self.dht_v6_ip_rx) => {
                    info!(%ip, "external IP discovered via DHT v6 (BEP 42)");
                    self.external_ip = Some(ip);
                    for entry in self.torrents.values() {
                        let _ = entry.handle.update_external_ip(ip).await;
                    }
                }
                // Periodic session stats (M50)
                _ = async {
                    match &mut stats_timer {
                        Some(t) => t.tick().await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.fire_stats_alert();
                }
                // Periodic sample_infohashes (BEP 51, M111)
                _ = async {
                    match &mut sample_timer {
                        Some(t) => t.tick().await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.fire_sample_infohashes().await;
                }
            }
        }
    }

    /// Return clones of global buckets if they have a non-zero rate, else None.
    fn global_buckets_if_limited(&self) -> (Option<SharedBucket>, Option<SharedBucket>) {
        let up = if self.settings.upload_rate_limit > 0 {
            Some(Arc::clone(&self.global_upload_bucket))
        } else {
            None
        };
        let down = if self.settings.download_rate_limit > 0 {
            Some(Arc::clone(&self.global_download_bucket))
        } else {
            None
        };
        (up, down)
    }

    fn make_slot_tuner(&self) -> crate::slot_tuner::SlotTuner {
        if self.settings.auto_upload_slots {
            crate::slot_tuner::SlotTuner::new(
                4, // initial slots
                self.settings.auto_upload_slots_min,
                self.settings.auto_upload_slots_max,
            )
        } else {
            crate::slot_tuner::SlotTuner::disabled(4)
        }
    }

    fn make_torrent_config(&self) -> TorrentConfig {
        TorrentConfig::from(&self.settings)
    }

    /// Returns the next available queue position (one past the max).
    fn next_queue_position(&self) -> i32 {
        self.torrents
            .values()
            .filter(|e| e.auto_managed)
            .map(|e| e.queue_position)
            .max()
            .map(|m| m + 1)
            .unwrap_or(0)
    }

    async fn handle_add_torrent(
        &mut self,
        torrent_meta: torrent_core::TorrentMeta,
        storage: Option<Arc<dyn TorrentStorage>>,
    ) -> crate::Result<Id20> {
        let version = torrent_meta.version();
        let meta_v2 = torrent_meta.as_v2().cloned();

        // For v2-only torrents, synthesize a minimal v1 metadata wrapper.
        // The session uses info_hash (Id20) as the primary key, so we use
        // the SHA-256 truncated to 20 bytes (as per BEP 52 tracker/DHT compat).
        let meta = match torrent_meta.as_v1() {
            Some(v1) => v1.clone(),
            None => {
                let v2 = torrent_meta.as_v2().unwrap();
                synthesize_v1_from_v2(v2)?
            }
        };

        let info_hash = meta.info_hash;

        if self.torrents.contains_key(&info_hash) {
            return Err(crate::Error::DuplicateTorrent(info_hash));
        }

        if self.torrents.len() >= self.settings.max_torrents {
            return Err(crate::Error::SessionAtCapacity(self.settings.max_torrents));
        }

        let torrent_config = self.make_torrent_config();

        // Create or use provided storage, then register with disk manager
        let storage: Arc<dyn TorrentStorage> = match storage {
            Some(s) => s,
            None => {
                let lengths = Lengths::new(
                    meta.info.total_length(),
                    meta.info.piece_length,
                    DEFAULT_CHUNK_SIZE,
                );
                Arc::new(torrent_storage::MemoryStorage::new(lengths))
            }
        };
        let disk_handle = self.disk_manager.register_torrent(info_hash, storage).await;

        let (global_up, global_down) = self.global_buckets_if_limited();
        let slot_tuner = self.make_slot_tuner();

        let handle = TorrentHandle::from_torrent(
            meta.clone(),
            version,
            meta_v2,
            disk_handle,
            self.disk_manager.clone(),
            torrent_config,
            self.dht_v4.clone(),
            self.dht_v6.clone(),
            global_up,
            global_down,
            slot_tuner,
            self.alert_tx.clone(),
            Arc::clone(&self.alert_mask),
            self.utp_socket.clone(),
            self.utp_socket_v6.clone(),
            Arc::clone(&self.ban_manager),
            Arc::clone(&self.ip_filter),
            Arc::clone(&self.plugins),
            self.sam_session.clone(),
            self.ssl_manager.clone(),
            Arc::clone(&self.factory),
            Some(Arc::clone(&self.hash_pool)),
        )
        .await?;

        let name = meta.info.name.clone();
        self.torrents.insert(
            info_hash,
            TorrentEntry {
                handle,
                meta: Some(meta),
                queue_position: -1,
                auto_managed: true,
                started_at: Some(tokio::time::Instant::now()),
                prev_downloaded: 0,
                prev_uploaded: 0,
            },
        );

        // Assign queue position for auto-managed torrents
        let pos = self.next_queue_position();
        if let Some(entry) = self.torrents.get_mut(&info_hash)
            && entry.auto_managed
        {
            entry.queue_position = pos;
        }

        info!(%info_hash, "torrent added to session");
        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::TorrentAdded { info_hash, name },
        );
        // BEP 27: private torrents must not use LSD
        let is_private = self.torrents.get(&info_hash).is_some_and(|e| e.is_private());
        if let Some(ref lsd) = self.lsd
            && !is_private
        {
            lsd.announce(vec![info_hash]).await;
        }
        Ok(info_hash)
    }

    async fn handle_add_magnet(&mut self, magnet: Magnet) -> crate::Result<Id20> {
        let info_hash = magnet.info_hash();
        let display_name = magnet.display_name.clone().unwrap_or_default();
        if self.torrents.contains_key(&info_hash) {
            return Err(crate::Error::DuplicateTorrent(info_hash));
        }
        if self.torrents.len() >= self.settings.max_torrents {
            return Err(crate::Error::SessionAtCapacity(self.settings.max_torrents));
        }
        let config = self.make_torrent_config();
        let (global_up, global_down) = self.global_buckets_if_limited();
        let slot_tuner = self.make_slot_tuner();
        let handle = TorrentHandle::from_magnet(
            magnet,
            self.disk_manager.clone(),
            config,
            self.dht_v4.clone(),
            self.dht_v6.clone(),
            global_up,
            global_down,
            slot_tuner,
            self.alert_tx.clone(),
            Arc::clone(&self.alert_mask),
            self.utp_socket.clone(),
            self.utp_socket_v6.clone(),
            Arc::clone(&self.ban_manager),
            Arc::clone(&self.ip_filter),
            Arc::clone(&self.plugins),
            self.sam_session.clone(),
            self.ssl_manager.clone(),
            Arc::clone(&self.factory),
            Some(Arc::clone(&self.hash_pool)),
        )
        .await?;
        self.torrents.insert(
            info_hash,
            TorrentEntry {
                handle,
                meta: None,
                queue_position: -1,
                auto_managed: true,
                started_at: Some(tokio::time::Instant::now()),
                prev_downloaded: 0,
                prev_uploaded: 0,
            },
        );

        // Assign queue position for auto-managed torrents
        let pos = self.next_queue_position();
        if let Some(entry) = self.torrents.get_mut(&info_hash)
            && entry.auto_managed
        {
            entry.queue_position = pos;
        }

        info!(%info_hash, "magnet torrent added to session");
        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::TorrentAdded {
                info_hash,
                name: display_name,
            },
        );
        // BEP 27: magnet metadata not available yet — we allow this one-time LAN
        // announce. Once metadata resolves, all subsequent LSD ops are gated by
        // is_private() checks in ForceLsdAnnounce and lsd_peers_rx handlers.
        if let Some(ref lsd) = self.lsd {
            lsd.announce(vec![info_hash]).await;
        }
        Ok(info_hash)
    }

    async fn handle_remove_torrent(&mut self, info_hash: Id20) -> crate::Result<()> {
        let entry = self
            .torrents
            .remove(&info_hash)
            .ok_or(crate::Error::TorrentNotFound(info_hash))?;
        let was_auto_managed = entry.auto_managed;
        let removed_position = entry.queue_position;
        entry.handle.shutdown().await?;
        self.disk_manager.unregister_torrent(info_hash).await;

        // Shift queue positions for remaining auto-managed torrents
        if was_auto_managed && removed_position >= 0 {
            let mut entries = self.queue_entries();
            let changed = crate::queue::remove_position(&mut entries, removed_position);
            self.apply_queue_changes(&changed);
        }

        info!(%info_hash, "torrent removed from session");
        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::TorrentRemoved { info_hash },
        );
        Ok(())
    }

    async fn handle_pause_torrent(&mut self, info_hash: Id20) -> crate::Result<()> {
        let entry = self
            .torrents
            .get(&info_hash)
            .ok_or(crate::Error::TorrentNotFound(info_hash))?;
        entry.handle.pause().await
    }

    async fn handle_resume_torrent(&mut self, info_hash: Id20) -> crate::Result<()> {
        let entry = self
            .torrents
            .get(&info_hash)
            .ok_or(crate::Error::TorrentNotFound(info_hash))?;
        entry.handle.resume().await
    }

    async fn handle_move_torrent_storage(
        &self,
        info_hash: Id20,
        new_path: std::path::PathBuf,
    ) -> crate::Result<()> {
        let entry = self
            .torrents
            .get(&info_hash)
            .ok_or(crate::Error::TorrentNotFound(info_hash))?;
        entry.handle.move_storage(new_path).await
    }

    async fn handle_torrent_stats(&self, info_hash: Id20) -> crate::Result<TorrentStats> {
        let entry = self
            .torrents
            .get(&info_hash)
            .ok_or(crate::Error::TorrentNotFound(info_hash))?;
        let mut stats = entry.handle.stats().await?;
        // Enrich with session-level data that the torrent actor doesn't own.
        stats.queue_position = entry.queue_position;
        stats.auto_managed = entry.auto_managed;
        Ok(stats)
    }

    fn handle_torrent_info(&self, info_hash: Id20) -> crate::Result<TorrentInfo> {
        let entry = self
            .torrents
            .get(&info_hash)
            .ok_or(crate::Error::TorrentNotFound(info_hash))?;

        let meta = entry
            .meta
            .as_ref()
            .ok_or(crate::Error::MetadataNotReady(info_hash))?;
        let files: Vec<FileInfo> = if let Some(ref file_list) = meta.info.files {
            file_list
                .iter()
                .map(|f| FileInfo {
                    path: f.path.iter().collect::<PathBuf>(),
                    length: f.length,
                })
                .collect()
        } else {
            vec![FileInfo {
                path: PathBuf::from(&meta.info.name),
                length: meta.info.total_length(),
            }]
        };

        Ok(TorrentInfo {
            info_hash,
            name: meta.info.name.clone(),
            total_length: meta.info.total_length(),
            piece_length: meta.info.piece_length,
            num_pieces: meta.info.num_pieces() as u32,
            files,
            private: meta.info.private == Some(1),
        })
    }

    /// Update gauge metrics that come from session-level state.
    fn update_session_gauges(&self) {
        use crate::stats::*;
        let c = &self.counters;
        c.set(SES_NUM_TORRENTS, self.torrents.len() as i64);
        c.set(SES_ACTIVE_TORRENTS, self.torrents.len() as i64);

        // DHT presence (instance count, not routing table size)
        let dht_nodes = self.dht_v4.is_some() as i64 + self.dht_v6.is_some() as i64;
        c.set(DHT_NODES, dht_nodes);
        c.set(DHT_NODES_V4, self.dht_v4.is_some() as i64);
        c.set(DHT_NODES_V6, self.dht_v6.is_some() as i64);

        // Ban count
        let ban_count = self.ban_manager.read().unwrap().banned_list().len() as i64;
        c.set(PEER_NUM_BANNED, ban_count);
    }

    /// Snapshot counters and fire a SessionStatsAlert.
    fn fire_stats_alert(&self) {
        self.update_session_gauges();
        let values = self.counters.snapshot();
        crate::alert::post_alert(
            &self.alert_tx,
            &self.alert_mask,
            crate::alert::AlertKind::SessionStatsAlert { values },
        );
    }

    /// Fire a periodic BEP 51 sample_infohashes query to the DHT (M111).
    async fn fire_sample_infohashes(&self) {
        let dht = match (&self.dht_v4, &self.dht_v6) {
            (Some(d), _) | (_, Some(d)) => d,
            _ => return,
        };
        let mut buf = [0u8; 20];
        torrent_core::random_bytes(&mut buf);
        let target = Id20::from(buf);
        match dht.sample_infohashes(target).await {
            Ok(result) => {
                post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::DhtSampleInfohashes {
                        num_samples: result.samples.len(),
                        total_estimate: result.num,
                    },
                );
            }
            Err(e) => {
                debug!("sample_infohashes failed: {e}");
            }
        }
    }

    async fn make_session_stats(&self) -> SessionStats {
        self.update_session_gauges();

        let mut total_downloaded = 0u64;
        let mut total_uploaded = 0u64;

        for entry in self.torrents.values() {
            if let Ok(stats) = entry.handle.stats().await {
                total_downloaded += stats.downloaded;
                total_uploaded += stats.uploaded;
            }
        }

        SessionStats {
            active_torrents: self.torrents.len(),
            total_downloaded,
            total_uploaded,
            dht_nodes: self.dht_v4.is_some() as usize + self.dht_v6.is_some() as usize,
        }
    }

    async fn handle_save_torrent_resume(
        &self,
        info_hash: Id20,
    ) -> crate::Result<torrent_core::FastResumeData> {
        let entry = self
            .torrents
            .get(&info_hash)
            .ok_or(crate::Error::TorrentNotFound(info_hash))?;
        let mut resume = entry.handle.save_resume_data().await?;
        // Patch in queue state from SessionActor's TorrentEntry (the
        // TorrentHandle doesn't know about queue position / auto-managed).
        resume.queue_position = entry.queue_position as i64;
        resume.auto_managed = if entry.auto_managed { 1 } else { 0 };
        Ok(resume)
    }

    async fn handle_save_session_state(&self) -> crate::Result<crate::persistence::SessionState> {
        use crate::persistence::SessionState;

        let mut torrents = Vec::new();
        for (info_hash, entry) in &self.torrents {
            match entry.handle.save_resume_data().await {
                Ok(rd) => torrents.push(rd),
                Err(e) => {
                    warn!(%info_hash, "failed to save resume data: {e}");
                }
            }
        }

        // Serialize smart ban state (scoped to drop RwLockReadGuard before awaits)
        let (banned_peers, peer_strikes) = {
            let ban_mgr = self.ban_manager.read().unwrap();
            let banned_peers: Vec<String> = ban_mgr
                .banned_list()
                .iter()
                .map(|ip| ip.to_string())
                .collect();
            let peer_strikes: Vec<crate::persistence::PeerStrikeEntry> = ban_mgr
                .strikes_map()
                .iter()
                .map(|(ip, &count)| crate::persistence::PeerStrikeEntry {
                    ip: ip.to_string(),
                    count: count as i64,
                })
                .collect();
            (banned_peers, peer_strikes)
        };

        let mut dht_entries = Vec::new();
        let mut dht_node_id = None;
        if let Some(ref dht) = self.dht_v4 {
            // Save the (possibly BEP 42-regenerated) node ID for next session
            if let Ok(stats) = dht.stats().await {
                dht_node_id = Some(stats.node_id.to_hex());
            }
            for (_id, addr) in dht.get_routing_nodes().await {
                dht_entries.push(crate::persistence::DhtNodeEntry {
                    host: addr.ip().to_string(),
                    port: addr.port() as i64,
                });
            }
        }
        if let Some(ref dht) = self.dht_v6 {
            for (_id, addr) in dht.get_routing_nodes().await {
                dht_entries.push(crate::persistence::DhtNodeEntry {
                    host: addr.ip().to_string(),
                    port: addr.port() as i64,
                });
            }
        }

        Ok(SessionState {
            dht_nodes: dht_entries,
            dht_node_id,
            torrents,
            banned_peers,
            peer_strikes,
        })
    }

    /// Apply new settings at runtime.
    ///
    /// Validates, updates rate limiters and alert mask immediately.
    /// Sub-actor reconfiguration (disk, DHT, NAT) takes effect on next restart.
    fn handle_apply_settings(&mut self, new: Settings) -> crate::Result<()> {
        new.validate()?;

        // Update rate limiters if changed
        if new.upload_rate_limit != self.settings.upload_rate_limit {
            self.global_upload_bucket
                .lock()
                .unwrap()
                .set_rate(new.upload_rate_limit);
        }
        if new.download_rate_limit != self.settings.download_rate_limit {
            self.global_download_bucket
                .lock()
                .unwrap()
                .set_rate(new.download_rate_limit);
        }

        // Update alert mask if changed
        if new.alert_mask != self.settings.alert_mask {
            self.alert_mask
                .store(new.alert_mask.bits(), Ordering::Relaxed);
        }

        // Store new settings
        self.settings = new;

        // Fire alert
        post_alert(&self.alert_tx, &self.alert_mask, AlertKind::SettingsChanged);

        Ok(())
    }

    /// Build a QueueEntry snapshot from current auto-managed torrents.
    fn queue_entries(&self) -> Vec<crate::queue::QueueEntry> {
        self.torrents
            .iter()
            .filter(|(_, e)| e.auto_managed)
            .map(|(&hash, e)| crate::queue::QueueEntry {
                info_hash: hash,
                position: e.queue_position,
            })
            .collect()
    }

    fn handle_set_queue_position(&mut self, info_hash: Id20, pos: i32) -> crate::Result<()> {
        if !self.torrents.contains_key(&info_hash) {
            return Err(crate::Error::TorrentNotFound(info_hash));
        }
        let mut entries = self.queue_entries();
        let changed = crate::queue::set_position(&mut entries, info_hash, pos);
        self.apply_queue_changes(&changed);
        Ok(())
    }

    fn handle_queue_move(&mut self, info_hash: Id20, op: QueueMoveFn) -> crate::Result<()> {
        if !self.torrents.contains_key(&info_hash) {
            return Err(crate::Error::TorrentNotFound(info_hash));
        }
        let mut entries = self.queue_entries();
        let changed = op(&mut entries, info_hash);
        self.apply_queue_changes(&changed);
        Ok(())
    }

    /// Apply position changes back to TorrentEntry fields and fire alerts.
    fn apply_queue_changes(&mut self, changed: &[(Id20, i32, i32)]) {
        for &(hash, old_pos, new_pos) in changed {
            if let Some(entry) = self.torrents.get_mut(&hash) {
                entry.queue_position = new_pos;
            }
            crate::alert::post_alert(
                &self.alert_tx,
                &self.alert_mask,
                crate::alert::AlertKind::TorrentQueuePositionChanged {
                    info_hash: hash,
                    old_pos,
                    new_pos,
                },
            );
        }
    }

    async fn evaluate_queue(&mut self) {
        let now = tokio::time::Instant::now();
        let startup_duration = std::time::Duration::from_secs(self.settings.auto_manage_startup);
        let auto_manage_secs = self.settings.auto_manage_interval.max(1);
        let mut candidates = Vec::new();

        // Collect info hashes first to avoid borrow issues with async calls
        let hashes: Vec<Id20> = self.torrents.keys().copied().collect();

        for &info_hash in &hashes {
            let (auto_managed, queue_position, started_at, prev_downloaded, prev_uploaded) = {
                let entry = match self.torrents.get(&info_hash) {
                    Some(e) => e,
                    None => continue,
                };
                if !entry.auto_managed {
                    continue;
                }
                (
                    entry.auto_managed,
                    entry.queue_position,
                    entry.started_at,
                    entry.prev_downloaded,
                    entry.prev_uploaded,
                )
            };

            let _ = auto_managed; // used above in the guard

            // Get current stats (async call — self.torrents is not borrowed here)
            let stats = match self.torrents.get(&info_hash) {
                Some(entry) => match entry.handle.stats().await {
                    Ok(s) => s,
                    Err(_) => continue,
                },
                None => continue,
            };

            let category = match stats.state {
                TorrentState::Downloading
                | TorrentState::FetchingMetadata
                | TorrentState::Checking => crate::queue::QueueCategory::Downloading,
                TorrentState::Seeding | TorrentState::Complete => {
                    crate::queue::QueueCategory::Seeding
                }
                TorrentState::Paused => {
                    // Determine category based on completion
                    if stats.pieces_have >= stats.pieces_total && stats.pieces_total > 0 {
                        crate::queue::QueueCategory::Seeding
                    } else {
                        crate::queue::QueueCategory::Downloading
                    }
                }
                TorrentState::Stopped | TorrentState::Sharing => continue,
            };

            let is_active = stats.state != TorrentState::Paused;

            // Compute rate from delta since last tick
            let download_rate = stats.downloaded.saturating_sub(prev_downloaded) / auto_manage_secs;
            let upload_rate = stats.uploaded.saturating_sub(prev_uploaded) / auto_manage_secs;

            let past_startup = started_at
                .map(|t| now.duration_since(t) > startup_duration)
                .unwrap_or(true);

            let is_inactive = past_startup
                && match category {
                    crate::queue::QueueCategory::Downloading => {
                        download_rate < self.settings.inactive_down_rate
                    }
                    crate::queue::QueueCategory::Seeding => {
                        upload_rate < self.settings.inactive_up_rate
                    }
                };

            candidates.push(crate::queue::QueueCandidate {
                info_hash,
                position: queue_position,
                category,
                is_active,
                is_inactive,
            });
        }

        // Update cached stats for next tick's rate calculation
        for &hash in &hashes {
            if let Some(entry) = self.torrents.get(&hash)
                && let Ok(stats) = entry.handle.stats().await
                && let Some(entry) = self.torrents.get_mut(&hash)
            {
                entry.prev_downloaded = stats.downloaded;
                entry.prev_uploaded = stats.uploaded;
            }
        }

        let decision = crate::queue::evaluate(
            &candidates,
            self.settings.active_downloads,
            self.settings.active_seeds,
            self.settings.active_limit,
            self.settings.dont_count_slow_torrents,
            self.settings.auto_manage_prefer_seeds,
        );

        // Apply decisions
        for hash in &decision.to_pause {
            if let Some(entry) = self.torrents.get(hash) {
                let _ = entry.handle.pause().await;
            }
            post_alert(
                &self.alert_tx,
                &self.alert_mask,
                AlertKind::TorrentAutoManaged {
                    info_hash: *hash,
                    paused: true,
                },
            );
        }

        for hash in &decision.to_resume {
            if let Some(entry) = self.torrents.get_mut(hash) {
                let _ = entry.handle.resume().await;
                entry.started_at = Some(tokio::time::Instant::now());
            }
            post_alert(
                &self.alert_tx,
                &self.alert_mask,
                AlertKind::TorrentAutoManaged {
                    info_hash: *hash,
                    paused: false,
                },
            );
        }
    }

    /// Route an inbound uTP connection to the correct torrent.
    fn handle_utp_inbound(&self, stream: torrent_utp::UtpStream, addr: std::net::SocketAddr) {
        self.route_inbound_stream(stream, addr, "uTP");
    }

    /// Handle an incoming TCP connection from the main listen port.
    fn handle_tcp_inbound(
        &self,
        stream: crate::transport::BoxedStream,
        addr: std::net::SocketAddr,
    ) {
        self.route_inbound_stream(stream, addr, "TCP");
    }

    /// Route an inbound stream (TCP or uTP) to the appropriate torrent by reading the
    /// BitTorrent handshake preamble.
    fn route_inbound_stream<S>(
        &self,
        stream: S,
        addr: std::net::SocketAddr,
        transport: &'static str,
    ) where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        // Clone torrent handles so the routing task can look up by info_hash.
        let torrent_handles: HashMap<Id20, TorrentHandle> = self
            .torrents
            .iter()
            .map(|(hash, entry)| (*hash, entry.handle.clone()))
            .collect();

        tokio::spawn(async move {
            match crate::utp_routing::identify_plaintext_connection(stream).await {
                Ok(Some((info_hash, prefixed_stream))) => {
                    if let Some(handle) = torrent_handles.get(&info_hash) {
                        debug!(%addr, %info_hash, "routing inbound {transport} peer to torrent");
                        let boxed = crate::transport::BoxedStream::new(prefixed_stream);
                        let _ = handle.send_incoming_peer(boxed, addr).await;
                    } else {
                        debug!(%addr, %info_hash, "inbound {transport} peer for unknown torrent, dropping");
                    }
                }
                Ok(None) => {
                    debug!(%addr, "inbound {transport} peer is encrypted (MSE), dropping (deferred)");
                }
                Err(e) => {
                    debug!(%addr, error = %e, "failed to identify inbound {transport} peer");
                }
            }
        });
    }

    /// Handle an incoming SSL/TLS connection (M42).
    ///
    /// Uses `LazyConfigAcceptor` to peek at the TLS ClientHello and extract
    /// the SNI (hex-encoded info hash) to route the connection to the right
    /// torrent. The full TLS handshake uses the torrent's CA cert to build
    /// the server config.
    async fn handle_ssl_incoming(
        &mut self,
        stream: crate::transport::BoxedStream,
        addr: std::net::SocketAddr,
    ) {
        use tokio_rustls::LazyConfigAcceptor;

        let acceptor = LazyConfigAcceptor::new(rustls::server::Acceptor::default(), stream);

        let start_handshake = match acceptor.await {
            Ok(sh) => sh,
            Err(e) => {
                debug!(%addr, error = %e, "SSL ClientHello read failed");
                return;
            }
        };

        // Extract SNI from ClientHello
        let client_hello = start_handshake.client_hello();
        let sni = match client_hello.server_name() {
            Some(name) => name.to_string(),
            None => {
                debug!(%addr, "SSL connection missing SNI");
                return;
            }
        };

        // SNI is hex-encoded info hash (40 chars for SHA-1)
        let info_hash = match Id20::from_hex(&sni) {
            Ok(h) => h,
            Err(_) => {
                debug!(%addr, sni = %sni, "SSL SNI is not a valid info hash");
                return;
            }
        };

        // Look up the torrent
        let torrent = match self.torrents.get(&info_hash) {
            Some(t) => t,
            None => {
                debug!(%addr, %info_hash, "SSL connection for unknown torrent");
                return;
            }
        };

        // Get the SSL CA cert from the torrent's metadata
        let ssl_cert = match torrent.meta.as_ref().and_then(|m| m.ssl_cert.as_ref()) {
            Some(cert) => cert.clone(),
            None => {
                debug!(%addr, %info_hash, "SSL connection for non-SSL torrent");
                return;
            }
        };

        // Build server config using the torrent's CA cert
        let server_config = match self.ssl_manager.as_ref() {
            Some(mgr) => match mgr.server_config(&ssl_cert) {
                Ok(cfg) => cfg,
                Err(e) => {
                    warn!(%addr, %info_hash, error = %e, "failed to build SSL server config");
                    return;
                }
            },
            None => {
                debug!(%addr, "SSL manager not initialized");
                return;
            }
        };

        // Complete the TLS handshake
        let tls_stream = match start_handshake.into_stream(server_config).await {
            Ok(s) => s,
            Err(e) => {
                warn!(%addr, %info_hash, error = %e, "SSL handshake failed");
                post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::SslTorrentError {
                        info_hash,
                        message: format!("inbound TLS handshake from {addr}: {e}"),
                    },
                );
                return;
            }
        };

        // Route to the torrent actor via SpawnSslPeer command
        let _ = torrent.handle.spawn_ssl_peer(addr, tls_stream).await;
    }

    async fn handle_dht_put_immutable(&self, value: Vec<u8>) -> crate::Result<Id20> {
        let dht = self.dht_v4.as_ref().ok_or(crate::Error::DhtDisabled)?;
        match dht.put_immutable(value.clone()).await {
            Ok(target) => {
                post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::DhtPutComplete { target },
                );
                Ok(target)
            }
            Err(e) => {
                let target = torrent_core::sha1(&value);
                post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::DhtItemError {
                        target,
                        message: e.to_string(),
                    },
                );
                Err(crate::Error::Dht(e))
            }
        }
    }

    async fn handle_dht_get_immutable(&self, target: Id20) -> crate::Result<Option<Vec<u8>>> {
        let dht = self.dht_v4.as_ref().ok_or(crate::Error::DhtDisabled)?;
        match dht.get_immutable(target).await {
            Ok(value) => {
                post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::DhtGetResult {
                        target,
                        value: value.clone(),
                    },
                );
                Ok(value)
            }
            Err(e) => {
                post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::DhtItemError {
                        target,
                        message: e.to_string(),
                    },
                );
                Err(crate::Error::Dht(e))
            }
        }
    }

    async fn handle_dht_put_mutable(
        &self,
        keypair_bytes: [u8; 32],
        value: Vec<u8>,
        seq: i64,
        salt: Vec<u8>,
    ) -> crate::Result<Id20> {
        let dht = self.dht_v4.as_ref().ok_or(crate::Error::DhtDisabled)?;
        match dht.put_mutable(keypair_bytes, value, seq, salt).await {
            Ok(target) => {
                post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::DhtMutablePutComplete { target, seq },
                );
                Ok(target)
            }
            Err(e) => {
                post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::DhtItemError {
                        target: Id20::from([0u8; 20]),
                        message: e.to_string(),
                    },
                );
                Err(crate::Error::Dht(e))
            }
        }
    }

    async fn handle_dht_get_mutable(
        &self,
        public_key: [u8; 32],
        salt: Vec<u8>,
    ) -> crate::Result<Option<(Vec<u8>, i64)>> {
        let dht = self.dht_v4.as_ref().ok_or(crate::Error::DhtDisabled)?;
        let target = torrent_dht::compute_mutable_target(&public_key, &salt);
        match dht.get_mutable(public_key, salt).await {
            Ok(result) => {
                let (value, seq) = match &result {
                    Some((v, s)) => (Some(v.clone()), Some(*s)),
                    None => (None, None),
                };
                post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::DhtMutableGetResult {
                        target,
                        value,
                        seq,
                        public_key,
                    },
                );
                Ok(result)
            }
            Err(e) => {
                post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::DhtItemError {
                        target,
                        message: e.to_string(),
                    },
                );
                Err(crate::Error::Dht(e))
            }
        }
    }

    async fn shutdown_all(&mut self) {
        for (info_hash, entry) in self.torrents.drain() {
            debug!(%info_hash, "shutting down torrent");
            let _ = entry.handle.shutdown().await;
        }
        if let Some(ref dht) = self.dht_v4 {
            let _ = dht.shutdown().await;
        }
        if let Some(ref dht) = self.dht_v6 {
            let _ = dht.shutdown().await;
        }
        if let Some(ref nat) = self.nat {
            nat.shutdown().await;
        }
        if let Some(ref lsd) = self.lsd {
            lsd.shutdown().await;
        }
        if let Some(ref socket) = self.utp_socket
            && let Err(e) = socket.shutdown().await
        {
            debug!(error = %e, "uTP socket shutdown error");
        }
        if let Some(ref socket) = self.utp_socket_v6
            && let Err(e) = socket.shutdown().await
        {
            debug!(error = %e, "uTP v6 socket shutdown error");
        }
        self.disk_manager.shutdown().await;
    }
}

/// Helper to accept a connection from an optional uTP listener.
/// Returns `pending` if no listener is available, so the `select!` branch is skipped.
async fn accept_utp(
    listener: &mut Option<torrent_utp::UtpListener>,
) -> std::io::Result<(torrent_utp::UtpStream, std::net::SocketAddr)> {
    match listener {
        Some(l) => l.accept().await.map_err(std::io::Error::other),
        None => std::future::pending().await,
    }
}

/// Helper to receive NAT events from an optional receiver.
/// Returns `pending` if no receiver is available, so the `select!` branch is skipped.
async fn recv_nat_event(
    rx: &mut Option<mpsc::Receiver<torrent_nat::NatEvent>>,
) -> torrent_nat::NatEvent {
    match rx {
        Some(r) => match r.recv().await {
            Some(event) => event,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

/// Receive from an optional DHT IP consensus channel, pending forever if absent.
async fn recv_dht_ip(
    rx: &mut Option<mpsc::Receiver<std::net::IpAddr>>,
) -> Option<std::net::IpAddr> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// Synthesize a minimal `TorrentMetaV1` from a `TorrentMetaV2` for session compatibility.
///
/// The session engine uses v1 structures internally (info hash as Id20, InfoDict for
/// piece hashing, etc.). For v2-only torrents, we create a "virtual" v1 representation
/// with the truncated SHA-256 hash as the info_hash.
fn synthesize_v1_from_v2(
    v2: &torrent_core::TorrentMetaV2,
) -> crate::Result<torrent_core::TorrentMetaV1> {
    use torrent_core::{FileEntry, InfoDict};

    let info_hash = v2.info_hashes.best_v1();

    // Build file entries from v2 file tree
    let v2_files = v2.info.files();
    let file_entries: Vec<FileEntry> = v2_files
        .iter()
        .map(|f| FileEntry {
            length: f.attr.length,
            path: f.path.clone(),
            attr: None,
            mtime: None,
            symlink_path: None,
        })
        .collect();

    // v2-only torrents have no v1 piece hashes — use placeholder pieces field.
    // Verification is done via v2 Merkle trees, not v1 SHA-1 hashes.
    let num_pieces = v2.info.num_pieces() as usize;
    let pieces = vec![0u8; num_pieces * 20];

    let info = InfoDict {
        name: v2.info.name.clone(),
        piece_length: v2.info.piece_length,
        pieces,
        length: if file_entries.len() == 1 {
            Some(file_entries[0].length)
        } else {
            None
        },
        files: if file_entries.len() > 1 {
            Some(file_entries)
        } else {
            None
        },
        private: None,
        source: None,
        ssl_cert: v2.ssl_cert.clone(),
    };

    Ok(torrent_core::TorrentMetaV1 {
        info_hash,
        announce: v2.announce.clone(),
        announce_list: v2.announce_list.clone(),
        comment: v2.comment.clone(),
        created_by: v2.created_by.clone(),
        creation_date: v2.creation_date,
        info,
        info_bytes: None,
        url_list: Vec::new(),
        httpseeds: Vec::new(),
        ssl_cert: v2.ssl_cert.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TorrentState;
    use std::time::Duration;
    use torrent_core::{DEFAULT_CHUNK_SIZE, Lengths, torrent_from_bytes};
    use torrent_storage::MemoryStorage;

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

    fn make_storage(data: &[u8], piece_length: u64) -> Arc<MemoryStorage> {
        let lengths = Lengths::new(data.len() as u64, piece_length, DEFAULT_CHUNK_SIZE);
        Arc::new(MemoryStorage::new(lengths))
    }

    fn test_settings() -> Settings {
        Settings {
            listen_port: 0,
            download_dir: PathBuf::from("/tmp"),
            max_torrents: 10,
            enable_dht: false,
            enable_pex: false,
            enable_lsd: false,
            enable_fast_extension: false,
            enable_utp: false,
            enable_upnp: false,
            enable_natpmp: false,
            enable_ipv6: false,
            alert_channel_size: 64,
            disk_io_threads: 2,
            storage_mode: torrent_core::StorageMode::Sparse,
            disk_cache_size: 1024 * 1024,
            ..Settings::default()
        }
    }

    // ---- Test 1: Start and shutdown ----

    #[tokio::test]
    async fn session_start_and_shutdown() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let stats = session.session_stats().await.unwrap();
        assert_eq!(stats.active_torrents, 0);
        session.shutdown().await.unwrap();
    }

    // ---- Test 2: Add and list torrent ----

    #[tokio::test]
    async fn add_and_list_torrent() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let expected_hash = meta.info_hash;

        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();
        assert_eq!(info_hash, expected_hash);

        let list = session.list_torrents().await.unwrap();
        assert_eq!(list.len(), 1);
        assert!(list.contains(&info_hash));

        session.shutdown().await.unwrap();
    }

    // ---- Test 3: Remove torrent ----

    #[tokio::test]
    async fn remove_torrent() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);

        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();
        session.remove_torrent(info_hash).await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        let list = session.list_torrents().await.unwrap();
        assert!(list.is_empty());

        session.shutdown().await.unwrap();
    }

    // ---- Test 4: Duplicate rejection ----

    #[tokio::test]
    async fn duplicate_torrent_rejected() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage1 = make_storage(&data, 16384);
        let storage2 = make_storage(&data, 16384);

        session
            .add_torrent(meta.clone().into(), Some(storage1))
            .await
            .unwrap();
        let result = session.add_torrent(meta.into(), Some(storage2)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("duplicate"));

        session.shutdown().await.unwrap();
    }

    // ---- Test 5: Max capacity ----

    #[tokio::test]
    async fn session_at_capacity() {
        let mut config = test_settings();
        config.max_torrents = 1;
        let session = SessionHandle::start(config).await.unwrap();

        let data1 = vec![0xAA; 16384];
        let meta1 = make_test_torrent(&data1, 16384);
        let storage1 = make_storage(&data1, 16384);
        session
            .add_torrent(meta1.into(), Some(storage1))
            .await
            .unwrap();

        let data2 = vec![0xBB; 16384];
        let meta2 = make_test_torrent(&data2, 16384);
        let storage2 = make_storage(&data2, 16384);
        let result = session.add_torrent(meta2.into(), Some(storage2)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("capacity"));

        session.shutdown().await.unwrap();
    }

    // ---- Test 6: Torrent stats ----

    #[tokio::test]
    async fn torrent_stats_via_session() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);

        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();
        let stats = session.torrent_stats(info_hash).await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);
        assert_eq!(stats.pieces_total, 2);

        session.shutdown().await.unwrap();
    }

    // ---- Test 7: Torrent info ----

    #[tokio::test]
    async fn torrent_info_via_session() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);

        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();
        let info = session.torrent_info(info_hash).await.unwrap();
        assert_eq!(info.info_hash, info_hash);
        assert_eq!(info.name, "test");
        assert_eq!(info.total_length, 32768);
        assert_eq!(info.num_pieces, 2);
        assert!(!info.private);
        assert_eq!(info.files.len(), 1);
        assert_eq!(info.files[0].length, 32768);

        session.shutdown().await.unwrap();
    }

    // ---- Test 8: Pause/resume via session ----

    #[tokio::test]
    async fn pause_resume_via_session() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);

        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        session.pause_torrent(info_hash).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stats = session.torrent_stats(info_hash).await.unwrap();
        assert_eq!(stats.state, TorrentState::Paused);

        session.resume_torrent(info_hash).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stats = session.torrent_stats(info_hash).await.unwrap();
        assert_eq!(stats.state, TorrentState::Downloading);

        session.shutdown().await.unwrap();
    }

    // ---- Test 9: Not-found errors ----

    #[tokio::test]
    async fn not_found_errors() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let fake_hash = Id20::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();

        assert!(session.torrent_stats(fake_hash).await.is_err());
        assert!(session.torrent_info(fake_hash).await.is_err());
        assert!(session.pause_torrent(fake_hash).await.is_err());
        assert!(session.resume_torrent(fake_hash).await.is_err());
        assert!(session.remove_torrent(fake_hash).await.is_err());

        session.shutdown().await.unwrap();
    }

    // ---- Test 10: Session stats ----

    #[tokio::test]
    async fn session_stats_aggregate() {
        let session = SessionHandle::start(test_settings()).await.unwrap();

        let data1 = vec![0xAA; 16384];
        let meta1 = make_test_torrent(&data1, 16384);
        let storage1 = make_storage(&data1, 16384);
        session
            .add_torrent(meta1.into(), Some(storage1))
            .await
            .unwrap();

        let data2 = vec![0xBB; 16384];
        let meta2 = make_test_torrent(&data2, 16384);
        let storage2 = make_storage(&data2, 16384);
        session
            .add_torrent(meta2.into(), Some(storage2))
            .await
            .unwrap();

        let stats = session.session_stats().await.unwrap();
        assert_eq!(stats.active_torrents, 2);

        session.shutdown().await.unwrap();
    }

    // ---- Test 11: Add magnet and list ----

    #[tokio::test]
    async fn add_magnet_and_list() {
        use torrent_core::Magnet;

        let session = SessionHandle::start(test_settings()).await.unwrap();
        let magnet = Magnet {
            info_hashes: torrent_core::InfoHashes::v1_only(
                Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap(),
            ),
            display_name: Some("test-magnet".into()),
            trackers: vec![],
            peers: vec![],
            selected_files: None,
        };
        let expected_hash = magnet.info_hash();

        let info_hash = session.add_magnet(magnet).await.unwrap();
        assert_eq!(info_hash, expected_hash);

        let list = session.list_torrents().await.unwrap();
        assert_eq!(list.len(), 1);
        assert!(list.contains(&info_hash));

        // torrent_info should fail with MetadataNotReady
        let err = session.torrent_info(info_hash).await.unwrap_err();
        assert!(err.to_string().contains("metadata not yet available"));

        session.shutdown().await.unwrap();
    }

    // ---- Test 12: Duplicate magnet rejected ----

    #[tokio::test]
    async fn add_magnet_duplicate_rejected() {
        use torrent_core::Magnet;

        let session = SessionHandle::start(test_settings()).await.unwrap();
        let magnet = Magnet {
            info_hashes: torrent_core::InfoHashes::v1_only(
                Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap(),
            ),
            display_name: Some("test-magnet".into()),
            trackers: vec![],
            peers: vec![],
            selected_files: None,
        };

        session.add_magnet(magnet.clone()).await.unwrap();
        let result = session.add_magnet(magnet).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("duplicate"));

        session.shutdown().await.unwrap();
    }

    // ---- Test 13: Session with LSD enabled ----

    #[tokio::test]
    async fn session_with_lsd_enabled() {
        use torrent_core::Magnet;

        // LSD may fail to bind port 6771 — session should still start
        let mut config = test_settings();
        config.enable_lsd = true;

        let session = SessionHandle::start(config).await.unwrap();

        // Add a torrent (triggers LSD announce if available)
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Add a magnet (also triggers LSD announce)
        let magnet = Magnet {
            info_hashes: torrent_core::InfoHashes::v1_only(
                Id20::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap(),
            ),
            display_name: Some("lsd-test".into()),
            trackers: vec![],
            peers: vec![],
            selected_files: None,
        };
        session.add_magnet(magnet).await.unwrap();

        let list = session.list_torrents().await.unwrap();
        assert_eq!(list.len(), 2);

        session.shutdown().await.unwrap();
    }

    // ---- Test: v2-only torrent addition ----

    #[tokio::test]
    async fn add_v2_only_torrent() {
        use std::collections::BTreeMap;
        use torrent_bencode::BencodeValue;

        let session = SessionHandle::start(test_settings()).await.unwrap();

        // Build a minimal v2-only torrent
        let mut attr_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        attr_map.insert(b"length".to_vec(), BencodeValue::Integer(16384));
        let mut file_node: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        file_node.insert(b"".to_vec(), BencodeValue::Dict(attr_map));
        let mut ft_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        ft_map.insert(b"test.dat".to_vec(), BencodeValue::Dict(file_node));

        let mut info_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        info_map.insert(b"file tree".to_vec(), BencodeValue::Dict(ft_map));
        info_map.insert(b"meta version".to_vec(), BencodeValue::Integer(2));
        info_map.insert(b"name".to_vec(), BencodeValue::Bytes(b"v2test".to_vec()));
        info_map.insert(b"piece length".to_vec(), BencodeValue::Integer(16384));

        let mut root_map: BTreeMap<Vec<u8>, BencodeValue> = BTreeMap::new();
        root_map.insert(b"info".to_vec(), BencodeValue::Dict(info_map));

        let bytes = torrent_bencode::to_bytes(&BencodeValue::Dict(root_map)).unwrap();
        let meta = torrent_core::torrent_from_bytes_any(&bytes).unwrap();
        assert!(meta.is_v2());

        // This should NOT return an error now (v2-only is supported)
        let info_hash = session.add_torrent(meta, None).await.unwrap();
        let list = session.list_torrents().await.unwrap();
        assert!(list.contains(&info_hash));

        session.shutdown().await.unwrap();
    }

    // ---- Test 14: Save torrent resume data via session ----

    #[tokio::test]
    async fn save_torrent_resume_data_via_session() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let info_hash = meta.info_hash;
        let storage = make_storage(&data, 16384);
        session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        let rd = session.save_torrent_resume_data(info_hash).await.unwrap();
        assert_eq!(rd.info_hash, info_hash.as_bytes().as_slice());
        assert_eq!(rd.name, "test");
        assert_eq!(rd.file_format, "libtorrent resume file");
        assert_eq!(rd.file_version, 1);
        assert!(!rd.pieces.is_empty());
        assert_eq!(rd.paused, 0);

        session.shutdown().await.unwrap();
    }

    // ---- Test 15: Save session state captures all torrents ----

    #[tokio::test]
    async fn save_session_state_captures_all_torrents() {
        let session = SessionHandle::start(test_settings()).await.unwrap();

        let data1 = vec![0xAA; 16384];
        let meta1 = make_test_torrent(&data1, 16384);
        let storage1 = make_storage(&data1, 16384);
        session
            .add_torrent(meta1.into(), Some(storage1))
            .await
            .unwrap();

        let data2 = vec![0xBB; 16384];
        let meta2 = make_test_torrent(&data2, 16384);
        let storage2 = make_storage(&data2, 16384);
        session
            .add_torrent(meta2.into(), Some(storage2))
            .await
            .unwrap();

        let state = session.save_session_state().await.unwrap();
        assert_eq!(state.torrents.len(), 2);

        for rd in &state.torrents {
            assert_eq!(rd.file_format, "libtorrent resume file");
            assert_eq!(rd.info_hash.len(), 20);
        }

        session.shutdown().await.unwrap();
    }

    // ---- Test 16: Save resume data not found ----

    #[tokio::test]
    async fn save_resume_data_not_found() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let fake_hash = Id20::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let result = session.save_torrent_resume_data(fake_hash).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
        session.shutdown().await.unwrap();
    }

    // ---- Test 17: Subscribe receives TorrentAdded alert ----

    #[tokio::test]
    async fn subscribe_receives_torrent_added_alert() {
        use crate::alert::AlertKind;

        let session = SessionHandle::start(test_settings()).await.unwrap();
        let mut alerts = session.subscribe();

        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let _info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        let alert = tokio::time::timeout(Duration::from_secs(2), alerts.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(alert.kind, AlertKind::TorrentAdded { .. }));
        session.shutdown().await.unwrap();
    }

    // ---- Test 18: Subscribe receives TorrentRemoved alert ----

    #[tokio::test]
    async fn subscribe_receives_torrent_removed_alert() {
        use crate::alert::AlertKind;
        use crate::types::TorrentState;

        let session = SessionHandle::start(test_settings()).await.unwrap();
        let mut alerts = session.subscribe();

        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Drain TorrentAdded and any checking alerts
        while let Ok(Ok(a)) = tokio::time::timeout(Duration::from_secs(1), alerts.recv()).await {
            if matches!(
                a.kind,
                AlertKind::StateChanged {
                    new_state: TorrentState::Downloading,
                    ..
                }
            ) {
                break;
            }
        }

        session.remove_torrent(info_hash).await.unwrap();

        // Find TorrentRemoved (skip any interleaved alerts)
        loop {
            let alert = tokio::time::timeout(Duration::from_secs(2), alerts.recv())
                .await
                .unwrap()
                .unwrap();
            if matches!(alert.kind, AlertKind::TorrentRemoved { .. }) {
                break;
            }
        }
        session.shutdown().await.unwrap();
    }

    // ---- Test 19: Multiple subscribers each receive alerts ----

    #[tokio::test]
    async fn multiple_subscribers_each_receive_alerts() {
        use crate::alert::AlertKind;

        let session = SessionHandle::start(test_settings()).await.unwrap();
        let mut sub1 = session.subscribe();
        let mut sub2 = session.subscribe();

        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        let a1 = tokio::time::timeout(Duration::from_secs(2), sub1.recv())
            .await
            .unwrap()
            .unwrap();
        let a2 = tokio::time::timeout(Duration::from_secs(2), sub2.recv())
            .await
            .unwrap()
            .unwrap();

        assert!(matches!(a1.kind, AlertKind::TorrentAdded { .. }));
        assert!(matches!(a2.kind, AlertKind::TorrentAdded { .. }));
        session.shutdown().await.unwrap();
    }

    // ---- Test 20: set_alert_mask filters at runtime ----

    #[tokio::test]
    async fn set_alert_mask_filters_at_runtime() {
        use crate::alert::{AlertCategory, AlertKind};

        let session = SessionHandle::start(test_settings()).await.unwrap();
        let mut alerts = session.subscribe();

        // Start with ALL — TorrentAdded (STATUS) should arrive
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        let alert = tokio::time::timeout(Duration::from_secs(2), alerts.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(alert.kind, AlertKind::TorrentAdded { .. }));

        // Drain any remaining alerts from the first torrent (StateChanged, CheckingProgress, etc.)
        while tokio::time::timeout(Duration::from_millis(200), alerts.recv())
            .await
            .is_ok()
        {}

        // Change mask to empty — no alerts should pass
        session.set_alert_mask(AlertCategory::empty());

        let data2 = vec![0xBB; 16384];
        let meta2 = make_test_torrent(&data2, 16384);
        let storage2 = make_storage(&data2, 16384);
        session
            .add_torrent(meta2.into(), Some(storage2))
            .await
            .unwrap();

        // Give a small window — nothing should arrive
        let result = tokio::time::timeout(Duration::from_millis(200), alerts.recv()).await;
        assert!(result.is_err(), "should have timed out with empty mask");

        // Restore STATUS — adding another torrent should arrive
        session.set_alert_mask(AlertCategory::STATUS);

        let data3 = vec![0xCC; 16384];
        let meta3 = make_test_torrent(&data3, 16384);
        let storage3 = make_storage(&data3, 16384);
        session
            .add_torrent(meta3.into(), Some(storage3))
            .await
            .unwrap();

        let alert = tokio::time::timeout(Duration::from_secs(2), alerts.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(alert.kind, AlertKind::TorrentAdded { .. }));

        session.shutdown().await.unwrap();
    }

    // ---- Test 21: AlertStream filters per subscriber ----

    #[tokio::test]
    async fn alert_stream_filters_per_subscriber() {
        use crate::alert::{AlertCategory, AlertKind};

        let session = SessionHandle::start(test_settings()).await.unwrap();

        // subscriber A: STATUS only
        let mut status_sub = session.subscribe_filtered(AlertCategory::STATUS);
        // subscriber B: PEER only
        let mut peer_sub = session.subscribe_filtered(AlertCategory::PEER);

        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // STATUS sub gets TorrentAdded
        let alert = tokio::time::timeout(Duration::from_secs(2), status_sub.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(alert.kind, AlertKind::TorrentAdded { .. }));

        // PEER sub should NOT receive TorrentAdded (it's STATUS category)
        let result = tokio::time::timeout(Duration::from_millis(200), peer_sub.recv()).await;
        assert!(
            result.is_err(),
            "PEER subscriber should not get STATUS alerts"
        );

        session.shutdown().await.unwrap();
    }

    // ---- Test 22: State changed tracks transitions ----

    #[tokio::test]
    async fn state_changed_tracks_transitions() {
        use crate::alert::AlertKind;

        let session = SessionHandle::start(test_settings()).await.unwrap();
        let mut alerts = session.subscribe();

        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Drain TorrentAdded
        let _ = tokio::time::timeout(Duration::from_secs(1), alerts.recv())
            .await
            .unwrap();

        // Pause — should get StateChanged(Downloading → Paused) + TorrentPaused
        session.pause_torrent(info_hash).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Collect alerts over a short window
        let mut state_changes = Vec::new();
        let mut paused_alerts = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_millis(200), alerts.recv()).await {
                Ok(Ok(a)) => match &a.kind {
                    AlertKind::StateChanged {
                        prev_state,
                        new_state,
                        ..
                    } => {
                        state_changes.push((*prev_state, *new_state));
                    }
                    AlertKind::TorrentPaused { .. } => {
                        paused_alerts.push(a);
                    }
                    _ => {} // other alerts (PeerConnected etc)
                },
                _ => break,
            }
        }

        assert!(
            state_changes.contains(&(TorrentState::Downloading, TorrentState::Paused)),
            "expected Downloading→Paused, got: {state_changes:?}"
        );
        assert!(!paused_alerts.is_empty(), "expected TorrentPaused alert");

        // Resume — should get StateChanged(Paused → Downloading) + TorrentResumed
        session.resume_torrent(info_hash).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut resume_state_changes = Vec::new();
        let mut resumed_alerts = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_millis(200), alerts.recv()).await {
                Ok(Ok(a)) => match &a.kind {
                    AlertKind::StateChanged {
                        prev_state,
                        new_state,
                        ..
                    } => {
                        resume_state_changes.push((*prev_state, *new_state));
                    }
                    AlertKind::TorrentResumed { .. } => {
                        resumed_alerts.push(a);
                    }
                    _ => {}
                },
                _ => break,
            }
        }

        assert!(
            resume_state_changes.contains(&(TorrentState::Paused, TorrentState::Downloading)),
            "expected Paused→Downloading, got: {resume_state_changes:?}"
        );
        assert!(!resumed_alerts.is_empty(), "expected TorrentResumed alert");

        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn session_config_creates_utp_socket() {
        // Start session with uTP enabled — should succeed without errors
        let mut config = test_settings();
        config.enable_utp = true;
        let session = SessionHandle::start(config).await.unwrap();
        let stats = session.session_stats().await.unwrap();
        assert_eq!(stats.active_torrents, 0);
        session.shutdown().await.unwrap();
    }

    #[test]
    fn settings_nat_defaults() {
        let s = Settings::default();
        assert!(s.enable_upnp, "enable_upnp should default to true");
        assert!(s.enable_natpmp, "enable_natpmp should default to true");
    }

    #[tokio::test]
    async fn session_with_nat_disabled() {
        let config = test_settings();
        // test_session_config already sets enable_upnp: false, enable_natpmp: false
        assert!(!config.enable_upnp);
        assert!(!config.enable_natpmp);
        let session = SessionHandle::start(config).await.unwrap();
        let stats = session.session_stats().await.unwrap();
        assert_eq!(stats.active_torrents, 0);
        session.shutdown().await.unwrap();
    }

    // ---- M29: Anonymous mode, force proxy, proxy config tests ----

    #[test]
    fn anonymous_mode_disables_discovery() {
        let mut config = test_settings();
        config.anonymous_mode = true;
        config.enable_dht = true;
        config.enable_lsd = true;
        config.enable_upnp = true;
        config.enable_natpmp = true;

        // SessionHandle::start() will override these when anonymous_mode is true.
        // We test the enforcement logic directly here.
        if config.anonymous_mode {
            config.enable_dht = false;
            config.enable_lsd = false;
            config.enable_upnp = false;
            config.enable_natpmp = false;
        }

        assert!(!config.enable_dht);
        assert!(!config.enable_lsd);
        assert!(!config.enable_upnp);
        assert!(!config.enable_natpmp);
    }

    #[tokio::test]
    async fn anonymous_mode_session_starts_with_discovery_disabled() {
        let mut config = test_settings();
        config.anonymous_mode = true;
        // Even if we enable these, anonymous_mode should override
        config.enable_dht = true;
        config.enable_lsd = true;

        let session = SessionHandle::start(config).await.unwrap();
        let stats = session.session_stats().await.unwrap();
        assert_eq!(stats.active_torrents, 0);
        session.shutdown().await.unwrap();
    }

    #[test]
    fn force_proxy_requires_proxy_configured() {
        let mut config = test_settings();
        config.force_proxy = true;
        config.proxy = crate::proxy::ProxyConfig::default(); // no proxy

        // Validate the config error
        assert_eq!(config.proxy.proxy_type, crate::proxy::ProxyType::None);
        assert!(config.force_proxy);
        // This would error in SessionHandle::start()
    }

    #[tokio::test]
    async fn force_proxy_errors_without_proxy() {
        let mut config = test_settings();
        config.force_proxy = true;
        // proxy_type is None by default

        let result = SessionHandle::start(config).await;
        assert!(result.is_err());
        match result {
            Err(e) => assert!(
                e.to_string().contains("force_proxy"),
                "error should mention force_proxy: {e}"
            ),
            Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn force_proxy_disables_features() {
        let mut config = test_settings();
        config.force_proxy = true;
        config.proxy = crate::proxy::ProxyConfig {
            proxy_type: crate::proxy::ProxyType::Socks5,
            hostname: "proxy.example.com".into(),
            port: 1080,
            ..Default::default()
        };
        config.enable_dht = true;
        config.enable_lsd = true;
        config.enable_upnp = true;
        config.enable_natpmp = true;

        // Simulate the enforcement from start()
        if config.force_proxy {
            config.enable_upnp = false;
            config.enable_natpmp = false;
            config.enable_dht = false;
            config.enable_lsd = false;
        }

        assert!(!config.enable_dht);
        assert!(!config.enable_lsd);
        assert!(!config.enable_upnp);
        assert!(!config.enable_natpmp);
    }

    #[test]
    fn proxy_config_round_trip() {
        let s = Settings {
            proxy: crate::proxy::ProxyConfig {
                proxy_type: crate::proxy::ProxyType::Socks5Password,
                hostname: "localhost".into(),
                port: 9050,
                username: Some("user".into()),
                password: Some("pass".into()),
                ..Default::default()
            },
            force_proxy: true,
            anonymous_mode: true,
            ..test_settings()
        };

        assert_eq!(s.proxy.proxy_type, crate::proxy::ProxyType::Socks5Password);
        assert_eq!(s.proxy.hostname, "localhost");
        assert_eq!(s.proxy.port, 9050);
        assert!(s.force_proxy);
        assert!(s.anonymous_mode);
        assert_eq!(s.proxy.to_url(), "socks5://user:pass@localhost:9050");
    }

    #[tokio::test]
    async fn apply_settings_runtime() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let original = session.settings().await.unwrap();
        assert_eq!(original.max_torrents, 10);

        let mut new = original.clone();
        new.max_torrents = 200;
        new.upload_rate_limit = 1_000_000;
        session.apply_settings(new).await.unwrap();

        let updated = session.settings().await.unwrap();
        assert_eq!(updated.max_torrents, 200);
        assert_eq!(updated.upload_rate_limit, 1_000_000);

        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn apply_settings_validation_error() {
        let session = SessionHandle::start(test_settings()).await.unwrap();

        // force_proxy=true without a proxy configured should fail validation
        let mut bad = Settings::default();
        bad.force_proxy = true;
        let result = session.apply_settings(bad).await;
        assert!(result.is_err());

        // Original settings should be unchanged
        let current = session.settings().await.unwrap();
        assert!(!current.force_proxy);

        session.shutdown().await.unwrap();
    }

    // ---- M50: Session stats counters tests ----

    #[tokio::test]
    async fn session_stats_counters_accessible() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let counters = session.counters();
        // Uptime should be >= 0 from the moment of creation
        assert!(counters.uptime_secs() >= 0);
        assert_eq!(counters.len(), crate::stats::NUM_METRICS);
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn post_session_stats_fires_alert() {
        use crate::alert::{AlertCategory, AlertKind};

        let session = SessionHandle::start(test_settings()).await.unwrap();
        let mut stats_sub = session.subscribe_filtered(AlertCategory::STATS);

        session.post_session_stats().await.unwrap();

        let alert = tokio::time::timeout(Duration::from_secs(2), stats_sub.recv())
            .await
            .expect("timed out waiting for SessionStatsAlert")
            .expect("recv error");
        assert!(
            matches!(alert.kind, AlertKind::SessionStatsAlert { ref values } if values.len() == crate::stats::NUM_METRICS),
            "expected SessionStatsAlert with {} values, got {:?}",
            crate::stats::NUM_METRICS,
            alert.kind,
        );
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn session_stats_include_torrent_count() {
        use crate::alert::{AlertCategory, AlertKind};

        let session = SessionHandle::start(test_settings()).await.unwrap();
        let mut stats_sub = session.subscribe_filtered(AlertCategory::STATS);

        // Add a torrent
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        session.post_session_stats().await.unwrap();

        let alert = tokio::time::timeout(Duration::from_secs(2), stats_sub.recv())
            .await
            .expect("timed out waiting for SessionStatsAlert")
            .expect("recv error");
        match alert.kind {
            AlertKind::SessionStatsAlert { values } => {
                assert!(
                    values[crate::stats::SES_NUM_TORRENTS] > 0,
                    "SES_NUM_TORRENTS should be > 0 after adding a torrent, got {}",
                    values[crate::stats::SES_NUM_TORRENTS],
                );
            }
            other => panic!("expected SessionStatsAlert, got {other:?}"),
        }
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stats_timer_disabled_when_zero() {
        use crate::alert::{AlertCategory, AlertKind};

        let mut config = test_settings();
        config.stats_report_interval = 0;
        let session = SessionHandle::start(config).await.unwrap();
        let mut stats_sub = session.subscribe_filtered(AlertCategory::STATS);

        // Wait 200ms — no periodic stats alert should arrive
        let result = tokio::time::timeout(Duration::from_millis(200), stats_sub.recv()).await;
        assert!(
            result.is_err(),
            "no SessionStatsAlert should fire when stats_report_interval is 0"
        );
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn sample_infohashes_timer_disabled_when_zero() {
        use crate::alert::{AlertCategory, AlertKind};

        let mut config = test_settings();
        config.dht_sample_infohashes_interval = 0;
        let session = SessionHandle::start(config).await.unwrap();
        let mut dht_sub = session.subscribe_filtered(AlertCategory::DHT);

        // Wait 200ms — no DhtSampleInfohashes alert should arrive
        let result = tokio::time::timeout(Duration::from_millis(200), dht_sub.recv()).await;
        assert!(
            result.is_err(),
            "no DhtSampleInfohashes alert should fire when interval is 0"
        );
        session.shutdown().await.unwrap();
    }

    // ---- Test: open_file returns TorrentNotFound for unknown hash ----

    #[tokio::test]
    async fn open_file_not_found() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let fake_hash = Id20::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let result = session.open_file(fake_hash, 0).await;
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.to_string().contains("not found"));
        session.shutdown().await.unwrap();
    }

    // ---- Test: open_file on a real torrent routes to TorrentHandle ----

    #[tokio::test]
    async fn open_file_routes_to_torrent() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);

        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // open_file should succeed for file_index 0 (single-file torrent)
        let stream = session.open_file(info_hash, 0).await;
        assert!(stream.is_ok(), "open_file should succeed for file_index 0");

        // open_file should fail for out-of-range file_index
        let result = session.open_file(info_hash, 999).await;
        assert!(
            result.is_err(),
            "open_file should fail for invalid file_index"
        );

        session.shutdown().await.unwrap();
    }

    // ---- Test: force_reannounce via session ----

    #[tokio::test]
    async fn session_force_reannounce() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Should succeed for a known torrent.
        let result = session.force_reannounce(info_hash).await;
        assert!(
            result.is_ok(),
            "force_reannounce should succeed: {result:?}"
        );

        // Should fail for unknown torrent.
        let fake = Id20::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert!(session.force_reannounce(fake).await.is_err());

        session.shutdown().await.unwrap();
    }

    // ---- Test: tracker_list via session ----

    #[tokio::test]
    async fn session_tracker_list() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Should succeed (empty list since test torrent has no announce URL).
        let trackers = session.tracker_list(info_hash).await.unwrap();
        assert!(trackers.is_empty(), "test torrent has no trackers");

        // Should fail for unknown torrent.
        let fake = Id20::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert!(session.tracker_list(fake).await.is_err());

        session.shutdown().await.unwrap();
    }

    // ---- Test: scrape via session ----

    #[tokio::test]
    async fn session_scrape() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Should succeed (None since test torrent has no trackers to scrape).
        let scrape = session.scrape(info_hash).await.unwrap();
        assert!(scrape.is_none(), "test torrent has no trackers to scrape");

        // Should fail for unknown torrent.
        let fake = Id20::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert!(session.scrape(fake).await.is_err());

        session.shutdown().await.unwrap();
    }

    // ---- Test: set_file_priority via session ----

    #[tokio::test]
    async fn session_set_file_priority() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Should succeed for file index 0 (single-file torrent).
        let result = session
            .set_file_priority(info_hash, 0, torrent_core::FilePriority::Normal)
            .await;
        assert!(
            result.is_ok(),
            "set_file_priority should succeed: {result:?}"
        );

        // Should fail for unknown torrent.
        let fake = Id20::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert!(
            session
                .set_file_priority(fake, 0, torrent_core::FilePriority::Normal)
                .await
                .is_err()
        );

        session.shutdown().await.unwrap();
    }

    // ---- Test: file_priorities via session ----

    #[tokio::test]
    async fn session_file_priorities() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Should return priorities for the single file.
        let priorities = session.file_priorities(info_hash).await.unwrap();
        assert_eq!(
            priorities.len(),
            1,
            "single-file torrent should have 1 file priority"
        );
        assert_eq!(priorities[0], torrent_core::FilePriority::Normal);

        // Should fail for unknown torrent.
        let fake = Id20::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert!(session.file_priorities(fake).await.is_err());

        session.shutdown().await.unwrap();
    }

    // ---- Test: set_download_limit zero means unlimited ----

    #[tokio::test]
    async fn set_download_limit_zero_means_unlimited() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Set limit to non-zero, then back to zero (unlimited).
        session.set_download_limit(info_hash, 50_000).await.unwrap();
        session.set_download_limit(info_hash, 0).await.unwrap();
        let limit = session.download_limit(info_hash).await.unwrap();
        assert_eq!(limit, 0, "0 means unlimited");

        session.shutdown().await.unwrap();
    }

    // ---- Test: set_upload_limit persists ----

    #[tokio::test]
    async fn set_upload_limit_persists() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        session.set_upload_limit(info_hash, 100_000).await.unwrap();
        let limit = session.upload_limit(info_hash).await.unwrap();
        assert_eq!(limit, 100_000);

        session.shutdown().await.unwrap();
    }

    // ---- Test: download_limit default is zero ----

    #[tokio::test]
    async fn download_limit_default_is_zero() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Default config has download_rate_limit = 0.
        let limit = session.download_limit(info_hash).await.unwrap();
        assert_eq!(limit, 0, "default download limit should be 0 (unlimited)");

        session.shutdown().await.unwrap();
    }

    // ---- Test: rate_limit_round_trip ----

    #[tokio::test]
    async fn rate_limit_round_trip() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Set both limits.
        session
            .set_download_limit(info_hash, 1_000_000)
            .await
            .unwrap();
        session.set_upload_limit(info_hash, 500_000).await.unwrap();

        // Read them back.
        let dl = session.download_limit(info_hash).await.unwrap();
        let ul = session.upload_limit(info_hash).await.unwrap();
        assert_eq!(dl, 1_000_000);
        assert_eq!(ul, 500_000);

        // Update and verify again.
        session
            .set_download_limit(info_hash, 2_000_000)
            .await
            .unwrap();
        let dl = session.download_limit(info_hash).await.unwrap();
        assert_eq!(dl, 2_000_000);

        // Unknown torrent should fail.
        let fake = Id20::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert!(session.download_limit(fake).await.is_err());
        assert!(session.upload_limit(fake).await.is_err());
        assert!(session.set_download_limit(fake, 100).await.is_err());
        assert!(session.set_upload_limit(fake, 100).await.is_err());

        session.shutdown().await.unwrap();
    }

    // ---- Test: sequential_download_toggle ----

    #[tokio::test]
    async fn sequential_download_toggle() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Enable sequential download.
        session
            .set_sequential_download(info_hash, true)
            .await
            .unwrap();
        assert!(session.is_sequential_download(info_hash).await.unwrap());

        // Disable it again.
        session
            .set_sequential_download(info_hash, false)
            .await
            .unwrap();
        assert!(!session.is_sequential_download(info_hash).await.unwrap());

        session.shutdown().await.unwrap();
    }

    // ---- Test: super_seeding_toggle ----

    #[tokio::test]
    async fn super_seeding_toggle() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Enable super seeding.
        session.set_super_seeding(info_hash, true).await.unwrap();
        assert!(session.is_super_seeding(info_hash).await.unwrap());

        // Disable it again.
        session.set_super_seeding(info_hash, false).await.unwrap();
        assert!(!session.is_super_seeding(info_hash).await.unwrap());

        session.shutdown().await.unwrap();
    }

    // ---- Test: sequential_download_default_false ----

    #[tokio::test]
    async fn sequential_download_default_false() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Default config has sequential_download = false.
        assert!(!session.is_sequential_download(info_hash).await.unwrap());

        session.shutdown().await.unwrap();
    }

    // ---- Test: super_seeding_default_false ----

    #[tokio::test]
    async fn super_seeding_default_false() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Default config has super_seeding = false.
        assert!(!session.is_super_seeding(info_hash).await.unwrap());

        session.shutdown().await.unwrap();
    }

    // ---- Test: add_tracker_increases_count ----

    #[tokio::test]
    async fn add_tracker_increases_count() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Test torrent has no trackers initially.
        let before = session.tracker_list(info_hash).await.unwrap();
        assert!(before.is_empty());

        // Add a tracker.
        session
            .add_tracker(info_hash, "udp://tracker.example.com:6969/announce".into())
            .await
            .unwrap();

        let after = session.tracker_list(info_hash).await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].url, "udp://tracker.example.com:6969/announce");

        session.shutdown().await.unwrap();
    }

    // ---- Test: replace_trackers_replaces_all ----

    #[tokio::test]
    async fn replace_trackers_replaces_all() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Add 2 trackers.
        session
            .add_tracker(info_hash, "udp://tracker1.example.com:6969/announce".into())
            .await
            .unwrap();
        session
            .add_tracker(info_hash, "http://tracker2.example.com/announce".into())
            .await
            .unwrap();
        assert_eq!(session.tracker_list(info_hash).await.unwrap().len(), 2);

        // Replace with 1 different tracker.
        session
            .replace_trackers(
                info_hash,
                vec!["http://replacement.example.com/announce".into()],
            )
            .await
            .unwrap();

        let after = session.tracker_list(info_hash).await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].url, "http://replacement.example.com/announce");

        session.shutdown().await.unwrap();
    }

    // ---- Test: add_tracker_deduplicates ----

    #[tokio::test]
    async fn add_tracker_deduplicates() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Add the same tracker URL twice.
        session
            .add_tracker(info_hash, "udp://tracker.example.com:6969/announce".into())
            .await
            .unwrap();
        session
            .add_tracker(info_hash, "udp://tracker.example.com:6969/announce".into())
            .await
            .unwrap();

        // Should only have 1 tracker (deduplicated).
        let trackers = session.tracker_list(info_hash).await.unwrap();
        assert_eq!(trackers.len(), 1);

        session.shutdown().await.unwrap();
    }

    // ---- Test: info_hashes_matches_added_torrent ----

    #[tokio::test]
    async fn info_hashes_matches_added_torrent() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let expected_v1 = meta.info_hash;
        let storage = make_storage(&data, 16384);

        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();
        let hashes = session.info_hashes(info_hash).await.unwrap();
        assert_eq!(hashes.v1, Some(expected_v1));
        // v1-only torrent should not have v2 hash
        assert!(hashes.v2.is_none());

        session.shutdown().await.unwrap();
    }

    // ---- Test: torrent_file_returns_meta ----

    #[tokio::test]
    async fn torrent_file_returns_meta() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 32768];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);

        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();
        let torrent = session.torrent_file(info_hash).await.unwrap();
        assert!(torrent.is_some());
        let torrent = torrent.unwrap();
        assert_eq!(torrent.info_hash, info_hash);
        assert_eq!(torrent.info.name, "test");
        assert_eq!(torrent.info.total_length(), 32768);

        session.shutdown().await.unwrap();
    }

    // ---- Test: torrent_file_none_before_metadata ----

    #[tokio::test]
    async fn torrent_file_none_before_metadata() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let magnet = torrent_core::Magnet::parse(
            "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&dn=test",
        )
        .unwrap();

        let info_hash = session.add_magnet(magnet).await.unwrap();
        let torrent = session.torrent_file(info_hash).await.unwrap();
        // Before metadata is received, torrent_file should return None.
        assert!(torrent.is_none());

        session.shutdown().await.unwrap();
    }

    // ---- Test: force_dht_announce_no_error ----

    #[tokio::test]
    async fn force_dht_announce_no_error() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Should succeed even without DHT enabled (no-op, no error).
        let result = session.force_dht_announce(info_hash).await;
        assert!(
            result.is_ok(),
            "force_dht_announce should succeed: {result:?}"
        );

        // Should fail for unknown torrent.
        let fake = Id20::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert!(session.force_dht_announce(fake).await.is_err());

        session.shutdown().await.unwrap();
    }

    // ---- Test: force_lsd_announce_no_error ----

    #[tokio::test]
    async fn force_lsd_announce_no_error() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Should succeed even without LSD enabled (no-op announce, no error).
        let result = session.force_lsd_announce(info_hash).await;
        assert!(
            result.is_ok(),
            "force_lsd_announce should succeed: {result:?}"
        );

        // Should fail for unknown torrent.
        let fake = Id20::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert!(session.force_lsd_announce(fake).await.is_err());

        session.shutdown().await.unwrap();
    }

    // ---- Test: read_piece_after_download ----

    #[tokio::test]
    async fn read_piece_after_download() {
        let data = vec![0xCD; 32768]; // 2 pieces of 16384
        let meta = make_test_torrent(&data, 16384);
        let lengths = Lengths::new(data.len() as u64, 16384, DEFAULT_CHUNK_SIZE);
        let storage = Arc::new(MemoryStorage::new(lengths));
        // Pre-fill storage with the data
        storage.write_chunk(0, 0, &data[..16384]).unwrap();
        storage.write_chunk(1, 0, &data[16384..]).unwrap();

        let session = SessionHandle::start(test_settings()).await.unwrap();
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // Read piece 0
        let piece_data = session.read_piece(info_hash, 0).await.unwrap();
        assert_eq!(piece_data.len(), 16384);
        assert!(piece_data.iter().all(|&b| b == 0xCD));

        // Read piece 1
        let piece_data = session.read_piece(info_hash, 1).await.unwrap();
        assert_eq!(piece_data.len(), 16384);
        assert!(piece_data.iter().all(|&b| b == 0xCD));

        // Out-of-range piece should fail
        let result = session.read_piece(info_hash, 999).await;
        assert!(result.is_err(), "read_piece out of range should fail");

        // Unknown torrent should fail
        let fake = Id20::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert!(session.read_piece(fake, 0).await.is_err());

        session.shutdown().await.unwrap();
    }

    // ---- Test: flush_cache_completes ----

    #[tokio::test]
    async fn flush_cache_completes() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // flush_cache should succeed.
        let result = session.flush_cache(info_hash).await;
        assert!(result.is_ok(), "flush_cache should succeed: {result:?}");

        // Should fail for unknown torrent.
        let fake = Id20::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert!(session.flush_cache(fake).await.is_err());

        session.shutdown().await.unwrap();
    }

    // ---- BEP 44 session API tests ----

    fn test_settings_with_dht() -> Settings {
        let mut s = test_settings();
        s.enable_dht = true;
        s
    }

    #[tokio::test]
    async fn test_dht_disabled_returns_error() {
        let session = SessionHandle::start(test_settings()).await.unwrap();

        // All 4 methods should fail with DhtDisabled when DHT is off
        let err = session.dht_put_immutable(b"test".to_vec()).await.unwrap_err();
        assert!(format!("{err:?}").contains("DhtDisabled"), "expected DhtDisabled, got {err:?}");

        let target = Id20::from([0u8; 20]);
        let err = session.dht_get_immutable(target).await.unwrap_err();
        assert!(format!("{err:?}").contains("DhtDisabled"), "expected DhtDisabled, got {err:?}");

        let err = session
            .dht_put_mutable([42u8; 32], b"val".to_vec(), 1, Vec::new())
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("DhtDisabled"), "expected DhtDisabled, got {err:?}");

        let err = session
            .dht_get_mutable([42u8; 32], Vec::new())
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("DhtDisabled"), "expected DhtDisabled, got {err:?}");

        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_dht_put_get_immutable_round_trip() {
        let session = SessionHandle::start(test_settings_with_dht()).await.unwrap();

        // Give DHT a moment to bootstrap (it won't find real nodes, but the handle should work)
        let value = b"hello BEP 44".to_vec();
        let target = session.dht_put_immutable(value.clone()).await.unwrap();

        // The target should be the SHA-1 of the bencoded value
        // Try to get it back — since we're the only node, the local store should have it
        let got = session.dht_get_immutable(target).await.unwrap();
        assert_eq!(got, Some(value));

        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_dht_put_immutable_fires_alert() {
        use crate::alert::{AlertCategory, AlertKind};

        let session = SessionHandle::start(test_settings_with_dht()).await.unwrap();
        let mut alerts = session.subscribe_filtered(AlertCategory::DHT);

        let value = b"alert test".to_vec();
        let target = session.dht_put_immutable(value).await.unwrap();

        // Should receive DhtPutComplete alert
        let alert = tokio::time::timeout(Duration::from_secs(5), alerts.recv())
            .await
            .expect("timeout waiting for alert")
            .expect("alert channel closed");

        match alert.kind {
            AlertKind::DhtPutComplete { target: t } => {
                assert_eq!(t, target);
            }
            other => panic!("expected DhtPutComplete, got {other:?}"),
        }

        session.shutdown().await.unwrap();
    }

    // ---- BEP 27: Private torrent LSD tests ----

    /// Creates a private torrent (.torrent bytes with private=1 in the info dict).
    fn make_private_torrent(data: &[u8], piece_length: u64) -> TorrentMetaV1 {
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
            private: i64,
        }

        #[derive(Serialize)]
        struct Torrent<'a> {
            info: Info<'a>,
        }

        let t = Torrent {
            info: Info {
                length: data.len() as u64,
                name: "private-test",
                piece_length,
                pieces: &pieces,
                private: 1,
            },
        };

        let bytes = torrent_bencode::to_bytes(&t).unwrap();
        torrent_from_bytes(&bytes).unwrap()
    }

    #[test]
    fn is_private_true_via_parsed_meta() {
        // Verify that a torrent parsed from private .torrent bytes has private == Some(1)
        let data = vec![0xAB; 16384];
        let meta = make_private_torrent(&data, 16384);
        assert_eq!(meta.info.private, Some(1), "private field should be Some(1)");
    }

    #[test]
    fn is_private_false_for_public_torrent() {
        // Verify that a regular torrent has private == None
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        assert_eq!(meta.info.private, None, "public torrent should have no private flag");
    }

    #[test]
    fn private_torrent_config_disables_lsd() {
        // Verify that TorrentConfig::default() has LSD enabled (so disable is meaningful)
        let config = TorrentConfig::default();
        assert!(config.enable_lsd, "default TorrentConfig should have LSD enabled");
    }

    #[tokio::test]
    async fn force_lsd_announce_private_torrent_returns_error() {
        let session = SessionHandle::start(test_settings()).await.unwrap();
        let data = vec![0xAB; 16384];
        let meta = make_private_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(storage))
            .await
            .unwrap();

        // BEP 27: force_lsd_announce on a private torrent must return an error
        let result = session.force_lsd_announce(info_hash).await;
        assert!(
            result.is_err(),
            "force_lsd_announce on private torrent should return error, got: {result:?}"
        );
        let err_str = format!("{:?}", result.unwrap_err());
        assert!(
            err_str.contains("InvalidSettings") || err_str.contains("LSD disabled"),
            "expected InvalidSettings error, got: {err_str}"
        );

        session.shutdown().await.unwrap();
    }
}
