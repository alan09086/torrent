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
}
