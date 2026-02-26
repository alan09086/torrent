//! A Rust BitTorrent library.
//!
//! `ferrite` is the public facade for the ferrite crate family. It re-exports
//! types from internal crates through a clean, ergonomic API.
//!
//! # Modules
//!
//! - [`bencode`] — Serde-based bencode serialization
//! - [`core`] — Hashes, metainfo, magnets, piece arithmetic
//! - [`wire`] — Peer wire protocol, handshake, extensions
//! - [`tracker`] — HTTP + UDP tracker announce

pub mod bencode;

// Note: "core" shadows std::core, but since this is a library crate and users
// access it as `ferrite::core`, there's no ambiguity. Internal code that needs
// std::core can use `::core::` path prefix.
pub mod core;

pub mod wire;

pub mod tracker;

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

    #[test]
    fn magnet_parse_through_facade() {
        let uri = "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d\
                   &dn=test%20file\
                   &tr=http%3A%2F%2Ftracker.example.com%2Fannounce";
        let magnet = core::Magnet::parse(uri).unwrap();

        assert_eq!(magnet.display_name.as_deref(), Some("test file"));
        assert_eq!(magnet.trackers.len(), 1);
        assert_eq!(magnet.info_hash.to_hex(), "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");

        // Round-trip back to URI
        let rebuilt = magnet.to_uri();
        let reparsed = core::Magnet::parse(&rebuilt).unwrap();
        assert_eq!(magnet.info_hash, reparsed.info_hash);
        assert_eq!(magnet.display_name, reparsed.display_name);
    }

    #[test]
    fn handshake_round_trip_through_facade() {
        let info_hash = core::Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let peer_id = core::Id20::from_hex("0102030405060708091011121314151617181920").unwrap();

        let hs = wire::Handshake::new(info_hash, peer_id);
        assert!(hs.supports_extensions());

        // Encode to bytes and decode back
        let bytes = hs.to_bytes();
        assert_eq!(bytes.len(), 68);

        let parsed = wire::Handshake::from_bytes(&bytes).unwrap();
        assert_eq!(hs, parsed);
        assert_eq!(parsed.info_hash, info_hash);
    }

    #[test]
    fn announce_request_construction_through_facade() {
        let info_hash = core::Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let peer_id = core::Id20::from_hex("0102030405060708091011121314151617181920").unwrap();

        let req = tracker::AnnounceRequest {
            info_hash,
            peer_id,
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 1048576,
            event: tracker::AnnounceEvent::Started,
            num_want: Some(50),
            compact: true,
        };

        assert_eq!(req.port, 6881);
        assert_eq!(req.left, 1048576);
        assert!(req.compact);
        assert_eq!(req.event, tracker::AnnounceEvent::Started);
    }
}
