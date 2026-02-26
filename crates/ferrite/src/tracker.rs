//! BitTorrent tracker client (BEP 3, 15, 48).
//!
//! Re-exports from [`ferrite_tracker`].

pub use ferrite_tracker::{
    // HTTP tracker
    HttpTracker,
    HttpAnnounceResponse,
    // UDP tracker
    UdpTracker,
    UdpAnnounceResponse,
    // Common request/response types
    AnnounceRequest,
    AnnounceResponse,
    AnnounceEvent,
    // Compact peer parsing
    parse_compact_peers,
    // Error types
    Error,
    Result,
};
