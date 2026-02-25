use std::net::SocketAddr;

use tokio::sync::mpsc;

use ferrite_storage::Bitfield;
use ferrite_wire::ExtHandshake;

use crate::types::PeerCommand;

/// Per-peer state tracked by the torrent actor.
#[allow(dead_code)] // consumed by torrent module (not yet implemented)
pub(crate) struct PeerState {
    pub addr: SocketAddr,
    /// Peer is choking us (default: true).
    pub peer_choking: bool,
    /// Peer is interested in us (default: false).
    pub peer_interested: bool,
    /// We are choking the peer (default: true).
    pub am_choking: bool,
    /// We are interested in the peer (default: false).
    pub am_interested: bool,
    /// What pieces the peer has.
    pub bitfield: Bitfield,
    /// Download rate in bytes/sec (for choker).
    pub download_rate: u64,
    /// Upload rate in bytes/sec.
    pub upload_rate: u64,
    /// Outstanding requests to this peer.
    pub pending_requests: usize,
    /// Peer's extension handshake, if received.
    pub ext_handshake: Option<ExtHandshake>,
    /// Channel to send commands to this peer's task.
    pub cmd_tx: mpsc::Sender<PeerCommand>,
}

#[allow(dead_code)]
impl PeerState {
    pub fn new(addr: SocketAddr, bitfield_len: u32, cmd_tx: mpsc::Sender<PeerCommand>) -> Self {
        Self {
            addr,
            peer_choking: true,
            peer_interested: false,
            am_choking: true,
            am_interested: false,
            bitfield: Bitfield::new(bitfield_len),
            download_rate: 0,
            upload_rate: 0,
            pending_requests: 0,
            ext_handshake: None,
            cmd_tx,
        }
    }
}
