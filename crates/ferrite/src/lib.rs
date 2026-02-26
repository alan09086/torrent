//! A Rust BitTorrent library.
//!
//! `ferrite` is the public facade for the ferrite crate family. It re-exports
//! types from internal crates through a clean, ergonomic API.
//!
//! # Modules
//!
//! - [`bencode`] — Serde-based bencode serialization
//! - [`core`] — Hashes, metainfo, magnets, piece arithmetic

pub mod bencode;

// Note: "core" shadows std::core, but since this is a library crate and users
// access it as `ferrite::core`, there's no ambiguity. Internal code that needs
// std::core can use `::core::` path prefix.
pub mod core;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bencode_round_trip_through_facade() {
        use serde::{Serialize, Deserialize};

        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Demo {
            name: String,
            value: i64,
        }

        let original = Demo { name: "ferrite".into(), value: 42 };
        let encoded = bencode::to_bytes(&original).unwrap();
        let decoded: Demo = bencode::from_bytes(&encoded).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn core_types_accessible_through_facade() {
        // Verify hash types
        let data = b"hello";
        let hash = core::sha1(data);
        assert_eq!(hash.to_hex().len(), 40);

        // Verify Id20 hex round-trip
        let hex = hash.to_hex();
        let parsed = core::Id20::from_hex(&hex).unwrap();
        assert_eq!(hash, parsed);

        // Verify Lengths arithmetic
        let lengths = core::Lengths::new(1048576, 262144, core::DEFAULT_CHUNK_SIZE);
        assert_eq!(lengths.num_pieces(), 4);

        // Verify PeerId generation
        let peer_id = core::PeerId::generate();
        assert_eq!(peer_id.0 .0.len(), 20);
    }
}
