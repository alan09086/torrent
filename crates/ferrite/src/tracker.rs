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
    // Compact peer parsing/encoding
    parse_compact_peers,
    encode_compact_peers,
    parse_compact_peers6,
    encode_compact_peers6,
    // Error types
    Error,
    Result,
};
