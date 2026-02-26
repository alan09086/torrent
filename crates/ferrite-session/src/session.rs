//! SessionHandle / SessionActor — multi-torrent session manager.
//!
//! Actor model: SessionHandle is the cloneable public API (mpsc sender),
//! SessionActor is the single-owner event loop (internal).

use std::collections::HashMap;
use std::net::SocketAddr;
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
use crate::types::{
    FileInfo, SessionConfig, SessionStats, TorrentConfig, TorrentInfo, TorrentStats,
};

/// Shared global rate limiter bucket.
type SharedBucket = Arc<std::sync::Mutex<crate::rate_limiter::TokenBucket>>;

/// Entry for a torrent managed by the session.
struct TorrentEntry {
    handle: TorrentHandle,
    meta: Option<TorrentMetaV1>,
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
    /// Start a new session with the given configuration.
    pub async fn start(config: SessionConfig) -> crate::Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);

        // Alert broadcast channel
        let (alert_tx, _) = broadcast::channel(config.alert_channel_size);
        let alert_mask = Arc::new(AtomicU32::new(config.alert_mask.bits()));

        let (lsd, lsd_peers_rx) = if config.enable_lsd {
            match crate::lsd::LsdHandle::start(config.listen_port).await {
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
            crate::rate_limiter::TokenBucket::new(config.upload_rate_limit),
        ));
        let global_download_bucket = Arc::new(std::sync::Mutex::new(
            crate::rate_limiter::TokenBucket::new(config.download_rate_limit),
        ));

        let actor = SessionActor {
            config,
            torrents: HashMap::new(),
            dht: None,
            lsd,
            lsd_peers_rx,
            cmd_rx,
            alert_tx: alert_tx.clone(),
            alert_mask: Arc::clone(&alert_mask),
            global_upload_bucket,
            global_download_bucket,
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
}

// ---------------------------------------------------------------------------
// SessionActor — internal single-owner event loop
// ---------------------------------------------------------------------------

struct SessionActor {
    config: SessionConfig,
    torrents: HashMap<Id20, TorrentEntry>,
    dht: Option<DhtHandle>,
    lsd: Option<crate::lsd::LsdHandle>,
    lsd_peers_rx: Option<mpsc::Receiver<(Id20, SocketAddr)>>,
    cmd_rx: mpsc::Receiver<SessionCommand>,
    alert_tx: broadcast::Sender<Alert>,
    alert_mask: Arc<AtomicU32>,
    global_upload_bucket: SharedBucket,
    global_download_bucket: SharedBucket,
}

impl SessionActor {
    async fn run(mut self) {
        let mut refill_interval = tokio::time::interval(std::time::Duration::from_millis(100));
        refill_interval.tick().await; // skip first immediate tick

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
                        let _ = entry.handle.add_peers(vec![peer_addr]).await;
                    }
                }
                // Global rate limiter refill (100ms)
                _ = refill_interval.tick() => {
                    let elapsed = std::time::Duration::from_millis(100);
                    self.global_upload_bucket.lock().unwrap().refill(elapsed);
                    self.global_download_bucket.lock().unwrap().refill(elapsed);
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
        let up = if self.config.upload_rate_limit > 0 {
            Some(Arc::clone(&self.global_upload_bucket))
        } else {
            None
        };
        let down = if self.config.download_rate_limit > 0 {
            Some(Arc::clone(&self.global_download_bucket))
        } else {
            None
        };
        (up, down)
    }

    fn make_slot_tuner(&self) -> crate::slot_tuner::SlotTuner {
        if self.config.auto_upload_slots {
            crate::slot_tuner::SlotTuner::new(
                4, // initial slots
                self.config.auto_upload_slots_min,
                self.config.auto_upload_slots_max,
            )
        } else {
            crate::slot_tuner::SlotTuner::disabled(4)
        }
    }

    fn make_torrent_config(&self) -> TorrentConfig {
        TorrentConfig {
            listen_port: 0, // Each torrent gets a random port
            max_peers: 50,
            target_request_queue: 5,
            download_dir: self.config.download_dir.clone(),
            enable_dht: self.config.enable_dht,
            enable_pex: self.config.enable_pex,
            enable_fast: self.config.enable_fast_extension,
            seed_ratio_limit: self.config.seed_ratio_limit,
            strict_end_game: true,
            upload_rate_limit: 0,
            download_rate_limit: 0,
        }
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

        if self.torrents.len() >= self.config.max_torrents {
            return Err(crate::Error::SessionAtCapacity(self.config.max_torrents));
        }

        let torrent_config = self.make_torrent_config();

        let storage = match storage {
            Some(s) => s,
            None => {
                // Default: create in-memory storage
                let lengths = Lengths::new(
                    meta.info.total_length(),
                    meta.info.piece_length,
                    DEFAULT_CHUNK_SIZE,
                );
                Arc::new(ferrite_storage::MemoryStorage::new(lengths))
            }
        };

        let (global_up, global_down) = self.global_buckets_if_limited();
        let slot_tuner = self.make_slot_tuner();

        let handle = TorrentHandle::from_torrent(
            meta.clone(),
            storage,
            torrent_config,
            self.dht.clone(),
            global_up,
            global_down,
            slot_tuner,
            self.alert_tx.clone(),
            Arc::clone(&self.alert_mask),
        )
        .await?;

        let name = meta.info.name.clone();
        self.torrents.insert(
            info_hash,
            TorrentEntry {
                handle,
                meta: Some(meta),
            },
        );

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
        if self.torrents.len() >= self.config.max_torrents {
            return Err(crate::Error::SessionAtCapacity(self.config.max_torrents));
        }
        let config = self.make_torrent_config();
        let (global_up, global_down) = self.global_buckets_if_limited();
        let slot_tuner = self.make_slot_tuner();
        let handle = TorrentHandle::from_magnet(
            magnet,
            config,
            self.dht.clone(),
            global_up,
            global_down,
            slot_tuner,
            self.alert_tx.clone(),
            Arc::clone(&self.alert_mask),
        )
        .await?;
        self.torrents.insert(
            info_hash,
            TorrentEntry {
                handle,
                meta: None,
            },
        );
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
        entry.handle.shutdown().await?;
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
            dht_nodes: 0, // DHT not yet wired
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
        entry.handle.save_resume_data().await
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

        Ok(SessionState {
            dht_nodes: Vec::new(), // DHT node cache — populated when DHT is wired
            torrents,
        })
    }

    async fn shutdown_all(&mut self) {
        for (info_hash, entry) in self.torrents.drain() {
            debug!(%info_hash, "shutting down torrent");
            let _ = entry.handle.shutdown().await;
        }
        if let Some(ref lsd) = self.lsd {
            lsd.shutdown().await;
        }
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

    fn test_session_config() -> SessionConfig {
        SessionConfig {
            listen_port: 0,
            download_dir: PathBuf::from("/tmp"),
            max_torrents: 10,
            enable_dht: false,
            enable_pex: false,
            enable_lsd: false,
            enable_fast_extension: false,
            seed_ratio_limit: None,
            resume_data_dir: None,
            upload_rate_limit: 0,
            download_rate_limit: 0,
            auto_upload_slots: true,
            auto_upload_slots_min: 2,
            auto_upload_slots_max: 20,
            alert_mask: crate::alert::AlertCategory::ALL,
            alert_channel_size: 64,
        }
    }

    // ---- Test 1: Start and shutdown ----

    #[tokio::test]
    async fn session_start_and_shutdown() {
        let session = SessionHandle::start(test_session_config()).await.unwrap();
        let stats = session.session_stats().await.unwrap();
        assert_eq!(stats.active_torrents, 0);
        session.shutdown().await.unwrap();
    }

    // ---- Test 2: Add and list torrent ----

    #[tokio::test]
    async fn add_and_list_torrent() {
        let session = SessionHandle::start(test_session_config()).await.unwrap();
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
        let session = SessionHandle::start(test_session_config()).await.unwrap();
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
        let session = SessionHandle::start(test_session_config()).await.unwrap();
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
        let mut config = test_session_config();
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
        let session = SessionHandle::start(test_session_config()).await.unwrap();
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
        let session = SessionHandle::start(test_session_config()).await.unwrap();
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
        let session = SessionHandle::start(test_session_config()).await.unwrap();
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
        let session = SessionHandle::start(test_session_config()).await.unwrap();
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
        let session = SessionHandle::start(test_session_config()).await.unwrap();

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

        let session = SessionHandle::start(test_session_config()).await.unwrap();
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

        let session = SessionHandle::start(test_session_config()).await.unwrap();
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
        let mut config = test_session_config();
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
        let session = SessionHandle::start(test_session_config()).await.unwrap();
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
        let session = SessionHandle::start(test_session_config()).await.unwrap();

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
        let session = SessionHandle::start(test_session_config()).await.unwrap();
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

        let session = SessionHandle::start(test_session_config()).await.unwrap();
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

        let session = SessionHandle::start(test_session_config()).await.unwrap();
        let mut alerts = session.subscribe();

        let data = vec![0xAB; 16384];
        let meta = make_test_torrent(&data, 16384);
        let storage = make_storage(&data, 16384);
        let info_hash = session.add_torrent(meta, Some(storage)).await.unwrap();

        // Drain TorrentAdded
        let _ = tokio::time::timeout(Duration::from_secs(1), alerts.recv())
            .await
            .unwrap();

        session.remove_torrent(info_hash).await.unwrap();

        let alert = tokio::time::timeout(Duration::from_secs(2), alerts.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(alert.kind, AlertKind::TorrentRemoved { .. }));
        session.shutdown().await.unwrap();
    }
}
