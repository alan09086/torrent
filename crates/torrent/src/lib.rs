#![warn(missing_docs)]

//! A Rust BitTorrent library.
//!
//! `torrent` is the public facade for the torrent crate family. It re-exports
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
//! - [`utp`] — uTP (BEP 29) micro transport protocol
//! - [`nat`] — NAT port mapping (PCP / NAT-PMP / UPnP IGD)
//! - [`client`] — Ergonomic `ClientBuilder` and `AddTorrentParams`
//! - [`prelude`] — Convenience re-exports for `use torrent::prelude::*`

pub mod bencode;

// Note: "core" shadows std::core, but since this is a library crate and users
// access it as `torrent::core`, there's no ambiguity. Internal code that needs
// std::core can use `::core::` path prefix.
pub mod core;

pub mod wire;

pub mod tracker;

pub mod dht;

pub mod storage;

pub mod session;

pub mod utp;

pub mod nat;

pub mod client;

pub mod error;
pub mod prelude;

// Top-level convenience re-exports
pub use client::{AddTorrentParams, ClientBuilder};
pub use error::{Error, Result};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bencode_round_trip_through_facade() {
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Demo {
            name: String,
            value: i64,
        }

        let original = Demo {
            name: "torrent".into(),
            value: 42,
        };
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
        assert_eq!(peer_id.0.0.len(), 20);
    }

    #[test]
    fn magnet_parse_through_facade() {
        let uri = "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d\
                   &dn=test%20file\
                   &tr=http%3A%2F%2Ftracker.example.com%2Fannounce";
        let magnet = core::Magnet::parse(uri).unwrap();

        assert_eq!(magnet.display_name.as_deref(), Some("test file"));
        assert_eq!(magnet.trackers.len(), 1);
        assert_eq!(
            magnet.info_hash().to_hex(),
            "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"
        );

        // Round-trip back to URI
        let rebuilt = magnet.to_uri();
        let reparsed = core::Magnet::parse(&rebuilt).unwrap();
        assert_eq!(magnet.info_hash(), reparsed.info_hash());
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
            i2p_destination: None,
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

        let config = builder.into_settings();
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
            "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&dn=test",
        )
        .unwrap();

        let params = crate::AddTorrentParams::from_magnet(magnet).download_dir("/tmp/downloads");

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
        let _magnet =
            Magnet::parse("magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
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
            "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&dn=test%20file",
        )
        .unwrap();

        assert_eq!(magnet.display_name.as_deref(), Some("test file"));

        let _params = AddTorrentParams::from_magnet(magnet).download_dir("/tmp/test");

        // Verify unified Result type works
        let ok_result: Result<i32> = Ok(42);
        assert_eq!(ok_result.unwrap(), 42);

        // Verify error conversion chain: bencode → unified
        let err_result: Result<()> =
            Err(Error::Bencode(crate::bencode::Error::Custom("test".into())));
        assert!(err_result.is_err());
    }

    #[test]
    fn resume_data_accessible_through_facade() {
        // Create resume data through facade
        let rd = core::FastResumeData::new(vec![0xAA; 20], "test".into(), "/tmp".into());
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
            dht_node_id: None,
            torrents: vec![rd],
            banned_peers: Vec::new(),
            peer_strikes: Vec::new(),
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
        let config = builder.into_settings();
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
        let _rd = FastResumeData::new(vec![0xCC; 20], "prelude-test".into(), "/tmp".into());
        let _state = SessionState {
            dht_nodes: Vec::new(),
            dht_node_id: None,
            torrents: Vec::new(),
            banned_peers: Vec::new(),
            peer_strikes: Vec::new(),
        };
    }

    #[tokio::test]
    async fn utp_outbound_and_accept() {
        // Create two UtpSocket instances on loopback, connect one to the other,
        // and verify data flows through the connection.
        let (socket_a, mut listener_a) = utp::UtpSocket::bind(utp::UtpConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            max_connections: 8,
            dscp: 0,
        })
        .await
        .unwrap();

        let addr_a = socket_a.local_addr();

        let (socket_b, _listener_b) = utp::UtpSocket::bind(utp::UtpConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            max_connections: 8,
            dscp: 0,
        })
        .await
        .unwrap();

        // B connects to A (keep socket_b alive so the stream isn't dropped)
        let connect_handle = tokio::spawn({
            let socket_b = socket_b.clone();
            async move { socket_b.connect(addr_a).await.unwrap() }
        });

        // A accepts
        let (mut stream_a, _peer_addr) = listener_a.accept().await.unwrap();
        let mut stream_b = connect_handle.await.unwrap();

        // Exchange data bidirectionally
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        stream_b.write_all(b"hello from B").await.unwrap();
        stream_b.flush().await.unwrap();
        let mut buf = vec![0u8; 20];
        let n = stream_a.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello from B");

        stream_a.write_all(b"hello from A").await.unwrap();
        stream_a.flush().await.unwrap();
        let n = stream_b.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello from A");

        socket_a.shutdown().await.unwrap();
        socket_b.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn utp_fallback_to_tcp() {
        // Session with uTP disabled starts successfully and operates normally via TCP.
        let session = crate::ClientBuilder::new()
            .listen_port(0)
            .download_dir("/tmp")
            .enable_utp(false)
            .enable_dht(false)
            .enable_lsd(false)
            .start()
            .await
            .unwrap();

        let stats = session.session_stats().await.unwrap();
        assert_eq!(stats.active_torrents, 0);

        // Add a torrent (proves the session works without uTP)
        let data = vec![0xAB; 16384];
        let meta = make_test_torrent_facade(&data, 16384);
        let info_hash = session
            .add_torrent(meta.into(), Some(make_storage_facade(&data, 16384)))
            .await
            .unwrap();

        let list = session.list_torrents().await.unwrap();
        assert_eq!(list.len(), 1);
        assert!(list.contains(&info_hash));

        session.shutdown().await.unwrap();
    }

    fn make_test_torrent_facade(data: &[u8], piece_length: u64) -> core::TorrentMetaV1 {
        use serde::Serialize;

        let mut pieces = Vec::new();
        let mut offset = 0;
        while offset < data.len() {
            let end = (offset + piece_length as usize).min(data.len());
            let hash = core::sha1(&data[offset..end]);
            pieces.extend_from_slice(hash.as_bytes());
            offset = end;
        }

        #[derive(Serialize)]
        struct Info<'a> {
            length: u64,
            name: &'a str,
            #[serde(rename = "piece length")]
            piece_length: u64,
            #[serde(with = "serde_bytes")]
            pieces: &'a [u8],
        }

        #[derive(Serialize)]
        struct Torrent<'a> {
            info: Info<'a>,
        }

        let t = Torrent {
            info: Info {
                length: data.len() as u64,
                name: "test",
                piece_length,
                pieces: &pieces,
            },
        };

        let bytes = bencode::to_bytes(&t).unwrap();
        core::torrent_from_bytes(&bytes).unwrap()
    }

    fn make_storage_facade(
        data: &[u8],
        piece_length: u64,
    ) -> std::sync::Arc<storage::MemoryStorage> {
        let lengths = core::Lengths::new(data.len() as u64, piece_length, core::DEFAULT_CHUNK_SIZE);
        std::sync::Arc::new(storage::MemoryStorage::new(lengths))
    }

    #[test]
    fn ipv6_types_accessible_through_facade() {
        use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};

        // AddressFamily enum
        let _v4 = core::AddressFamily::V4;
        let _v6 = core::AddressFamily::V6;

        // DhtConfig::default_v6
        let config = dht::DhtConfig::default_v6();
        assert_eq!(config.address_family, core::AddressFamily::V6);

        // CompactNodeInfo6 round-trip
        let node = dht::CompactNodeInfo6 {
            id: core::Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap(),
            addr: SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 6881, 0, 0)),
        };
        let encoded = dht::encode_compact_nodes6(&[node.clone()]);
        assert_eq!(encoded.len(), 38);
        let decoded = dht::parse_compact_nodes6(&encoded).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].id, node.id);
        assert_eq!(decoded[0].addr, node.addr);

        // Compact peers6 round-trip
        let peers = vec![SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::LOCALHOST,
            6881,
            0,
            0,
        ))];
        let encoded = tracker::encode_compact_peers6(&peers);
        assert_eq!(encoded.len(), 18);
        let decoded = tracker::parse_compact_peers6(&encoded).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0], peers[0]);
    }

    #[test]
    fn client_builder_ipv6_config() {
        // Default: IPv6 enabled
        let config = crate::ClientBuilder::new().into_settings();
        assert!(config.enable_ipv6);

        // Explicitly disabled
        let config = crate::ClientBuilder::new()
            .enable_ipv6(false)
            .into_settings();
        assert!(!config.enable_ipv6);
    }

    #[test]
    fn client_builder_pipeline_config() {
        let config = crate::ClientBuilder::new()
            .max_request_queue_depth(100)
            .request_queue_time(5.0)
            .block_request_timeout_secs(30)
            .max_concurrent_stream_reads(4)
            .into_settings();
        assert_eq!(config.max_request_queue_depth, 100);
        assert!((config.request_queue_time - 5.0).abs() < f64::EPSILON);
        assert_eq!(config.block_request_timeout_secs, 30);
        assert_eq!(config.max_concurrent_stream_reads, 4);
    }

    #[test]
    fn file_stream_in_prelude() {
        // Verify FileStream is accessible via the prelude
        use crate::prelude::*;
        let _: fn() -> &'static str = || std::any::type_name::<FileStream>();
    }

    #[test]
    fn client_builder_nat_config() {
        // Defaults: both enabled
        let config = crate::ClientBuilder::new().into_settings();
        assert!(config.enable_upnp);
        assert!(config.enable_natpmp);

        // Explicitly disabled
        let config = crate::ClientBuilder::new()
            .enable_upnp(false)
            .enable_natpmp(false)
            .into_settings();
        assert!(!config.enable_upnp);
        assert!(!config.enable_natpmp);
    }

    #[test]
    fn nat_types_accessible_through_facade() {
        // NatConfig accessible and has expected defaults
        let config = nat::NatConfig::default();
        assert!(config.enable_upnp);
        assert!(config.enable_natpmp);
        assert_eq!(config.upnp_lease_duration, 3600);

        // Error type accessible
        let err: nat::Error = nat::Error::Timeout;
        assert!(err.to_string().contains("timeout"));
    }

    #[test]
    fn extension_plugin_accessible_through_facade() {
        use crate::prelude::*;

        struct TestPlugin;

        impl ExtensionPlugin for TestPlugin {
            fn name(&self) -> &str {
                "ut_test"
            }
        }

        let plugin: Box<dyn ExtensionPlugin> = Box::new(TestPlugin);
        assert_eq!(plugin.name(), "ut_test");

        // BencodeValue accessible for on_handshake return type
        let _val = crate::bencode::BencodeValue::Integer(42);
    }

    #[test]
    fn client_builder_add_extension() {
        struct DummyPlugin;
        impl session::ExtensionPlugin for DummyPlugin {
            fn name(&self) -> &str {
                "ut_dummy"
            }
        }

        let builder = crate::ClientBuilder::new()
            .listen_port(0)
            .download_dir("/tmp")
            .add_extension(Box::new(DummyPlugin));

        // Verify builder chains correctly — the extension is stored internally
        let _ = builder.into_settings();
    }

    #[test]
    fn i2p_types_accessible_through_facade() {
        // I2pDestination construction and display
        let dest = session::I2pDestination::from_bytes(vec![42u8; 516]);
        assert_eq!(dest.len(), 516);
        assert!(!dest.is_empty());

        // Base64 round-trip
        let b64 = dest.to_base64();
        let parsed = session::I2pDestination::from_base64(&b64).unwrap();
        assert_eq!(parsed, dest);

        // b32 address
        let b32 = dest.to_b32_address();
        assert!(b32.ends_with(".b32.i2p"));

        // Error type accessible
        let err = session::I2pDestinationError::Empty;
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn ssl_types_accessible_through_facade() {
        // SslConfig is accessible via the wire module
        let config = wire::ssl::SslConfig {
            ca_cert_pem: vec![],
            our_cert_pem: vec![],
            our_key_pem: vec![],
        };
        assert!(config.ca_cert_pem.is_empty());

        // generate_self_signed_cert is accessible
        let (cert, key) = wire::ssl::generate_self_signed_cert().unwrap();
        assert!(!cert.is_empty());
        assert!(!key.is_empty());

        // build_client_config and build_server_config are accessible (test compilation)
        let _: fn(&wire::ssl::SslConfig) -> wire::Result<_> = wire::ssl::build_client_config;
        let _: fn(&wire::ssl::SslConfig) -> wire::Result<_> = wire::ssl::build_server_config;
    }

    #[test]
    fn tracker_scrape_types_accessible_through_facade() {
        // ScrapeInfo construction
        let info = tracker::ScrapeInfo {
            complete: 10,
            incomplete: 3,
            downloaded: 50,
        };
        assert_eq!(info.complete, 10);

        // announce_url_to_scrape
        let scrape = tracker::announce_url_to_scrape("http://t.co/announce");
        assert_eq!(scrape, Some("http://t.co/scrape".into()));

        // TrackerStatus enum accessible
        let _status = session::TrackerStatus::NotContacted;
        let _working = session::TrackerStatus::Working;
        let _error = session::TrackerStatus::Error;
    }
}
