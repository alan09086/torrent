use std::collections::HashSet;
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
    /// Bytes downloaded from this peer in current rate window.
    pub download_bytes_window: u64,
    /// Bytes uploaded to this peer in current rate window.
    pub upload_bytes_window: u64,
    /// Outstanding requests to this peer (index, begin, length).
    pub pending_requests: Vec<(u32, u32, u32)>,
    /// Requests from this peer to us (index, begin, length).
    pub incoming_requests: Vec<(u32, u32, u32)>,
    /// Peer's extension handshake, if received.
    pub ext_handshake: Option<ExtHandshake>,
    /// Whether the peer supports BEP 6 Fast Extension.
    pub supports_fast: bool,
    /// Set of piece indices the peer is allowed to request while choked.
    pub allowed_fast: HashSet<u32>,
    /// BEP 21: peer declared upload-only status.
    pub upload_only: bool,
    /// BEP 16: piece index we revealed to this peer in super-seed mode.
    pub super_seed_assigned: Option<u32>,
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
            download_bytes_window: 0,
            upload_bytes_window: 0,
            pending_requests: Vec::new(),
            incoming_requests: Vec::new(),
            ext_handshake: None,
            supports_fast: false,
            allowed_fast: HashSet::new(),
            upload_only: false,
            super_seed_assigned: None,
            cmd_tx,
        }
    }
}
