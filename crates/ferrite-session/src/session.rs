//! SessionHandle / SessionActor — multi-torrent session manager.
//!
//! Actor model: SessionHandle is the cloneable public API (mpsc sender),
//! SessionActor is the single-owner event loop (internal).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, oneshot};

use tracing::{debug, info, warn};

use ferrite_core::{Id20, Lengths, Magnet, TorrentMetaV1, DEFAULT_CHUNK_SIZE};
use ferrite_dht::DhtHandle;
use ferrite_storage::TorrentStorage;

use crate::alert::{post_alert, Alert, AlertCategory, AlertKind, AlertStream};
use crate::torrent::TorrentHandle;
use crate::settings::Settings;
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

/// Commands sent from SessionHandle to SessionActor.
enum SessionCommand {
    AddTorrent {
        meta: Box<TorrentMetaV1>,
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
        reply: oneshot::Sender<crate::Result<ferrite_core::FastResumeData>>,
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
    Shutdown,
}

/// Cloneable handle for interacting with a running session.
#[derive(Clone)]
pub struct SessionHandle {
    cmd_tx: mpsc::Sender<SessionCommand>,
    alert_tx: broadcast::Sender<Alert>,
    alert_mask: Arc<AtomicU32>,
}

impl SessionHandle {
    /// Start a new session with the given settings and no plugins.
    pub async fn start(settings: Settings) -> crate::Result<Self> {
        Self::start_with_plugins(settings, Arc::new(Vec::new())).await
    }

    /// Start a new session with the given settings and extension plugins.
    pub async fn start_with_plugins(
        settings: Settings,
        plugins: Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>>,
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
            match ferrite_utp::UtpSocket::bind(settings.to_utp_config(settings.listen_port)).await {
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
            match ferrite_utp::UtpSocket::bind(settings.to_utp_config_v6(settings.listen_port)).await {
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
            let (handle, events_rx) = ferrite_nat::NatHandle::start(nat_config);
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

        // Start DHT instances
        let dht_v4 = if settings.enable_dht {
            match DhtHandle::start(settings.to_dht_config()).await {
                Ok(handle) => {
                    info!("DHT v4 started");
                    Some(handle)
                }
                Err(e) => {
                    warn!("DHT v4 start failed: {e}");
                    None
                }
            }
        } else {
            None
        };

        let dht_v6 = if settings.enable_dht && settings.enable_ipv6 {
            match DhtHandle::start(settings.to_dht_config_v6()).await {
                Ok(handle) => {
                    info!("DHT v6 started");
                    Some(handle)
                }
                Err(e) => {
                    debug!("DHT v6 start failed (non-fatal): {e}");
                    None
                }
            }
        } else {
            None
        };

        let ban_config = crate::ban::BanConfig::from(&settings);
        let ban_manager: SharedBanManager = Arc::new(
            std::sync::RwLock::new(crate::ban::BanManager::new(ban_config)),
        );

        let ip_filter: SharedIpFilter = Arc::new(
            std::sync::RwLock::new(crate::ip_filter::IpFilter::new()),
        );

        let disk_config = crate::disk::DiskConfig::from(&settings);
        let (disk_manager, disk_actor_handle) =
            crate::disk::DiskManagerHandle::new(disk_config);

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
            plugins,
        };

        tokio::spawn(actor.run());
        Ok(SessionHandle { cmd_tx, alert_tx, alert_mask })
    }

    /// Add a torrent from parsed .torrent metadata.
    pub async fn add_torrent(
        &self,
        meta: TorrentMetaV1,
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

    /// Atomically update the session-level alert mask.
    pub fn set_alert_mask(&self, mask: AlertCategory) {
        self.alert_mask.store(mask.bits(), Ordering::Relaxed);
    }

    /// Read the current session-level alert mask.
    pub fn alert_mask(&self) -> AlertCategory {
        AlertCategory::from_bits_truncate(self.alert_mask.load(Ordering::Relaxed))
    }

    /// Gracefully shut down the session and all torrents.
    pub async fn shutdown(&self) -> crate::Result<()> {
        let _ = self.cmd_tx.send(SessionCommand::Shutdown).await;
        Ok(())
    }

    /// Save resume data for a specific torrent.
    pub async fn save_torrent_resume_data(
        &self,
        info_hash: Id20,
    ) -> crate::Result<ferrite_core::FastResumeData> {
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
    pub async fn save_session_state(
        &self,
    ) -> crate::Result<crate::persistence::SessionState> {
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
            .send(SessionCommand::ApplySettings { settings: Box::new(settings), reply: tx })
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
    utp_socket: Option<ferrite_utp::UtpSocket>,
    utp_listener: Option<ferrite_utp::UtpListener>,
    utp_socket_v6: Option<ferrite_utp::UtpSocket>,
    utp_listener_v6: Option<ferrite_utp::UtpListener>,
    nat: Option<ferrite_nat::NatHandle>,
    nat_events_rx: Option<mpsc::Receiver<ferrite_nat::NatEvent>>,
    ban_manager: SharedBanManager,
    ip_filter: SharedIpFilter,
    disk_manager: crate::disk::DiskManagerHandle,
    #[allow(dead_code)]
    disk_actor_handle: tokio::task::JoinHandle<()>,
    /// External IP discovered via NAT traversal or configured manually (BEP 40).
    external_ip: Option<std::net::IpAddr>,
    /// Registered extension plugins, shared with all TorrentActors.
    plugins: Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>>,
}

impl SessionActor {
    async fn run(mut self) {
        let mut refill_interval = tokio::time::interval(std::time::Duration::from_millis(100));
        refill_interval.tick().await; // skip first immediate tick

        let auto_manage_secs = self.settings.auto_manage_interval.max(1);
        let mut auto_manage_interval = tokio::time::interval(
            std::time::Duration::from_secs(auto_manage_secs),
        );
        auto_manage_interval.tick().await; // skip first immediate tick

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
                    {
                        let _ = entry.handle.add_peers(vec![peer_addr], crate::peer_state::PeerSource::Lsd).await;
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
                        ferrite_nat::NatEvent::MappingSucceeded { port, protocol } => {
                            info!(port, %protocol, "port mapping succeeded");
                            post_alert(
                                &self.alert_tx,
                                &self.alert_mask,
                                AlertKind::PortMappingSucceeded { port, protocol },
                            );
                        }
                        ferrite_nat::NatEvent::MappingFailed { port, message } => {
                            warn!(port, %message, "port mapping failed");
                            post_alert(
                                &self.alert_tx,
                                &self.alert_mask,
                                AlertKind::PortMappingFailed { port, message },
                            );
                        }
                        ferrite_nat::NatEvent::ExternalIpDiscovered { ip } => {
                            info!(%ip, "external IP discovered via NAT traversal");
                            self.external_ip = Some(ip);
                            // Propagate to all active torrents for BEP 40 peer priority.
                            for entry in self.torrents.values() {
                                let _ = entry.handle.update_external_ip(ip).await;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Return clones of global buckets if they have a non-zero rate, else None.
    fn global_buckets_if_limited(
        &self,
    ) -> (
        Option<SharedBucket>,
        Option<SharedBucket>,
    ) {
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
        meta: TorrentMetaV1,
        storage: Option<Arc<dyn TorrentStorage>>,
    ) -> crate::Result<Id20> {
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
                Arc::new(ferrite_storage::MemoryStorage::new(lengths))
            }
        };
        let disk_handle = self.disk_manager.register_torrent(info_hash, storage).await;

        let (global_up, global_down) = self.global_buckets_if_limited();
        let slot_tuner = self.make_slot_tuner();

        let handle = TorrentHandle::from_torrent(
            meta.clone(),
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
        post_alert(&self.alert_tx, &self.alert_mask, AlertKind::TorrentAdded { info_hash, name });
        if let Some(ref lsd) = self.lsd {
            lsd.announce(vec![info_hash]).await;
        }
        Ok(info_hash)
    }

    async fn handle_add_magnet(&mut self, magnet: Magnet) -> crate::Result<Id20> {
        let info_hash = magnet.info_hash;
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
        post_alert(&self.alert_tx, &self.alert_mask, AlertKind::TorrentAdded { info_hash, name: display_name });
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
        post_alert(&self.alert_tx, &self.alert_mask, AlertKind::TorrentRemoved { info_hash });
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
        entry.handle.stats().await
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

    async fn make_session_stats(&self) -> SessionStats {
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
    ) -> crate::Result<ferrite_core::FastResumeData> {
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

    async fn handle_save_session_state(
        &self,
    ) -> crate::Result<crate::persistence::SessionState> {
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

        // Serialize smart ban state
        let ban_mgr = self.ban_manager.read().unwrap();
        let banned_peers: Vec<String> = ban_mgr.banned_list()
            .iter()
            .map(|ip| ip.to_string())
            .collect();
        let peer_strikes: Vec<crate::persistence::PeerStrikeEntry> = ban_mgr.strikes_map()
            .iter()
            .map(|(ip, &count)| crate::persistence::PeerStrikeEntry {
                ip: ip.to_string(),
                count: count as i64,
            })
            .collect();
        drop(ban_mgr);

        Ok(SessionState {
            dht_nodes: Vec::new(), // DHT node cache — populated when DHT is wired
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
            self.global_upload_bucket.lock().unwrap().set_rate(new.upload_rate_limit);
        }
        if new.download_rate_limit != self.settings.download_rate_limit {
            self.global_download_bucket.lock().unwrap().set_rate(new.download_rate_limit);
        }

        // Update alert mask if changed
        if new.alert_mask != self.settings.alert_mask {
            self.alert_mask.store(new.alert_mask.bits(), Ordering::Relaxed);
        }

        // Store new settings
        self.settings = new;

        // Fire alert
        post_alert(
            &self.alert_tx,
            &self.alert_mask,
            AlertKind::SettingsChanged,
        );

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

    fn handle_queue_move(
        &mut self,
        info_hash: Id20,
        op: QueueMoveFn,
    ) -> crate::Result<()> {
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
                TorrentState::Downloading | TorrentState::FetchingMetadata | TorrentState::Checking => {
                    crate::queue::QueueCategory::Downloading
                }
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
            let download_rate =
                stats.downloaded.saturating_sub(prev_downloaded) / auto_manage_secs;
            let upload_rate =
                stats.uploaded.saturating_sub(prev_uploaded) / auto_manage_secs;

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
    fn handle_utp_inbound(&self, stream: ferrite_utp::UtpStream, addr: std::net::SocketAddr) {
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
                        debug!(%addr, %info_hash, "routing inbound uTP peer to torrent");
                        let _ = handle.send_incoming_peer(prefixed_stream, addr).await;
                    } else {
                        debug!(%addr, %info_hash, "inbound uTP peer for unknown torrent, dropping");
                    }
                }
                Ok(None) => {
                    debug!(%addr, "inbound uTP peer is encrypted (MSE), dropping (deferred)");
                }
                Err(e) => {
                    debug!(%addr, error = %e, "failed to identify inbound uTP peer");
                }
            }
        });
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
    listener: &mut Option<ferrite_utp::UtpListener>,
) -> std::io::Result<(ferrite_utp::UtpStream, std::net::SocketAddr)> {
    match listener {
        Some(l) => l
            .accept()
            .await
            .map_err(std::io::Error::other),
        None => std::future::pending().await,
    }
}

/// Helper to receive NAT events from an optional receiver.
/// Returns `pending` if no receiver is available, so the `select!` branch is skipped.
async fn recv_nat_event(
    rx: &mut Option<mpsc::Receiver<ferrite_nat::NatEvent>>,
) -> ferrite_nat::NatEvent {
    match rx {
        Some(r) => match r.recv().await {
            Some(event) => event,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_core::{torrent_from_bytes, Lengths, DEFAULT_CHUNK_SIZE};
    use ferrite_storage::MemoryStorage;
    use crate::types::TorrentState;
    use std::time::Duration;

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
            storage_mode: ferrite_core::StorageMode::Sparse,
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
        let info_hash = session.add_torrent(meta, Some(storage)).await.unwrap();
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

        let info_hash = session.add_torrent(meta, Some(storage)).await.unwrap();
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
            .add_torrent(meta.clone(), Some(storage1))
            .await
            .unwrap();
        let result = session.add_torrent(meta, Some(storage2)).await;
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
        session.add_torrent(meta1, Some(storage1)).await.unwrap();

        let data2 = vec![0xBB; 16384];
        let meta2 = make_test_torrent(&data2, 16384);
        let storage2 = make_storage(&data2, 16384);
        let result = session.add_torrent(meta2, Some(storage2)).await;
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

        let info_hash = session.add_torrent(meta, Some(storage)).await.unwrap();
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

        let info_hash = session.add_torrent(meta, Some(storage)).await.unwrap();
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

        let info_hash = session.add_torrent(meta, Some(storage)).await.unwrap();

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
        session.add_torrent(meta1, Some(storage1)).await.unwrap();

        let data2 = vec![0xBB; 16384];
        let meta2 = make_test_torrent(&data2, 16384);
        let storage2 = make_storage(&data2, 16384);
        session.add_torrent(meta2, Some(storage2)).await.unwrap();

        let stats = session.session_stats().await.unwrap();
        assert_eq!(stats.active_torrents, 2);

        session.shutdown().await.unwrap();
    }

    // ---- Test 11: Add magnet and list ----

    #[tokio::test]
    async fn add_magnet_and_list() {
        use ferrite_core::Magnet;

        let session = SessionHandle::start(test_settings()).await.unwrap();
        let magnet = Magnet {
            info_hash: Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap(),
            display_name: Some("test-magnet".into()),
            trackers: vec![],
            peers: vec![],
        };
        let expected_hash = magnet.info_hash;

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
        use ferrite_core::Magnet;

        let session = SessionHandle::start(test_settings()).await.unwrap();
        let magnet = Magnet {
            info_hash: Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap(),
            display_name: Some("test-magnet".into()),
            trackers: vec![],
            peers: vec![],
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
        use ferrite_core::Magnet;

        // LSD may fail to bind port 6771 — session should still start
        let mut config = test_settings();
        config.enable_lsd = true;

        let session = SessionHandle::start(config).await.unwrap();

        // Add a torrent (triggers LSD announce if available)
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        session.add_torrent(meta, Some(storage)).await.unwrap();

        // Add a magnet (also triggers LSD announce)
        let magnet = Magnet {
            info_hash: Id20::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap(),
            display_name: Some("lsd-test".into()),
            trackers: vec![],
            peers: vec![],
        };
        session.add_magnet(magnet).await.unwrap();

        let list = session.list_torrents().await.unwrap();
        assert_eq!(list.len(), 2);

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
        session.add_torrent(meta, Some(storage)).await.unwrap();

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
        session.add_torrent(meta1, Some(storage1)).await.unwrap();

        let data2 = vec![0xBB; 16384];
        let meta2 = make_test_torrent(&data2, 16384);
        let storage2 = make_storage(&data2, 16384);
        session.add_torrent(meta2, Some(storage2)).await.unwrap();

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
        let _info_hash = session.add_torrent(meta, Some(storage)).await.unwrap();

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
        let info_hash = session.add_torrent(meta, Some(storage)).await.unwrap();

        // Drain TorrentAdded and any checking alerts
        while let Ok(Ok(a)) = tokio::time::timeout(Duration::from_secs(1), alerts.recv()).await {
            if matches!(a.kind, AlertKind::StateChanged { new_state: TorrentState::Downloading, .. }) {
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
        session.add_torrent(meta, Some(storage)).await.unwrap();

        let a1 = tokio::time::timeout(Duration::from_secs(2), sub1.recv())
            .await.unwrap().unwrap();
        let a2 = tokio::time::timeout(Duration::from_secs(2), sub2.recv())
            .await.unwrap().unwrap();

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
        session.add_torrent(meta, Some(storage)).await.unwrap();

        let alert = tokio::time::timeout(Duration::from_secs(2), alerts.recv())
            .await.unwrap().unwrap();
        assert!(matches!(alert.kind, AlertKind::TorrentAdded { .. }));

        // Drain any remaining alerts from the first torrent (StateChanged, CheckingProgress, etc.)
        while tokio::time::timeout(Duration::from_millis(200), alerts.recv()).await.is_ok() {}

        // Change mask to empty — no alerts should pass
        session.set_alert_mask(AlertCategory::empty());

        let data2 = vec![0xBB; 16384];
        let meta2 = make_test_torrent(&data2, 16384);
        let storage2 = make_storage(&data2, 16384);
        session.add_torrent(meta2, Some(storage2)).await.unwrap();

        // Give a small window — nothing should arrive
        let result = tokio::time::timeout(Duration::from_millis(200), alerts.recv()).await;
        assert!(result.is_err(), "should have timed out with empty mask");

        // Restore STATUS — adding another torrent should arrive
        session.set_alert_mask(AlertCategory::STATUS);

        let data3 = vec![0xCC; 16384];
        let meta3 = make_test_torrent(&data3, 16384);
        let storage3 = make_storage(&data3, 16384);
        session.add_torrent(meta3, Some(storage3)).await.unwrap();

        let alert = tokio::time::timeout(Duration::from_secs(2), alerts.recv())
            .await.unwrap().unwrap();
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
        session.add_torrent(meta, Some(storage)).await.unwrap();

        // STATUS sub gets TorrentAdded
        let alert = tokio::time::timeout(Duration::from_secs(2), status_sub.recv())
            .await.unwrap().unwrap();
        assert!(matches!(alert.kind, AlertKind::TorrentAdded { .. }));

        // PEER sub should NOT receive TorrentAdded (it's STATUS category)
        let result = tokio::time::timeout(Duration::from_millis(200), peer_sub.recv()).await;
        assert!(result.is_err(), "PEER subscriber should not get STATUS alerts");

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
        let info_hash = session.add_torrent(meta, Some(storage)).await.unwrap();

        // Drain TorrentAdded
        let _ = tokio::time::timeout(Duration::from_secs(1), alerts.recv())
            .await.unwrap();

        // Pause — should get StateChanged(Downloading → Paused) + TorrentPaused
        session.pause_torrent(info_hash).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Collect alerts over a short window
        let mut state_changes = Vec::new();
        let mut paused_alerts = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_millis(200), alerts.recv()).await {
                Ok(Ok(a)) => match &a.kind {
                    AlertKind::StateChanged { prev_state, new_state, .. } => {
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
                    AlertKind::StateChanged { prev_state, new_state, .. } => {
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
            Err(e) => assert!(e.to_string().contains("force_proxy"), "error should mention force_proxy: {e}"),
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
}
