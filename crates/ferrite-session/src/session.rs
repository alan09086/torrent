//! SessionHandle / SessionActor — multi-torrent session manager.
//!
//! Actor model: SessionHandle is the cloneable public API (mpsc sender),
//! SessionActor is the single-owner event loop (internal).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info};

use ferrite_core::{Id20, Lengths, TorrentMetaV1, DEFAULT_CHUNK_SIZE};
use ferrite_dht::DhtHandle;
use ferrite_storage::TorrentStorage;

use crate::torrent::TorrentHandle;
use crate::types::{
    FileInfo, SessionConfig, SessionStats, TorrentConfig, TorrentInfo, TorrentStats,
};

/// Entry for a torrent managed by the session.
struct TorrentEntry {
    handle: TorrentHandle,
    meta: TorrentMetaV1,
}

/// Commands sent from SessionHandle to SessionActor.
enum SessionCommand {
    AddTorrent {
        meta: Box<TorrentMetaV1>,
        storage: Option<Arc<dyn TorrentStorage>>,
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
    Shutdown,
}

/// Cloneable handle for interacting with a running session.
#[derive(Clone)]
pub struct SessionHandle {
    cmd_tx: mpsc::Sender<SessionCommand>,
}

impl SessionHandle {
    /// Start a new session with the given configuration.
    pub async fn start(config: SessionConfig) -> crate::Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);

        let actor = SessionActor {
            config,
            torrents: HashMap::new(),
            dht: None,
            cmd_rx,
        };

        tokio::spawn(actor.run());
        Ok(SessionHandle { cmd_tx })
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

    /// Gracefully shut down the session and all torrents.
    pub async fn shutdown(&self) -> crate::Result<()> {
        let _ = self.cmd_tx.send(SessionCommand::Shutdown).await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SessionActor — internal single-owner event loop
// ---------------------------------------------------------------------------

struct SessionActor {
    config: SessionConfig,
    torrents: HashMap<Id20, TorrentEntry>,
    dht: Option<DhtHandle>,
    cmd_rx: mpsc::Receiver<SessionCommand>,
}

impl SessionActor {
    async fn run(mut self) {
        loop {
            match self.cmd_rx.recv().await {
                Some(SessionCommand::AddTorrent {
                    meta,
                    storage,
                    reply,
                }) => {
                    let result = self.handle_add_torrent(*meta, storage).await;
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
                Some(SessionCommand::Shutdown) | None => {
                    self.shutdown_all().await;
                    return;
                }
            }
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

        let handle =
            TorrentHandle::from_torrent(meta.clone(), storage, torrent_config, self.dht.clone())
                .await?;

        self.torrents.insert(
            info_hash,
            TorrentEntry {
                handle,
                meta,
            },
        );

        info!(%info_hash, "torrent added to session");
        Ok(info_hash)
    }

    async fn handle_remove_torrent(&mut self, info_hash: Id20) -> crate::Result<()> {
        let entry = self
            .torrents
            .remove(&info_hash)
            .ok_or(crate::Error::TorrentNotFound(info_hash))?;
        entry.handle.shutdown().await?;
        info!(%info_hash, "torrent removed from session");
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

        let meta = &entry.meta;
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

    async fn shutdown_all(&mut self) {
        for (info_hash, entry) in self.torrents.drain() {
            debug!(%info_hash, "shutting down torrent");
            let _ = entry.handle.shutdown().await;
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
}
