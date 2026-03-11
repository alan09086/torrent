#![warn(missing_docs)]
//! BitTorrent session management: peers, torrents, and piece selection.

pub mod alert;
mod error;
pub(crate) mod metadata;
pub(crate) mod peer_state;
mod settings;
mod types;
// These will be added as they're implemented:
pub(crate) mod ban;
#[allow(dead_code)] // M73: retained for endgame pathway and future use
pub(crate) mod chunk_mask;
pub(crate) mod choker;
/// Disk I/O manager: configuration, handles, and statistics.
pub mod disk;
/// Pluggable disk I/O backend trait and implementations.
pub mod disk_backend;
#[allow(dead_code)] // Wired in during Task 2 (DSCP wiring to session/torrent actors).
pub(crate) mod dscp;
pub(crate) mod end_game;
pub mod extension;
pub(crate) mod have_buffer;
pub mod i2p;
pub(crate) mod ip_filter;
pub(crate) mod lsd;
pub(crate) mod lt_trackers;
pub(crate) mod peer;
pub(crate) mod peer_priority;
mod persistence;
pub(crate) mod pex;
pub(crate) mod piece_reservation;
pub(crate) mod request_driver;
#[allow(dead_code)] // M73: retained for endgame pathway and future use
pub(crate) mod piece_selector;
pub(crate) mod pipeline;
#[allow(dead_code)] // Wired in during Step 5 (proxy integration).
pub(crate) mod proxy;
pub(crate) mod queue;
pub(crate) mod rate_limiter;
mod session;
pub(crate) mod slot_tuner;
#[allow(dead_code)] // Wired in during Phase 3c (session SSL integration).
pub(crate) mod ssl_manager;
pub mod stats;
pub mod streaming;
pub(crate) mod super_seed;
mod torrent;
pub(crate) mod tracker_manager;
pub mod transport;
pub(crate) mod url_guard;
pub(crate) mod utp_routing;
pub(crate) mod web_seed;
pub(crate) mod write_buffer;

pub use crate::piece_selector::build_wanted_pieces;
pub use crate::tracker_manager::{TrackerInfo, TrackerStatus};
pub use alert::{Alert, AlertCategory, AlertKind, AlertStream};
pub use ban::BanConfig;
pub use choker::{ChokingAlgorithm, SeedChokingAlgorithm};
pub use disk::{DiskConfig, DiskHandle, DiskJobFlags, DiskManagerHandle, DiskStats};
pub use disk_backend::{DisabledDiskIo, DiskIoBackend, DiskIoStats};
pub use error::{Error, Result};
pub use extension::ExtensionPlugin;
pub use i2p::{I2pDestination, I2pDestinationError};
pub use ip_filter::{IpFilter, IpFilterError, PortFilter, parse_dat, parse_p2p};
pub use peer_state::PeerSource;
pub use persistence::{DhtNodeEntry, PeerStrikeEntry, SessionState, validate_resume_bitfield};
pub use proxy::{ProxyConfig, ProxyType};
pub use rate_limiter::MixedModeAlgorithm;
pub use session::SessionHandle;
pub use settings::Settings;
pub use stats::{
    MetricKind, NUM_METRICS, SessionCounters, SessionStatsMetric, session_stats_metrics,
};
pub use streaming::FileStream;
pub use torrent::TorrentHandle;
pub use transport::{BoxedStream, NetworkFactory, TransportListener};
pub use types::{
    FileInfo, FileMode, FileStatus, PartialPieceInfo, PeerInfo, SessionStats, StorageFactory,
    TorrentConfig, TorrentFlags, TorrentInfo, TorrentState, TorrentStats,
};
