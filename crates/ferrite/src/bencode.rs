//! Serde-based bencode codec for BitTorrent.
//!
//! Re-exports from [`ferrite_bencode`].

pub use ferrite_bencode::{
    // Codec functions
    to_bytes,
    from_bytes,
    // Span utility
    find_dict_key_span,
    // Dynamic value type
    BencodeValue,
    // Serde impls (for advanced use)
    Serializer,
    Deserializer,
    // Error types
    Error,
    Result,
};
