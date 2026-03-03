//! Unified settings pack for session configuration.
//!
//! Replaces the old `SessionConfig` with a single strongly-typed struct that
//! consolidates all configurable knobs. Supports presets, validation, and
//! serde serialization (bencode + JSON).

use std::net::IpAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use ferrite_core::StorageMode;
use ferrite_wire::mse::EncryptionMode;

use crate::alert::AlertCategory;
use crate::choker::{ChokingAlgorithm, SeedChokingAlgorithm};
use crate::proxy::ProxyConfig;
use crate::rate_limiter::MixedModeAlgorithm;

// ── Serde default helpers ────────────────────────────────────────────

fn default_true() -> bool {
    true
}
fn default_listen_port() -> u16 {
    6881
}
fn default_download_dir() -> PathBuf {
    PathBuf::from(".")
}
fn default_max_torrents() -> usize {
    100
}
fn default_encryption() -> EncryptionMode {
    EncryptionMode::Enabled
}
fn default_auto_upload_slots_min() -> usize {
    2
}
fn default_auto_upload_slots_max() -> usize {
    20
}
fn default_active_downloads() -> i32 {
    3
}
fn default_active_seeds() -> i32 {
    5
}
fn default_active_limit() -> i32 {
    500
}
fn default_active_checking() -> i32 {
    1
}
fn default_inactive_rate() -> u64 {
    2048
}
fn default_auto_manage_interval() -> u64 {
    30
}
fn default_auto_manage_startup() -> u64 {
    60
}
fn default_alert_mask() -> AlertCategory {
    AlertCategory::ALL
}
fn default_alert_channel_size() -> usize {
    1024
}
fn default_smart_ban_max_failures() -> u32 {
    3
}
fn default_disk_io_threads() -> usize {
    4
}
fn default_storage_mode() -> StorageMode {
    StorageMode::Auto
}
fn default_disk_cache_size() -> usize {
    64 * 1024 * 1024
}
fn default_disk_write_cache_ratio() -> f32 {
    0.25
}
fn default_disk_channel_capacity() -> usize {
    512
}
fn default_hashing_threads() -> usize {
    2
}
fn default_max_request_queue_depth() -> usize {
    250
}
fn default_request_queue_time() -> f64 {
    3.0
}
fn default_block_request_timeout() -> u32 {
    60
}
fn default_max_concurrent_streams() -> usize {
    8
}
fn default_dht_qps() -> usize {
    50
}
fn default_dht_timeout() -> u64 {
    10
}
fn default_upnp_lease() -> u32 {
    3600
}
fn default_natpmp_lifetime() -> u32 {
    7200
}
fn default_utp_max_conns() -> usize {
    256
}
fn default_dht_max_items() -> usize {
    700
}
fn default_dht_item_lifetime() -> u64 {
    7200
}
fn default_dht_sample_interval() -> u64 {
    0
}
fn default_max_suggest_pieces() -> usize {
    10
}
fn default_predictive_piece_announce_ms() -> u64 {
    0
}
fn default_ssl_listen_port() -> u16 {
    0 // 0 = disabled
}
fn default_seed_choking_algorithm() -> SeedChokingAlgorithm {
    SeedChokingAlgorithm::FastestUpload
}
fn default_choking_algorithm() -> ChokingAlgorithm {
    ChokingAlgorithm::FixedSlots
}
fn default_mixed_mode() -> MixedModeAlgorithm {
    MixedModeAlgorithm::PeerProportional
}
fn default_peer_turnover() -> f64 {
    0.04
}
fn default_peer_turnover_cutoff() -> f64 {
    0.9
}
fn default_peer_turnover_interval() -> u64 {
    300
}
fn default_peer_dscp() -> u8 {
    0x08 // CS1 (scavenger/low-priority)
}
fn default_max_peers_per_torrent() -> usize {
    200
}
fn default_stats_report_interval() -> u64 {
    1000
}
fn default_i2p_hostname() -> String {
    "127.0.0.1".into()
}
fn default_i2p_port() -> u16 {
    7656
}
fn default_i2p_tunnel_quantity() -> u8 {
    3
}
fn default_i2p_tunnel_length() -> u8 {
    3
}

// ── Settings ─────────────────────────────────────────────────────────

/// Unified session settings (replaces `SessionConfig`).
///
/// All 56 configurable fields in a single strongly-typed struct.
/// Supports presets via factory functions and runtime mutation via
/// `SessionHandle::apply_settings()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    // ── General ──
    /// TCP listen port for incoming peer connections (default: 6881).
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
    /// Default download directory for new torrents (default: ".").
    #[serde(default = "default_download_dir")]
    pub download_dir: PathBuf,
    /// Maximum number of concurrent torrents (default: 100).
    #[serde(default = "default_max_torrents")]
    pub max_torrents: usize,
    /// Directory for fast-resume data files. If `None`, resume data is not persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_data_dir: Option<PathBuf>,

    // ── Protocol features ──
    /// Enable Kademlia DHT peer discovery (BEP 5). Default: true.
    #[serde(default = "default_true")]
    pub enable_dht: bool,
    /// Enable Peer Exchange (BEP 11). Default: true.
    #[serde(default = "default_true")]
    pub enable_pex: bool,
    /// Enable Local Service Discovery via multicast (BEP 14). Default: true.
    #[serde(default = "default_true")]
    pub enable_lsd: bool,
    /// Enable BEP 6 Fast Extension (AllowedFast, HaveAll, HaveNone, Reject,
    /// SuggestPiece). Default: true.
    #[serde(default = "default_true")]
    pub enable_fast_extension: bool,
    /// Enable uTP (BEP 29) micro transport protocol. When enabled, outbound
    /// connections try uTP first with a 5-second timeout before falling back
    /// to TCP. Default: true.
    #[serde(default = "default_true")]
    pub enable_utp: bool,
    /// Enable UPnP IGD port mapping (last resort after PCP and NAT-PMP).
    /// Default: true.
    #[serde(default = "default_true")]
    pub enable_upnp: bool,
    /// Enable NAT-PMP (RFC 6886) and PCP (RFC 6887) port mapping.
    /// PCP is tried first, then NAT-PMP as fallback. Default: true.
    #[serde(default = "default_true")]
    pub enable_natpmp: bool,
    /// Enable IPv6 dual-stack support (BEP 7, 24). Binds listeners on both
    /// IPv4 and IPv6, starts a second DHT instance, and processes IPv6 peers
    /// in PEX and tracker responses. Default: true.
    #[serde(default = "default_true")]
    pub enable_ipv6: bool,
    /// Enable HTTP/web seeding (BEP 19 GetRight, BEP 17 Hoffman). Torrents
    /// with `url-list` or `httpseeds` download pieces from HTTP servers
    /// alongside peer-to-peer transfers. Default: true.
    #[serde(default = "default_true")]
    pub enable_web_seed: bool,
    /// Enable BEP 55 holepunch extension for NAT traversal. Advertises
    /// `ut_holepunch` in the extension handshake and can act as initiator,
    /// relay, or target for holepunch connections. Default: true.
    #[serde(default = "default_true")]
    pub enable_holepunch: bool,
    /// Connection encryption mode (MSE/PE). Default: Enabled.
    #[serde(default = "default_encryption")]
    pub encryption_mode: EncryptionMode,
    /// Suppress identifying information (client version in BEP 10 handshake)
    /// and disable DHT, LSD, UPnP, and NAT-PMP. Default: false.
    #[serde(default)]
    pub anonymous_mode: bool,
    /// Manually configured external IP for BEP 40 peer priority.
    /// If not set, discovered automatically via NAT traversal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ip: Option<IpAddr>,

    // ── Seeding ──
    /// Stop seeding when this upload/download ratio is reached. `None` = unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_ratio_limit: Option<f64>,
    /// Enable BEP 16 super seeding for new torrents. Reveals pieces one-per-peer
    /// to maximize piece diversity across the swarm. Default: false.
    #[serde(default)]
    pub default_super_seeding: bool,
    /// Default share mode for new torrents. When true, torrents relay pieces
    /// in memory without writing to disk. Requires fast extension (BEP 6).
    #[serde(default)]
    pub default_share_mode: bool,
    /// Advertise upload-only status via extension handshake when a torrent
    /// transitions to seeding (BEP 21). Default: true.
    #[serde(default = "default_true")]
    pub upload_only_announce: bool,
    /// Have message batching delay in milliseconds. When > 0, Have messages are
    /// buffered and sent in batches; if the batch exceeds 50% of total pieces,
    /// a full Bitfield is sent instead. Default: 0 (immediate).
    #[serde(default)]
    pub have_send_delay_ms: u64,

    // ── Rate limiting ──
    /// Global upload rate limit in bytes/sec (0 = unlimited).
    #[serde(default)]
    pub upload_rate_limit: u64,
    /// Global download rate limit in bytes/sec (0 = unlimited).
    #[serde(default)]
    pub download_rate_limit: u64,
    /// TCP upload rate limit in bytes/sec (0 = unlimited).
    #[serde(default)]
    pub tcp_upload_rate_limit: u64,
    /// TCP download rate limit in bytes/sec (0 = unlimited).
    #[serde(default)]
    pub tcp_download_rate_limit: u64,
    /// uTP upload rate limit in bytes/sec (0 = unlimited).
    #[serde(default)]
    pub utp_upload_rate_limit: u64,
    /// uTP download rate limit in bytes/sec (0 = unlimited).
    #[serde(default)]
    pub utp_download_rate_limit: u64,
    /// Automatically adjust the number of upload slots based on bandwidth. Default: true.
    #[serde(default = "default_true")]
    pub auto_upload_slots: bool,
    /// Minimum number of automatic upload slots (default: 2).
    #[serde(default = "default_auto_upload_slots_min")]
    pub auto_upload_slots_min: usize,
    /// Maximum number of automatic upload slots (default: 20).
    #[serde(default = "default_auto_upload_slots_max")]
    pub auto_upload_slots_max: usize,
    /// Mixed-mode TCP/uTP bandwidth allocation algorithm.
    #[serde(default = "default_mixed_mode")]
    pub mixed_mode_algorithm: MixedModeAlgorithm,

    // ── Queue management ──
    /// Maximum concurrent auto-managed downloading torrents (-1 = unlimited, default: 3).
    #[serde(default = "default_active_downloads")]
    pub active_downloads: i32,
    /// Maximum concurrent auto-managed seeding torrents (-1 = unlimited, default: 5).
    #[serde(default = "default_active_seeds")]
    pub active_seeds: i32,
    /// Hard cap on all active auto-managed torrents (-1 = unlimited, default: 500).
    #[serde(default = "default_active_limit")]
    pub active_limit: i32,
    /// Maximum concurrent hash-check operations (default: 1).
    #[serde(default = "default_active_checking")]
    pub active_checking: i32,
    /// Exempt inactive torrents from download/seed limits. A torrent is inactive
    /// if its rate is below `inactive_down_rate` / `inactive_up_rate`. Default: true.
    #[serde(default = "default_true")]
    pub dont_count_slow_torrents: bool,
    /// Download rate threshold (bytes/sec) below which a torrent is considered
    /// inactive for queue management purposes (default: 2048).
    #[serde(default = "default_inactive_rate")]
    pub inactive_down_rate: u64,
    /// Upload rate threshold (bytes/sec) below which a torrent is considered
    /// inactive for queue management purposes (default: 2048).
    #[serde(default = "default_inactive_rate")]
    pub inactive_up_rate: u64,
    /// Interval in seconds between queue evaluations (default: 30).
    #[serde(default = "default_auto_manage_interval")]
    pub auto_manage_interval: u64,
    /// Grace period in seconds where a torrent is considered active regardless
    /// of speed after being started (default: 60).
    #[serde(default = "default_auto_manage_startup")]
    pub auto_manage_startup: u64,
    /// Allocate seeding slots before download slots. Default: false.
    #[serde(default)]
    pub auto_manage_prefer_seeds: bool,

    // ── Alerts ──
    /// Bitmask of alert categories to receive (default: ALL).
    #[serde(default = "default_alert_mask")]
    pub alert_mask: AlertCategory,
    /// Capacity of the alert broadcast channel (default: 1024).
    #[serde(default = "default_alert_channel_size")]
    pub alert_channel_size: usize,

    // ── Smart banning ──
    /// Number of hash-failure involvements before a peer is auto-banned.
    /// Lower values ban faster but risk false positives (default: 3).
    #[serde(default = "default_smart_ban_max_failures")]
    pub smart_ban_max_failures: u32,
    /// Enable parole mode: re-download a failed piece from a single uninvolved
    /// peer to definitively attribute fault before striking. Default: true.
    #[serde(default = "default_true")]
    pub smart_ban_parole: bool,

    // ── Disk I/O ──
    /// Number of concurrent disk I/O threads (default: 4).
    #[serde(default = "default_disk_io_threads")]
    pub disk_io_threads: usize,
    /// Storage allocation mode: Auto, FullPreallocate, or SparseFile (default: Auto).
    #[serde(default = "default_storage_mode")]
    pub storage_mode: StorageMode,
    /// Total ARC disk cache size in bytes (default: 64 MiB, minimum: 1 MiB).
    #[serde(default = "default_disk_cache_size")]
    pub disk_cache_size: usize,
    /// Fraction of disk cache reserved for write buffering (0.0–1.0, default: 0.25).
    #[serde(default = "default_disk_write_cache_ratio")]
    pub disk_write_cache_ratio: f32,
    /// Capacity of the async disk I/O command channel (default: 512).
    #[serde(default = "default_disk_channel_capacity")]
    pub disk_channel_capacity: usize,

    // ── Hashing & piece picking ──
    /// Number of concurrent piece hash verification threads (default: 2).
    #[serde(default = "default_hashing_threads")]
    pub hashing_threads: usize,
    /// Maximum per-peer request queue depth (default: 250).
    #[serde(default = "default_max_request_queue_depth")]
    pub max_request_queue_depth: usize,
    /// Request queue time multiplier in seconds. Controls how many seconds
    /// worth of requests to keep in flight per peer (default: 3.0).
    #[serde(default = "default_request_queue_time")]
    pub request_queue_time: f64,
    /// Block request timeout in seconds before the request is considered
    /// lost and re-issued (default: 60).
    #[serde(default = "default_block_request_timeout")]
    pub block_request_timeout_secs: u32,
    /// Maximum concurrent `FileStream` readers. Controls how many simultaneous
    /// file-streaming reads can proceed (default: 8).
    #[serde(default = "default_max_concurrent_streams")]
    pub max_concurrent_stream_reads: usize,
    /// Automatically switch to sequential piece picking when too many partial
    /// pieces accumulate. Uses hysteresis (1.6x activate / 1.3x deactivate).
    #[serde(default = "default_true")]
    pub auto_sequential: bool,

    // ── Piece picker enhancements (M44) ──
    /// Prefer pieces adjacent to those already downloaded for improved sequential
    /// disk access patterns (4 MiB extent groups). Default: true.
    #[serde(default = "default_true")]
    pub piece_extent_affinity: bool,
    /// Enable BEP 6 SuggestPiece: suggest newly verified pieces to peers that
    /// don't have them, improving piece diversity in the swarm. Default: false.
    #[serde(default)]
    pub suggest_mode: bool,
    /// Maximum SuggestPiece messages per peer to avoid flooding (default: 10).
    #[serde(default = "default_max_suggest_pieces")]
    pub max_suggest_pieces: usize,
    /// Predictive piece announce delay in milliseconds. When > 0, a Have message
    /// is sent before hash verification completes, reducing piece availability
    /// latency at the cost of a possible false announce. Default: 0 (disabled).
    #[serde(default = "default_predictive_piece_announce_ms")]
    pub predictive_piece_announce_ms: u64,

    // ── Proxy ──
    /// Proxy configuration for peer and tracker connections. Default: no proxy.
    #[serde(default)]
    pub proxy: ProxyConfig,
    /// Force all connections through the configured proxy. Disables listen
    /// sockets, UPnP, NAT-PMP, DHT, and LSD. Default: false.
    #[serde(default)]
    pub force_proxy: bool,
    /// Check tracker IP addresses against the IP filter. When false, trackers
    /// are exempt from IP filtering. Default: true.
    #[serde(default = "default_true")]
    pub apply_ip_filter_to_trackers: bool,

    // ── DHT tuning ──
    /// Maximum DHT queries per second to control network traffic (default: 50).
    #[serde(default = "default_dht_qps")]
    pub dht_queries_per_second: usize,
    /// Timeout in seconds for a single DHT query before it is abandoned (default: 10).
    #[serde(default = "default_dht_timeout")]
    pub dht_query_timeout_secs: u64,
    /// BEP 42: Enforce node ID verification in DHT routing table.
    #[serde(default = "default_true")]
    pub dht_enforce_node_id: bool,
    /// BEP 42: Restrict DHT routing table to one node per IP.
    #[serde(default = "default_true")]
    pub dht_restrict_routing_ips: bool,
    /// Maximum number of BEP 44 items stored in the DHT (immutable + mutable).
    #[serde(default = "default_dht_max_items")]
    pub dht_max_items: usize,
    /// Lifetime of BEP 44 DHT items in seconds before expiry (default: 7200 = 2 hours).
    #[serde(default = "default_dht_item_lifetime")]
    pub dht_item_lifetime_secs: u64,
    /// Interval in seconds for periodic sample_infohashes queries (BEP 51).
    /// 0 = disabled (default). Non-zero enables background DHT indexing.
    #[serde(default = "default_dht_sample_interval")]
    pub dht_sample_infohashes_interval: u64,

    // ── NAT tuning ──
    /// UPnP lease duration in seconds (default: 3600).
    #[serde(default = "default_upnp_lease")]
    pub upnp_lease_duration: u32,
    /// NAT-PMP mapping lifetime in seconds (default: 7200).
    #[serde(default = "default_natpmp_lifetime")]
    pub natpmp_lifetime: u32,

    // ── uTP tuning ──
    /// Maximum concurrent uTP connections (default: 256).
    #[serde(default = "default_utp_max_conns")]
    pub utp_max_connections: usize,

    // ── I2P ──
    /// Enable I2P anonymous network support (requires SAM bridge).
    #[serde(default)]
    pub enable_i2p: bool,
    /// SAM bridge hostname (default: "127.0.0.1").
    #[serde(default = "default_i2p_hostname")]
    pub i2p_hostname: String,
    /// SAM bridge port (default: 7656).
    #[serde(default = "default_i2p_port")]
    pub i2p_port: u16,
    /// Number of inbound I2P tunnels (1-16, default: 3).
    #[serde(default = "default_i2p_tunnel_quantity")]
    pub i2p_inbound_quantity: u8,
    /// Number of outbound I2P tunnels (1-16, default: 3).
    #[serde(default = "default_i2p_tunnel_quantity")]
    pub i2p_outbound_quantity: u8,
    /// Number of hops in inbound I2P tunnels (0-7, default: 3).
    #[serde(default = "default_i2p_tunnel_length")]
    pub i2p_inbound_length: u8,
    /// Number of hops in outbound I2P tunnels (0-7, default: 3).
    #[serde(default = "default_i2p_tunnel_length")]
    pub i2p_outbound_length: u8,
    /// Allow mixing I2P and clearnet peers in the same torrent.
    /// When false (default), I2P-enabled torrents only connect to I2P peers.
    #[serde(default)]
    pub allow_i2p_mixed: bool,

    // ── SSL torrents (M42) ──
    /// SSL listen port for SSL torrent incoming connections.
    /// 0 = disabled (no SSL listener). When set, a TLS listener is bound
    /// on this port for torrents with `ssl-cert` in their info dict.
    #[serde(default = "default_ssl_listen_port")]
    pub ssl_listen_port: u16,
    /// Path to the PEM-encoded certificate file for SSL torrent connections.
    /// If not set, a self-signed certificate is auto-generated on first use
    /// and stored in `resume_data_dir` (or a temp directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssl_cert_path: Option<PathBuf>,
    /// Path to the PEM-encoded private key file for SSL torrent connections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssl_key_path: Option<PathBuf>,

    // ── Choking algorithms (M43) ──
    /// Algorithm for ranking peers during seed-mode choking.
    #[serde(default = "default_seed_choking_algorithm")]
    pub seed_choking_algorithm: SeedChokingAlgorithm,
    /// Algorithm for determining the number of unchoke slots.
    #[serde(default = "default_choking_algorithm")]
    pub choking_algorithm: ChokingAlgorithm,

    // ── Peer connections ──
    /// Maximum peer connections per torrent (default: 200).
    #[serde(default = "default_max_peers_per_torrent")]
    pub max_peers_per_torrent: usize,

    // ── Peer turnover ──
    /// Fraction of peers to disconnect per turnover interval (0.0–1.0, default: 0.04).
    #[serde(default = "default_peer_turnover")]
    pub peer_turnover: f64,
    /// Only trigger turnover if download rate < this fraction of peak rate (0.0–1.0, default: 0.9).
    #[serde(default = "default_peer_turnover_cutoff")]
    pub peer_turnover_cutoff: f64,
    /// Seconds between turnover checks (default: 300, 0 = disabled).
    #[serde(default = "default_peer_turnover_interval")]
    pub peer_turnover_interval: u64,

    // ── Security ──
    /// Enable SSRF mitigation: restrict localhost tracker paths, block
    /// public-to-private redirects, and reject query strings on local web seeds.
    #[serde(default = "default_true")]
    pub ssrf_mitigation: bool,
    /// Allow internationalised (non-ASCII) domain names in tracker/web seed URLs.
    #[serde(default)]
    pub allow_idna: bool,
    /// Require HTTPS for HTTP tracker announces (UDP trackers are unaffected).
    #[serde(default = "default_true")]
    pub validate_https_trackers: bool,
    /// DSCP (Differentiated Services Code Point) value for peer traffic sockets.
    /// Applied to TCP listeners, outbound TCP connections, uTP sockets, and UDP tracker sockets.
    /// Default 0x08 (CS1/scavenger — low-priority background). Set to 0 to disable DSCP marking.
    #[serde(default = "default_peer_dscp")]
    pub peer_dscp: u8,

    // ── Session Stats (M50) ──
    /// Interval in milliseconds between `SessionStatsAlert` emissions.
    /// Default 1000 (1 second). Set to 0 to disable periodic stats alerts.
    #[serde(default = "default_stats_report_interval")]
    pub stats_report_interval: u64,

    // ── DHT bootstrap (M56) ──
    /// Previously saved DHT routing table nodes for fast bootstrap.
    /// These are prepended to the bootstrap node list on startup so that
    /// peer discovery starts instantly instead of bootstrapping from scratch.
    /// Runtime-injected, not serialized.
    #[serde(skip)]
    pub dht_saved_nodes: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            // General
            listen_port: 6881,
            download_dir: PathBuf::from("."),
            max_torrents: 100,
            resume_data_dir: None,
            // Protocol features
            enable_dht: true,
            enable_pex: true,
            enable_lsd: true,
            enable_fast_extension: true,
            enable_utp: true,
            enable_upnp: true,
            enable_natpmp: true,
            enable_ipv6: true,
            enable_web_seed: true,
            enable_holepunch: true,
            encryption_mode: EncryptionMode::Enabled,
            anonymous_mode: false,
            external_ip: None,
            // Seeding
            seed_ratio_limit: None,
            default_super_seeding: false,
            default_share_mode: false,
            upload_only_announce: true,
            have_send_delay_ms: 0,
            // Rate limiting
            upload_rate_limit: 0,
            download_rate_limit: 0,
            tcp_upload_rate_limit: 0,
            tcp_download_rate_limit: 0,
            utp_upload_rate_limit: 0,
            utp_download_rate_limit: 0,
            auto_upload_slots: true,
            auto_upload_slots_min: 2,
            auto_upload_slots_max: 20,
            mixed_mode_algorithm: MixedModeAlgorithm::PeerProportional,
            // Queue management
            active_downloads: 3,
            active_seeds: 5,
            active_limit: 500,
            active_checking: 1,
            dont_count_slow_torrents: true,
            inactive_down_rate: 2048,
            inactive_up_rate: 2048,
            auto_manage_interval: 30,
            auto_manage_startup: 60,
            auto_manage_prefer_seeds: false,
            // Alerts
            alert_mask: AlertCategory::ALL,
            alert_channel_size: 1024,
            // Smart banning
            smart_ban_max_failures: 3,
            smart_ban_parole: true,
            // Disk I/O
            disk_io_threads: 4,
            storage_mode: StorageMode::Auto,
            disk_cache_size: 64 * 1024 * 1024,
            disk_write_cache_ratio: 0.25,
            disk_channel_capacity: 512,
            // Hashing & piece picking
            hashing_threads: 2,
            max_request_queue_depth: 250,
            request_queue_time: 3.0,
            block_request_timeout_secs: 60,
            max_concurrent_stream_reads: 8,
            auto_sequential: true,
            // Piece picker enhancements (M44)
            piece_extent_affinity: true,
            suggest_mode: false,
            max_suggest_pieces: 10,
            predictive_piece_announce_ms: 0,
            // Proxy
            proxy: ProxyConfig::default(),
            force_proxy: false,
            apply_ip_filter_to_trackers: true,
            // DHT tuning
            dht_queries_per_second: 50,
            dht_query_timeout_secs: 10,
            dht_enforce_node_id: true,
            dht_restrict_routing_ips: true,
            dht_max_items: 700,
            dht_item_lifetime_secs: 7200,
            dht_sample_infohashes_interval: 0,
            // NAT tuning
            upnp_lease_duration: 3600,
            natpmp_lifetime: 7200,
            // uTP tuning
            utp_max_connections: 256,
            // I2P
            enable_i2p: false,
            i2p_hostname: "127.0.0.1".into(),
            i2p_port: 7656,
            i2p_inbound_quantity: 3,
            i2p_outbound_quantity: 3,
            i2p_inbound_length: 3,
            i2p_outbound_length: 3,
            allow_i2p_mixed: false,
            // SSL torrents
            ssl_listen_port: 0,
            ssl_cert_path: None,
            ssl_key_path: None,
            // Choking algorithms
            seed_choking_algorithm: SeedChokingAlgorithm::FastestUpload,
            choking_algorithm: ChokingAlgorithm::FixedSlots,
            // Peer connections
            max_peers_per_torrent: 200,
            // Peer turnover
            peer_turnover: 0.04,
            peer_turnover_cutoff: 0.9,
            peer_turnover_interval: 300,
            // Security
            ssrf_mitigation: true,
            allow_idna: false,
            validate_https_trackers: true,
            peer_dscp: 0x08,
            // Session Stats (M50)
            stats_report_interval: 1000,
            // DHT bootstrap (M56)
            dht_saved_nodes: Vec::new(),
        }
    }
}

impl Settings {
    /// Preset for constrained/embedded environments.
    pub fn min_memory() -> Self {
        Self {
            disk_cache_size: 8 * 1024 * 1024,
            max_torrents: 20,
            max_peers_per_torrent: 30,
            active_downloads: 1,
            active_seeds: 2,
            active_limit: 10,
            alert_channel_size: 256,
            utp_max_connections: 64,
            max_request_queue_depth: 50,
            max_concurrent_stream_reads: 2,
            hashing_threads: 1,
            disk_io_threads: 1,
            dht_max_items: 100,
            ..Self::default()
        }
    }

    /// Preset for desktop/server environments with ample resources.
    pub fn high_performance() -> Self {
        Self {
            disk_cache_size: 256 * 1024 * 1024,
            max_torrents: 2000,
            max_peers_per_torrent: 500,
            active_downloads: 30,
            active_seeds: 100,
            active_limit: 2000,
            alert_channel_size: 4096,
            utp_max_connections: 1024,
            max_request_queue_depth: 1000,
            max_concurrent_stream_reads: 32,
            hashing_threads: 4,
            disk_io_threads: 8,
            auto_upload_slots_max: 100,
            suggest_mode: true,
            ..Self::default()
        }
    }

    /// Validate settings. Returns error on the first invalid combination found.
    pub fn validate(&self) -> crate::Result<()> {
        use crate::proxy::ProxyType;

        if self.force_proxy && self.proxy.proxy_type == ProxyType::None {
            return Err(crate::Error::InvalidSettings(
                "force_proxy is enabled but no proxy type is configured".into(),
            ));
        }

        if self.active_downloads > 0
            && self.active_limit > 0
            && self.active_downloads > self.active_limit
        {
            return Err(crate::Error::InvalidSettings(
                "active_downloads exceeds active_limit".into(),
            ));
        }

        if self.active_seeds > 0
            && self.active_limit > 0
            && self.active_seeds > self.active_limit
        {
            return Err(crate::Error::InvalidSettings(
                "active_seeds exceeds active_limit".into(),
            ));
        }

        if !(0.0..=1.0).contains(&self.disk_write_cache_ratio) {
            return Err(crate::Error::InvalidSettings(
                "disk_write_cache_ratio must be between 0.0 and 1.0".into(),
            ));
        }

        if self.disk_cache_size < 1024 * 1024 {
            return Err(crate::Error::InvalidSettings(
                "disk_cache_size must be at least 1 MiB".into(),
            ));
        }

        if self.hashing_threads == 0 {
            return Err(crate::Error::InvalidSettings(
                "hashing_threads must be at least 1".into(),
            ));
        }

        if self.disk_io_threads == 0 {
            return Err(crate::Error::InvalidSettings(
                "disk_io_threads must be at least 1".into(),
            ));
        }

        if self.default_share_mode && !self.enable_fast_extension {
            return Err(crate::Error::InvalidSettings(
                "share_mode requires enable_fast_extension for RejectRequest messages".into(),
            ));
        }

        // SSL cert/key must both be set or both absent
        if self.ssl_cert_path.is_some() != self.ssl_key_path.is_some() {
            return Err(crate::Error::InvalidSettings(
                "ssl_cert_path and ssl_key_path must both be set or both absent".into(),
            ));
        }

        if !(0.0..=1.0).contains(&self.peer_turnover) {
            return Err(crate::Error::InvalidSettings(
                "peer_turnover must be between 0.0 and 1.0".into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.peer_turnover_cutoff) {
            return Err(crate::Error::InvalidSettings(
                "peer_turnover_cutoff must be between 0.0 and 1.0".into(),
            ));
        }

        if self.enable_i2p {
            if self.i2p_inbound_quantity == 0 || self.i2p_inbound_quantity > 16 {
                return Err(crate::Error::InvalidSettings(
                    "i2p_inbound_quantity must be 1-16".into(),
                ));
            }
            if self.i2p_outbound_quantity == 0 || self.i2p_outbound_quantity > 16 {
                return Err(crate::Error::InvalidSettings(
                    "i2p_outbound_quantity must be 1-16".into(),
                ));
            }
            if self.i2p_inbound_length > 7 {
                return Err(crate::Error::InvalidSettings(
                    "i2p_inbound_length must be 0-7".into(),
                ));
            }
            if self.i2p_outbound_length > 7 {
                return Err(crate::Error::InvalidSettings(
                    "i2p_outbound_length must be 0-7".into(),
                ));
            }
        }

        Ok(())
    }
}

// ── Sub-config conversions ───────────────────────────────────────────

impl From<&Settings> for crate::disk::DiskConfig {
    fn from(s: &Settings) -> Self {
        Self {
            io_threads: s.disk_io_threads,
            storage_mode: s.storage_mode,
            cache_size: s.disk_cache_size,
            write_cache_ratio: s.disk_write_cache_ratio,
            channel_capacity: s.disk_channel_capacity,
        }
    }
}

impl From<&Settings> for crate::ban::BanConfig {
    fn from(s: &Settings) -> Self {
        Self {
            max_failures: s.smart_ban_max_failures,
            use_parole: s.smart_ban_parole,
        }
    }
}

impl Settings {
    pub(crate) fn to_dht_config(&self) -> ferrite_dht::DhtConfig {
        let default = ferrite_dht::DhtConfig::default();
        let mut bootstrap = self.dht_saved_nodes.clone();
        bootstrap.extend(default.bootstrap_nodes.iter().cloned());
        ferrite_dht::DhtConfig {
            bootstrap_nodes: bootstrap,
            queries_per_second: self.dht_queries_per_second,
            query_timeout: std::time::Duration::from_secs(self.dht_query_timeout_secs),
            enforce_node_id: self.dht_enforce_node_id,
            restrict_routing_ips: self.dht_restrict_routing_ips,
            dht_max_items: self.dht_max_items,
            dht_item_lifetime_secs: self.dht_item_lifetime_secs,
            ..default
        }
    }

    pub(crate) fn to_dht_config_v6(&self) -> ferrite_dht::DhtConfig {
        let default = ferrite_dht::DhtConfig::default_v6();
        let mut bootstrap = self.dht_saved_nodes.clone();
        bootstrap.extend(default.bootstrap_nodes.iter().cloned());
        ferrite_dht::DhtConfig {
            bootstrap_nodes: bootstrap,
            queries_per_second: self.dht_queries_per_second,
            query_timeout: std::time::Duration::from_secs(self.dht_query_timeout_secs),
            enforce_node_id: self.dht_enforce_node_id,
            restrict_routing_ips: self.dht_restrict_routing_ips,
            dht_max_items: self.dht_max_items,
            dht_item_lifetime_secs: self.dht_item_lifetime_secs,
            ..default
        }
    }

    pub(crate) fn to_nat_config(&self) -> ferrite_nat::NatConfig {
        ferrite_nat::NatConfig {
            enable_upnp: self.enable_upnp,
            enable_natpmp: self.enable_natpmp,
            upnp_lease_duration: self.upnp_lease_duration,
            natpmp_lifetime: self.natpmp_lifetime,
        }
    }

    pub(crate) fn to_utp_config(&self, port: u16) -> ferrite_utp::UtpConfig {
        ferrite_utp::UtpConfig {
            bind_addr: std::net::SocketAddr::from(([0, 0, 0, 0], port)),
            max_connections: self.utp_max_connections,
            dscp: self.peer_dscp,
        }
    }

    pub(crate) fn to_utp_config_v6(&self, port: u16) -> ferrite_utp::UtpConfig {
        ferrite_utp::UtpConfig {
            bind_addr: std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port)),
            max_connections: self.utp_max_connections,
            dscp: self.peer_dscp,
        }
    }

    /// Build a `SamTunnelConfig` from the I2P-related settings.
    pub(crate) fn to_sam_tunnel_config(&self) -> crate::i2p::SamTunnelConfig {
        crate::i2p::SamTunnelConfig {
            inbound_quantity: self.i2p_inbound_quantity,
            outbound_quantity: self.i2p_outbound_quantity,
            inbound_length: self.i2p_inbound_length,
            outbound_length: self.i2p_outbound_length,
        }
    }
}

// ── PartialEq (manual — f32/f64 fields need special handling) ────────

impl PartialEq for Settings {
    fn eq(&self, other: &Self) -> bool {
        self.listen_port == other.listen_port
            && self.download_dir == other.download_dir
            && self.max_torrents == other.max_torrents
            && self.resume_data_dir == other.resume_data_dir
            && self.enable_dht == other.enable_dht
            && self.enable_pex == other.enable_pex
            && self.enable_lsd == other.enable_lsd
            && self.enable_fast_extension == other.enable_fast_extension
            && self.enable_utp == other.enable_utp
            && self.enable_upnp == other.enable_upnp
            && self.enable_natpmp == other.enable_natpmp
            && self.enable_ipv6 == other.enable_ipv6
            && self.enable_web_seed == other.enable_web_seed
            && self.enable_holepunch == other.enable_holepunch
            && self.encryption_mode == other.encryption_mode
            && self.anonymous_mode == other.anonymous_mode
            && self.external_ip == other.external_ip
            && self.seed_ratio_limit == other.seed_ratio_limit
            && self.default_super_seeding == other.default_super_seeding
            && self.default_share_mode == other.default_share_mode
            && self.upload_only_announce == other.upload_only_announce
            && self.have_send_delay_ms == other.have_send_delay_ms
            && self.upload_rate_limit == other.upload_rate_limit
            && self.download_rate_limit == other.download_rate_limit
            && self.tcp_upload_rate_limit == other.tcp_upload_rate_limit
            && self.tcp_download_rate_limit == other.tcp_download_rate_limit
            && self.utp_upload_rate_limit == other.utp_upload_rate_limit
            && self.utp_download_rate_limit == other.utp_download_rate_limit
            && self.auto_upload_slots == other.auto_upload_slots
            && self.auto_upload_slots_min == other.auto_upload_slots_min
            && self.auto_upload_slots_max == other.auto_upload_slots_max
            && self.mixed_mode_algorithm == other.mixed_mode_algorithm
            && self.active_downloads == other.active_downloads
            && self.active_seeds == other.active_seeds
            && self.active_limit == other.active_limit
            && self.active_checking == other.active_checking
            && self.dont_count_slow_torrents == other.dont_count_slow_torrents
            && self.inactive_down_rate == other.inactive_down_rate
            && self.inactive_up_rate == other.inactive_up_rate
            && self.auto_manage_interval == other.auto_manage_interval
            && self.auto_manage_startup == other.auto_manage_startup
            && self.auto_manage_prefer_seeds == other.auto_manage_prefer_seeds
            && self.alert_mask == other.alert_mask
            && self.alert_channel_size == other.alert_channel_size
            && self.smart_ban_max_failures == other.smart_ban_max_failures
            && self.smart_ban_parole == other.smart_ban_parole
            && self.disk_io_threads == other.disk_io_threads
            && self.storage_mode == other.storage_mode
            && self.disk_cache_size == other.disk_cache_size
            && self.disk_write_cache_ratio.to_bits() == other.disk_write_cache_ratio.to_bits()
            && self.disk_channel_capacity == other.disk_channel_capacity
            && self.hashing_threads == other.hashing_threads
            && self.max_request_queue_depth == other.max_request_queue_depth
            && self.request_queue_time.to_bits() == other.request_queue_time.to_bits()
            && self.block_request_timeout_secs == other.block_request_timeout_secs
            && self.max_concurrent_stream_reads == other.max_concurrent_stream_reads
            && self.auto_sequential == other.auto_sequential
            && self.piece_extent_affinity == other.piece_extent_affinity
            && self.suggest_mode == other.suggest_mode
            && self.max_suggest_pieces == other.max_suggest_pieces
            && self.predictive_piece_announce_ms == other.predictive_piece_announce_ms
            && self.force_proxy == other.force_proxy
            && self.apply_ip_filter_to_trackers == other.apply_ip_filter_to_trackers
            && self.dht_queries_per_second == other.dht_queries_per_second
            && self.dht_query_timeout_secs == other.dht_query_timeout_secs
            && self.dht_enforce_node_id == other.dht_enforce_node_id
            && self.dht_restrict_routing_ips == other.dht_restrict_routing_ips
            && self.dht_max_items == other.dht_max_items
            && self.dht_item_lifetime_secs == other.dht_item_lifetime_secs
            && self.dht_sample_infohashes_interval == other.dht_sample_infohashes_interval
            && self.upnp_lease_duration == other.upnp_lease_duration
            && self.natpmp_lifetime == other.natpmp_lifetime
            && self.utp_max_connections == other.utp_max_connections
            && self.enable_i2p == other.enable_i2p
            && self.i2p_hostname == other.i2p_hostname
            && self.i2p_port == other.i2p_port
            && self.i2p_inbound_quantity == other.i2p_inbound_quantity
            && self.i2p_outbound_quantity == other.i2p_outbound_quantity
            && self.i2p_inbound_length == other.i2p_inbound_length
            && self.i2p_outbound_length == other.i2p_outbound_length
            && self.allow_i2p_mixed == other.allow_i2p_mixed
            && self.ssl_listen_port == other.ssl_listen_port
            && self.ssl_cert_path == other.ssl_cert_path
            && self.ssl_key_path == other.ssl_key_path
            && self.seed_choking_algorithm == other.seed_choking_algorithm
            && self.choking_algorithm == other.choking_algorithm
            && self.max_peers_per_torrent == other.max_peers_per_torrent
            && self.peer_turnover.to_bits() == other.peer_turnover.to_bits()
            && self.peer_turnover_cutoff.to_bits() == other.peer_turnover_cutoff.to_bits()
            && self.peer_turnover_interval == other.peer_turnover_interval
            && self.ssrf_mitigation == other.ssrf_mitigation
            && self.allow_idna == other.allow_idna
            && self.validate_https_trackers == other.validate_https_trackers
            && self.peer_dscp == other.peer_dscp
            && self.stats_report_interval == other.stats_report_interval
            && self.dht_saved_nodes == other.dht_saved_nodes
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_values() {
        let s = Settings::default();
        assert_eq!(s.listen_port, 6881);
        assert_eq!(s.download_dir, PathBuf::from("."));
        assert_eq!(s.max_torrents, 100);
        assert!(s.resume_data_dir.is_none());
        assert!(s.enable_dht);
        assert!(s.enable_pex);
        assert!(s.enable_lsd);
        assert!(s.enable_fast_extension);
        assert!(s.enable_utp);
        assert!(s.enable_upnp);
        assert!(s.enable_natpmp);
        assert!(s.enable_ipv6);
        assert!(s.enable_web_seed);
        assert_eq!(s.encryption_mode, EncryptionMode::Enabled);
        assert!(!s.anonymous_mode);
        assert!(s.seed_ratio_limit.is_none());
        assert!(!s.default_super_seeding);
        assert!(!s.default_share_mode);
        assert!(s.upload_only_announce);
        assert_eq!(s.upload_rate_limit, 0);
        assert_eq!(s.download_rate_limit, 0);
        assert!(s.auto_upload_slots);
        assert_eq!(s.active_downloads, 3);
        assert_eq!(s.active_seeds, 5);
        assert_eq!(s.active_limit, 500);
        assert_eq!(s.active_checking, 1);
        assert!(s.dont_count_slow_torrents);
        assert_eq!(s.alert_mask, AlertCategory::ALL);
        assert_eq!(s.alert_channel_size, 1024);
        assert_eq!(s.smart_ban_max_failures, 3);
        assert!(s.smart_ban_parole);
        assert_eq!(s.disk_io_threads, 4);
        assert_eq!(s.storage_mode, StorageMode::Auto);
        assert_eq!(s.disk_cache_size, 64 * 1024 * 1024);
        assert!((s.disk_write_cache_ratio - 0.25).abs() < f32::EPSILON);
        assert_eq!(s.disk_channel_capacity, 512);
        assert_eq!(s.hashing_threads, 2);
        assert_eq!(s.max_request_queue_depth, 250);
        assert!((s.request_queue_time - 3.0).abs() < f64::EPSILON);
        assert_eq!(s.block_request_timeout_secs, 60);
        assert_eq!(s.max_concurrent_stream_reads, 8);
        assert!(!s.force_proxy);
        assert!(s.apply_ip_filter_to_trackers);
        assert_eq!(s.dht_queries_per_second, 50);
        assert_eq!(s.dht_query_timeout_secs, 10);
        assert!(s.dht_enforce_node_id);
        assert!(s.dht_restrict_routing_ips);
        assert_eq!(s.upnp_lease_duration, 3600);
        assert_eq!(s.natpmp_lifetime, 7200);
        assert_eq!(s.utp_max_connections, 256);
        assert_eq!(s.mixed_mode_algorithm, MixedModeAlgorithm::PeerProportional);
        assert!(s.auto_sequential);
        assert_eq!(s.max_peers_per_torrent, 200);
    }

    #[test]
    fn min_memory_preset() {
        let s = Settings::min_memory();
        assert_eq!(s.disk_cache_size, 8 * 1024 * 1024);
        assert_eq!(s.max_torrents, 20);
        assert_eq!(s.max_peers_per_torrent, 30);
        assert_eq!(s.active_downloads, 1);
        assert_eq!(s.active_seeds, 2);
        assert_eq!(s.active_limit, 10);
        assert_eq!(s.alert_channel_size, 256);
        assert_eq!(s.utp_max_connections, 64);
        assert_eq!(s.max_request_queue_depth, 50);
        assert_eq!(s.max_concurrent_stream_reads, 2);
        assert_eq!(s.hashing_threads, 1);
        assert_eq!(s.disk_io_threads, 1);
    }

    #[test]
    fn high_performance_preset() {
        let s = Settings::high_performance();
        assert_eq!(s.disk_cache_size, 256 * 1024 * 1024);
        assert_eq!(s.max_torrents, 2000);
        assert_eq!(s.max_peers_per_torrent, 500);
        assert_eq!(s.active_downloads, 30);
        assert_eq!(s.active_seeds, 100);
        assert_eq!(s.active_limit, 2000);
        assert_eq!(s.alert_channel_size, 4096);
        assert_eq!(s.utp_max_connections, 1024);
        assert_eq!(s.max_request_queue_depth, 1000);
        assert_eq!(s.max_concurrent_stream_reads, 32);
        assert_eq!(s.hashing_threads, 4);
        assert_eq!(s.disk_io_threads, 8);
        assert_eq!(s.auto_upload_slots_max, 100);
    }

    #[test]
    fn json_round_trip() {
        let original = Settings::default();
        let json = serde_json::to_string(&original).unwrap();
        let decoded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn json_round_trip_presets() {
        // Verify all presets survive JSON serialization
        for original in [Settings::min_memory(), Settings::high_performance()] {
            let json = serde_json::to_string(&original).unwrap();
            let decoded: Settings = serde_json::from_str(&json).unwrap();
            assert_eq!(original, decoded);
        }
    }

    #[test]
    fn json_missing_fields_use_defaults() {
        // An empty JSON object should deserialize to defaults (via serde(default))
        let decoded: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(decoded, Settings::default());
    }

    #[test]
    fn validation_force_proxy_no_proxy() {
        let mut s = Settings::default();
        s.force_proxy = true;
        // proxy_type defaults to None
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("force_proxy"));
    }

    #[test]
    fn validation_valid_defaults() {
        Settings::default().validate().unwrap();
        Settings::min_memory().validate().unwrap();
        Settings::high_performance().validate().unwrap();
    }

    #[test]
    fn disk_config_from_settings() {
        let s = Settings::default();
        let dc = crate::disk::DiskConfig::from(&s);
        assert_eq!(dc.io_threads, 4);
        assert_eq!(dc.storage_mode, StorageMode::Auto);
        assert_eq!(dc.cache_size, 64 * 1024 * 1024);
        assert!((dc.write_cache_ratio - 0.25).abs() < f32::EPSILON);
        assert_eq!(dc.channel_capacity, 512);
    }

    #[test]
    fn torrent_config_from_settings() {
        let s = Settings::default();
        let tc = crate::types::TorrentConfig::from(&s);
        assert_eq!(tc.listen_port, 0); // random per-torrent
        assert_eq!(tc.max_peers, s.max_peers_per_torrent);
        assert_eq!(tc.download_dir, s.download_dir);
        assert_eq!(tc.enable_dht, s.enable_dht);
        assert_eq!(tc.enable_pex, s.enable_pex);
        assert_eq!(tc.encryption_mode, s.encryption_mode);
        assert_eq!(tc.enable_utp, s.enable_utp);
        assert_eq!(tc.enable_web_seed, s.enable_web_seed);
        assert_eq!(tc.hashing_threads, s.hashing_threads);
        assert_eq!(tc.max_concurrent_stream_reads, s.max_concurrent_stream_reads);
        assert_eq!(tc.anonymous_mode, s.anonymous_mode);
        assert_eq!(tc.enable_i2p, s.enable_i2p);
        assert_eq!(tc.allow_i2p_mixed, s.allow_i2p_mixed);
    }

    #[test]
    fn external_ip_default_and_json() {
        let s = Settings::default();
        assert!(s.external_ip.is_none());

        // JSON with external_ip set
        let json = r#"{"external_ip": "203.0.113.5"}"#;
        let decoded: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(
            decoded.external_ip,
            Some(std::net::IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 5)))
        );

        // Round-trip preserves external_ip
        let encoded = serde_json::to_string(&decoded).unwrap();
        let roundtrip: Settings = serde_json::from_str(&encoded).unwrap();
        assert_eq!(roundtrip.external_ip, decoded.external_ip);
    }

    #[test]
    fn validation_zero_threads() {
        let mut s = Settings::default();
        s.hashing_threads = 0;
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("hashing_threads"));

        let mut s = Settings::default();
        s.disk_io_threads = 0;
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("disk_io_threads"));
    }

    #[test]
    fn share_mode_requires_fast_extension() {
        let mut s = Settings::default();
        s.default_share_mode = true;
        s.enable_fast_extension = false;
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("share_mode"));

        // With fast extension enabled, share mode is valid
        s.enable_fast_extension = true;
        s.validate().unwrap();
    }

    #[test]
    fn share_mode_default_false() {
        let cfg = crate::types::TorrentConfig::default();
        assert!(!cfg.share_mode);
    }

    #[test]
    fn dht_storage_settings_defaults() {
        let s = Settings::default();
        assert_eq!(s.dht_max_items, 700);
        assert_eq!(s.dht_item_lifetime_secs, 7200);
    }

    #[test]
    fn dht_sample_interval_default_disabled() {
        let s = Settings::default();
        assert_eq!(s.dht_sample_infohashes_interval, 0);
    }

    #[test]
    fn dht_sample_interval_json_round_trip() {
        let json = r#"{"dht_sample_infohashes_interval": 300}"#;
        let decoded: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(decoded.dht_sample_infohashes_interval, 300);

        let encoded = serde_json::to_string(&decoded).unwrap();
        let roundtrip: Settings = serde_json::from_str(&encoded).unwrap();
        assert_eq!(roundtrip.dht_sample_infohashes_interval, 300);
    }

    #[test]
    fn min_memory_restricts_dht_items() {
        let s = Settings::min_memory();
        assert_eq!(s.dht_max_items, 100);
    }

    #[test]
    fn dht_config_inherits_security_settings() {
        let mut s = Settings::default();
        s.dht_enforce_node_id = false;
        let dht = s.to_dht_config();
        assert!(!dht.enforce_node_id);
        assert!(dht.restrict_routing_ips);

        let dht_v6 = s.to_dht_config_v6();
        assert!(!dht_v6.enforce_node_id);
        assert!(dht_v6.restrict_routing_ips);
    }

    #[test]
    fn enable_holepunch_default_true() {
        let s = Settings::default();
        assert!(s.enable_holepunch);
    }

    #[test]
    fn enable_holepunch_json_round_trip() {
        let json = r#"{"enable_holepunch": false}"#;
        let decoded: Settings = serde_json::from_str(json).unwrap();
        assert!(!decoded.enable_holepunch);

        let encoded = serde_json::to_string(&decoded).unwrap();
        let roundtrip: Settings = serde_json::from_str(&encoded).unwrap();
        assert!(!roundtrip.enable_holepunch);
    }

    #[test]
    fn i2p_settings_defaults() {
        let s = Settings::default();
        assert!(!s.enable_i2p);
        assert_eq!(s.i2p_hostname, "127.0.0.1");
        assert_eq!(s.i2p_port, 7656);
        assert_eq!(s.i2p_inbound_quantity, 3);
        assert_eq!(s.i2p_outbound_quantity, 3);
        assert_eq!(s.i2p_inbound_length, 3);
        assert_eq!(s.i2p_outbound_length, 3);
        assert!(!s.allow_i2p_mixed);
    }

    #[test]
    fn i2p_settings_json_roundtrip() {
        let mut s = Settings::default();
        s.enable_i2p = true;
        s.i2p_hostname = "10.0.0.1".into();
        s.i2p_port = 7700;
        s.i2p_inbound_quantity = 5;
        s.i2p_outbound_quantity = 4;
        s.i2p_inbound_length = 2;
        s.i2p_outbound_length = 1;
        s.allow_i2p_mixed = true;
        let json = serde_json::to_string(&s).unwrap();
        let decoded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(s, decoded);
    }

    #[test]
    fn i2p_validation_quantity_zero() {
        let mut s = Settings::default();
        s.enable_i2p = true;
        s.i2p_inbound_quantity = 0;
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("i2p_inbound_quantity"));
    }

    #[test]
    fn i2p_validation_quantity_too_high() {
        let mut s = Settings::default();
        s.enable_i2p = true;
        s.i2p_outbound_quantity = 17;
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("i2p_outbound_quantity"));
    }

    #[test]
    fn i2p_validation_length_too_high() {
        let mut s = Settings::default();
        s.enable_i2p = true;
        s.i2p_inbound_length = 8;
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("i2p_inbound_length"));
    }

    #[test]
    fn i2p_validation_passes_when_disabled() {
        // Invalid values should not trigger errors when I2P is disabled
        let mut s = Settings::default();
        s.enable_i2p = false;
        s.i2p_inbound_quantity = 0; // would be invalid if enabled
        s.validate().unwrap(); // should pass
    }

    #[test]
    fn i2p_validation_valid_config() {
        let mut s = Settings::default();
        s.enable_i2p = true;
        s.i2p_inbound_quantity = 1;
        s.i2p_outbound_quantity = 16;
        s.i2p_inbound_length = 0;
        s.i2p_outbound_length = 7;
        s.validate().unwrap();
    }

    #[test]
    fn ssl_settings_defaults() {
        let s = Settings::default();
        assert_eq!(s.ssl_listen_port, 0);
        assert!(s.ssl_cert_path.is_none());
        assert!(s.ssl_key_path.is_none());
    }

    #[test]
    fn ssl_settings_json_round_trip() {
        let mut s = Settings::default();
        s.ssl_listen_port = 4433;
        s.ssl_cert_path = Some(PathBuf::from("/etc/ssl/cert.pem"));
        s.ssl_key_path = Some(PathBuf::from("/etc/ssl/key.pem"));
        let json = serde_json::to_string(&s).unwrap();
        let decoded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(s, decoded);
    }

    #[test]
    fn ssl_validation_cert_without_key() {
        let mut s = Settings::default();
        s.ssl_cert_path = Some(PathBuf::from("/tmp/cert.pem"));
        // ssl_key_path is None
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("ssl_cert_path"));
    }

    #[test]
    fn ssl_validation_key_without_cert() {
        let mut s = Settings::default();
        s.ssl_key_path = Some(PathBuf::from("/tmp/key.pem"));
        // ssl_cert_path is None
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("ssl_cert_path"));
    }

    #[test]
    fn ssl_validation_both_set_passes() {
        let mut s = Settings::default();
        s.ssl_cert_path = Some(PathBuf::from("/tmp/cert.pem"));
        s.ssl_key_path = Some(PathBuf::from("/tmp/key.pem"));
        s.validate().unwrap();
    }

    #[test]
    fn ssl_validation_both_absent_passes() {
        let s = Settings::default();
        // Both are None by default
        s.validate().unwrap();
    }

    #[test]
    fn default_choking_algorithms() {
        let s = Settings::default();
        assert_eq!(s.seed_choking_algorithm, SeedChokingAlgorithm::FastestUpload);
        assert_eq!(s.choking_algorithm, ChokingAlgorithm::FixedSlots);
    }

    #[test]
    fn choking_algorithm_json_round_trip() {
        let mut s = Settings::default();
        s.seed_choking_algorithm = SeedChokingAlgorithm::AntiLeech;
        s.choking_algorithm = ChokingAlgorithm::RateBased;
        let json = serde_json::to_string(&s).unwrap();
        let decoded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.seed_choking_algorithm, SeedChokingAlgorithm::AntiLeech);
        assert_eq!(decoded.choking_algorithm, ChokingAlgorithm::RateBased);
    }

    #[test]
    fn m44_settings_defaults() {
        let s = Settings::default();
        assert!(s.piece_extent_affinity);
        assert!(!s.suggest_mode);
        assert_eq!(s.max_suggest_pieces, 10);
        assert_eq!(s.predictive_piece_announce_ms, 0);
    }

    #[test]
    fn m44_high_performance_enables_suggest() {
        let s = Settings::high_performance();
        assert!(s.suggest_mode);
    }

    #[test]
    fn m44_json_round_trip() {
        let mut s = Settings::default();
        s.piece_extent_affinity = false;
        s.suggest_mode = true;
        s.max_suggest_pieces = 5;
        s.predictive_piece_announce_ms = 50;
        let json = serde_json::to_string(&s).unwrap();
        let decoded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(s, decoded);
    }

    #[test]
    fn peer_turnover_defaults() {
        let s = Settings::default();
        assert!((s.peer_turnover - 0.04).abs() < f64::EPSILON);
        assert!((s.peer_turnover_cutoff - 0.9).abs() < f64::EPSILON);
        assert_eq!(s.peer_turnover_interval, 300);
    }

    #[test]
    fn peer_turnover_json_round_trip() {
        let mut s = Settings::default();
        s.peer_turnover = 0.1;
        s.peer_turnover_cutoff = 0.8;
        s.peer_turnover_interval = 120;
        let json = serde_json::to_string(&s).unwrap();
        let decoded: Settings = serde_json::from_str(&json).unwrap();
        assert!((decoded.peer_turnover - 0.1).abs() < f64::EPSILON);
        assert!((decoded.peer_turnover_cutoff - 0.8).abs() < f64::EPSILON);
        assert_eq!(decoded.peer_turnover_interval, 120);
    }

    #[test]
    fn peer_turnover_validation() {
        let mut s = Settings::default();
        s.peer_turnover = 1.5;
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("peer_turnover"));

        let mut s = Settings::default();
        s.peer_turnover = -0.1;
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("peer_turnover"));

        let mut s = Settings::default();
        s.peer_turnover_cutoff = 1.5;
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("peer_turnover_cutoff"));

        let mut s = Settings::default();
        s.peer_turnover_interval = 0;
        s.validate().unwrap();
    }

    #[test]
    fn security_settings_defaults() {
        let s = Settings::default();
        assert!(s.ssrf_mitigation);
        assert!(!s.allow_idna);
        assert!(s.validate_https_trackers);
    }

    #[test]
    fn security_settings_json_round_trip() {
        let mut s = Settings::default();
        s.ssrf_mitigation = false;
        s.allow_idna = true;
        s.validate_https_trackers = false;
        let json = serde_json::to_string(&s).unwrap();
        let decoded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(s, decoded);
    }

    #[test]
    fn security_settings_missing_use_defaults() {
        // An empty JSON object should deserialize security fields to defaults.
        let decoded: Settings = serde_json::from_str("{}").unwrap();
        assert!(decoded.ssrf_mitigation);
        assert!(!decoded.allow_idna);
        assert!(decoded.validate_https_trackers);
    }

    #[test]
    fn url_security_config_from_settings() {
        let mut s = Settings::default();
        s.ssrf_mitigation = false;
        s.allow_idna = true;
        s.validate_https_trackers = false;
        let cfg = crate::url_guard::UrlSecurityConfig::from(&s);
        assert!(!cfg.ssrf_mitigation);
        assert!(cfg.allow_idna);
        assert!(!cfg.validate_https_trackers);
    }

    #[test]
    fn default_peer_dscp_value() {
        let s = Settings::default();
        assert_eq!(s.peer_dscp, 0x08);
    }

    #[test]
    fn peer_dscp_json_round_trip() {
        let mut s = Settings::default();
        s.peer_dscp = 0x2E; // EF
        let json = serde_json::to_string(&s).unwrap();
        let decoded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.peer_dscp, 0x2E);
    }

    #[test]
    fn peer_dscp_zero_disables() {
        let mut s = Settings::default();
        s.peer_dscp = 0;
        let json = serde_json::to_string(&s).unwrap();
        let decoded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.peer_dscp, 0);
    }

    #[test]
    fn utp_config_includes_dscp() {
        let mut s = Settings::default();
        s.peer_dscp = 0x0A;
        let utp = s.to_utp_config(6881);
        assert_eq!(utp.dscp, 0x0A);

        let utp_v6 = s.to_utp_config_v6(6881);
        assert_eq!(utp_v6.dscp, 0x0A);
    }

    #[test]
    fn default_stats_report_interval() {
        let s = Settings::default();
        assert_eq!(s.stats_report_interval, 1000);
    }

    #[test]
    fn stats_report_interval_json_round_trip() {
        let mut s = Settings::default();
        s.stats_report_interval = 5000;
        let json = serde_json::to_string(&s).unwrap();
        let decoded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.stats_report_interval, 5000);
    }

    #[test]
    fn stats_report_interval_zero_disables() {
        let mut s = Settings::default();
        s.stats_report_interval = 0;
        let json = serde_json::to_string(&s).unwrap();
        let decoded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.stats_report_interval, 0);
    }
}
