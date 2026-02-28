//! Ergonomic builder types for creating sessions and adding torrents.

use std::path::PathBuf;
use std::sync::Arc;

use ferrite_core::{Magnet, TorrentMetaV1};
use ferrite_session::SessionConfig;
use ferrite_storage::TorrentStorage;

/// Ergonomic builder for creating a ferrite session.
///
/// Wraps [`SessionConfig`] with a fluent API. Call [`start()`](Self::start)
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
    config: SessionConfig,
}

impl ClientBuilder {
    /// Create a new builder with default configuration.
    pub fn new() -> Self {
        Self {
            config: SessionConfig::default(),
        }
    }

    /// Set the TCP listen port for incoming peer connections.
    pub fn listen_port(mut self, port: u16) -> Self {
        self.config.listen_port = port;
        self
    }

    /// Set the default download directory.
    pub fn download_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.download_dir = path.into();
        self
    }

    /// Set the maximum number of concurrent torrents.
    pub fn max_torrents(mut self, n: usize) -> Self {
        self.config.max_torrents = n;
        self
    }

    /// Enable or disable DHT peer discovery.
    pub fn enable_dht(mut self, v: bool) -> Self {
        self.config.enable_dht = v;
        self
    }

    /// Enable or disable Local Service Discovery.
    pub fn enable_lsd(mut self, v: bool) -> Self {
        self.config.enable_lsd = v;
        self
    }

    /// Enable or disable Peer Exchange.
    pub fn enable_pex(mut self, v: bool) -> Self {
        self.config.enable_pex = v;
        self
    }

    /// Enable or disable BEP 6 Fast Extension.
    pub fn enable_fast_extension(mut self, v: bool) -> Self {
        self.config.enable_fast_extension = v;
        self
    }

    /// Set the seed ratio limit. Torrents stop seeding when this ratio is reached.
    pub fn seed_ratio_limit(mut self, ratio: f64) -> Self {
        self.config.seed_ratio_limit = Some(ratio);
        self
    }

    /// Set the alert category mask (default: all categories).
    pub fn alert_mask(mut self, mask: ferrite_session::AlertCategory) -> Self {
        self.config.alert_mask = mask;
        self
    }

    /// Set the alert broadcast channel capacity (default: 1024).
    pub fn alert_channel_size(mut self, size: usize) -> Self {
        self.config.alert_channel_size = size;
        self
    }

    /// Set the maximum number of concurrent auto-managed downloading torrents (-1 = unlimited).
    pub fn active_downloads(mut self, n: i32) -> Self {
        self.config.active_downloads = n;
        self
    }

    /// Set the maximum number of concurrent auto-managed seeding torrents (-1 = unlimited).
    pub fn active_seeds(mut self, n: i32) -> Self {
        self.config.active_seeds = n;
        self
    }

    /// Set the hard cap on all active auto-managed torrents (-1 = unlimited).
    pub fn active_limit(mut self, n: i32) -> Self {
        self.config.active_limit = n;
        self
    }

    /// Set the maximum number of concurrent hash-check operations.
    pub fn active_checking(mut self, n: i32) -> Self {
        self.config.active_checking = n;
        self
    }

    /// Set whether inactive torrents are exempt from download/seed limits.
    pub fn dont_count_slow_torrents(mut self, v: bool) -> Self {
        self.config.dont_count_slow_torrents = v;
        self
    }

    /// Set the interval (seconds) between queue evaluations.
    pub fn auto_manage_interval(mut self, secs: u64) -> Self {
        self.config.auto_manage_interval = secs;
        self
    }

    /// Set the startup grace period (seconds) where a torrent is considered active regardless of speed.
    pub fn auto_manage_startup(mut self, secs: u64) -> Self {
        self.config.auto_manage_startup = secs;
        self
    }

    /// Set whether seeding slots are allocated before download slots.
    pub fn auto_manage_prefer_seeds(mut self, v: bool) -> Self {
        self.config.auto_manage_prefer_seeds = v;
        self
    }

    /// Set the connection encryption mode (MSE/PE).
    pub fn encryption_mode(mut self, mode: ferrite_wire::mse::EncryptionMode) -> Self {
        self.config.encryption_mode = mode;
        self
    }

    /// Enable or disable uTP (micro Transport Protocol) for peer connections.
    ///
    /// When enabled, outbound connections try uTP first with a 5-second timeout
    /// before falling back to TCP. Inbound uTP connections are routed to the
    /// correct torrent by reading the BT preamble.
    pub fn enable_utp(mut self, v: bool) -> Self {
        self.config.enable_utp = v;
        self
    }

    /// Enable or disable UPnP IGD port mapping.
    ///
    /// When enabled, the session automatically attempts to open ports on the
    /// router via UPnP as a last resort (after PCP and NAT-PMP).
    pub fn enable_upnp(mut self, v: bool) -> Self {
        self.config.enable_upnp = v;
        self
    }

    /// Enable or disable NAT-PMP / PCP port mapping.
    ///
    /// When enabled, the session tries PCP first (RFC 6887), then falls back
    /// to NAT-PMP (RFC 6886), to open ports on the router.
    pub fn enable_natpmp(mut self, v: bool) -> Self {
        self.config.enable_natpmp = v;
        self
    }

    /// Enable or disable IPv6 dual-stack support (BEP 7, 24).
    ///
    /// When enabled, the session binds listeners on both IPv4 and IPv6,
    /// starts a second DHT instance for IPv6, and processes IPv6 peers
    /// in PEX and tracker responses. Default: true.
    pub fn enable_ipv6(mut self, v: bool) -> Self {
        self.config.enable_ipv6 = v;
        self
    }

    /// Enable or disable HTTP/web seeding (BEP 19 GetRight, BEP 17 Hoffman).
    ///
    /// When enabled, torrents with `url-list` or `httpseeds` will download
    /// pieces from HTTP servers alongside peer-to-peer transfers. Default: true.
    pub fn enable_web_seed(mut self, v: bool) -> Self {
        self.config.enable_web_seed = v;
        self
    }

    /// Enable or disable BEP 16 super seeding for new torrents.
    ///
    /// Super seeding reveals pieces one-per-peer to maximize piece diversity
    /// across the swarm. Most useful for initial seeders. Default: false.
    pub fn super_seeding(mut self, v: bool) -> Self {
        self.config.default_super_seeding = v;
        self
    }

    /// Enable or disable BEP 21 upload-only announcement.
    ///
    /// When enabled, the client advertises upload-only status via the
    /// extension handshake when a torrent transitions to seeding. Default: true.
    pub fn upload_only_announce(mut self, v: bool) -> Self {
        self.config.upload_only_announce = v;
        self
    }

    /// Set the Have message batching delay in milliseconds.
    ///
    /// When > 0, Have messages are buffered and sent in batches at this interval.
    /// If the batch exceeds 50% of total pieces, a full Bitfield is sent instead.
    /// Default: 0 (immediate, no batching).
    pub fn have_send_delay_ms(mut self, ms: u64) -> Self {
        self.config.have_send_delay_ms = ms;
        self
    }

    /// Set the number of hash-failure involvements before a peer is auto-banned.
    ///
    /// Default: 3. Lower values ban faster but risk false positives.
    pub fn smart_ban_max_failures(mut self, n: u32) -> Self {
        self.config.smart_ban_max_failures = n;
        self
    }

    /// Enable or disable parole mode for smart banning.
    ///
    /// When enabled (default), a failed piece is re-downloaded from a single
    /// uninvolved peer to definitively attribute fault before striking.
    pub fn smart_ban_parole(mut self, enabled: bool) -> Self {
        self.config.smart_ban_parole = enabled;
        self
    }

    /// Set the number of concurrent disk I/O threads. Default: 4.
    pub fn disk_io_threads(mut self, n: usize) -> Self {
        self.config.disk_io_threads = n;
        self
    }

    /// Set the storage allocation mode. Default: Auto.
    pub fn storage_mode(mut self, mode: ferrite_core::StorageMode) -> Self {
        self.config.storage_mode = mode;
        self
    }

    /// Set the number of concurrent piece hash verifications. Default: 2.
    pub fn hashing_threads(mut self, n: usize) -> Self {
        self.config.hashing_threads = n;
        self
    }

    /// Set the total disk cache size in bytes. Default: 64 MiB.
    pub fn disk_cache_size(mut self, bytes: usize) -> Self {
        self.config.disk_cache_size = bytes;
        self
    }

    /// Consume the builder and return the underlying `SessionConfig`.
    pub fn into_config(self) -> SessionConfig {
        self.config
    }

    /// Start the session, spawning the background actor.
    pub async fn start(self) -> ferrite_session::Result<ferrite_session::SessionHandle> {
        ferrite_session::SessionHandle::start(self.config).await
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
    /// Parsed torrent metainfo.
    Meta(TorrentMetaV1),
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
    /// Create params from parsed torrent metainfo.
    pub fn from_torrent(meta: TorrentMetaV1) -> Self {
        Self {
            source: TorrentSource::Meta(meta),
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
            .into_config();
        assert_eq!(config.encryption_mode, EncryptionMode::Forced);
    }

    #[test]
    fn client_builder_utp_config() {
        // Default: uTP enabled
        let config = ClientBuilder::new().into_config();
        assert!(config.enable_utp);

        // Explicitly disabled
        let config = ClientBuilder::new().enable_utp(false).into_config();
        assert!(!config.enable_utp);
    }

    #[test]
    fn client_builder_super_seeding_config() {
        // Default: super seeding disabled
        let config = ClientBuilder::new().into_config();
        assert!(!config.default_super_seeding);
        assert!(config.upload_only_announce);
        assert_eq!(config.have_send_delay_ms, 0);

        // Explicitly enabled
        let config = ClientBuilder::new()
            .super_seeding(true)
            .upload_only_announce(false)
            .have_send_delay_ms(500)
            .into_config();
        assert!(config.default_super_seeding);
        assert!(!config.upload_only_announce);
        assert_eq!(config.have_send_delay_ms, 500);
    }

    #[test]
    fn client_builder_web_seed_config() {
        // Default: web seed enabled
        let config = ClientBuilder::new().into_config();
        assert!(config.enable_web_seed);

        // Explicitly disabled
        let config = ClientBuilder::new().enable_web_seed(false).into_config();
        assert!(!config.enable_web_seed);
    }

    #[test]
    fn client_builder_smart_ban_config() {
        // Defaults
        let config = ClientBuilder::new().into_config();
        assert_eq!(config.smart_ban_max_failures, 3);
        assert!(config.smart_ban_parole);

        // Custom
        let config = ClientBuilder::new()
            .smart_ban_max_failures(5)
            .smart_ban_parole(false)
            .into_config();
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
}
