//! BitTorrent tracker client (BEP 3, 15, 48).
//!
//! Re-exports from [`torrent_tracker`].

pub use torrent_tracker::{
    AnnounceEvent,
    // Common request/response types
    AnnounceRequest,
    AnnounceResponse,
    // Error types
    Error,
    HttpAnnounceResponse,
    HttpScrapeResponse,
    // HTTP tracker
    HttpTracker,
    Result,
    // Scrape
    ScrapeInfo,
    UdpAnnounceResponse,
    UdpScrapeResponse,
    // UDP tracker
    UdpTracker,
    announce_url_to_scrape,
    encode_compact_peers,
    encode_compact_peers6,
    // Compact peer parsing/encoding
    parse_compact_peers,
    parse_compact_peers6,
};
