//! BitTorrent session management: peers, torrents, and piece selection.

pub mod alert;
mod error;
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
pub(crate) mod pipeline;
pub mod streaming;
pub mod disk;
pub(crate) mod write_buffer;
mod torrent;
mod session;
mod persistence;

pub use alert::{Alert, AlertCategory, AlertKind, AlertStream};
pub use error::{Error, Result};
pub use disk::{DiskConfig, DiskHandle, DiskJobFlags, DiskManagerHandle, DiskStats};
pub use types::{FileInfo, SessionConfig, SessionStats, StorageFactory, TorrentConfig, TorrentInfo, TorrentState, TorrentStats};
pub use torrent::TorrentHandle;
pub use session::SessionHandle;
pub use ban::BanConfig;
pub use persistence::{SessionState, DhtNodeEntry, PeerStrikeEntry, validate_resume_bitfield};
pub use crate::piece_selector::build_wanted_pieces;
pub use crate::tracker_manager::{TrackerInfo, TrackerStatus};
pub use streaming::FileStream;
