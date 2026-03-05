//! Serde-based bencode codec for BitTorrent.
//!
//! Re-exports from [`torrent_bencode`].

pub use torrent_bencode::{
    // Dynamic value type
    BencodeValue,
    Deserializer,
    // Error types
    Error,
    Result,
    // Serde impls (for advanced use)
    Serializer,
    // Span utility
    find_dict_key_span,
    from_bytes,
    // Codec functions
    to_bytes,
};
