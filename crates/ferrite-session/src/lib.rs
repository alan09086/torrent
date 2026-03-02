//! BitTorrent session management: peers, torrents, and piece selection.

pub mod alert;
mod error;
mod settings;
mod types;
pub(crate) mod peer_state;
pub(crate) mod metadata;
// These will be added as they're implemented:
pub(crate) mod piece_selector;
pub(crate) mod pex;
pub(crate) mod lt_trackers;
pub(crate) mod choker;
pub(crate) mod end_game;
pub(crate) mod rate_limiter;
pub(crate) mod slot_tuner;
pub(crate) mod tracker_manager;
pub(crate) mod lsd;
pub(crate) mod peer;
pub(crate) mod queue;
pub(crate) mod utp_routing;
pub(crate) mod web_seed;
pub(crate) mod super_seed;
pub(crate) mod have_buffer;
pub(crate) mod ban;
pub(crate) mod ip_filter;
#[allow(dead_code)] // Wired in during Step 5 (proxy integration).
pub(crate) mod proxy;
pub(crate) mod pipeline;
pub(crate) mod peer_priority;
pub(crate) mod url_guard;
#[allow(dead_code)] // Wired in during Task 2 (DSCP wiring to session/torrent actors).
pub(crate) mod dscp;
pub mod i2p;
#[allow(dead_code)] // Wired in during Phase 3c (session SSL integration).
pub(crate) mod ssl_manager;
pub mod extension;
pub mod streaming;
pub mod disk;
pub mod disk_backend;
pub(crate) mod write_buffer;
mod torrent;
mod session;
mod persistence;

pub use alert::{Alert, AlertCategory, AlertKind, AlertStream};
pub use error::{Error, Result};
pub use settings::Settings;
pub use disk::{DiskConfig, DiskHandle, DiskJobFlags, DiskManagerHandle, DiskStats};
pub use disk_backend::{DiskIoBackend, DiskIoStats, DisabledDiskIo};
pub use types::{FileInfo, SessionStats, StorageFactory, TorrentConfig, TorrentInfo, TorrentState, TorrentStats};
pub use torrent::TorrentHandle;
pub use session::SessionHandle;
pub use ban::BanConfig;
pub use ip_filter::{IpFilter, IpFilterError, PortFilter, parse_dat, parse_p2p};
pub use proxy::{ProxyType, ProxyConfig};
pub use persistence::{SessionState, DhtNodeEntry, PeerStrikeEntry, validate_resume_bitfield};
pub use crate::piece_selector::build_wanted_pieces;
pub use crate::tracker_manager::{TrackerInfo, TrackerStatus};
pub use streaming::FileStream;
pub use peer_state::PeerSource;
pub use extension::ExtensionPlugin;
pub use i2p::{I2pDestination, I2pDestinationError};
pub use choker::{SeedChokingAlgorithm, ChokingAlgorithm};
pub use rate_limiter::MixedModeAlgorithm;
