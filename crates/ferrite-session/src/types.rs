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
    /// Connection encryption mode (MSE/PE).
    pub encryption_mode: ferrite_wire::mse::EncryptionMode,
    /// Enable uTP (micro Transport Protocol) for peer connections.
    pub enable_utp: bool,
    /// Enable HTTP/web seeding (BEP 19, BEP 17).
    pub enable_web_seed: bool,
    /// Maximum concurrent web seed connections.
    pub max_web_seeds: usize,
    /// BEP 16: super seeding mode — reveal pieces one-per-peer for maximum diversity.
    pub super_seeding: bool,
    /// BEP 21: advertise upload-only status via extension handshake when seeding.
    pub upload_only_announce: bool,
    /// Batched Have: buffer Have messages for this many ms before sending (0 = disabled).
    pub have_send_delay_ms: u64,
    /// Number of concurrent piece verifications during torrent checking.
    pub hashing_threads: usize,
    /// Enable sequential (in-order) piece downloading.
    pub sequential_download: bool,
    /// Completed piece count below which the picker uses random selection to promote diversity.
    pub initial_picker_threshold: u32,
    /// Seconds below which a fast peer downloads a whole piece; if under this, picker grants
    /// exclusive assignment (no block splitting).
    pub whole_pieces_threshold: u32,
    /// Seconds without data from a peer before marking it as snubbed.
    pub snub_timeout_secs: u32,
    /// Number of pieces ahead of the streaming cursor to prioritize.
    pub readahead_pieces: u32,
    /// When true, escalate streaming piece requests that exceed the mean RTT.
    pub streaming_timeout_escalation: bool,
    /// Maximum concurrent file stream readers per torrent.
    pub max_concurrent_stream_reads: usize,
    /// Proxy configuration for outbound peer connections.
    pub proxy: crate::proxy::ProxyConfig,
    /// Anonymous mode: suppress client identity in peer handshakes.
    pub anonymous_mode: bool,
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
            encryption_mode: ferrite_wire::mse::EncryptionMode::Enabled,
            enable_utp: true,
            enable_web_seed: true,
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
        }
    }
}

impl From<&crate::settings::Settings> for TorrentConfig {
    fn from(s: &crate::settings::Settings) -> Self {
        Self {
            listen_port: 0, // Each torrent gets a random port (matches make_torrent_config)
            max_peers: 50,
            target_request_queue: 5,
            download_dir: s.download_dir.clone(),
            enable_dht: s.enable_dht,
            enable_pex: s.enable_pex,
            enable_fast: s.enable_fast_extension,
            seed_ratio_limit: s.seed_ratio_limit,
            strict_end_game: true,
            upload_rate_limit: 0,
            download_rate_limit: 0,
            encryption_mode: s.encryption_mode,
            enable_utp: s.enable_utp,
            enable_web_seed: s.enable_web_seed,
            max_web_seeds: 4,
            super_seeding: s.default_super_seeding,
            upload_only_announce: s.upload_only_announce,
            have_send_delay_ms: s.have_send_delay_ms,
            hashing_threads: s.hashing_threads,
            sequential_download: false,
            initial_picker_threshold: 4,
            whole_pieces_threshold: 20,
            snub_timeout_secs: 60,
            readahead_pieces: 8,
            streaming_timeout_escalation: true,
            max_concurrent_stream_reads: s.max_concurrent_stream_reads,
            proxy: s.proxy.clone(),
            anonymous_mode: s.anonymous_mode,
        }
    }
}

/// Current state of a torrent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TorrentState {
    FetchingMetadata,
    Checking,
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
    /// Progress of piece checking (0.0–1.0), meaningful when state is `Checking`.
    pub checking_progress: f32,
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
    TrackersReceived {
        tracker_urls: Vec<String>,
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
    WebSeedPieceData {
        url: String,
        index: u32,
        data: Bytes,
    },
    WebSeedError {
        url: String,
        piece: u32,
        message: String,
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
    /// Send an updated extension handshake (e.g. BEP 21 upload-only).
    SendExtHandshake(ferrite_wire::ExtHandshake),
    /// Send a full bitfield mid-connection (batched Have fallback).
    SendBitfield(Bytes),
    Shutdown,
}

/// Commands sent from a `TorrentHandle` to the `TorrentActor`.
#[derive(Debug)]
#[allow(dead_code)] // consumed by torrent module (not yet implemented)
pub(crate) enum TorrentCommand {
    AddPeers { peers: Vec<SocketAddr>, source: crate::peer_state::PeerSource },
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
    ForceReannounce,
    TrackerList {
        reply: oneshot::Sender<Vec<crate::tracker_manager::TrackerInfo>>,
    },
    Scrape {
        reply: oneshot::Sender<Option<(String, ferrite_tracker::ScrapeInfo)>>,
    },
    /// Incoming uTP peer routed from the session-level accept loop.
    IncomingPeer {
        stream: crate::utp_routing::PrefixedStream<ferrite_utp::UtpStream>,
        addr: SocketAddr,
    },
    /// Open a streaming reader for a file within the torrent.
    OpenFile {
        file_index: usize,
        reply: oneshot::Sender<crate::Result<crate::streaming::FileStreamHandle>>,
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
    fn torrent_config_encryption_default() {
        let cfg = TorrentConfig::default();
        assert_eq!(cfg.encryption_mode, ferrite_wire::mse::EncryptionMode::Enabled);
    }

    #[test]
    fn torrent_config_utp_default() {
        let cfg = TorrentConfig::default();
        assert!(cfg.enable_utp);
    }

    #[test]
    fn torrent_config_web_seed_defaults() {
        let cfg = TorrentConfig::default();
        assert!(cfg.enable_web_seed);
        assert_eq!(cfg.max_web_seeds, 4);
    }

    #[test]
    fn torrent_config_super_seeding_default() {
        let cfg = TorrentConfig::default();
        assert!(!cfg.super_seeding);
        assert!(cfg.upload_only_announce);
    }

    #[test]
    fn torrent_config_have_delay_default() {
        let cfg = TorrentConfig::default();
        assert_eq!(cfg.have_send_delay_ms, 0);
    }

    #[test]
    fn torrent_config_picker_defaults() {
        let cfg = TorrentConfig::default();
        assert!(!cfg.sequential_download);
        assert_eq!(cfg.initial_picker_threshold, 4);
        assert_eq!(cfg.whole_pieces_threshold, 20);
        assert_eq!(cfg.snub_timeout_secs, 60);
        assert_eq!(cfg.readahead_pieces, 8);
        assert!(cfg.streaming_timeout_escalation);
    }

}
