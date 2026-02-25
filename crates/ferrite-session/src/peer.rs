//! PeerTask — one tokio task per TCP connection.
//!
//! Handles handshake, BEP 10 extension negotiation, message loop,
//! and communicates with the TorrentActor via channels.

use std::net::SocketAddr;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite};
use tracing::warn;

use ferrite_core::Id20;
use ferrite_storage::Bitfield;
use ferrite_wire::{ExtHandshake, Handshake, Message, MessageCodec, MetadataMessage, MetadataMessageType};

use crate::pex::PexMessage;
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
    mut stream: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    info_hash: Id20,
    our_peer_id: Id20,
    our_bitfield: Bitfield,
    num_pieces: u32,
    event_tx: mpsc::Sender<PeerEvent>,
    mut cmd_rx: mpsc::Receiver<PeerCommand>,
    enable_dht: bool,
) -> crate::Result<()> {
    // --- Phase 1: Handshake (raw read/write, not framed) ---
    let mut our_hs = Handshake::new(info_hash, our_peer_id);
    if enable_dht {
        our_hs = our_hs.with_dht();
    }

    stream.write_all(&our_hs.to_bytes()).await?;
    stream.flush().await?;

    let mut hs_buf = [0u8; HANDSHAKE_SIZE];
    stream.read_exact(&mut hs_buf).await?;

    let their_hs = Handshake::from_bytes(&hs_buf)?;
    if their_hs.info_hash != info_hash {
        return Err(crate::Error::Connection("info_hash mismatch".into()));
    }

    // --- Phase 2: Wrap stream in framed codec ---
    let (reader, writer) = tokio::io::split(stream);
    let mut framed_read = FramedRead::new(reader, MessageCodec::new());
    let mut framed_write = FramedWrite::new(writer, MessageCodec::new());

    // --- Phase 3: BEP 10 Extension Handshake ---
    let peer_supports_extensions = their_hs.supports_extensions();
    if peer_supports_extensions {
        let ext_hs = ExtHandshake::new();
        let payload = ext_hs.to_bytes().map_err(crate::Error::Wire)?;
        framed_write
            .send(Message::Extended {
                ext_id: 0,
                payload,
            })
            .await
            .map_err(crate::Error::Wire)?;
    }

    // --- Phase 4: Send our bitfield (if non-empty) ---
    if our_bitfield.count_ones() > 0 {
        framed_write
            .send(Message::Bitfield(Bytes::copy_from_slice(
                our_bitfield.as_bytes(),
            )))
            .await
            .map_err(crate::Error::Wire)?;
    }

    // Track extension ID mappings:
    // - peer_ut_*: IDs from the remote's ext handshake (used when SENDING to them)
    // - our_ut_*: IDs from OUR ext handshake (used for matching INCOMING messages)
    let mut peer_ut_metadata: Option<u8> = None;
    let mut peer_ut_pex: Option<u8> = None;
    let our_ext = ExtHandshake::new();
    let our_ut_metadata: Option<u8> = our_ext.ext_id("ut_metadata");
    let our_ut_pex: Option<u8> = our_ext.ext_id("ut_pex");

    // --- Phase 5: Main loop ---
    let disconnect_reason: Option<String> = loop {
        tokio::select! {
            frame = framed_read.next() => {
                match frame {
                    Some(Ok(msg)) => {
                        if let Err(e) = handle_message(
                            msg,
                            addr,
                            num_pieces,
                            &event_tx,
                            &mut peer_ut_metadata,
                            &mut peer_ut_pex,
                            our_ut_metadata,
                            our_ut_pex,
                        ).await {
                            warn!(%addr, "error handling message: {e}");
                        }
                    }
                    Some(Err(e)) => {
                        warn!(%addr, "wire error: {e}");
                        break Some(e.to_string());
                    }
                    None => {
                        // Stream closed
                        break Some("connection closed".into());
                    }
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(PeerCommand::Shutdown) => {
                        break None;
                    }
                    Some(cmd) => {
                        if let Err(e) = handle_command(
                            cmd,
                            &mut framed_write,
                            peer_ut_metadata,
                        ).await {
                            warn!(%addr, "error sending message: {e}");
                            break Some(e.to_string());
                        }
                    }
                    None => {
                        // Actor dropped the sender — shut down
                        break None;
                    }
                }
            }
        }
    };

    // Send disconnect event (best-effort)
    let _ = event_tx
        .send(PeerEvent::Disconnected {
            peer_addr: addr,
            reason: disconnect_reason,
        })
        .await;

    Ok(())
}

/// Handle an incoming message from the remote peer.
///
/// `peer_ut_metadata`/`peer_ut_pex`: stored from remote's ext handshake (for sending).
/// `our_ut_metadata`/`our_ut_pex`: our assigned IDs (for matching incoming).
#[allow(clippy::too_many_arguments)]
async fn handle_message(
    msg: Message,
    addr: SocketAddr,
    num_pieces: u32,
    event_tx: &mpsc::Sender<PeerEvent>,
    peer_ut_metadata: &mut Option<u8>,
    peer_ut_pex: &mut Option<u8>,
    our_ut_metadata: Option<u8>,
    our_ut_pex: Option<u8>,
) -> crate::Result<()> {
    match msg {
        Message::Choke => {
            event_tx
                .send(PeerEvent::PeerChoking {
                    peer_addr: addr,
                    choking: true,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        Message::Unchoke => {
            event_tx
                .send(PeerEvent::PeerChoking {
                    peer_addr: addr,
                    choking: false,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        Message::Interested => {
            event_tx
                .send(PeerEvent::PeerInterested {
                    peer_addr: addr,
                    interested: true,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        Message::NotInterested => {
            event_tx
                .send(PeerEvent::PeerInterested {
                    peer_addr: addr,
                    interested: false,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        Message::Have { index } => {
            event_tx
                .send(PeerEvent::Have {
                    peer_addr: addr,
                    index,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        Message::Bitfield(data) => {
            let bitfield = Bitfield::from_bytes(data.to_vec(), num_pieces)?;
            event_tx
                .send(PeerEvent::Bitfield {
                    peer_addr: addr,
                    bitfield,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        Message::Piece { index, begin, data } => {
            event_tx
                .send(PeerEvent::PieceData {
                    peer_addr: addr,
                    index,
                    begin,
                    data,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        Message::Extended { ext_id: 0, payload } => {
            // Extension handshake
            let ext_hs = ExtHandshake::from_bytes(&payload)?;
            *peer_ut_metadata = ext_hs.ext_id("ut_metadata");
            *peer_ut_pex = ext_hs.ext_id("ut_pex");
            event_tx
                .send(PeerEvent::ExtHandshake {
                    peer_addr: addr,
                    handshake: ext_hs,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        Message::Extended { ext_id, payload } => {
            // Routed extension message: the remote sends using OUR assigned IDs
            if Some(ext_id) == our_ut_metadata {
                handle_metadata_message(addr, &payload, event_tx).await?;
            } else if Some(ext_id) == our_ut_pex {
                handle_pex_message(&payload, event_tx).await?;
            } else {
                warn!(%addr, ext_id, "unknown extension message");
            }
        }
        Message::KeepAlive | Message::Request { .. } | Message::Cancel { .. } | Message::Port(_) => {
            // Ignored
        }
    }
    Ok(())
}

/// Handle an incoming ut_metadata extension message.
async fn handle_metadata_message(
    addr: SocketAddr,
    payload: &[u8],
    event_tx: &mpsc::Sender<PeerEvent>,
) -> crate::Result<()> {
    let meta_msg = MetadataMessage::from_bytes(payload)?;
    match meta_msg.msg_type {
        MetadataMessageType::Data => {
            if let (Some(data), Some(total_size)) = (meta_msg.data, meta_msg.total_size) {
                event_tx
                    .send(PeerEvent::MetadataPiece {
                        peer_addr: addr,
                        piece: meta_msg.piece,
                        data,
                        total_size,
                    })
                    .await
                    .map_err(|_| crate::Error::Shutdown)?;
            }
        }
        MetadataMessageType::Reject => {
            event_tx
                .send(PeerEvent::MetadataReject {
                    peer_addr: addr,
                    piece: meta_msg.piece,
                })
                .await
                .map_err(|_| crate::Error::Shutdown)?;
        }
        MetadataMessageType::Request => {
            // We don't serve metadata yet — ignore
        }
    }
    Ok(())
}

/// Handle an incoming ut_pex extension message.
async fn handle_pex_message(
    payload: &[u8],
    event_tx: &mpsc::Sender<PeerEvent>,
) -> crate::Result<()> {
    let pex = PexMessage::from_bytes(payload)?;
    let new_peers = pex.added_peers();
    if !new_peers.is_empty() {
        event_tx
            .send(PeerEvent::PexPeers { new_peers })
            .await
            .map_err(|_| crate::Error::Shutdown)?;
    }
    Ok(())
}

/// Handle a command from the TorrentActor.
async fn handle_command(
    cmd: PeerCommand,
    framed_write: &mut FramedWrite<tokio::io::WriteHalf<impl AsyncWrite>, MessageCodec>,
    peer_ut_metadata: Option<u8>,
) -> crate::Result<()> {
    let msg = match cmd {
        PeerCommand::Request {
            index,
            begin,
            length,
        } => Message::Request {
            index,
            begin,
            length,
        },
        PeerCommand::Cancel {
            index,
            begin,
            length,
        } => Message::Cancel {
            index,
            begin,
            length,
        },
        PeerCommand::SetChoking(choking) => {
            if choking {
                Message::Choke
            } else {
                Message::Unchoke
            }
        }
        PeerCommand::SetInterested(interested) => {
            if interested {
                Message::Interested
            } else {
                Message::NotInterested
            }
        }
        PeerCommand::Have(index) => Message::Have { index },
        PeerCommand::RequestMetadata { piece } => {
            let ext_id = peer_ut_metadata.ok_or_else(|| {
                crate::Error::Connection("peer does not support ut_metadata".into())
            })?;
            let payload = MetadataMessage::request(piece)
                .to_bytes()
                .map_err(crate::Error::Wire)?;
            Message::Extended { ext_id, payload }
        }
        PeerCommand::Shutdown => {
            // Should have been handled in the main loop; this is unreachable.
            return Ok(());
        }
    };
    framed_write
        .send(msg)
        .await
        .map_err(crate::Error::Wire)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

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
        Message::from_payload(&payload).unwrap()
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
            Message::Extended { ext_id: 0, payload } => {
                ExtHandshake::from_bytes(&payload).unwrap()
            }
            other => panic!("expected ext handshake, got: {other:?}"),
        };

        // Send remote ext handshake back with different IDs
        let mut remote_ext = ExtHandshake::default();
        remote_ext.m.insert("ut_metadata".into(), 3);
        remote_ext.m.insert("ut_pex".into(), 4);
        remote_ext.v = Some("TestPeer 1.0".into());

        let payload = remote_ext.to_bytes().unwrap();
        write_framed_message(
            stream,
            &Message::Extended {
                ext_id: 0,
                payload,
            },
        )
        .await;

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
                assert!(
                    ext_hs.ext_id("ut_pex").is_some(),
                    "should advertise ut_pex"
                );
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
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;

        // Send SetInterested command
        cmd_tx
            .send(PeerCommand::SetInterested(true))
            .await
            .unwrap();

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

    // ---- Test 7: Have Forwarding ----

    #[tokio::test]
    async fn have_forwarding() {
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
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;

        cmd_tx.send(PeerCommand::Have(5)).await.unwrap();

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
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;

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
                data: piece_data.clone(),
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
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Read ext handshake
        let _ext_hs_msg = read_framed_message(&mut server_stream).await;

        // Send KeepAlive from remote
        write_framed_message(&mut server_stream, &Message::KeepAlive).await;

        // Verify the peer is still alive by sending a command
        cmd_tx
            .send(PeerCommand::SetInterested(true))
            .await
            .unwrap();

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
            )
            .await
        });

        do_remote_handshake(&mut server_stream, info_hash, remote_peer_id()).await;

        // Do extension handshake (remote advertises ut_metadata=3)
        let _our_ext = do_remote_ext_handshake(&mut server_stream).await;

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
        let pex = PexMessage {
            added: vec![10, 0, 0, 1, 0x1F, 0x90],
            added_flags: vec![0x00],
            dropped: Vec::new(),
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
}
