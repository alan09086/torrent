#![warn(missing_docs)]
//! HTTP and UDP BitTorrent tracker clients (BEP 3, BEP 15, BEP 48).
//!
//! Supports HTTP and UDP tracker protocols for announce and scrape.

/// Compact peer list encoding/decoding (BEP 23, BEP 7).
pub mod compact;
mod error;
mod http;
mod udp;

pub use compact::{
    parse_compact_peers, encode_compact_peers,
    parse_compact_peers6, encode_compact_peers6,
};
pub use error::{Error, Result};
pub use http::{HttpTracker, HttpAnnounceResponse, HttpScrapeResponse};
pub use udp::{UdpTracker, UdpAnnounceResponse, UdpScrapeResponse};

use std::net::SocketAddr;

use ferrite_core::Id20;

/// Scrape response data for a single info_hash (BEP 48).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrapeInfo {
    /// Number of seeders (peers with complete file).
    pub complete: u32,
    /// Number of leechers (peers still downloading).
    pub incomplete: u32,
    /// Number of times the torrent has been fully downloaded.
    pub downloaded: u32,
}

/// Convert an announce URL to a scrape URL (BEP 48).
///
/// Replaces the last occurrence of "announce" in the URL path with "scrape".
/// Returns `None` if no "announce" is found in the URL.
pub fn announce_url_to_scrape(url: &str) -> Option<String> {
    let last_pos = url.rfind("announce")?;
    let mut result = String::with_capacity(url.len());
    result.push_str(&url[..last_pos]);
    result.push_str("scrape");
    result.push_str(&url[last_pos + "announce".len()..]);
    Some(result)
}

/// Common announce request parameters.
#[derive(Debug, Clone)]
pub struct AnnounceRequest {
    /// SHA-1 info hash of the torrent.
    pub info_hash: Id20,
    /// Our 20-byte peer ID.
    pub peer_id: Id20,
    /// Port we are listening on.
    pub port: u16,
    /// Total bytes uploaded since last announce.
    pub uploaded: u64,
    /// Total bytes downloaded since last announce.
    pub downloaded: u64,
    /// Bytes remaining to download.
    pub left: u64,
    /// Optional announce event (started/stopped/completed).
    pub event: AnnounceEvent,
    /// Maximum number of peers to return (`None` = tracker default).
    pub num_want: Option<i32>,
    /// Request compact peer list (6 bytes per IPv4 peer).
    pub compact: bool,
}

/// Announce event type sent to trackers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnounceEvent {
    /// Regular periodic re-announce (no event).
    None = 0,
    /// Download just finished -- all pieces verified.
    Completed = 1,
    /// First announce after adding the torrent.
    Started = 2,
    /// Torrent removed or client shutting down.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrape_info_equality() {
        let a = ScrapeInfo { complete: 5, incomplete: 3, downloaded: 50 };
        let b = ScrapeInfo { complete: 5, incomplete: 3, downloaded: 50 };
        assert_eq!(a, b);
    }

    #[test]
    fn announce_to_scrape_url_http() {
        assert_eq!(
            announce_url_to_scrape("http://t.co/announce"),
            Some("http://t.co/scrape".into()),
        );
    }

    #[test]
    fn announce_to_scrape_url_with_path() {
        assert_eq!(
            announce_url_to_scrape("http://t.co/path/announce"),
            Some("http://t.co/path/scrape".into()),
        );
    }

    #[test]
    fn announce_to_scrape_url_no_announce() {
        assert_eq!(announce_url_to_scrape("http://t.co/track"), None);
    }
}
