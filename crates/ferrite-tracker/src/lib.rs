//! BitTorrent tracker client (BEP 3, 15, 48).
//!
//! Supports HTTP and UDP tracker protocols for announce and scrape.

pub mod compact;
mod error;
mod http;
mod udp;

pub use compact::{
    parse_compact_peers, encode_compact_peers,
    parse_compact_peers6, encode_compact_peers6,
};
pub use error::{Error, Result};
pub use http::{HttpTracker, HttpAnnounceResponse};
pub use udp::{UdpTracker, UdpAnnounceResponse};

use std::net::SocketAddr;

use ferrite_core::Id20;

/// Common announce request parameters.
#[derive(Debug, Clone)]
pub struct AnnounceRequest {
    pub info_hash: Id20,
    pub peer_id: Id20,
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub event: AnnounceEvent,
    pub num_want: Option<i32>,
    pub compact: bool,
}

/// Announce event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnounceEvent {
    None = 0,
    Completed = 1,
    Started = 2,
    Stopped = 3,
}

/// Common announce response data.
#[derive(Debug, Clone)]
pub struct AnnounceResponse {
    /// Re-announce interval in seconds.
    pub interval: u32,
    /// Number of seeders (optional).
    pub seeders: Option<u32>,
    /// Number of leechers (optional).
    pub leechers: Option<u32>,
    /// Peer addresses.
    pub peers: Vec<SocketAddr>,
}
