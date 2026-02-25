//! BitTorrent session management: peers, torrents, and piece selection.

mod error;
mod types;
pub(crate) mod peer_state;
pub(crate) mod metadata;
// These will be added as they're implemented:
pub(crate) mod piece_selector;
pub(crate) mod pex;
pub(crate) mod choker;
pub(crate) mod peer;
mod torrent;

pub use error::{Error, Result};
pub use types::{TorrentConfig, TorrentState, TorrentStats};
pub use torrent::TorrentHandle;
