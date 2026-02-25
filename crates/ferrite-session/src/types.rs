use std::net::SocketAddr;
use std::path::PathBuf;

use bytes::Bytes;
use tokio::sync::oneshot;

use ferrite_storage::Bitfield;
use ferrite_wire::ExtHandshake;

/// Configurable parameters for a torrent session.
#[derive(Debug, Clone)]
pub struct TorrentConfig {
    pub listen_port: u16,
    pub max_peers: usize,
    pub target_request_queue: usize,
    pub download_dir: PathBuf,
    pub enable_dht: bool,
    pub enable_pex: bool,
}

impl Default for TorrentConfig {
    fn default() -> Self {
        Self {
            listen_port: 6881,
            max_peers: 50,
            target_request_queue: 5,
            download_dir: PathBuf::from("."),
            enable_dht: true,
            enable_pex: true,
        }
    }
}

/// Current state of a torrent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorrentState {
    FetchingMetadata,
    Downloading,
    Complete,
    Stopped,
}

/// Aggregate statistics for a torrent.
#[derive(Debug, Clone)]
pub struct TorrentStats {
    pub state: TorrentState,
    pub downloaded: u64,
    pub uploaded: u64,
    pub pieces_have: u32,
    pub pieces_total: u32,
    pub peers_connected: usize,
    pub peers_available: usize,
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
    Disconnected {
        peer_addr: SocketAddr,
        reason: Option<String>,
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
    Shutdown,
}

/// Commands sent from a `TorrentHandle` to the `TorrentActor`.
#[derive(Debug)]
#[allow(dead_code)] // consumed by torrent module (not yet implemented)
pub(crate) enum TorrentCommand {
    AddPeers { peers: Vec<SocketAddr> },
    Stats { reply: oneshot::Sender<TorrentStats> },
    Shutdown,
}
