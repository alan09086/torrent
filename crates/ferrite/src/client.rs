//! Ergonomic builder types for creating sessions and adding torrents.

use std::path::PathBuf;
use std::sync::Arc;

use ferrite_core::Magnet;
use ferrite_session::{ExtensionPlugin, Settings};
use ferrite_storage::TorrentStorage;

/// Ergonomic builder for creating a ferrite session.
///
/// Wraps [`Settings`] with a fluent API. Call [`start()`](Self::start)
/// to spawn the session actor and get a [`SessionHandle`](ferrite_session::SessionHandle).
///
/// # Example
///
/// ```no_run
/// # async fn example() -> ferrite::session::Result<()> {
/// let session = ferrite::ClientBuilder::new()
///     .download_dir("/tmp/downloads")
///     .listen_port(6881)
///     .enable_dht(true)
///     .start()
///     .await?;
/// # Ok(())
/// # }
/// ```
pub struct ClientBuilder {
    settings: Settings,
    plugins: Vec<Box<dyn ExtensionPlugin>>,
}

impl ClientBuilder {
    /// Create a new builder with default settings.
    pub fn new() -> Self {
        Self {
            settings: Settings::default(),
            plugins: Vec::new(),
        }
    }

    /// Register a custom BEP 10 extension plugin.
    ///
    /// Plugins are assigned extension IDs starting at 10 in registration order.
    /// Built-in extensions (ut_metadata=1, ut_pex=2, lt_trackers=3) cannot be
    /// overridden.
    pub fn add_extension(mut self, plugin: Box<dyn ExtensionPlugin>) -> Self {
        self.plugins.push(plugin);
        self
    }

    /// Set the TCP listen port for incoming peer connections.
    pub fn listen_port(mut self, port: u16) -> Self {
        self.settings.listen_port = port;
        self
    }

    /// Set the default download directory.
    pub fn download_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.settings.download_dir = path.into();
        self
    }

    /// Set the maximum number of concurrent torrents.
    pub fn max_torrents(mut self, n: usize) -> Self {
        self.settings.max_torrents = n;
        self
    }

    /// Enable or disable DHT peer discovery.
    pub fn enable_dht(mut self, v: bool) -> Self {
        self.settings.enable_dht = v;
        self
    }

    /// Enable or disable Local Service Discovery.
    pub fn enable_lsd(mut self, v: bool) -> Self {
        self.settings.enable_lsd = v;
        self
    }

    /// Enable or disable Peer Exchange.
    pub fn enable_pex(mut self, v: bool) -> Self {
        self.settings.enable_pex = v;
        self
    }

    /// Enable or disable BEP 6 Fast Extension.
    pub fn enable_fast_extension(mut self, v: bool) -> Self {
        self.settings.enable_fast_extension = v;
        self
    }

    /// Set the seed ratio limit. Torrents stop seeding when this ratio is reached.
    pub fn seed_ratio_limit(mut self, ratio: f64) -> Self {
        self.settings.seed_ratio_limit = Some(ratio);
        self
    }

    /// Set the alert category mask (default: all categories).
    pub fn alert_mask(mut self, mask: ferrite_session::AlertCategory) -> Self {
        self.settings.alert_mask = mask;
        self
    }

    /// Set the alert broadcast channel capacity (default: 1024).
    pub fn alert_channel_size(mut self, size: usize) -> Self {
        self.settings.alert_channel_size = size;
        self
    }

    /// Set the maximum number of concurrent auto-managed downloading torrents (-1 = unlimited).
    pub fn active_downloads(mut self, n: i32) -> Self {
        self.settings.active_downloads = n;
        self
    }

    /// Set the maximum number of concurrent auto-managed seeding torrents (-1 = unlimited).
    pub fn active_seeds(mut self, n: i32) -> Self {
        self.settings.active_seeds = n;
        self
    }

    /// Set the hard cap on all active auto-managed torrents (-1 = unlimited).
    pub fn active_limit(mut self, n: i32) -> Self {
        self.settings.active_limit = n;
        self
    }

    /// Set the maximum number of concurrent hash-check operations.
    pub fn active_checking(mut self, n: i32) -> Self {
        self.settings.active_checking = n;
        self
    }

    /// Set whether inactive torrents are exempt from download/seed limits.
    pub fn dont_count_slow_torrents(mut self, v: bool) -> Self {
        self.settings.dont_count_slow_torrents = v;
        self
    }

    /// Set the interval (seconds) between queue evaluations.
    pub fn auto_manage_interval(mut self, secs: u64) -> Self {
        self.settings.auto_manage_interval = secs;
        self
    }

    /// Set the startup grace period (seconds) where a torrent is considered active regardless of speed.
    pub fn auto_manage_startup(mut self, secs: u64) -> Self {
        self.settings.auto_manage_startup = secs;
        self
    }

    /// Set whether seeding slots are allocated before download slots.
    pub fn auto_manage_prefer_seeds(mut self, v: bool) -> Self {
        self.settings.auto_manage_prefer_seeds = v;
        self
    }

    /// Set the connection encryption mode (MSE/PE).
    pub fn encryption_mode(mut self, mode: ferrite_wire::mse::EncryptionMode) -> Self {
        self.settings.encryption_mode = mode;
        self
    }

    /// Enable or disable uTP (micro Transport Protocol) for peer connections.
    ///
    /// When enabled, outbound connections try uTP first with a 5-second timeout
    /// before falling back to TCP. Inbound uTP connections are routed to the
    /// correct torrent by reading the BT preamble.
    pub fn enable_utp(mut self, v: bool) -> Self {
        self.settings.enable_utp = v;
        self
    }

    /// Enable or disable UPnP IGD port mapping.
    ///
    /// When enabled, the session automatically attempts to open ports on the
    /// router via UPnP as a last resort (after PCP and NAT-PMP).
    pub fn enable_upnp(mut self, v: bool) -> Self {
        self.settings.enable_upnp = v;
        self
    }

    /// Enable or disable NAT-PMP / PCP port mapping.
    ///
    /// When enabled, the session tries PCP first (RFC 6887), then falls back
    /// to NAT-PMP (RFC 6886), to open ports on the router.
    pub fn enable_natpmp(mut self, v: bool) -> Self {
        self.settings.enable_natpmp = v;
        self
    }

    /// Enable or disable BEP 55 holepunch extension for NAT traversal.
    ///
    /// When enabled, the client advertises `ut_holepunch` in the extension
    /// handshake and can act as initiator, relay, or target for holepunch
    /// connections. Default: true.
    pub fn enable_holepunch(mut self, v: bool) -> Self {
        self.settings.enable_holepunch = v;
        self
    }

    /// Enable or disable IPv6 dual-stack support (BEP 7, 24).
    ///
    /// When enabled, the session binds listeners on both IPv4 and IPv6,
    /// starts a second DHT instance for IPv6, and processes IPv6 peers
    /// in PEX and tracker responses. Default: true.
    pub fn enable_ipv6(mut self, v: bool) -> Self {
        self.settings.enable_ipv6 = v;
        self
    }

    /// Enable or disable HTTP/web seeding (BEP 19 GetRight, BEP 17 Hoffman).
    ///
    /// When enabled, torrents with `url-list` or `httpseeds` will download
    /// pieces from HTTP servers alongside peer-to-peer transfers. Default: true.
    pub fn enable_web_seed(mut self, v: bool) -> Self {
        self.settings.enable_web_seed = v;
        self
    }

    /// Enable or disable BEP 16 super seeding for new torrents.
    ///
    /// Super seeding reveals pieces one-per-peer to maximize piece diversity
    /// across the swarm. Most useful for initial seeders. Default: false.
    pub fn super_seeding(mut self, v: bool) -> Self {
        self.settings.default_super_seeding = v;
        self
    }

    /// Enable or disable BEP 21 upload-only announcement.
    ///
    /// When enabled, the client advertises upload-only status via the
    /// extension handshake when a torrent transitions to seeding. Default: true.
    pub fn upload_only_announce(mut self, v: bool) -> Self {
        self.settings.upload_only_announce = v;
        self
    }

    /// Set the Have message batching delay in milliseconds.
    ///
    /// When > 0, Have messages are buffered and sent in batches at this interval.
    /// If the batch exceeds 50% of total pieces, a full Bitfield is sent instead.
    /// Default: 0 (immediate, no batching).
    pub fn have_send_delay_ms(mut self, ms: u64) -> Self {
        self.settings.have_send_delay_ms = ms;
        self
    }

    /// Set the number of hash-failure involvements before a peer is auto-banned.
    ///
    /// Default: 3. Lower values ban faster but risk false positives.
    pub fn smart_ban_max_failures(mut self, n: u32) -> Self {
        self.settings.smart_ban_max_failures = n;
        self
    }

    /// Enable or disable parole mode for smart banning.
    ///
    /// When enabled (default), a failed piece is re-downloaded from a single
    /// uninvolved peer to definitively attribute fault before striking.
    pub fn smart_ban_parole(mut self, enabled: bool) -> Self {
        self.settings.smart_ban_parole = enabled;
        self
    }

    /// Set the number of concurrent disk I/O threads. Default: 4.
    pub fn disk_io_threads(mut self, n: usize) -> Self {
        self.settings.disk_io_threads = n;
        self
    }

    /// Set the storage allocation mode. Default: Auto.
    pub fn storage_mode(mut self, mode: ferrite_core::StorageMode) -> Self {
        self.settings.storage_mode = mode;
        self
    }

    /// Set the number of concurrent piece hash verifications. Default: 2.
    pub fn hashing_threads(mut self, n: usize) -> Self {
        self.settings.hashing_threads = n;
        self
    }

    /// Set the total disk cache size in bytes. Default: 64 MiB.
    pub fn disk_cache_size(mut self, bytes: usize) -> Self {
        self.settings.disk_cache_size = bytes;
        self
    }

    /// Set the maximum per-peer request queue depth. Default: 250.
    pub fn max_request_queue_depth(mut self, n: usize) -> Self {
        self.settings.max_request_queue_depth = n;
        self
    }

    /// Set the request queue time multiplier (seconds). Default: 3.0.
    pub fn request_queue_time(mut self, secs: f64) -> Self {
        self.settings.request_queue_time = secs;
        self
    }

    /// Set the block request timeout in seconds. Default: 60.
    pub fn block_request_timeout_secs(mut self, secs: u32) -> Self {
        self.settings.block_request_timeout_secs = secs;
        self
    }

    /// Set the maximum concurrent file stream readers. Default: 8.
    pub fn max_concurrent_stream_reads(mut self, n: usize) -> Self {
        self.settings.max_concurrent_stream_reads = n;
        self
    }

    /// Set the proxy configuration for peer and tracker connections.
    pub fn proxy(mut self, proxy: ferrite_session::ProxyConfig) -> Self {
        self.settings.proxy = proxy;
        self
    }

    /// Enable force proxy mode.
    ///
    /// When enabled, all connections must go through the configured proxy.
    /// Disables listen sockets, UPnP, NAT-PMP, DHT, and LSD.
    /// Fails at start if no proxy is configured.
    pub fn force_proxy(mut self, v: bool) -> Self {
        self.settings.force_proxy = v;
        self
    }

    /// Enable anonymous mode.
    ///
    /// Suppresses identifying information (client version in BEP 10
    /// handshake) and disables DHT, LSD, UPnP, and NAT-PMP.
    pub fn anonymous_mode(mut self, v: bool) -> Self {
        self.settings.anonymous_mode = v;
        self
    }

    /// Set whether the IP filter applies to tracker connections.
    ///
    /// When true (default), tracker IP addresses are checked against
    /// the IP filter. When false, trackers are exempt.
    pub fn apply_ip_filter_to_trackers(mut self, v: bool) -> Self {
        self.settings.apply_ip_filter_to_trackers = v;
        self
    }

    /// Set the DHT query rate (queries per second). Default: 5.
    pub fn dht_queries_per_second(mut self, n: usize) -> Self {
        self.settings.dht_queries_per_second = n;
        self
    }

    /// Set the DHT query timeout in seconds. Default: 15.
    pub fn dht_query_timeout_secs(mut self, secs: u64) -> Self {
        self.settings.dht_query_timeout_secs = secs;
        self
    }

    /// BEP 42: Enforce node ID verification in the DHT routing table. Default: true.
    pub fn dht_enforce_node_id(mut self, v: bool) -> Self {
        self.settings.dht_enforce_node_id = v;
        self
    }

    /// BEP 42: Restrict the DHT routing table to one node per IP. Default: true.
    pub fn dht_restrict_routing_ips(mut self, v: bool) -> Self {
        self.settings.dht_restrict_routing_ips = v;
        self
    }

    /// BEP 44: Set the maximum number of stored DHT items. Default: 700.
    pub fn dht_max_items(mut self, v: usize) -> Self {
        self.settings.dht_max_items = v;
        self
    }

    /// BEP 44: Set the DHT item lifetime in seconds. Default: 7200.
    pub fn dht_item_lifetime_secs(mut self, v: u64) -> Self {
        self.settings.dht_item_lifetime_secs = v;
        self
    }

    /// Set the UPnP lease duration in seconds. Default: 3600.
    pub fn upnp_lease_duration(mut self, secs: u32) -> Self {
        self.settings.upnp_lease_duration = secs;
        self
    }

    /// Set the NAT-PMP mapping lifetime in seconds. Default: 3600.
    pub fn natpmp_lifetime(mut self, secs: u32) -> Self {
        self.settings.natpmp_lifetime = secs;
        self
    }

    /// Set the maximum uTP connections. Default: 256.
    pub fn utp_max_connections(mut self, n: usize) -> Self {
        self.settings.utp_max_connections = n;
        self
    }

    /// Enable I2P anonymous network support.
    ///
    /// Requires a local I2P router with SAM enabled (default: 127.0.0.1:7656).
    /// When enabled, the session creates a SAM session on startup and accepts
    /// anonymous peer connections.
    pub fn enable_i2p(mut self, v: bool) -> Self {
        self.settings.enable_i2p = v;
        self
    }

    /// Set the SAM bridge hostname. Default: "127.0.0.1".
    pub fn i2p_hostname(mut self, host: impl Into<String>) -> Self {
        self.settings.i2p_hostname = host.into();
        self
    }

    /// Set the SAM bridge port. Default: 7656.
    pub fn i2p_port(mut self, port: u16) -> Self {
        self.settings.i2p_port = port;
        self
    }

    /// Set the number of inbound I2P tunnels (1-16). Default: 3.
    pub fn i2p_inbound_quantity(mut self, n: u8) -> Self {
        self.settings.i2p_inbound_quantity = n;
        self
    }

    /// Set the number of outbound I2P tunnels (1-16). Default: 3.
    pub fn i2p_outbound_quantity(mut self, n: u8) -> Self {
        self.settings.i2p_outbound_quantity = n;
        self
    }

    /// Set the number of hops in inbound I2P tunnels (0-7). Default: 3.
    pub fn i2p_inbound_length(mut self, n: u8) -> Self {
        self.settings.i2p_inbound_length = n;
        self
    }

    /// Set the number of hops in outbound I2P tunnels (0-7). Default: 3.
    pub fn i2p_outbound_length(mut self, n: u8) -> Self {
        self.settings.i2p_outbound_length = n;
        self
    }

    /// Allow mixing I2P and clearnet peers. Default: false.
    ///
    /// When false, I2P-enabled torrents only connect to I2P peers.
    /// When true, both I2P and clearnet peers are used.
    pub fn allow_i2p_mixed(mut self, v: bool) -> Self {
        self.settings.allow_i2p_mixed = v;
        self
    }

    /// Set the TCP listen port for incoming SSL torrent connections.
    ///
    /// When non-zero, a TLS listener is bound on this port for torrents with
    /// `ssl-cert` in their info dict. SNI-based routing dispatches connections
    /// to the correct torrent. Default: 0 (disabled).
    pub fn ssl_listen_port(mut self, v: u16) -> Self {
        self.settings.ssl_listen_port = v;
        self
    }

    /// Set the path to a PEM-encoded certificate file for SSL torrent connections.
    ///
    /// If not set, a self-signed certificate is auto-generated on first use
    /// and persisted to `resume_data_dir`. Must be paired with `ssl_key_path`.
    pub fn ssl_cert_path(mut self, v: impl Into<PathBuf>) -> Self {
        self.settings.ssl_cert_path = Some(v.into());
        self
    }

    /// Set the path to a PEM-encoded private key file for SSL torrent connections.
    ///
    /// Must be paired with `ssl_cert_path`.
    pub fn ssl_key_path(mut self, v: impl Into<PathBuf>) -> Self {
        self.settings.ssl_key_path = Some(v.into());
        self
    }

    /// Set the disk I/O channel capacity. Default: 1024.
    pub fn disk_channel_capacity(mut self, n: usize) -> Self {
        self.settings.disk_channel_capacity = n;
        self
    }

    /// Set the seed-mode choking algorithm.
    pub fn seed_choking_algorithm(mut self, algorithm: ferrite_session::SeedChokingAlgorithm) -> Self {
        self.settings.seed_choking_algorithm = algorithm;
        self
    }

    /// Set the choking algorithm.
    pub fn choking_algorithm(mut self, algorithm: ferrite_session::ChokingAlgorithm) -> Self {
        self.settings.choking_algorithm = algorithm;
        self
    }

    /// Enable or disable piece extent affinity for disk cache locality.
    ///
    /// When enabled, the piece picker prefers pieces adjacent to those already
    /// downloaded, improving sequential disk access patterns. Default: true.
    pub fn piece_extent_affinity(mut self, enabled: bool) -> Self {
        self.settings.piece_extent_affinity = enabled;
        self
    }

    /// Enable or disable suggest mode (BEP 6 SuggestPiece).
    ///
    /// When enabled, newly verified pieces are suggested to peers that don't
    /// have them, helping improve piece diversity in the swarm. Default: false.
    pub fn suggest_mode(mut self, enabled: bool) -> Self {
        self.settings.suggest_mode = enabled;
        self
    }

    /// Set the maximum number of SuggestPiece messages per peer.
    ///
    /// Limits how many pieces are suggested to each connected peer to avoid
    /// flooding. Default: 10.
    pub fn max_suggest_pieces(mut self, count: usize) -> Self {
        self.settings.max_suggest_pieces = count;
        self
    }

    /// Set the predictive piece announce delay in milliseconds.
    ///
    /// When > 0, a Have message is sent to peers as soon as all blocks of a
    /// piece are received, before hash verification completes. This reduces
    /// latency for piece availability at the cost of a possible false announce
    /// if the piece fails verification. Default: 0 (disabled).
    pub fn predictive_piece_announce(mut self, ms: u64) -> Self {
        self.settings.predictive_piece_announce_ms = ms;
        self
    }

    /// Set the mixed-mode TCP/uTP bandwidth allocation algorithm.
    pub fn mixed_mode_algorithm(mut self, algorithm: ferrite_session::MixedModeAlgorithm) -> Self {
        self.settings.mixed_mode_algorithm = algorithm;
        self
    }

    /// Enable or disable automatic sequential mode switching.
    pub fn auto_sequential(mut self, enable: bool) -> Self {
        self.settings.auto_sequential = enable;
        self
    }

    /// Set the peer turnover fraction (0.0–1.0).
    pub fn peer_turnover(mut self, fraction: f64) -> Self {
        self.settings.peer_turnover = fraction;
        self
    }

    /// Set the peer turnover cutoff (0.0–1.0).
    pub fn peer_turnover_cutoff(mut self, cutoff: f64) -> Self {
        self.settings.peer_turnover_cutoff = cutoff;
        self
    }

    /// Set the peer turnover interval in seconds (0 = disabled).
    pub fn peer_turnover_interval(mut self, secs: u64) -> Self {
        self.settings.peer_turnover_interval = secs;
        self
    }

    /// Consume the builder and return the underlying `Settings`.
    pub fn into_settings(self) -> Settings {
        self.settings
    }

    /// Start the session, spawning the background actor.
    pub async fn start(self) -> ferrite_session::Result<ferrite_session::SessionHandle> {
        let plugins = Arc::new(self.plugins);
        ferrite_session::SessionHandle::start_with_plugins(self.settings, plugins).await
    }
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Source for adding a torrent to a session.
#[allow(dead_code)]
enum TorrentSource {
    /// Parsed torrent metainfo (v1, v2, or hybrid).
    Meta(Box<ferrite_core::TorrentMeta>),
    /// Magnet link (metadata fetched via BEP 9).
    Magnet(Magnet),
    /// Path to a .torrent file on disk.
    File(PathBuf),
    /// Raw .torrent file bytes.
    Bytes(Vec<u8>),
}

/// Unified parameters for adding a torrent to a session.
///
/// Construct via [`from_torrent()`](Self::from_torrent),
/// [`from_magnet()`](Self::from_magnet), [`from_file()`](Self::from_file),
/// or [`from_bytes()`](Self::from_bytes).
pub struct AddTorrentParams {
    #[allow(dead_code)]
    source: TorrentSource,
    #[allow(dead_code)]
    download_dir: Option<PathBuf>,
    #[allow(dead_code)]
    storage: Option<Arc<dyn TorrentStorage>>,
}

impl AddTorrentParams {
    /// Create params from parsed torrent metainfo (v1, v2, or hybrid).
    pub fn from_torrent(meta: ferrite_core::TorrentMeta) -> Self {
        Self {
            source: TorrentSource::Meta(Box::new(meta)),
            download_dir: None,
            storage: None,
        }
    }

    /// Create params from a magnet link.
    pub fn from_magnet(magnet: Magnet) -> Self {
        Self {
            source: TorrentSource::Magnet(magnet),
            download_dir: None,
            storage: None,
        }
    }

    /// Create params from a .torrent file path.
    pub fn from_file(path: impl Into<PathBuf>) -> Self {
        Self {
            source: TorrentSource::File(path.into()),
            download_dir: None,
            storage: None,
        }
    }

    /// Create params from raw .torrent file bytes.
    pub fn from_bytes(data: Vec<u8>) -> Self {
        Self {
            source: TorrentSource::Bytes(data),
            download_dir: None,
            storage: None,
        }
    }

    /// Override the download directory for this torrent.
    pub fn download_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.download_dir = Some(path.into());
        self
    }

    /// Provide custom storage for this torrent.
    pub fn storage(mut self, s: Arc<dyn TorrentStorage>) -> Self {
        self.storage = Some(s);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_builder_encryption_mode() {
        use ferrite_wire::mse::EncryptionMode;
        let config = ClientBuilder::new()
            .encryption_mode(EncryptionMode::Forced)
            .into_settings();
        assert_eq!(config.encryption_mode, EncryptionMode::Forced);
    }

    #[test]
    fn client_builder_utp_config() {
        // Default: uTP enabled
        let config = ClientBuilder::new().into_settings();
        assert!(config.enable_utp);

        // Explicitly disabled
        let config = ClientBuilder::new().enable_utp(false).into_settings();
        assert!(!config.enable_utp);
    }

    #[test]
    fn client_builder_super_seeding_config() {
        // Default: super seeding disabled
        let config = ClientBuilder::new().into_settings();
        assert!(!config.default_super_seeding);
        assert!(config.upload_only_announce);
        assert_eq!(config.have_send_delay_ms, 0);

        // Explicitly enabled
        let config = ClientBuilder::new()
            .super_seeding(true)
            .upload_only_announce(false)
            .have_send_delay_ms(500)
            .into_settings();
        assert!(config.default_super_seeding);
        assert!(!config.upload_only_announce);
        assert_eq!(config.have_send_delay_ms, 500);
    }

    #[test]
    fn client_builder_web_seed_config() {
        // Default: web seed enabled
        let config = ClientBuilder::new().into_settings();
        assert!(config.enable_web_seed);

        // Explicitly disabled
        let config = ClientBuilder::new().enable_web_seed(false).into_settings();
        assert!(!config.enable_web_seed);
    }

    #[test]
    fn client_builder_smart_ban_config() {
        // Defaults
        let config = ClientBuilder::new().into_settings();
        assert_eq!(config.smart_ban_max_failures, 3);
        assert!(config.smart_ban_parole);

        // Custom
        let config = ClientBuilder::new()
            .smart_ban_max_failures(5)
            .smart_ban_parole(false)
            .into_settings();
        assert_eq!(config.smart_ban_max_failures, 5);
        assert!(!config.smart_ban_parole);
    }

    #[test]
    fn facade_ban_config_reexport() {
        // Verify BanConfig is accessible via the facade
        let _cfg = ferrite_session::BanConfig::default();
        assert_eq!(_cfg.max_failures, 3);
        assert!(_cfg.use_parole);
    }

    #[test]
    fn client_builder_proxy_config() {
        let config = ClientBuilder::new()
            .proxy(ferrite_session::ProxyConfig {
                proxy_type: ferrite_session::ProxyType::Socks5Password,
                hostname: "localhost".into(),
                port: 9050,
                username: Some("user".into()),
                password: Some("pass".into()),
                ..Default::default()
            })
            .force_proxy(true)
            .anonymous_mode(true)
            .apply_ip_filter_to_trackers(false)
            .into_settings();

        assert_eq!(config.proxy.proxy_type, ferrite_session::ProxyType::Socks5Password);
        assert_eq!(config.proxy.hostname, "localhost");
        assert_eq!(config.proxy.port, 9050);
        assert!(config.force_proxy);
        assert!(config.anonymous_mode);
        assert!(!config.apply_ip_filter_to_trackers);
    }

    #[test]
    fn client_builder_ssl_config() {
        let config = ClientBuilder::new()
            .ssl_listen_port(4433)
            .ssl_cert_path("/tmp/cert.pem")
            .ssl_key_path("/tmp/key.pem")
            .into_settings();
        assert_eq!(config.ssl_listen_port, 4433);
        assert_eq!(
            config.ssl_cert_path,
            Some(std::path::PathBuf::from("/tmp/cert.pem"))
        );
        assert_eq!(
            config.ssl_key_path,
            Some(std::path::PathBuf::from("/tmp/key.pem"))
        );
    }

    #[test]
    fn client_builder_i2p_config() {
        // Default: I2P disabled
        let config = ClientBuilder::new().into_settings();
        assert!(!config.enable_i2p);
        assert_eq!(config.i2p_hostname, "127.0.0.1");
        assert_eq!(config.i2p_port, 7656);
        assert_eq!(config.i2p_inbound_quantity, 3);
        assert_eq!(config.i2p_outbound_quantity, 3);
        assert_eq!(config.i2p_inbound_length, 3);
        assert_eq!(config.i2p_outbound_length, 3);
        assert!(!config.allow_i2p_mixed);

        // Explicitly configured
        let config = ClientBuilder::new()
            .enable_i2p(true)
            .i2p_hostname("10.0.0.1")
            .i2p_port(7700)
            .i2p_inbound_quantity(5)
            .i2p_outbound_quantity(4)
            .i2p_inbound_length(2)
            .i2p_outbound_length(1)
            .allow_i2p_mixed(true)
            .into_settings();
        assert!(config.enable_i2p);
        assert_eq!(config.i2p_hostname, "10.0.0.1");
        assert_eq!(config.i2p_port, 7700);
        assert_eq!(config.i2p_inbound_quantity, 5);
        assert_eq!(config.i2p_outbound_quantity, 4);
        assert_eq!(config.i2p_inbound_length, 2);
        assert_eq!(config.i2p_outbound_length, 1);
        assert!(config.allow_i2p_mixed);
    }

    #[test]
    fn builder_m44_settings() {
        let settings = ClientBuilder::new()
            .piece_extent_affinity(false)
            .suggest_mode(true)
            .max_suggest_pieces(5)
            .predictive_piece_announce(100)
            .into_settings();
        assert!(!settings.piece_extent_affinity);
        assert!(settings.suggest_mode);
        assert_eq!(settings.max_suggest_pieces, 5);
        assert_eq!(settings.predictive_piece_announce_ms, 100);
    }

    #[test]
    fn facade_ip_filter_reexport() {
        // Verify IpFilter, ProxyConfig, ProxyType are accessible via facade
        let mut filter = ferrite_session::IpFilter::new();
        filter.add_rule(
            "203.0.113.0".parse().unwrap(),
            "203.0.113.255".parse().unwrap(),
            1,
        );
        assert!(filter.is_blocked("203.0.113.42".parse().unwrap()));
        assert!(!filter.is_blocked("198.51.100.1".parse().unwrap()));

        let _proxy = ferrite_session::ProxyConfig::default();
        assert_eq!(_proxy.proxy_type, ferrite_session::ProxyType::None);
    }
}
