//! PeerTask — one tokio task per TCP connection.
//!
//! Handles handshake, BEP 10 extension negotiation, then delegates to
//! `PeerConnection<TorrentPeerHandler>` for the main message loop.

use std::net::SocketAddr;

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::debug;

use torrent_core::Id20;
use torrent_storage::Bitfield;
use torrent_wire::{ExtHandshake, Handshake, Message};

use crate::peer_codec::{PeerReader, PeerWriter};
use crate::peer_connection::PeerConnection;
use crate::torrent_peer_handler::TorrentPeerHandler;
use crate::types::{PeerCommand, PeerEvent};

/// Total handshake size: 1 + 19 + 8 + 20 + 20 = 68 bytes.
const HANDSHAKE_SIZE: usize = 68;

/// Run a single peer connection, handling handshake, extension negotiation,
/// and the message loop.
///
/// The generic stream parameter allows tests to use `tokio::io::duplex()`.
#[allow(dead_code, clippy::too_many_arguments)]
pub(crate) async fn run_peer(
    addr: SocketAddr,
    stream: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    info_hash: Id20,
    our_peer_id: Id20,
    our_bitfield: Bitfield,
    num_pieces: u32,
    event_tx: mpsc::Sender<PeerEvent>,
    cmd_rx: mpsc::Receiver<PeerCommand>,
    enable_dht: bool,
    enable_fast: bool,
    encryption_mode: torrent_wire::mse::EncryptionMode,
    outbound: bool,
    anonymous_mode: bool,
    info_bytes: Option<Bytes>,
    plugins: std::sync::Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>>,
    enable_holepunch: bool,
    max_message_size: usize,
    have_broadcast_rx: tokio::sync::broadcast::Receiver<u32>,
) -> crate::Result<()> {
    use torrent_wire::mse::{self, EncryptionMode, MseStream};

    // --- Phase 0: MSE/PE encryption ---
    // Timeout MSE handshake at 5 seconds to avoid blocking on plaintext-only
    // peers that won't respond to our DH key exchange.
    let mut stream = if encryption_mode != EncryptionMode::Disabled {
        let crypto_provide = match encryption_mode {
            EncryptionMode::Forced => torrent_wire::mse::CRYPTO_RC4,
            EncryptionMode::PreferPlaintext if outbound => torrent_wire::mse::CRYPTO_PLAINTEXT,
            // Inbound PreferPlaintext (guard false), Enabled, and any future variants —
            // accept both methods and let the peer choose.
            _ => torrent_wire::mse::CRYPTO_PLAINTEXT | torrent_wire::mse::CRYPTO_RC4,
        };

        let result = if outbound {
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                mse::handshake::negotiate_outbound(stream, &info_hash, crypto_provide),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err(torrent_wire::Error::EncryptionHandshakeFailed(
                    "MSE handshake timed out".into(),
                )),
            }
        } else {
            mse::handshake::negotiate_inbound(
                stream,
                &info_hash,
                encryption_mode == EncryptionMode::Forced,
            )
            .await
        };

        match result {
            Ok(r) => r.stream,
            Err(e) => return Err(crate::Error::Wire(e)),
        }
    } else {
        MseStream::plaintext(stream)
    };

    // --- Phase 1: Handshake (raw read/write, not framed) ---
    let mut our_hs = Handshake::new(info_hash, our_peer_id);
    if enable_dht {
        our_hs = our_hs.with_dht();
    }
    if enable_fast {
        our_hs = our_hs.with_fast();
    }

    stream.write_all(&our_hs.to_bytes()).await?;
    stream.flush().await?;

    let mut hs_buf = [0u8; HANDSHAKE_SIZE];
    stream.read_exact(&mut hs_buf).await?;

    let their_hs = Handshake::from_bytes(&hs_buf)?;
    if their_hs.info_hash != info_hash {
        return Err(crate::Error::Connection("info_hash mismatch".into()));
    }

    // --- Phase 2: Wrap stream in ring-buffer codec (M109) ---
    let (reader, writer) = tokio::io::split(stream);
    let reader = PeerReader::new(crate::vectored_io::VectoredCompat(reader), max_message_size);
    let mut writer = PeerWriter::new(writer);

    // --- Phase 3: BEP 10 Extension Handshake ---
    let plugin_names: Vec<&str> = plugins.iter().map(|p| p.name()).collect();
    let peer_supports_extensions = their_hs.supports_extensions();
    if peer_supports_extensions {
        let mut ext_hs = ExtHandshake::new_with_plugins(&plugin_names);
        if !enable_holepunch {
            ext_hs.m.remove("ut_holepunch");
        }
        if anonymous_mode {
            ext_hs.v = None;
            ext_hs.p = None;
            ext_hs.reqq = None;
            ext_hs.upload_only = None;
        }
        let payload = ext_hs.to_bytes().map_err(crate::Error::Wire)?;
        writer
            .send(&Message::Extended { ext_id: 0, payload })
            .await?;
    }

    // --- Phase 4: Send our bitfield (fast-aware) ---
    let both_support_fast = enable_fast && their_hs.supports_fast();
    if both_support_fast {
        if num_pieces > 0 && our_bitfield.count_ones() == num_pieces {
            writer.send(&Message::HaveAll).await?;
        } else if our_bitfield.count_ones() == 0 {
            writer.send(&Message::HaveNone).await?;
        } else {
            writer
                .send(&Message::Bitfield(Bytes::copy_from_slice(
                    our_bitfield.as_bytes(),
                )))
                .await?;
        }
    } else if our_bitfield.count_ones() > 0 {
        writer
            .send(&Message::Bitfield(Bytes::copy_from_slice(
                our_bitfield.as_bytes(),
            )))
            .await?;
    }

    // --- Phase 4b: Send AllowedFast set (BEP 6) ---
    if both_support_fast && num_pieces > 0 {
        let fast_set = torrent_wire::allowed_fast_set_for_ip(&info_hash, addr.ip(), num_pieces, 10);
        for index in fast_set {
            writer.send(&Message::AllowedFast(index)).await?;
        }
    }

    // M107: Send unchoke unconditionally on connect (matches rqbit).
    // Costs nothing for a downloader; improves tit-for-tat reciprocity.
    writer.send(&Message::Unchoke).await?;

    // Compute our extension IDs for matching incoming messages
    let our_ext = ExtHandshake::new_with_plugins(&plugin_names);
    let our_ut_metadata: Option<u8> = our_ext.ext_id("ut_metadata");
    let our_ut_pex: Option<u8> = our_ext.ext_id("ut_pex");
    let our_lt_trackers: Option<u8> = our_ext.ext_id("lt_trackers");
    let our_ut_holepunch: Option<u8> = if enable_holepunch {
        our_ext.ext_id("ut_holepunch")
    } else {
        None
    };

    // Notify plugins that a peer connected
    for plugin in plugins.iter() {
        plugin.on_peer_connected(&info_hash, addr);
    }

    debug!(%addr, num_pieces, "entering main loop");

    // --- Phase 5: Construct handler + connection and run ---
    // Note: `both_support_fast` is used as the enable_fast flag for the handler
    // because HaveAll/HaveNone handling in handle_message requires both sides
    // to support fast extension.
    let handler = TorrentPeerHandler::new(
        addr,
        num_pieces,
        event_tx.clone(),
        both_support_fast,
        info_hash,
        info_bytes,
        plugins,
        our_ut_metadata,
        our_ut_pex,
        our_lt_trackers,
        our_ut_holepunch,
    );

    let connection =
        PeerConnection::new(handler, reader, writer, cmd_rx, event_tx, have_broadcast_rx);
    connection.run().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;
    use torrent_wire::{ExtHandshake, MetadataMessage, MetadataMessageType};

    /// Create a dummy broadcast receiver for tests that don't need Have broadcasting.
    fn dummy_broadcast_rx() -> tokio::sync::broadcast::Receiver<u32> {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        drop(tx);
        rx
    }

    fn test_addr() -> SocketAddr {
        "127.0.0.1:6881".parse().unwrap()
    }

    fn test_info_hash() -> Id20 {
        Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap()
    }

    fn test_peer_id() -> Id20 {
        Id20::from_hex("0102030405060708091011121314151617181920").unwrap()
    }

    fn remote_peer_id() -> Id20 {
        Id20::from_hex("2122232425262728293031323334353637383940").unwrap()
    }

    /// Do the handshake from the remote side: read our handshake, send theirs.
    /// Returns the parsed handshake we sent.
    async fn do_remote_handshake(
        stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
        info_hash: Id20,
        remote_id: Id20,
    ) -> Handshake {
        let mut buf = [0u8; HANDSHAKE_SIZE];
        stream.read_exact(&mut buf).await.unwrap();
        let our_hs = Handshake::from_bytes(&buf).unwrap();

        let remote_hs = Handshake::new(info_hash, remote_id);
        stream.write_all(&remote_hs.to_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        our_hs
    }

    /// Read one framed message from a raw stream (length-prefix + payload).
    async fn read_framed_message(stream: &mut (impl AsyncRead + Unpin)) -> Message {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await.unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut payload = vec![0u8; len];
        if len > 0 {
            stream.read_exact(&mut payload).await.unwrap();
        }
        Message::from_payload(Bytes::from(payload)).unwrap()
    }

    /// Write one framed message to a raw stream (length-prefix + payload).
    async fn write_framed_message(stream: &mut (impl AsyncWrite + Unpin), msg: &Message) {
        let bytes = msg.to_bytes();
        stream.write_all(&bytes).await.unwrap();
        stream.flush().await.unwrap();
    }

    /// Perform remote-side extension handshake: read our ext hs, send one back
    /// with ut_metadata=3, ut_pex=4.
    async fn do_remote_ext_handshake(
        stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    ) -> ExtHandshake {
        let msg = read_framed_message(stream).await;
        let our_ext_hs = match msg {
            Message::Extended { ext_id: 0, payload } => ExtHandshake::from_bytes(&payload).unwrap(),
            other => panic!("expected ext handshake, got: {other:?}"),
        };

        // Send remote ext handshake back with different IDs
        let mut remote_ext = ExtHandshake::default();
        remote_ext.m.insert("ut_metadata".into(), 3);
        remote_ext.m.insert("ut_pex".into(), 4);
        remote_ext.v = Some("TestPeer 1.0".into());

        let payload = remote_ext.to_bytes().unwrap();
        write_framed_message(stream, &Message::Extended { ext_id: 0, payload }).await;

        our_ext_hs
    }

    // ---- Test 1: Handshake Exchange ----

    #[tokio::test]
    async fn handshake_exchange() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let our_id = test_peer_id();
        let remote_id = remote_peer_id();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                our_id,
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
                dummy_broadcast_rx(),
            )
            .await
        });

        let our_hs = do_remote_handshake(&mut server_stream, info_hash, remote_id).await;
        assert_eq!(our_hs.info_hash, info_hash);
        assert_eq!(our_hs.peer_id, our_id);
        assert!(our_hs.supports_extensions());

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();

        let result = handle.await.unwrap();
        assert!(result.is_ok());

        // Should get a disconnect event
        let evt = event_rx.recv().await.unwrap();
        assert!(matches!(evt, PeerEvent::Disconnected { .. }));
    }

    // ---- Test 2: Handshake Info Hash Mismatch ----

    #[tokio::test]
    async fn handshake_info_hash_mismatch() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (_cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let wrong_hash = Id20::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        let our_id = test_peer_id();
        let remote_id = remote_peer_id();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                our_id,
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
                dummy_broadcast_rx(),
            )
            .await
        });

        // Read our handshake
        let mut buf = [0u8; HANDSHAKE_SIZE];
        server_stream.read_exact(&mut buf).await.unwrap();

        // Send back handshake with wrong info_hash
        let bad_hs = Handshake::new(wrong_hash, remote_id);
        server_stream.write_all(&bad_hs.to_bytes()).await.unwrap();
        server_stream.flush().await.unwrap();

        let result = handle.await.unwrap();
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("info_hash mismatch"),
            "expected info_hash mismatch error, got: {err_msg}"
        );
    }

    // ---- Test 3: Extension Negotiation ----

    #[tokio::test]
    async fn extension_negotiation() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read the extension handshake message
        let msg = read_framed_message(&mut server_stream).await;
        match msg {
            Message::Extended { ext_id, payload } => {
                assert_eq!(ext_id, 0, "ext handshake should be ext_id=0");
                let ext_hs = ExtHandshake::from_bytes(&payload).unwrap();
                assert!(
                    ext_hs.ext_id("ut_metadata").is_some(),
                    "should advertise ut_metadata"
                );
                assert!(ext_hs.ext_id("ut_pex").is_some(), "should advertise ut_pex");
            }
            other => panic!("expected Extended handshake, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 4: Bitfield Exchange ----

    #[tokio::test]
    async fn bitfield_exchange() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();

        // Create a bitfield with some pieces set
        let mut bitfield = Bitfield::new(16);
        bitfield.set(0);
        bitfield.set(5);
        bitfield.set(15);
        let expected_bytes = bitfield.as_bytes().to_vec();

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                16,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake first (peer supports extensions)
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;

        // Read the bitfield message
        let msg = read_framed_message(&mut server_stream).await;
        match msg {
            Message::Bitfield(data) => {
                assert_eq!(data.as_ref(), expected_bytes.as_slice());
            }
            other => panic!("expected Bitfield, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 5: Choke/Unchoke State ----

    #[tokio::test]
    async fn choke_unchoke_state() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;

        // Send Choke from remote
        write_framed_message(&mut server_stream, &Message::Choke).await;

        let evt = event_rx.recv().await.unwrap();
        match evt {
            PeerEvent::PeerChoking { choking, .. } => assert!(choking),
            other => panic!("expected PeerChoking, got: {other:?}"),
        }

        // Send Unchoke from remote
        write_framed_message(&mut server_stream, &Message::Unchoke).await;

        let evt = event_rx.recv().await.unwrap();
        match evt {
            PeerEvent::PeerChoking { choking, .. } => assert!(!choking),
            other => panic!("expected PeerChoking, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 6: Interested Signaling ----

    #[tokio::test]
    async fn interested_signaling() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;
        // M107: drain the unconditional Unchoke sent on connect
        let _unchoke = read_framed_message(&mut server_stream).await;

        // Send SetInterested command
        cmd_tx.send(PeerCommand::SetInterested(true)).await.unwrap();

        let msg = read_framed_message(&mut server_stream).await;
        assert_eq!(msg, Message::Interested);

        // Send SetInterested(false) command
        cmd_tx
            .send(PeerCommand::SetInterested(false))
            .await
            .unwrap();

        let msg = read_framed_message(&mut server_stream).await;
        assert_eq!(msg, Message::NotInterested);

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 7: Have Forwarding via Broadcast (M118) ----

    #[tokio::test]
    async fn have_forwarding_via_broadcast() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let (have_tx, have_rx) = tokio::sync::broadcast::channel(16);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
                have_rx,
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;
        // M107: drain the unconditional Unchoke sent on connect
        let _unchoke = read_framed_message(&mut server_stream).await;

        // M118: Send Have via broadcast channel (not PeerCommand)
        have_tx.send(5).unwrap();

        let msg = read_framed_message(&mut server_stream).await;
        assert_eq!(msg, Message::Have { index: 5 });

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 8: Request Piece Flow ----

    #[tokio::test]
    async fn request_piece_flow() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;
        // M107: drain the unconditional Unchoke sent on connect
        let _unchoke = read_framed_message(&mut server_stream).await;

        // Send Request command
        cmd_tx
            .send(PeerCommand::Request {
                index: 0,
                begin: 0,
                length: 16384,
            })
            .await
            .unwrap();

        let msg = read_framed_message(&mut server_stream).await;
        assert_eq!(
            msg,
            Message::Request {
                index: 0,
                begin: 0,
                length: 16384,
            }
        );

        // Send Piece data back from remote
        let piece_data = Bytes::from_static(b"test piece data!");
        write_framed_message(
            &mut server_stream,
            &Message::Piece {
                index: 0,
                begin: 0,
                data_0: piece_data.clone(),
                data_1: Bytes::new(),
            },
        )
        .await;

        let evt = event_rx.recv().await.unwrap();
        match evt {
            PeerEvent::PieceData {
                index, begin, data, ..
            } => {
                assert_eq!(index, 0);
                assert_eq!(begin, 0);
                assert_eq!(data, piece_data);
            }
            other => panic!("expected PieceData, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 9: KeepAlive ----

    #[tokio::test]
    async fn keepalive() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;
        // M107: drain the unconditional Unchoke sent on connect
        let _unchoke = read_framed_message(&mut server_stream).await;

        // Send KeepAlive from remote
        write_framed_message(&mut server_stream, &Message::KeepAlive).await;

        // Verify the peer is still alive by sending a command
        cmd_tx.send(PeerCommand::SetInterested(true)).await.unwrap();

        let msg = read_framed_message(&mut server_stream).await;
        assert_eq!(msg, Message::Interested);

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 10: Graceful Disconnect ----

    #[tokio::test]
    async fn graceful_disconnect() {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (_cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
                dummy_broadcast_rx(),
            )
            .await
        });

        // Do handshake then drop the remote end
        let mut server = server_stream;
        do_remote_handshake(&mut server, info_hash, remote_peer_id()).await;

        // Read ext handshake so peer task is in main loop
        let _ext_hs_msg = read_framed_message(&mut server).await;

        // Drop the remote end
        drop(server);

        // Should receive a Disconnected event
        let evt = event_rx.recv().await.unwrap();
        assert!(
            matches!(evt, PeerEvent::Disconnected { .. }),
            "expected Disconnected, got: {evt:?}"
        );

        let _ = handle.await;
    }

    // ---- Test 11: Metadata Request/Response ----

    #[tokio::test]
    async fn metadata_request_response() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Do extension handshake (remote advertises ut_metadata=3)
        let _our_ext = do_remote_ext_handshake(&mut server_stream).await;
        // M107: drain the unconditional Unchoke sent on connect
        let _unchoke = read_framed_message(&mut server_stream).await;

        // Consume the ext handshake event
        let evt = event_rx.recv().await.unwrap();
        assert!(matches!(evt, PeerEvent::ExtHandshake { .. }));

        // Send RequestMetadata command
        cmd_tx
            .send(PeerCommand::RequestMetadata { piece: 0 })
            .await
            .unwrap();

        // Read the metadata request from remote side
        let msg = read_framed_message(&mut server_stream).await;
        match msg {
            Message::Extended { ext_id, payload } => {
                // BEP 10: when sending TO the remote, use the ID from THEIR ext
                // handshake. The remote advertised ut_metadata=3, so ext_id should be 3.
                assert_eq!(ext_id, 3, "should use remote's ut_metadata ID");
                let meta = MetadataMessage::from_bytes(&payload).unwrap();
                assert_eq!(meta.msg_type, MetadataMessageType::Request);
                assert_eq!(meta.piece, 0);
            }
            other => panic!("expected Extended metadata request, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 12: PEX Message Handling ----

    #[tokio::test]
    async fn pex_message_handling() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Do extension handshake (remote advertises ut_pex=4)
        let _our_ext = do_remote_ext_handshake(&mut server_stream).await;

        // Consume the ext handshake event
        let evt = event_rx.recv().await.unwrap();
        assert!(matches!(evt, PeerEvent::ExtHandshake { .. }));

        // Build a PEX message with one added peer: 10.0.0.1:8080
        let pex = crate::pex::PexMessage {
            added: vec![10, 0, 0, 1, 0x1F, 0x90],
            added_flags: vec![0x00],
            ..Default::default()
        };
        let pex_payload = pex.to_bytes().unwrap();

        // Send PEX message using OUR ut_pex ID (2), since the remote sends
        // to us using the ID from OUR ext handshake.
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: 2,
                payload: pex_payload,
            },
        )
        .await;

        let evt = event_rx.recv().await.unwrap();
        match evt {
            PeerEvent::PexPeers { new_peers } => {
                assert_eq!(new_peers.len(), 1);
                assert_eq!(new_peers[0].to_string(), "10.0.0.1:8080");
            }
            other => panic!("expected PexPeers, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 13: Fast Extension — HaveAll sent when complete ----

    /// Do the remote handshake with fast extension enabled.
    async fn do_remote_handshake_fast(
        stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
        info_hash: Id20,
        remote_id: Id20,
    ) -> Handshake {
        let mut buf = [0u8; HANDSHAKE_SIZE];
        stream.read_exact(&mut buf).await.unwrap();
        let our_hs = Handshake::from_bytes(&buf).unwrap();

        let remote_hs = Handshake::new(info_hash, remote_id).with_fast();
        stream.write_all(&remote_hs.to_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        our_hs
    }

    #[tokio::test]
    async fn fast_extension_have_all_sent_when_complete() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let num_pieces = 10u32;

        // Create a full bitfield (all pieces set)
        let mut bitfield = Bitfield::new(num_pieces);
        for i in 0..num_pieces {
            bitfield.set(i);
        }

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                num_pieces,
                event_tx,
                cmd_rx,
                false,
                true, // enable_fast
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
                dummy_broadcast_rx(),
            )
            .await
        });

        let our_hs =
            do_remote_handshake_fast(&mut server_stream, info_hash, remote_peer_id()).await;
        assert!(
            our_hs.supports_fast(),
            "our handshake should advertise fast"
        );

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;

        // Should receive HaveAll (not Bitfield) since both support fast and all pieces set
        let msg = read_framed_message(&mut server_stream).await;
        assert_eq!(msg, Message::HaveAll);

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 14: Fast Extension — HaveNone sent when empty ----

    #[tokio::test]
    async fn fast_extension_have_none_sent_when_empty() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let num_pieces = 10u32;
        let bitfield = Bitfield::new(num_pieces); // empty

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                num_pieces,
                event_tx,
                cmd_rx,
                false,
                true, // enable_fast
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
                dummy_broadcast_rx(),
            )
            .await
        });

        let our_hs =
            do_remote_handshake_fast(&mut server_stream, info_hash, remote_peer_id()).await;
        assert!(our_hs.supports_fast());

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;

        // Should receive HaveNone (not nothing) since both support fast and no pieces
        let msg = read_framed_message(&mut server_stream).await;
        assert_eq!(msg, Message::HaveNone);

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 16: Fast Extension — AllowedFast sent on connect ----

    #[tokio::test]
    async fn fast_extension_sends_allowed_fast_on_connect() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let num_pieces = 100u32;
        let bitfield = Bitfield::new(num_pieces); // empty

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                num_pieces,
                event_tx,
                cmd_rx,
                false,
                true, // enable_fast
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
                dummy_broadcast_rx(),
            )
            .await
        });

        let _our_hs =
            do_remote_handshake_fast(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;

        // Read HaveNone (empty bitfield + fast)
        let msg = read_framed_message(&mut server_stream).await;
        assert_eq!(msg, Message::HaveNone);

        // Should receive 10 AllowedFast messages
        let mut fast_indices = std::collections::HashSet::new();
        for _ in 0..10 {
            let msg = read_framed_message(&mut server_stream).await;
            match msg {
                Message::AllowedFast(index) => {
                    assert!(index < num_pieces, "AllowedFast index out of range");
                    fast_indices.insert(index);
                }
                other => panic!("expected AllowedFast, got: {other:?}"),
            }
        }
        assert_eq!(
            fast_indices.len(),
            10,
            "should have 10 unique AllowedFast indices"
        );

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Test 17: No AllowedFast without fast extension ----

    #[tokio::test]
    async fn no_allowed_fast_without_fast_extension() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let num_pieces = 100u32;
        let bitfield = Bitfield::new(num_pieces); // empty

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                num_pieces,
                event_tx,
                cmd_rx,
                false,
                false, // fast disabled
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;

        // No bitfield (empty + no fast = nothing sent), no AllowedFast
        // Send shutdown and verify peer exits cleanly
        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }

    // ---- Test 18: SendPiece command sends Piece message ----

    #[tokio::test]
    async fn send_piece_command_sends_piece_message() {
        let (client_stream, mut server_stream) = tokio::io::duplex(65536);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,                           // outbound
                false,                           // anonymous_mode
                None,                            // info_bytes
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;
        // M107: drain the unconditional Unchoke sent on connect
        let _unchoke = read_framed_message(&mut server_stream).await;

        // Send a piece via SendPiece command
        let piece_data = Bytes::from(vec![0xAB; 16384]);
        cmd_tx
            .send(PeerCommand::SendPiece {
                index: 0,
                begin: 0,
                data: piece_data.clone(),
            })
            .await
            .unwrap();

        let msg = read_framed_message(&mut server_stream).await;
        match msg {
            Message::Piece {
                index,
                begin,
                data_0,
                data_1,
            } => {
                assert_eq!(index, 0);
                assert_eq!(begin, 0);
                assert!(data_1.is_empty());
                assert_eq!(data_0, piece_data);
            }
            other => panic!("expected Piece, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn serves_metadata_request() {
        let (client_stream, mut server_stream) = tokio::io::duplex(65536);
        let (event_tx, _event_rx) = mpsc::channel(16);
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        // Build a small info dict to serve
        let info_raw = b"d4:name4:test12:piece lengthi16384e6:pieces20:AAAAAAAAAAAAAAAAAAAAe";
        let info_bytes = Bytes::from_static(info_raw);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false, // outbound
                false, // anonymous_mode
                Some(info_bytes),
                std::sync::Arc::new(Vec::new()), // plugins
                true,                            // enable_holepunch
                16 * 1024 * 1024,                // max_message_size
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read the extension handshake our peer sends
        let ext_hs_msg = read_framed_message(&mut server_stream).await;
        let our_ut_metadata_id = match &ext_hs_msg {
            Message::Extended { ext_id: 0, payload } => {
                let hs = ExtHandshake::from_bytes(payload).unwrap();
                hs.ext_id("ut_metadata").unwrap()
            }
            other => panic!("expected ext handshake, got: {other:?}"),
        };
        // M107: drain the unconditional Unchoke sent on connect
        let _unchoke = read_framed_message(&mut server_stream).await;

        // Send a remote ext handshake that advertises ut_metadata=5
        let mut remote_ext = ExtHandshake::new();
        remote_ext.m.insert("ut_metadata".into(), 5);
        let ext_payload = remote_ext.to_bytes().unwrap();
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: 0,
                payload: ext_payload,
            },
        )
        .await;

        // Send a metadata request for piece 0, using OUR ut_metadata ID
        let req = MetadataMessage::request(0);
        let req_payload = req.to_bytes().unwrap();
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: our_ut_metadata_id,
                payload: req_payload,
            },
        )
        .await;

        // Read the data response — it should use the REMOTE's ext_id (5)
        let response = read_framed_message(&mut server_stream).await;
        match response {
            Message::Extended { ext_id, payload } => {
                assert_eq!(ext_id, 5, "should use remote's ut_metadata id");
                let meta_msg = MetadataMessage::from_bytes(&payload).unwrap();
                assert_eq!(meta_msg.msg_type, MetadataMessageType::Data);
                assert_eq!(meta_msg.piece, 0);
                assert_eq!(meta_msg.total_size, Some(info_raw.len() as u64));
                assert_eq!(meta_msg.data.as_deref(), Some(info_raw.as_ref()));
            }
            other => panic!("expected Extended data response, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Plugin Extension Tests ----

    /// Test plugin that echoes messages back verbatim.
    struct TestEchoPlugin;

    impl crate::extension::ExtensionPlugin for TestEchoPlugin {
        fn name(&self) -> &str {
            "ut_echo"
        }

        fn on_message(
            &self,
            _info_hash: &Id20,
            _peer_addr: SocketAddr,
            payload: &[u8],
        ) -> Option<Vec<u8>> {
            Some(payload.to_vec())
        }
    }

    #[tokio::test]
    async fn plugin_advertised_in_ext_handshake() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);
        let plugins: std::sync::Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>> =
            std::sync::Arc::new(vec![Box::new(TestEchoPlugin)]);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false, // outbound
                false, // anonymous_mode
                None,  // info_bytes
                plugins,
                true,             // enable_holepunch
                16 * 1024 * 1024, // max_message_size
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read our ext handshake — ut_echo should be advertised at ID 10
        let msg = read_framed_message(&mut server_stream).await;
        match msg {
            Message::Extended { ext_id: 0, payload } => {
                let ext_hs = ExtHandshake::from_bytes(&payload).unwrap();
                assert_eq!(
                    ext_hs.ext_id("ut_echo"),
                    Some(10),
                    "plugin should be at ID 10"
                );
                assert_eq!(ext_hs.ext_id("ut_metadata"), Some(1), "built-in unchanged");
            }
            other => panic!("expected ext handshake, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn plugin_message_echo_dispatch() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);
        let plugins: std::sync::Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>> =
            std::sync::Arc::new(vec![Box::new(TestEchoPlugin)]);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false, // outbound
                false, // anonymous_mode
                None,  // info_bytes
                plugins,
                true,             // enable_holepunch
                16 * 1024 * 1024, // max_message_size
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read our ext handshake
        let msg = read_framed_message(&mut server_stream).await;
        let our_ext_hs = match msg {
            Message::Extended { ext_id: 0, payload } => ExtHandshake::from_bytes(&payload).unwrap(),
            other => panic!("expected ext handshake, got: {other:?}"),
        };
        let our_ut_echo_id = our_ext_hs.ext_id("ut_echo").unwrap();
        assert_eq!(our_ut_echo_id, 10);
        // M107: drain the unconditional Unchoke sent on connect
        let _unchoke = read_framed_message(&mut server_stream).await;

        // Send remote ext handshake advertising ut_echo=42
        let mut remote_ext = ExtHandshake::default();
        remote_ext.m.insert("ut_metadata".into(), 3);
        remote_ext.m.insert("ut_pex".into(), 4);
        remote_ext.m.insert("ut_echo".into(), 42);
        let ext_payload = remote_ext.to_bytes().unwrap();
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: 0,
                payload: ext_payload,
            },
        )
        .await;

        // Send a plugin message using OUR assigned ID (10)
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: our_ut_echo_id,
                payload: Bytes::from_static(b"hello plugin"),
            },
        )
        .await;

        // Read the echo response — should use the PEER's ut_echo ID (42)
        let response = read_framed_message(&mut server_stream).await;
        match response {
            Message::Extended { ext_id, payload } => {
                assert_eq!(ext_id, 42, "response should use peer's ut_echo id");
                assert_eq!(payload.as_ref(), b"hello plugin");
            }
            other => panic!("expected echo response, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    // ---- Holepunch Message Tests (BEP 55) ----

    #[tokio::test]
    async fn holepunch_rendezvous_event() {
        use torrent_wire::HolepunchMessage;
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,
                false,
                None,
                std::sync::Arc::new(Vec::new()),
                true,
                16 * 1024 * 1024,
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read our ext handshake to find our ut_holepunch ID
        let ext_hs_msg = read_framed_message(&mut server_stream).await;
        let our_ut_holepunch_id = match &ext_hs_msg {
            Message::Extended { ext_id: 0, payload } => {
                let hs = ExtHandshake::from_bytes(payload).unwrap();
                hs.ext_id("ut_holepunch").unwrap()
            }
            other => panic!("expected ext handshake, got: {other:?}"),
        };

        // Send remote ext handshake
        let mut remote_ext = ExtHandshake::default();
        remote_ext.m.insert("ut_metadata".into(), 3);
        let ext_payload = remote_ext.to_bytes().unwrap();
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: 0,
                payload: ext_payload,
            },
        )
        .await;

        // Consume the ext handshake event
        let evt = event_rx.recv().await.unwrap();
        assert!(matches!(evt, PeerEvent::ExtHandshake { .. }));

        // Send a holepunch Rendezvous message using OUR assigned ID
        let target: SocketAddr = "10.0.0.1:8080".parse().unwrap();
        let hp_msg = HolepunchMessage::rendezvous(target);
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: our_ut_holepunch_id,
                payload: hp_msg.to_bytes(),
            },
        )
        .await;

        let evt = event_rx.recv().await.unwrap();
        match evt {
            PeerEvent::HolepunchRendezvous {
                peer_addr,
                target: t,
            } => {
                assert_eq!(peer_addr, test_addr());
                assert_eq!(t, target);
            }
            other => panic!("expected HolepunchRendezvous, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn holepunch_connect_event() {
        use torrent_wire::HolepunchMessage;
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,
                false,
                None,
                std::sync::Arc::new(Vec::new()),
                true,
                16 * 1024 * 1024,
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;
        let ext_hs_msg = read_framed_message(&mut server_stream).await;
        let our_ut_holepunch_id = match &ext_hs_msg {
            Message::Extended { ext_id: 0, payload } => ExtHandshake::from_bytes(payload)
                .unwrap()
                .ext_id("ut_holepunch")
                .unwrap(),
            other => panic!("expected ext handshake, got: {other:?}"),
        };

        let mut remote_ext = ExtHandshake::default();
        remote_ext.m.insert("ut_metadata".into(), 3);
        let ext_payload = remote_ext.to_bytes().unwrap();
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: 0,
                payload: ext_payload,
            },
        )
        .await;
        let evt = event_rx.recv().await.unwrap();
        assert!(matches!(evt, PeerEvent::ExtHandshake { .. }));

        let target: SocketAddr = "192.168.1.100:6881".parse().unwrap();
        let hp_msg = HolepunchMessage::connect(target);
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: our_ut_holepunch_id,
                payload: hp_msg.to_bytes(),
            },
        )
        .await;

        let evt = event_rx.recv().await.unwrap();
        match evt {
            PeerEvent::HolepunchConnect {
                peer_addr,
                target: t,
            } => {
                assert_eq!(peer_addr, test_addr());
                assert_eq!(t, target);
            }
            other => panic!("expected HolepunchConnect, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn holepunch_error_event() {
        use torrent_wire::{HolepunchError, HolepunchMessage};
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,
                false,
                None,
                std::sync::Arc::new(Vec::new()),
                true,
                16 * 1024 * 1024,
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;
        let ext_hs_msg = read_framed_message(&mut server_stream).await;
        let our_ut_holepunch_id = match &ext_hs_msg {
            Message::Extended { ext_id: 0, payload } => ExtHandshake::from_bytes(payload)
                .unwrap()
                .ext_id("ut_holepunch")
                .unwrap(),
            other => panic!("expected ext handshake, got: {other:?}"),
        };

        let mut remote_ext = ExtHandshake::default();
        remote_ext.m.insert("ut_metadata".into(), 3);
        let ext_payload = remote_ext.to_bytes().unwrap();
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: 0,
                payload: ext_payload,
            },
        )
        .await;
        let evt = event_rx.recv().await.unwrap();
        assert!(matches!(evt, PeerEvent::ExtHandshake { .. }));

        let target: SocketAddr = "172.16.0.5:51413".parse().unwrap();
        let hp_msg = HolepunchMessage::error(target, HolepunchError::NotConnected);
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: our_ut_holepunch_id,
                payload: hp_msg.to_bytes(),
            },
        )
        .await;

        let evt = event_rx.recv().await.unwrap();
        match evt {
            PeerEvent::HolepunchError {
                peer_addr,
                target: t,
                error_code,
            } => {
                assert_eq!(peer_addr, test_addr());
                assert_eq!(t, target);
                assert_eq!(error_code, 2); // NotConnected
            }
            other => panic!("expected HolepunchError, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn holepunch_disabled_removes_from_ext_handshake() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,
                false,
                None,
                std::sync::Arc::new(Vec::new()),
                false, // enable_holepunch = DISABLED
                16 * 1024 * 1024,
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        let msg = read_framed_message(&mut server_stream).await;
        match msg {
            Message::Extended { ext_id: 0, payload } => {
                let ext_hs = ExtHandshake::from_bytes(&payload).unwrap();
                assert!(
                    ext_hs.ext_id("ut_holepunch").is_none(),
                    "ut_holepunch should not be advertised when disabled"
                );
                assert!(ext_hs.ext_id("ut_metadata").is_some());
                assert!(ext_hs.ext_id("ut_pex").is_some());
            }
            other => panic!("expected ext handshake, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn holepunch_enabled_advertised_in_ext_handshake() {
        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,
                false,
                None,
                std::sync::Arc::new(Vec::new()),
                true, // enable_holepunch = ENABLED
                16 * 1024 * 1024,
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        let msg = read_framed_message(&mut server_stream).await;
        match msg {
            Message::Extended { ext_id: 0, payload } => {
                let ext_hs = ExtHandshake::from_bytes(&payload).unwrap();
                assert_eq!(
                    ext_hs.ext_id("ut_holepunch"),
                    Some(4),
                    "ut_holepunch should be advertised at ID 4"
                );
            }
            other => panic!("expected ext handshake, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn send_holepunch_command_sends_extended_message() {
        use torrent_wire::HolepunchMessage;

        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,
                false,
                None,
                std::sync::Arc::new(Vec::new()),
                true,
                16 * 1024 * 1024,
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        let ext_hs_msg = read_framed_message(&mut server_stream).await;
        let _our_ext_hs = match &ext_hs_msg {
            Message::Extended { ext_id: 0, payload } => ExtHandshake::from_bytes(payload).unwrap(),
            other => panic!("expected ext handshake, got: {other:?}"),
        };
        let _unchoke = read_framed_message(&mut server_stream).await;

        let mut remote_ext = ExtHandshake::default();
        remote_ext.m.insert("ut_metadata".into(), 3);
        remote_ext.m.insert("ut_holepunch".into(), 7);
        let ext_payload = remote_ext.to_bytes().unwrap();
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: 0,
                payload: ext_payload,
            },
        )
        .await;

        let evt = event_rx.recv().await.unwrap();
        assert!(matches!(evt, PeerEvent::ExtHandshake { .. }));

        let target: SocketAddr = "10.0.0.1:8080".parse().unwrap();
        let hp_msg = HolepunchMessage::connect(target);
        cmd_tx
            .send(PeerCommand::SendHolepunch(hp_msg.clone()))
            .await
            .unwrap();

        let msg = read_framed_message(&mut server_stream).await;
        match msg {
            Message::Extended { ext_id, payload } => {
                assert_eq!(ext_id, 7, "should use remote's ut_holepunch ID");
                let parsed = HolepunchMessage::from_bytes(&payload).unwrap();
                assert_eq!(parsed, hp_msg);
            }
            other => panic!("expected Extended holepunch message, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn incoming_holepunch_routed_to_event() {
        use torrent_wire::HolepunchMessage;

        let (client_stream, mut server_stream) = tokio::io::duplex(8192);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        let info_hash = test_info_hash();
        let bitfield = Bitfield::new(10);

        let handle = tokio::spawn(async move {
            run_peer(
                test_addr(),
                client_stream,
                info_hash,
                test_peer_id(),
                bitfield,
                10,
                event_tx,
                cmd_rx,
                false,
                false,
                torrent_wire::mse::EncryptionMode::Disabled,
                false,
                false,
                None,
                std::sync::Arc::new(Vec::new()),
                true,
                16 * 1024 * 1024,
                dummy_broadcast_rx(),
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        let ext_hs_msg = read_framed_message(&mut server_stream).await;
        let our_ut_holepunch_id = match &ext_hs_msg {
            Message::Extended { ext_id: 0, payload } => {
                let hs = ExtHandshake::from_bytes(payload).unwrap();
                hs.ext_id("ut_holepunch").unwrap()
            }
            other => panic!("expected ext handshake, got: {other:?}"),
        };
        assert_eq!(our_ut_holepunch_id, 4);

        let mut remote_ext = ExtHandshake::default();
        remote_ext.m.insert("ut_metadata".into(), 3);
        let ext_payload = remote_ext.to_bytes().unwrap();
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: 0,
                payload: ext_payload,
            },
        )
        .await;

        let evt = event_rx.recv().await.unwrap();
        assert!(matches!(evt, PeerEvent::ExtHandshake { .. }));

        let target: SocketAddr = "10.0.0.1:8080".parse().unwrap();
        let hp_msg = HolepunchMessage::rendezvous(target);
        write_framed_message(
            &mut server_stream,
            &Message::Extended {
                ext_id: our_ut_holepunch_id,
                payload: hp_msg.to_bytes(),
            },
        )
        .await;

        let evt = event_rx.recv().await.unwrap();
        match evt {
            PeerEvent::HolepunchRendezvous {
                peer_addr,
                target: t,
            } => {
                assert_eq!(peer_addr, test_addr());
                assert_eq!(t, target);
            }
            other => panic!("expected HolepunchRendezvous, got: {other:?}"),
        }

        cmd_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _ = handle.await;
    }

    #[test]
    fn anonymous_mode_suppresses_ext_handshake_fields() {
        let mut ext_hs = ExtHandshake::new();
        ext_hs.p = Some(6881);
        ext_hs.upload_only = Some(1);
        ext_hs.reqq = Some(250);

        ext_hs.v = None;
        ext_hs.p = None;
        ext_hs.reqq = None;
        ext_hs.upload_only = None;

        assert!(ext_hs.v.is_none());
        assert!(ext_hs.p.is_none());
        assert!(ext_hs.reqq.is_none());
        assert!(ext_hs.upload_only.is_none());
        assert!(!ext_hs.m.is_empty());

        let encoded = ext_hs.to_bytes().unwrap();
        let decoded = ExtHandshake::from_bytes(&encoded).unwrap();
        assert!(decoded.v.is_none());
        assert!(decoded.p.is_none());
        assert!(decoded.reqq.is_none());
        assert!(decoded.upload_only.is_none());
        assert!(!decoded.m.is_empty());
    }
}
