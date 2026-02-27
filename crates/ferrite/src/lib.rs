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
//! - [`dht`] — Kademlia DHT peer discovery
//! - [`storage`] — Piece storage, verification, disk I/O
//! - [`session`] — Session management, torrent orchestration
//! - [`client`] — Ergonomic `ClientBuilder` and `AddTorrentParams`
//! - [`prelude`] — Convenience re-exports for `use ferrite::prelude::*`

pub mod bencode;

// Note: "core" shadows std::core, but since this is a library crate and users
// access it as `ferrite::core`, there's no ambiguity. Internal code that needs
// std::core can use `::core::` path prefix.
pub mod core;

pub mod wire;

pub mod tracker;

pub mod dht;

pub mod storage;

pub mod session;

pub mod client;

pub mod error;
pub mod prelude;

// Top-level convenience re-exports
pub use client::{ClientBuilder, AddTorrentParams};
pub use error::{Error, Result};

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

    #[test]
    fn dht_compact_node_round_trip_through_facade() {
        use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

        let node = dht::CompactNodeInfo {
            id: core::Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap(),
            addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), 6881)),
        };

        let encoded = dht::encode_compact_nodes(&[node.clone()]);
        assert_eq!(encoded.len(), 26); // 20-byte ID + 4-byte IP + 2-byte port

        let decoded = dht::parse_compact_nodes(&encoded).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].id, node.id);
        assert_eq!(decoded[0].addr, node.addr);
    }

    #[test]
    fn storage_bitfield_through_facade() {
        let mut bf = storage::Bitfield::new(16);
        assert_eq!(bf.len(), 16);
        assert!(!bf.get(0));
        assert_eq!(bf.count_ones(), 0);

        bf.set(0);
        bf.set(5);
        bf.set(15);
        assert!(bf.get(0));
        assert!(bf.get(5));
        assert!(bf.get(15));
        assert!(!bf.get(1));
        assert_eq!(bf.count_ones(), 3);
    }

    #[test]
    fn client_builder_defaults_and_chaining() {
        let builder = crate::ClientBuilder::new()
            .listen_port(6882)
            .download_dir("/tmp/test")
            .max_torrents(50)
            .enable_dht(false)
            .enable_lsd(false)
            .enable_pex(true)
            .enable_fast_extension(true)
            .seed_ratio_limit(2.0);

        let config = builder.into_config();
        assert_eq!(config.listen_port, 6882);
        assert_eq!(config.download_dir, std::path::PathBuf::from("/tmp/test"));
        assert_eq!(config.max_torrents, 50);
        assert!(!config.enable_dht);
        assert!(!config.enable_lsd);
        assert!(config.enable_pex);
        assert!(config.enable_fast_extension);
        assert_eq!(config.seed_ratio_limit, Some(2.0));
    }

    #[test]
    fn add_torrent_params_from_magnet() {
        let magnet = core::Magnet::parse(
            "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&dn=test"
        ).unwrap();

        let params = crate::AddTorrentParams::from_magnet(magnet)
            .download_dir("/tmp/downloads");

        // Verify the builder pattern works (we can't inspect private fields,
        // but we can verify it compiles and chains correctly)
        let _ = params;
    }

    #[test]
    fn unified_error_from_conversions() {
        // Bencode error → unified Error
        let bencode_err = bencode::Error::Custom("test".into());
        let unified: crate::Error = bencode_err.into();
        assert!(matches!(unified, crate::Error::Bencode(_)));
        assert!(unified.to_string().contains("bencode:"));

        // Core error → unified Error
        let core_err = core::Error::InvalidHex("bad".into());
        let unified: crate::Error = core_err.into();
        assert!(matches!(unified, crate::Error::Core(_)));

        // IO error → unified Error
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let unified: crate::Error = io_err.into();
        assert!(matches!(unified, crate::Error::Io(_)));
    }

    #[test]
    fn prelude_types_accessible() {
        use crate::prelude::*;

        // Verify key types are in scope from prelude
        let _builder = ClientBuilder::new();
        let _magnet = Magnet::parse(
            "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"
        ).unwrap();
        let _hash = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();

        // Verify TorrentState variants accessible
        let _state = TorrentState::Downloading;
        let _state2 = TorrentState::Seeding;
    }

    #[test]
    fn full_type_chain_through_facade() {
        use crate::prelude::*;

        // Parse magnet → create AddTorrentParams → verify types compose
        let magnet = Magnet::parse(
            "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&dn=test%20file"
        ).unwrap();

        assert_eq!(magnet.display_name.as_deref(), Some("test file"));

        let _params = AddTorrentParams::from_magnet(magnet)
            .download_dir("/tmp/test");

        // Verify unified Result type works
        let ok_result: Result<i32> = Ok(42);
        assert_eq!(ok_result.unwrap(), 42);

        // Verify error conversion chain: bencode → unified
        let err_result: Result<()> = Err(
            Error::Bencode(crate::bencode::Error::Custom("test".into()))
        );
        assert!(err_result.is_err());
    }

    #[test]
    fn resume_data_accessible_through_facade() {
        // Create resume data through facade
        let rd = core::FastResumeData::new(
            vec![0xAA; 20],
            "test".into(),
            "/tmp".into(),
        );
        assert_eq!(rd.file_format, "libtorrent resume file");
        assert_eq!(rd.file_version, 1);
        assert_eq!(rd.info_hash.len(), 20);

        // Round-trip through bencode via facade
        let encoded = bencode::to_bytes(&rd).unwrap();
        let decoded: core::FastResumeData = bencode::from_bytes(&encoded).unwrap();
        assert_eq!(rd, decoded);

        // SessionState accessible
        let state = session::SessionState {
            dht_nodes: vec![session::DhtNodeEntry {
                host: "127.0.0.1".into(),
                port: 6881,
            }],
            torrents: vec![rd],
        };
        let encoded = bencode::to_bytes(&state).unwrap();
        let decoded: session::SessionState = bencode::from_bytes(&encoded).unwrap();
        assert_eq!(state, decoded);

        // Validate bitfield helper
        assert!(session::validate_resume_bitfield(&[0xFF], 8));
        assert!(!session::validate_resume_bitfield(&[0xFF], 9));
    }

    #[test]
    fn file_priority_accessible_through_facade() {
        use crate::prelude::*;

        // Default is Normal
        assert_eq!(FilePriority::default(), FilePriority::Normal);

        // Ordering works
        assert!(FilePriority::Skip < FilePriority::High);

        // From<u8> conversion
        assert_eq!(FilePriority::from(0u8), FilePriority::Skip);
        assert_eq!(FilePriority::from(7u8), FilePriority::High);
    }

    #[test]
    fn client_builder_queue_config() {
        let builder = crate::ClientBuilder::new()
            .active_downloads(5)
            .active_seeds(10)
            .active_limit(100)
            .active_checking(2)
            .dont_count_slow_torrents(false)
            .auto_manage_interval(60)
            .auto_manage_startup(120)
            .auto_manage_prefer_seeds(true);
        let config = builder.into_config();
        assert_eq!(config.active_downloads, 5);
        assert_eq!(config.active_seeds, 10);
        assert_eq!(config.active_limit, 100);
        assert_eq!(config.active_checking, 2);
        assert!(!config.dont_count_slow_torrents);
        assert_eq!(config.auto_manage_interval, 60);
        assert_eq!(config.auto_manage_startup, 120);
        assert!(config.auto_manage_prefer_seeds);
    }

    #[test]
    fn resume_data_in_prelude() {
        use crate::prelude::*;
        let _rd = FastResumeData::new(
            vec![0xCC; 20],
            "prelude-test".into(),
            "/tmp".into(),
        );
        let _state = SessionState {
            dht_nodes: Vec::new(),
            torrents: Vec::new(),
        };
    }
}
