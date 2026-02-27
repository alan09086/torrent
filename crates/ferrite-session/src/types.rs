use std::net::SocketAddr;
use std::path::PathBuf;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use ferrite_storage::Bitfield;
use ferrite_wire::ExtHandshake;

/// Configurable parameters for a torrent session.
#[derive(Debug, Clone)]
pub struct TorrentConfig {
    pub listen_port: u16,
    pub max_peers: usize,
    pub target_request_queue: usize,
    pub download_dir: PathBuf,
    pub enable_dht: bool,
    pub enable_pex: bool,
    pub enable_fast: bool,
    pub seed_ratio_limit: Option<f64>,
    pub strict_end_game: bool,
    /// Upload rate limit in bytes/sec (0 = unlimited).
    pub upload_rate_limit: u64,
    /// Download rate limit in bytes/sec (0 = unlimited).
    pub download_rate_limit: u64,
}

impl Default for TorrentConfig {
    fn default() -> Self {
        Self {
            listen_port: 6881,
            max_peers: 50,
            target_request_queue: 5,
            download_dir: PathBuf::from("."),
            enable_dht: true,
            enable_pex: true,
            enable_fast: false,
            seed_ratio_limit: None,
            strict_end_game: true,
            upload_rate_limit: 0,
            download_rate_limit: 0,
        }
    }
}

/// Current state of a torrent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TorrentState {
    FetchingMetadata,
    Downloading,
    Complete,
    Seeding,
    Paused,
    Stopped,
}

/// Aggregate statistics for a torrent.
#[derive(Debug, Clone)]
pub struct TorrentStats {
    pub state: TorrentState,
    pub downloaded: u64,
    pub uploaded: u64,
    pub pieces_have: u32,
    pub pieces_total: u32,
    pub peers_connected: usize,
    pub peers_available: usize,
}

/// Events sent from a `PeerTask` back to the `TorrentActor`.
#[derive(Debug)]
#[allow(dead_code)] // consumed by peer/torrent modules (not yet implemented)
pub(crate) enum PeerEvent {
    Bitfield {
        peer_addr: SocketAddr,
        bitfield: Bitfield,
    },
    Have {
        peer_addr: SocketAddr,
        index: u32,
    },
    PieceData {
        peer_addr: SocketAddr,
        index: u32,
        begin: u32,
        data: Bytes,
    },
    PeerChoking {
        peer_addr: SocketAddr,
        choking: bool,
    },
    PeerInterested {
        peer_addr: SocketAddr,
        interested: bool,
    },
    ExtHandshake {
        peer_addr: SocketAddr,
        handshake: ExtHandshake,
    },
    MetadataPiece {
        peer_addr: SocketAddr,
        piece: u32,
        data: Bytes,
        total_size: u64,
    },
    MetadataReject {
        peer_addr: SocketAddr,
        piece: u32,
    },
    PexPeers {
        new_peers: Vec<SocketAddr>,
    },
    IncomingRequest {
        peer_addr: SocketAddr,
        index: u32,
        begin: u32,
        length: u32,
    },
    RejectRequest {
        peer_addr: SocketAddr,
        index: u32,
        begin: u32,
        length: u32,
    },
    AllowedFast {
        peer_addr: SocketAddr,
        index: u32,
    },
    SuggestPiece {
        peer_addr: SocketAddr,
        index: u32,
    },
    Disconnected {
        peer_addr: SocketAddr,
        reason: Option<String>,
    },
}

/// Commands sent from the `TorrentActor` to a `PeerTask`.
#[derive(Debug)]
#[allow(dead_code)] // consumed by peer/torrent modules (not yet implemented)
pub(crate) enum PeerCommand {
    Request { index: u32, begin: u32, length: u32 },
    Cancel { index: u32, begin: u32, length: u32 },
    SetChoking(bool),
    SetInterested(bool),
    Have(u32),
    RequestMetadata { piece: u32 },
    RejectRequest { index: u32, begin: u32, length: u32 },
    AllowedFast(u32),
    SendPiece { index: u32, begin: u32, data: Bytes },
    Shutdown,
}

/// Commands sent from a `TorrentHandle` to the `TorrentActor`.
#[derive(Debug)]
#[allow(dead_code)] // consumed by torrent module (not yet implemented)
pub(crate) enum TorrentCommand {
    AddPeers { peers: Vec<SocketAddr> },
    Stats { reply: oneshot::Sender<TorrentStats> },
    Pause,
    Resume,
    Shutdown,
    SaveResumeData {
        reply: oneshot::Sender<crate::Result<ferrite_core::FastResumeData>>,
    },
    SetFilePriority {
        index: usize,
        priority: ferrite_core::FilePriority,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    FilePriorities {
        reply: oneshot::Sender<Vec<ferrite_core::FilePriority>>,
    },
}

/// Info about a file within a torrent.
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: PathBuf,
    pub length: u64,
}

/// Metadata about a torrent (available after metadata is fetched).
#[derive(Debug, Clone)]
pub struct TorrentInfo {
    pub info_hash: ferrite_core::Id20,
    pub name: String,
    pub total_length: u64,
    pub piece_length: u64,
    pub num_pieces: u32,
    pub files: Vec<FileInfo>,
    pub private: bool,
}

/// Configuration for a multi-torrent session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub listen_port: u16,
    pub download_dir: PathBuf,
    pub max_torrents: usize,
    pub enable_dht: bool,
    pub enable_pex: bool,
    pub enable_lsd: bool,
    pub enable_fast_extension: bool,
    pub seed_ratio_limit: Option<f64>,
    /// Directory for resume data files. Defaults to `<download_dir>/.ferrite/`.
    pub resume_data_dir: Option<std::path::PathBuf>,
    /// Global upload rate limit in bytes/sec (0 = unlimited).
    pub upload_rate_limit: u64,
    /// Global download rate limit in bytes/sec (0 = unlimited).
    pub download_rate_limit: u64,
    /// Automatically adjust unchoke slot count based on upload capacity.
    pub auto_upload_slots: bool,
    /// Minimum unchoke slots when auto-tuning.
    pub auto_upload_slots_min: usize,
    /// Maximum unchoke slots when auto-tuning.
    pub auto_upload_slots_max: usize,
    /// Bitmask of alert categories to deliver (default: all).
    pub alert_mask: crate::alert::AlertCategory,
    /// Capacity of the alert broadcast channel (default: 1024).
    pub alert_channel_size: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            listen_port: 6881,
            download_dir: PathBuf::from("."),
            max_torrents: 100,
            enable_dht: true,
            enable_pex: true,
            enable_lsd: true,
            enable_fast_extension: true,
            seed_ratio_limit: None,
            resume_data_dir: None,
            upload_rate_limit: 0,
            download_rate_limit: 0,
            auto_upload_slots: true,
            auto_upload_slots_min: 2,
            auto_upload_slots_max: 20,
            alert_mask: crate::alert::AlertCategory::ALL,
            alert_channel_size: 1024,
        }
    }
}

/// Aggregate statistics for the whole session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStats {
    pub active_torrents: usize,
    pub total_downloaded: u64,
    pub total_uploaded: u64,
    pub dht_nodes: usize,
}

/// Type alias for a factory that creates per-torrent storage.
pub type StorageFactory = Box<
    dyn Fn(&ferrite_core::TorrentMetaV1, &std::path::Path) -> std::sync::Arc<dyn ferrite_storage::TorrentStorage>
        + Send
        + Sync,
>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn torrent_config_strict_end_game_default() {
        let config = TorrentConfig::default();
        assert!(config.strict_end_game);
    }

    #[test]
    fn torrent_config_bandwidth_defaults() {
        let config = TorrentConfig::default();
        assert_eq!(config.upload_rate_limit, 0);
        assert_eq!(config.download_rate_limit, 0);
    }

    #[test]
    fn session_config_alert_defaults() {
        let config = SessionConfig::default();
        assert_eq!(config.alert_mask, crate::alert::AlertCategory::ALL);
        assert_eq!(config.alert_channel_size, 1024);
    }

    #[test]
    fn session_config_bandwidth_defaults() {
        let config = SessionConfig::default();
        assert_eq!(config.upload_rate_limit, 0);
        assert_eq!(config.download_rate_limit, 0);
        assert!(config.auto_upload_slots);
        assert_eq!(config.auto_upload_slots_min, 2);
        assert_eq!(config.auto_upload_slots_max, 20);
    }
}
