//! BitTorrent session management: peers, torrents, and piece selection.

pub mod alert;
mod error;
mod types;
pub(crate) mod peer_state;
pub(crate) mod metadata;
// These will be added as they're implemented:
pub(crate) mod piece_selector;
pub(crate) mod pex;
pub(crate) mod choker;
pub(crate) mod end_game;
pub(crate) mod rate_limiter;
pub(crate) mod slot_tuner;
pub(crate) mod tracker_manager;
pub(crate) mod lsd;
pub(crate) mod peer;
pub(crate) mod queue;
mod torrent;
mod session;
mod persistence;

pub use alert::{Alert, AlertCategory, AlertKind, AlertStream};
pub use error::{Error, Result};
pub use types::{FileInfo, SessionConfig, SessionStats, StorageFactory, TorrentConfig, TorrentInfo, TorrentState, TorrentStats};
pub use torrent::TorrentHandle;
pub use session::SessionHandle;
pub use persistence::{SessionState, DhtNodeEntry, validate_resume_bitfield};
pub use crate::piece_selector::build_wanted_pieces;
