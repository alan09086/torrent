use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use bitflags::bitflags;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use ferrite_storage::Bitfield;
use ferrite_wire::ExtHandshake;

use crate::choker::{ChokingAlgorithm, SeedChokingAlgorithm};

/// Configurable parameters for a torrent session.
#[derive(Debug, Clone)]
pub struct TorrentConfig {
    /// TCP listen port for incoming peer connections.
    pub listen_port: u16,
    /// Maximum number of peer connections per torrent.
    pub max_peers: usize,
    /// Number of outstanding piece requests to maintain per peer.
    pub target_request_queue: usize,
    /// Directory where downloaded files are stored.
    pub download_dir: PathBuf,
    /// Enable DHT for peer discovery.
    pub enable_dht: bool,
    /// Enable Peer Exchange (BEP 11) for peer discovery.
    pub enable_pex: bool,
    /// Enable Fast Extension (BEP 6) for reject/suggest/allowed-fast messages.
    pub enable_fast: bool,
    /// Stop seeding after reaching this upload/download ratio (None = unlimited).
    pub seed_ratio_limit: Option<f64>,
    /// In end game mode, cancel duplicate requests when a piece completes.
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
    /// Algorithm for ranking peers during seed-mode choking.
    pub seed_choking_algorithm: SeedChokingAlgorithm,
    /// Algorithm for determining the number of unchoke slots.
    pub choking_algorithm: ChokingAlgorithm,
    /// Prefer grouping piece requests within the same 4 MiB disk extent.
    pub piece_extent_affinity: bool,
    /// Enable sending SuggestPiece messages for cached pieces.
    pub suggest_mode: bool,
    /// Maximum number of pieces to suggest per peer.
    pub max_suggest_pieces: usize,
    /// Delay (ms) before announcing Have for a piece still being written to disk (0 = disabled).
    pub predictive_piece_announce_ms: u64,
    /// Mixed-mode TCP/uTP bandwidth allocation algorithm.
    pub mixed_mode_algorithm: crate::rate_limiter::MixedModeAlgorithm,
    /// Enable automatic sequential mode switching on partial-piece explosion.
    pub auto_sequential: bool,
    /// Storage allocation mode for disk I/O.
    pub storage_mode: ferrite_core::StorageMode,
    /// Block request timeout in seconds before re-issuing (0 = disabled).
    pub block_request_timeout_secs: u32,
    /// Enable Local Service Discovery (BEP 14) for this torrent.
    pub enable_lsd: bool,
    /// Force all connections through the configured proxy.
    pub force_proxy: bool,
    /// Steal blocks from peers this many times slower than the requesting peer (0.0 = disabled).
    pub steal_threshold_ratio: f64,
    /// Fraction of peers to disconnect per turnover interval (0.0–1.0).
    pub peer_turnover: f64,
    /// Only trigger turnover if download rate < this fraction of peak rate (0.0–1.0).
    pub peer_turnover_cutoff: f64,
    /// Seconds between turnover checks (0 = disabled).
    pub peer_turnover_interval: u64,
    /// URL security configuration for SSRF mitigation and IDNA checking.
    pub url_security: crate::url_guard::UrlSecurityConfig,
    /// Timeout in seconds for outbound TCP peer connections (0 = OS default).
    pub peer_connect_timeout: u64,
    /// DSCP (Differentiated Services Code Point) value for peer traffic sockets.
    pub peer_dscp: u8,
    /// Initial per-peer request queue depth for the pipeline slow-start.
    pub initial_queue_depth: usize,
    /// Maximum per-peer request queue depth.
    pub max_request_queue_depth: usize,
    /// Request queue time multiplier in seconds for steady-state depth.
    pub request_queue_time: f64,
}

impl Default for TorrentConfig {
    fn default() -> Self {
        Self {
            listen_port: 6881,
            max_peers: 200,
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
            seed_choking_algorithm: SeedChokingAlgorithm::FastestUpload,
            choking_algorithm: ChokingAlgorithm::FixedSlots,
            piece_extent_affinity: true,
            suggest_mode: false,
            max_suggest_pieces: 10,
            predictive_piece_announce_ms: 0,
            mixed_mode_algorithm: crate::rate_limiter::MixedModeAlgorithm::PeerProportional,
            auto_sequential: true,
            storage_mode: ferrite_core::StorageMode::Auto,
            block_request_timeout_secs: 60,
            enable_lsd: true,
            force_proxy: false,
            steal_threshold_ratio: 10.0,
            peer_turnover: 0.04,
            peer_turnover_cutoff: 0.9,
            peer_turnover_interval: 300,
            url_security: crate::url_guard::UrlSecurityConfig::default(),
            peer_connect_timeout: 5,
            peer_dscp: 0x08,
            initial_queue_depth: 128,
            max_request_queue_depth: 250,
            request_queue_time: 3.0,
        }
    }
}

impl From<&crate::settings::Settings> for TorrentConfig {
    fn from(s: &crate::settings::Settings) -> Self {
        Self {
            listen_port: 0, // Each torrent gets a random port (matches make_torrent_config)
            max_peers: s.max_peers_per_torrent,
            target_request_queue: 5,
            download_dir: s.download_dir.clone(),
            enable_dht: s.enable_dht,
            enable_pex: s.enable_pex,
            enable_fast: s.enable_fast_extension,
            seed_ratio_limit: s.seed_ratio_limit,
            strict_end_game: s.strict_end_game,
            upload_rate_limit: s.upload_rate_limit,
            download_rate_limit: s.download_rate_limit,
            encryption_mode: s.encryption_mode,
            enable_utp: s.enable_utp,
            enable_web_seed: s.enable_web_seed,
            enable_holepunch: s.enable_holepunch,
            max_web_seeds: s.max_web_seeds,
            super_seeding: s.default_super_seeding,
            upload_only_announce: s.upload_only_announce,
            have_send_delay_ms: s.have_send_delay_ms,
            hashing_threads: s.hashing_threads,
            sequential_download: false,
            initial_picker_threshold: s.initial_picker_threshold,
            whole_pieces_threshold: s.whole_pieces_threshold,
            snub_timeout_secs: s.snub_timeout_secs,
            readahead_pieces: s.readahead_pieces,
            streaming_timeout_escalation: s.streaming_timeout_escalation,
            max_concurrent_stream_reads: s.max_concurrent_stream_reads,
            proxy: s.proxy.clone(),
            anonymous_mode: s.anonymous_mode,
            share_mode: s.default_share_mode,
            enable_i2p: s.enable_i2p,
            allow_i2p_mixed: s.allow_i2p_mixed,
            ssl_listen_port: s.ssl_listen_port,
            seed_choking_algorithm: s.seed_choking_algorithm,
            choking_algorithm: s.choking_algorithm,
            piece_extent_affinity: s.piece_extent_affinity,
            suggest_mode: s.suggest_mode,
            max_suggest_pieces: s.max_suggest_pieces,
            predictive_piece_announce_ms: s.predictive_piece_announce_ms,
            mixed_mode_algorithm: s.mixed_mode_algorithm,
            auto_sequential: s.auto_sequential,
            storage_mode: s.storage_mode,
            block_request_timeout_secs: s.block_request_timeout_secs,
            enable_lsd: s.enable_lsd,
            force_proxy: s.force_proxy,
            steal_threshold_ratio: s.steal_threshold_ratio,
            peer_turnover: s.peer_turnover,
            peer_turnover_cutoff: s.peer_turnover_cutoff,
            peer_turnover_interval: s.peer_turnover_interval,
            url_security: crate::url_guard::UrlSecurityConfig::from(s),
            peer_connect_timeout: s.peer_connect_timeout,
            peer_dscp: s.peer_dscp,
            initial_queue_depth: s.initial_queue_depth,
            max_request_queue_depth: s.max_request_queue_depth,
            request_queue_time: s.request_queue_time,
        }
    }
}

/// Current state of a torrent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TorrentState {
    /// Waiting for peers to send torrent metadata via BEP 9.
    FetchingMetadata,
    /// Verifying existing data on disk against piece hashes.
    Checking,
    /// Actively downloading pieces from peers.
    Downloading,
    /// All pieces downloaded, awaiting transition to seeding.
    Complete,
    /// Upload-only: all pieces verified, serving to other peers.
    Seeding,
    /// Manually paused by the user. No peer connections maintained.
    Paused,
    /// Removed from the session. Terminal state.
    Stopped,
    /// Share mode: relay pieces in memory without writing to disk.
    Sharing,
}

/// Aggregate statistics for a torrent.
#[derive(Debug, Clone)]
pub struct TorrentStats {
    // ── Original fields (unchanged) ──

    /// Current torrent state.
    pub state: TorrentState,
    /// Total bytes downloaded (payload only).
    pub downloaded: u64,
    /// Total bytes uploaded (payload only).
    pub uploaded: u64,
    /// Number of pieces that have been verified.
    pub pieces_have: u32,
    /// Total number of pieces in the torrent.
    pub pieces_total: u32,
    /// Number of currently connected peers.
    pub peers_connected: usize,
    /// Number of known peers (connected + available).
    pub peers_available: usize,
    /// Progress of piece checking (0.0–1.0), meaningful when state is `Checking`.
    pub checking_progress: f32,
    /// Number of connected peers broken down by discovery source.
    pub peers_by_source: HashMap<crate::peer_state::PeerSource, usize>,

    // ── Identity ──

    /// Info hashes (v1 SHA-1 and/or v2 SHA-256) for this torrent.
    pub info_hashes: ferrite_core::InfoHashes,
    /// Display name from the torrent metadata.
    pub name: String,

    // ── State flags ──

    /// Whether metadata has been received (always true for .torrent adds).
    pub has_metadata: bool,
    /// Whether we have all pieces and are seeding.
    pub is_seeding: bool,
    /// Whether all wanted pieces are downloaded (may differ from is_seeding with file priorities).
    pub is_finished: bool,
    /// Whether the torrent is paused.
    pub is_paused: bool,
    /// Whether the torrent is auto-managed by the session queuing system.
    pub auto_managed: bool,
    /// Whether sequential piece downloading is enabled.
    pub sequential_download: bool,
    /// Whether BEP 16 super seeding mode is active.
    pub super_seeding: bool,
    /// Whether we have accepted any incoming peer connections.
    pub has_incoming: bool,
    /// Whether resume data needs to be saved.
    pub need_save_resume: bool,
    /// Whether a storage move operation is in progress.
    pub moving_storage: bool,

    // ── Progress ──

    /// Download progress as a fraction (0.0–1.0).
    pub progress: f32,
    /// Download progress in parts per million (0–1_000_000).
    pub progress_ppm: u32,
    /// Total bytes of verified (downloaded and hash-checked) data.
    pub total_done: u64,
    /// Total size of the torrent in bytes.
    pub total: u64,
    /// Total bytes of wanted data that have been verified.
    pub total_wanted_done: u64,
    /// Total bytes of wanted data (respecting file priorities).
    pub total_wanted: u64,
    /// Block (sub-piece request) size in bytes.
    pub block_size: u32,

    // ── Transfer (session counters) ──

    /// Total bytes downloaded this session (including protocol overhead).
    pub total_download: u64,
    /// Total bytes uploaded this session (including protocol overhead).
    pub total_upload: u64,
    /// Total payload bytes downloaded this session.
    pub total_payload_download: u64,
    /// Total payload bytes uploaded this session.
    pub total_payload_upload: u64,
    /// Total bytes of data that failed hash check.
    pub total_failed_bytes: u64,
    /// Total bytes of redundant (duplicate) data received.
    pub total_redundant_bytes: u64,

    // ── Transfer (all-time, persisted) ──

    /// All-time total bytes downloaded (persisted across sessions via resume data).
    pub all_time_download: u64,
    /// All-time total bytes uploaded (persisted across sessions via resume data).
    pub all_time_upload: u64,

    // ── Rates ──

    /// Current download rate in bytes/sec (including protocol overhead).
    pub download_rate: u64,
    /// Current upload rate in bytes/sec (including protocol overhead).
    pub upload_rate: u64,
    /// Current payload download rate in bytes/sec.
    pub download_payload_rate: u64,
    /// Current payload upload rate in bytes/sec.
    pub upload_payload_rate: u64,

    // ── Connection details ──

    /// Number of peers connected (including half-open).
    pub num_peers: usize,
    /// Number of connected peers that are seeds.
    pub num_seeds: usize,
    /// Number of complete copies known from tracker scrape (-1 = unknown).
    pub num_complete: i32,
    /// Number of incomplete copies known from tracker scrape (-1 = unknown).
    pub num_incomplete: i32,
    /// Total number of seeds across all trackers.
    pub list_seeds: usize,
    /// Total number of peers across all trackers.
    pub list_peers: usize,
    /// Number of peers available to connect to (not yet connected).
    pub connect_candidates: usize,
    /// Number of active peer connections (TCP + uTP).
    pub num_connections: usize,
    /// Number of unchoked peers we are uploading to.
    pub num_uploads: usize,

    // ── Limits ──

    /// Maximum number of connections for this torrent.
    pub connections_limit: usize,
    /// Maximum number of unchoke slots for this torrent.
    pub uploads_limit: usize,

    // ── Distributed copies ──

    /// Number of full distributed copies available in the swarm.
    pub distributed_full_copies: u32,
    /// Fractional part of distributed copies (0–999).
    pub distributed_fraction: u32,
    /// Distributed copies as a float (full + fraction/1000).
    pub distributed_copies: f32,

    // ── Tracker ──

    /// URL of the tracker we most recently announced to.
    pub current_tracker: String,
    /// Whether we are currently announcing to any tracker.
    pub announcing_to_trackers: bool,
    /// Whether we are currently announcing to LSD (Local Service Discovery).
    pub announcing_to_lsd: bool,
    /// Whether we are currently announcing to DHT.
    pub announcing_to_dht: bool,

    // ── Timestamps (POSIX seconds) ──

    /// Time when the torrent was added to the session.
    pub added_time: i64,
    /// Time when the torrent completed downloading (0 = not completed).
    pub completed_time: i64,
    /// Last time a complete copy was seen in the swarm (0 = never).
    pub last_seen_complete: i64,
    /// Time of last upload activity (0 = never).
    pub last_upload: i64,
    /// Time of last download activity (0 = never).
    pub last_download: i64,

    // ── Durations (cumulative seconds) ──

    /// Total seconds the torrent has been active (downloading or seeding).
    pub active_duration: i64,
    /// Total seconds the torrent has been in finished state.
    pub finished_duration: i64,
    /// Total seconds the torrent has been seeding.
    pub seeding_duration: i64,

    // ── Storage ──

    /// Current save path for the torrent data.
    pub save_path: String,

    // ── Queue ──

    /// Position in the session queue (-1 = not queued).
    pub queue_position: i32,

    // ── Error ──

    /// Human-readable error message (empty = no error).
    pub error: String,
    /// Index of the file that caused the error (-1 = not file-specific).
    pub error_file: i32,
}

impl Default for TorrentStats {
    fn default() -> Self {
        Self {
            // Original fields
            state: TorrentState::Paused,
            downloaded: 0,
            uploaded: 0,
            pieces_have: 0,
            pieces_total: 0,
            peers_connected: 0,
            peers_available: 0,
            checking_progress: 0.0,
            peers_by_source: HashMap::new(),

            // Identity
            info_hashes: ferrite_core::InfoHashes::v1_only(ferrite_core::Id20::from([0u8; 20])),
            name: String::new(),

            // State flags
            has_metadata: false,
            is_seeding: false,
            is_finished: false,
            is_paused: false,
            auto_managed: false,
            sequential_download: false,
            super_seeding: false,
            has_incoming: false,
            need_save_resume: false,
            moving_storage: false,

            // Progress
            progress: 0.0,
            progress_ppm: 0,
            total_done: 0,
            total: 0,
            total_wanted_done: 0,
            total_wanted: 0,
            block_size: 16384,

            // Transfer (session counters)
            total_download: 0,
            total_upload: 0,
            total_payload_download: 0,
            total_payload_upload: 0,
            total_failed_bytes: 0,
            total_redundant_bytes: 0,

            // Transfer (all-time)
            all_time_download: 0,
            all_time_upload: 0,

            // Rates
            download_rate: 0,
            upload_rate: 0,
            download_payload_rate: 0,
            upload_payload_rate: 0,

            // Connection details
            num_peers: 0,
            num_seeds: 0,
            num_complete: -1,
            num_incomplete: -1,
            list_seeds: 0,
            list_peers: 0,
            connect_candidates: 0,
            num_connections: 0,
            num_uploads: 0,

            // Limits
            connections_limit: 0,
            uploads_limit: 0,

            // Distributed copies
            distributed_full_copies: 0,
            distributed_fraction: 0,
            distributed_copies: 0.0,

            // Tracker
            current_tracker: String::new(),
            announcing_to_trackers: false,
            announcing_to_lsd: false,
            announcing_to_dht: false,

            // Timestamps
            added_time: 0,
            completed_time: 0,
            last_seen_complete: 0,
            last_upload: 0,
            last_download: 0,

            // Durations
            active_duration: 0,
            finished_duration: 0,
            seeding_duration: 0,

            // Storage
            save_path: String::new(),

            // Queue
            queue_position: -1,

            // Error
            error: String::new(),
            error_file: -1,
        }
    }
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
    /// Peer successfully connected with a specific transport.
    TransportIdentified {
        peer_addr: SocketAddr,
        transport: crate::rate_limiter::PeerTransport,
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
    /// MSE handshake failed — peer is being retried with plaintext.
    /// Carries the new command channel sender so the TorrentActor can
    /// update its PeerState.
    MseRetry {
        peer_addr: SocketAddr,
        cmd_tx: tokio::sync::mpsc::Sender<PeerCommand>,
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
    /// BEP 6: Suggest a piece to the peer.
    SuggestPiece(u32),
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
    /// Update the piece count after BEP 9 metadata assembly.
    UpdateNumPieces(u32),
    Shutdown,
}

/// Helper trait combining [`AsyncRead`] + [`AsyncWrite`] for trait-object erasure.
///
/// Rust doesn't allow `dyn AsyncRead + AsyncWrite` directly, so this trait
/// combines both into a single trait that can be used as a trait object.
pub(crate) trait AsyncReadWrite:
    tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send
{
}

impl<T> AsyncReadWrite for T where T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}

/// Boxed async stream (AsyncRead + AsyncWrite + Unpin + Send) with a Debug impl.
///
/// Used for incoming SSL peer connections where the concrete TLS type is erased.
pub(crate) struct BoxedAsyncStream(pub Box<dyn AsyncReadWrite>);

impl std::fmt::Debug for BoxedAsyncStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BoxedAsyncStream(..)")
    }
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
    /// Incoming peer routed from the session-level accept loop (TCP or uTP).
    IncomingPeer {
        stream: crate::transport::BoxedStream,
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
    /// Incoming SSL peer routed from the session-level SSL listener (M42).
    ///
    /// The TLS handshake has already been completed by the session actor.
    SpawnSslPeer {
        addr: SocketAddr,
        stream: BoxedAsyncStream,
    },
    /// Set the per-torrent download rate limit (bytes/sec, 0 = unlimited).
    SetDownloadLimit {
        bytes_per_sec: u64,
        reply: oneshot::Sender<()>,
    },
    /// Set the per-torrent upload rate limit (bytes/sec, 0 = unlimited).
    SetUploadLimit {
        bytes_per_sec: u64,
        reply: oneshot::Sender<()>,
    },
    /// Get the current per-torrent download rate limit (bytes/sec, 0 = unlimited).
    DownloadLimit {
        reply: oneshot::Sender<u64>,
    },
    /// Get the current per-torrent upload rate limit (bytes/sec, 0 = unlimited).
    UploadLimit {
        reply: oneshot::Sender<u64>,
    },
    /// Enable or disable sequential (in-order) piece downloading.
    SetSequentialDownload {
        enabled: bool,
        reply: oneshot::Sender<()>,
    },
    /// Query whether sequential downloading is enabled.
    IsSequentialDownload {
        reply: oneshot::Sender<bool>,
    },
    /// Enable or disable BEP 16 super seeding mode.
    SetSuperSeeding {
        enabled: bool,
        reply: oneshot::Sender<()>,
    },
    /// Query whether super seeding mode is enabled.
    IsSuperSeeding {
        reply: oneshot::Sender<bool>,
    },
    /// Add a new tracker URL (fire-and-forget at torrent level).
    AddTracker {
        url: String,
    },
    /// Replace all tracker URLs with a new set.
    ReplaceTrackers {
        urls: Vec<String>,
        reply: oneshot::Sender<()>,
    },
    /// Trigger a full piece verification (force recheck).
    ForceRecheck {
        reply: oneshot::Sender<crate::Result<()>>,
    },
    /// Rename a file within the torrent on disk.
    RenameFile {
        file_index: usize,
        new_name: String,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    /// Set the per-torrent maximum number of connections (0 = use global default).
    SetMaxConnections {
        limit: usize,
        reply: oneshot::Sender<()>,
    },
    /// Get the current per-torrent maximum connection limit.
    MaxConnections {
        reply: oneshot::Sender<usize>,
    },
    /// Set the per-torrent maximum number of unchoke slots (upload slots).
    SetMaxUploads {
        limit: usize,
        reply: oneshot::Sender<()>,
    },
    /// Get the current per-torrent maximum unchoke slots (upload slots).
    MaxUploads {
        reply: oneshot::Sender<usize>,
    },
    /// Get per-peer details for all connected peers.
    GetPeerInfo {
        reply: oneshot::Sender<Vec<PeerInfo>>,
    },
    /// Get in-flight piece download status (the download queue).
    GetDownloadQueue {
        reply: oneshot::Sender<Vec<PartialPieceInfo>>,
    },
    /// Check whether a specific piece has been downloaded.
    HavePiece {
        index: u32,
        reply: oneshot::Sender<bool>,
    },
    /// Get per-piece availability counts from connected peers.
    PieceAvailability {
        reply: oneshot::Sender<Vec<u32>>,
    },
    /// Get per-file bytes-downloaded progress.
    FileProgress {
        reply: oneshot::Sender<Vec<u64>>,
    },
    /// Get the torrent's identity hashes (v1 and/or v2).
    InfoHashes {
        reply: oneshot::Sender<ferrite_core::InfoHashes>,
    },
    /// Get the full v1 metainfo (None for magnet links before metadata received).
    TorrentFile {
        reply: oneshot::Sender<Option<ferrite_core::TorrentMetaV1>>,
    },
    /// Get the full v2 metainfo (None if not a v2/hybrid torrent or before metadata received).
    TorrentFileV2 {
        reply: oneshot::Sender<Option<ferrite_core::TorrentMetaV2>>,
    },
    /// Force an immediate DHT announce (fire-and-forget at torrent level).
    ForceDhtAnnounce,
    /// Read all data for a specific piece from disk.
    ReadPiece {
        index: u32,
        reply: oneshot::Sender<crate::Result<bytes::Bytes>>,
    },
    /// Flush the disk write cache for this torrent.
    FlushCache {
        reply: oneshot::Sender<crate::Result<()>>,
    },
    /// Clear the error state and resume if the torrent was paused due to error.
    ClearError,
    /// Get per-file open/mode status based on torrent state.
    FileStatus {
        reply: oneshot::Sender<Vec<crate::types::FileStatus>>,
    },
    /// Read the current torrent flags as a bitflag set.
    Flags {
        reply: oneshot::Sender<TorrentFlags>,
    },
    /// Set (enable) the specified torrent flags.
    SetFlags {
        flags: TorrentFlags,
        reply: oneshot::Sender<()>,
    },
    /// Unset (disable) the specified torrent flags.
    UnsetFlags {
        flags: TorrentFlags,
        reply: oneshot::Sender<()>,
    },
    /// Immediately initiate a peer connection to the given address.
    ConnectPeer {
        addr: SocketAddr,
    },
}

/// Per-peer details exported for client UI introspection.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    /// Remote peer address (IP + port).
    pub addr: SocketAddr,
    /// Client identification string (from extension handshake `v` field, or empty).
    pub client: String,
    /// Whether the peer is choking us.
    pub peer_choking: bool,
    /// Whether the peer is interested in our data.
    pub peer_interested: bool,
    /// Whether we are choking the peer.
    pub am_choking: bool,
    /// Whether we are interested in the peer's data.
    pub am_interested: bool,
    /// Current download rate from this peer in bytes/sec.
    pub download_rate: u64,
    /// Current upload rate to this peer in bytes/sec.
    pub upload_rate: u64,
    /// Number of pieces the peer has (bitfield population count).
    pub num_pieces: u32,
    /// How the peer was discovered.
    pub source: crate::peer_state::PeerSource,
    /// Whether the peer supports BEP 6 Fast Extension.
    pub supports_fast: bool,
    /// Whether the peer declared upload-only status (BEP 21).
    pub upload_only: bool,
    /// Whether the peer is snubbed (no data for snub_timeout_secs).
    pub snubbed: bool,
    /// Seconds since the peer connection was established.
    pub connected_duration_secs: u64,
    /// Number of outstanding piece requests to this peer.
    pub num_pending_requests: usize,
    /// Number of incoming piece requests from this peer.
    pub num_incoming_requests: usize,
}

/// In-flight piece download status for the download queue.
#[derive(Debug, Clone)]
pub struct PartialPieceInfo {
    /// Index of the piece being downloaded.
    pub piece_index: u32,
    /// Total number of blocks in this piece.
    pub blocks_in_piece: u32,
    /// Number of blocks that have been assigned to peers.
    pub blocks_assigned: u32,
}

/// Info about a file within a torrent.
#[derive(Debug, Clone)]
pub struct FileInfo {
    /// Relative path of the file within the torrent.
    pub path: PathBuf,
    /// File size in bytes.
    pub length: u64,
}

/// Metadata about a torrent (available after metadata is fetched).
#[derive(Debug, Clone)]
pub struct TorrentInfo {
    /// SHA-1 info hash of the torrent.
    pub info_hash: ferrite_core::Id20,
    /// Display name from the torrent metadata.
    pub name: String,
    /// Total size of all files in bytes.
    pub total_length: u64,
    /// Size of each piece in bytes (last piece may be smaller).
    pub piece_length: u64,
    /// Total number of pieces in the torrent.
    pub num_pieces: u32,
    /// List of files contained in the torrent.
    pub files: Vec<FileInfo>,
    /// Whether this is a private torrent (DHT/PEX disabled).
    pub private: bool,
}

/// Aggregate statistics for the whole session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStats {
    /// Number of non-paused torrents in the session.
    pub active_torrents: usize,
    /// Total bytes downloaded across all torrents since session start.
    pub total_downloaded: u64,
    /// Total bytes uploaded across all torrents since session start.
    pub total_uploaded: u64,
    /// Number of nodes in the DHT routing table.
    pub dht_nodes: usize,
}

/// Whether a file in a torrent is open and its I/O access mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    /// File is open for reading only (e.g. seeding).
    ReadOnly,
    /// File is open for reading and writing (e.g. downloading).
    ReadWrite,
    /// File is not currently open.
    Closed,
}

/// Status of a single file within a torrent.
#[derive(Debug, Clone)]
pub struct FileStatus {
    /// Whether the file is currently open.
    pub open: bool,
    /// The current access mode.
    pub mode: FileMode,
}

bitflags! {
    /// Bitflag convenience wrapper for common torrent state flags.
    ///
    /// These map to existing torrent actor fields; `set_flags` / `unset_flags`
    /// delegate to the underlying operations (pause/resume, set_sequential, etc.).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TorrentFlags: u32 {
        /// Torrent is paused.
        const PAUSED = 0x1;
        /// Torrent is auto-managed by the session queuing system.
        const AUTO_MANAGED = 0x2;
        /// Sequential (in-order) piece downloading is enabled.
        const SEQUENTIAL_DOWNLOAD = 0x4;
        /// BEP 16 super seeding mode is active.
        const SUPER_SEEDING = 0x8;
        /// Upload-only status (seeding complete, no wanted pieces).
        const UPLOAD_ONLY = 0x10;
    }
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
            pieces_total: 10,
            ..Default::default()
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
    fn torrent_stats_default_values() {
        let stats = TorrentStats::default();

        // State
        assert_eq!(stats.state, TorrentState::Paused);

        // Original fields are zeroed
        assert_eq!(stats.downloaded, 0);
        assert_eq!(stats.uploaded, 0);
        assert_eq!(stats.pieces_have, 0);
        assert_eq!(stats.pieces_total, 0);
        assert_eq!(stats.peers_connected, 0);
        assert_eq!(stats.peers_available, 0);
        assert!((stats.checking_progress - 0.0).abs() < f32::EPSILON);
        assert!(stats.peers_by_source.is_empty());

        // Identity: zeroed info hash
        assert_eq!(
            stats.info_hashes,
            ferrite_core::InfoHashes::v1_only(ferrite_core::Id20::from([0u8; 20]))
        );
        assert!(stats.name.is_empty());

        // State flags are all false
        assert!(!stats.has_metadata);
        assert!(!stats.is_seeding);
        assert!(!stats.is_finished);
        assert!(!stats.is_paused);
        assert!(!stats.auto_managed);
        assert!(!stats.sequential_download);
        assert!(!stats.super_seeding);
        assert!(!stats.has_incoming);
        assert!(!stats.need_save_resume);
        assert!(!stats.moving_storage);

        // Progress
        assert!((stats.progress - 0.0).abs() < f32::EPSILON);
        assert_eq!(stats.progress_ppm, 0);
        assert_eq!(stats.total_done, 0);
        assert_eq!(stats.total, 0);
        assert_eq!(stats.total_wanted_done, 0);
        assert_eq!(stats.total_wanted, 0);
        assert_eq!(stats.block_size, 16384);

        // Sentinel values
        assert_eq!(stats.num_complete, -1);
        assert_eq!(stats.num_incomplete, -1);
        assert_eq!(stats.queue_position, -1);
        assert_eq!(stats.error_file, -1);

        // Strings are empty
        assert!(stats.current_tracker.is_empty());
        assert!(stats.save_path.is_empty());
        assert!(stats.error.is_empty());

        // Rates are zero
        assert_eq!(stats.download_rate, 0);
        assert_eq!(stats.upload_rate, 0);
        assert_eq!(stats.download_payload_rate, 0);
        assert_eq!(stats.upload_payload_rate, 0);

        // Distributed copies
        assert_eq!(stats.distributed_full_copies, 0);
        assert_eq!(stats.distributed_fraction, 0);
        assert!((stats.distributed_copies - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn torrent_stats_seeding_flags() {
        let stats = TorrentStats {
            state: TorrentState::Seeding,
            is_seeding: true,
            is_finished: true,
            has_metadata: true,
            progress: 1.0,
            progress_ppm: 1_000_000,
            ..Default::default()
        };
        assert_eq!(stats.state, TorrentState::Seeding);
        assert!(stats.is_seeding);
        assert!(stats.is_finished);
        assert!(stats.has_metadata);
        assert!((stats.progress - 1.0).abs() < f32::EPSILON);
        assert_eq!(stats.progress_ppm, 1_000_000);
        // Other fields remain default
        assert!(!stats.is_paused);
        assert_eq!(stats.downloaded, 0);
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
    fn torrent_config_choking_defaults() {
        let cfg = TorrentConfig::default();
        assert_eq!(cfg.seed_choking_algorithm, SeedChokingAlgorithm::FastestUpload);
        assert_eq!(cfg.choking_algorithm, ChokingAlgorithm::FixedSlots);
    }

    #[test]
    fn torrent_config_m44_defaults() {
        let cfg = TorrentConfig::default();
        assert!(cfg.piece_extent_affinity);
        assert!(!cfg.suggest_mode);
        assert_eq!(cfg.max_suggest_pieces, 10);
        assert_eq!(cfg.predictive_piece_announce_ms, 0);
    }

    #[test]
    fn torrent_config_from_settings_choking() {
        let mut s = crate::settings::Settings::default();
        s.seed_choking_algorithm = SeedChokingAlgorithm::RoundRobin;
        s.choking_algorithm = ChokingAlgorithm::RateBased;
        let cfg = TorrentConfig::from(&s);
        assert_eq!(cfg.seed_choking_algorithm, SeedChokingAlgorithm::RoundRobin);
        assert_eq!(cfg.choking_algorithm, ChokingAlgorithm::RateBased);
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

    #[test]
    fn torrent_config_peer_turnover_defaults() {
        let cfg = TorrentConfig::default();
        assert!((cfg.peer_turnover - 0.04).abs() < f64::EPSILON);
        assert!((cfg.peer_turnover_cutoff - 0.9).abs() < f64::EPSILON);
        assert_eq!(cfg.peer_turnover_interval, 300);
    }

    #[test]
    fn torrent_config_from_settings_peer_turnover() {
        let mut s = crate::settings::Settings::default();
        s.peer_turnover = 0.1;
        s.peer_turnover_cutoff = 0.75;
        s.peer_turnover_interval = 120;
        let cfg = TorrentConfig::from(&s);
        assert!((cfg.peer_turnover - 0.1).abs() < f64::EPSILON);
        assert!((cfg.peer_turnover_cutoff - 0.75).abs() < f64::EPSILON);
        assert_eq!(cfg.peer_turnover_interval, 120);
    }

    #[test]
    fn torrent_config_url_security_default() {
        let cfg = TorrentConfig::default();
        assert!(cfg.url_security.ssrf_mitigation);
        assert!(!cfg.url_security.allow_idna);
        assert!(cfg.url_security.validate_https_trackers);
    }

    #[test]
    fn torrent_config_url_security_from_settings() {
        let mut s = crate::settings::Settings::default();
        s.ssrf_mitigation = false;
        s.allow_idna = true;
        s.validate_https_trackers = false;
        let cfg = TorrentConfig::from(&s);
        assert!(!cfg.url_security.ssrf_mitigation);
        assert!(cfg.url_security.allow_idna);
        assert!(!cfg.url_security.validate_https_trackers);
    }

    #[test]
    fn torrent_config_peer_dscp_default() {
        let cfg = TorrentConfig::default();
        assert_eq!(cfg.peer_dscp, 0x08);
    }

    #[test]
    fn torrent_config_peer_dscp_from_settings() {
        let mut s = crate::settings::Settings::default();
        s.peer_dscp = 0x2E;
        let cfg = TorrentConfig::from(&s);
        assert_eq!(cfg.peer_dscp, 0x2E);
    }
}
