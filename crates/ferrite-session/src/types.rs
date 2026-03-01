use std::collections::HashMap;
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
    /// Enable BEP 55 holepunch extension for NAT traversal.
    pub enable_holepunch: bool,
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
    /// Share mode: relay pieces in memory without writing to disk.
    /// Requires `enable_fast` for RejectRequest when evicting pieces.
    pub share_mode: bool,
    /// Whether this torrent should use I2P for peer connections.
    pub enable_i2p: bool,
    /// Whether to allow mixing I2P and clearnet peers.
    pub allow_i2p_mixed: bool,
    /// SSL listen port for SSL torrent connections (0 = disabled).
    pub ssl_listen_port: u16,
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
            enable_holepunch: true,
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
            enable_holepunch: s.enable_holepunch,
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
            share_mode: s.default_share_mode,
            enable_i2p: s.enable_i2p,
            allow_i2p_mixed: s.allow_i2p_mixed,
            ssl_listen_port: s.ssl_listen_port,
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
    /// Share mode: relay pieces in memory without writing to disk.
    Sharing,
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
    /// Number of connected peers broken down by discovery source.
    pub peers_by_source: HashMap<crate::peer_state::PeerSource, usize>,
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
    /// BEP 52: Received hash response from peer.
    HashesReceived {
        peer_addr: SocketAddr,
        request: ferrite_core::HashRequest,
        hashes: Vec<ferrite_core::Id32>,
    },
    /// BEP 52: Peer rejected our hash request.
    HashRequestRejected {
        peer_addr: SocketAddr,
        request: ferrite_core::HashRequest,
    },
    /// BEP 52: Peer sent a hash request to us.
    IncomingHashRequest {
        peer_addr: SocketAddr,
        request: ferrite_core::HashRequest,
    },
    /// BEP 55: Received a Rendezvous request (we are the relay).
    HolepunchRendezvous {
        peer_addr: SocketAddr,
        target: SocketAddr,
    },
    /// BEP 55: Received a Connect message (we should initiate simultaneous connect).
    HolepunchConnect {
        peer_addr: SocketAddr,
        target: SocketAddr,
    },
    /// BEP 55: Received an Error message from the relay.
    HolepunchError {
        peer_addr: SocketAddr,
        target: SocketAddr,
        error_code: u32,
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
    /// BEP 52: Send a hash request to the peer.
    SendHashRequest(ferrite_core::HashRequest),
    /// BEP 52: Send hashes in response to a peer's request.
    SendHashes {
        request: ferrite_core::HashRequest,
        hashes: Vec<ferrite_core::Id32>,
    },
    /// BEP 52: Reject a peer's hash request.
    SendHashReject(ferrite_core::HashRequest),
    /// BEP 55: Send a holepunch message to this peer.
    SendHolepunch(ferrite_wire::HolepunchMessage),
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
    /// Update the external IP for BEP 40 peer priority calculation.
    UpdateExternalIp {
        ip: std::net::IpAddr,
    },
    /// Move torrent data files to a new directory.
    MoveStorage {
        new_path: PathBuf,
        reply: oneshot::Sender<crate::Result<()>>,
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

    #[test]
    fn torrent_stats_has_peers_by_source() {
        use std::collections::HashMap;
        use crate::peer_state::PeerSource;

        let stats = TorrentStats {
            state: TorrentState::Downloading,
            downloaded: 0,
            uploaded: 0,
            pieces_have: 0,
            pieces_total: 10,
            peers_connected: 0,
            peers_available: 0,
            checking_progress: 0.0,
            peers_by_source: HashMap::new(),
        };
        assert!(stats.peers_by_source.is_empty());

        let mut map = HashMap::new();
        map.insert(PeerSource::Tracker, 5);
        map.insert(PeerSource::Dht, 3);
        let stats2 = TorrentStats {
            peers_by_source: map.clone(),
            ..stats
        };
        assert_eq!(stats2.peers_by_source[&PeerSource::Tracker], 5);
        assert_eq!(stats2.peers_by_source[&PeerSource::Dht], 3);
    }

    #[test]
    fn torrent_state_sharing_variant() {
        let state = TorrentState::Sharing;
        assert_ne!(state, TorrentState::Downloading);
        assert_ne!(state, TorrentState::Seeding);
        // Verify JSON round-trip
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, "\"Sharing\"");
        let decoded: TorrentState = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, TorrentState::Sharing);
    }

    #[test]
    fn torrent_config_i2p_defaults() {
        let cfg = TorrentConfig::default();
        assert!(!cfg.enable_i2p);
        assert!(!cfg.allow_i2p_mixed);
    }

    #[test]
    fn torrent_config_ssl_listen_port_default() {
        let cfg = TorrentConfig::default();
        assert_eq!(cfg.ssl_listen_port, 0);
    }

    #[test]
    fn torrent_config_ssl_listen_port_from_settings() {
        let mut s = crate::settings::Settings::default();
        s.ssl_listen_port = 4433;
        let tc = TorrentConfig::from(&s);
        assert_eq!(tc.ssl_listen_port, 4433);
    }

    #[test]
    fn torrent_config_holepunch_default() {
        let cfg = TorrentConfig::default();
        assert!(cfg.enable_holepunch);

        // Also verify it inherits from Settings
        let s = crate::settings::Settings::default();
        let tc = TorrentConfig::from(&s);
        assert!(tc.enable_holepunch);

        // And when disabled in Settings
        let mut s2 = crate::settings::Settings::default();
        s2.enable_holepunch = false;
        let tc2 = TorrentConfig::from(&s2);
        assert!(!tc2.enable_holepunch);
    }
}
