use std::collections::HashSet;
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use ferrite_storage::Bitfield;
use ferrite_wire::ExtHandshake;

use crate::pipeline::PeerPipelineState;
use crate::types::PeerCommand;

/// Origin of a peer address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PeerSource {
    /// Returned by a tracker announce.
    Tracker,
    /// Discovered via the DHT.
    Dht,
    /// Received via Peer Exchange (BEP 11).
    Pex,
    /// Found via Local Service Discovery (BEP 14).
    Lsd,
    /// Connected to us (incoming connection).
    Incoming,
    /// Loaded from saved resume data.
    ResumeData,
    /// Discovered via I2P SAM bridge.
    I2p,
}

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
    /// Per-peer dynamic request queue sizing (M28).
    pub pipeline: PeerPipelineState,
    /// Whether this peer is snubbed (no data for snub_timeout_secs).
    pub snubbed: bool,
    /// Last time we received data from this peer.
    pub last_data_received: Option<std::time::Instant>,
    /// When this peer connection was established.
    pub connected_at: std::time::Instant,
    /// BEP 6: pieces suggested by this peer.
    pub suggested_pieces: HashSet<u32>,
    /// How this peer was discovered.
    pub source: PeerSource,
    /// BEP 55: peer advertised `ut_holepunch` support in their extension handshake.
    pub supports_holepunch: bool,
    /// Whether this peer appears to be NATed (no incoming connections observed).
    pub appears_nated: bool,
    /// Transport protocol used for this peer connection.
    pub transport: Option<crate::rate_limiter::PeerTransport>,
}

#[allow(dead_code)]
impl PeerState {
    pub fn new(
        addr: SocketAddr,
        bitfield_len: u32,
        cmd_tx: mpsc::Sender<PeerCommand>,
        source: PeerSource,
        max_queue_depth: usize,
        request_queue_time: f64,
        initial_queue_depth: usize,
    ) -> Self {
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
            pipeline: PeerPipelineState::new(max_queue_depth, request_queue_time, initial_queue_depth),
            snubbed: false,
            last_data_received: None,
            connected_at: std::time::Instant::now(),
            suggested_pieces: HashSet::new(),
            source,
            supports_holepunch: false,
            appears_nated: false,
            transport: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_source_serialization() {
        let source = PeerSource::Tracker;
        let json = serde_json::to_string(&source).unwrap();
        assert_eq!(json, "\"Tracker\"");
        let roundtrip: PeerSource = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, PeerSource::Tracker);
    }

    #[test]
    fn peer_source_all_variants() {
        let variants = [
            PeerSource::Tracker,
            PeerSource::Dht,
            PeerSource::Pex,
            PeerSource::Lsd,
            PeerSource::Incoming,
            PeerSource::ResumeData,
            PeerSource::I2p,
        ];
        for source in variants {
            let json = serde_json::to_string(&source).unwrap();
            let roundtrip: PeerSource = serde_json::from_str(&json).unwrap();
            assert_eq!(roundtrip, source);
        }
    }

    #[test]
    fn peer_state_has_connected_at() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let peer = PeerState::new(
            "127.0.0.1:6881".parse().unwrap(),
            100,
            tx,
            PeerSource::Tracker,
            250,
            3.0,
            128,
        );
        assert!(peer.connected_at.elapsed().as_secs() < 1);
    }

    #[test]
    fn peer_source_i2p_serialization() {
        let source = PeerSource::I2p;
        let json = serde_json::to_string(&source).unwrap();
        assert_eq!(json, "\"I2p\"");
        let roundtrip: PeerSource = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, PeerSource::I2p);
    }
}
