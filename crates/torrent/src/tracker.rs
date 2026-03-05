//! BitTorrent tracker client (BEP 3, 15, 48).
//!
//! Re-exports from [`torrent_tracker`].

pub use torrent_tracker::{
    // HTTP tracker
    HttpTracker,
    HttpAnnounceResponse,
    HttpScrapeResponse,
    // UDP tracker
    UdpTracker,
    UdpAnnounceResponse,
    UdpScrapeResponse,
    // Scrape
    ScrapeInfo,
    announce_url_to_scrape,
    // Common request/response types
    AnnounceRequest,
    AnnounceResponse,
    AnnounceEvent,
    // Compact peer parsing/encoding
    parse_compact_peers,
    encode_compact_peers,
    parse_compact_peers6,
    encode_compact_peers6,
    // Error types
    Error,
    Result,
};
