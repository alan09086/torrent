//! Isolated listener task for accepting and identifying inbound connections.
//!
//! Moves TCP and uTP listener polling out of `SessionActor`'s main `select!`
//! loop into a dedicated task. Each accepted connection is identified by reading
//! the 48-byte BEP 3 preamble, validated against a registry of active info
//! hashes, and forwarded to the session via an mpsc channel.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::transport::{BoxedStream, TransportListener};
use crate::utp_routing::identify_plaintext_connection;

/// Timeout for reading the 48-byte BEP 3 preamble from a new connection.
const IDENTIFY_TIMEOUT: Duration = Duration::from_secs(5);

/// Backoff sleep after a persistent accept error (e.g. EMFILE/ENFILE fd exhaustion)
/// to prevent tight-loop CPU spinning.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_secs(10);

/// An inbound connection that has been identified by its BEP 3 preamble.
///
/// The `stream` replays the 48 consumed preamble bytes before yielding
/// subsequent data, so `run_peer()` can read the full handshake.
pub(crate) struct IdentifiedConnection {
    /// The type-erased bidirectional stream (wraps a `PrefixedStream`).
    pub stream: BoxedStream,
    /// Remote peer address.
    pub addr: SocketAddr,
    /// The info hash extracted from the preamble.
    pub info_hash: torrent_core::Id20,
}

/// Dedicated task that accepts inbound TCP and uTP connections, identifies
/// them via the BEP 3 preamble, and forwards validated connections to the
/// session actor.
pub(crate) struct ListenerTask {
    tcp_listener: Option<Box<dyn TransportListener>>,
    utp_listener: Option<torrent_utp::UtpListener>,
    utp_listener_v6: Option<torrent_utp::UtpListener>,
    info_hash_registry: Arc<DashMap<torrent_core::Id20, ()>>,
    validated_tx: mpsc::Sender<IdentifiedConnection>,
}

impl ListenerTask {
    /// Create a new listener task.
    pub fn new(
        tcp_listener: Option<Box<dyn TransportListener>>,
        utp_listener: Option<torrent_utp::UtpListener>,
        utp_listener_v6: Option<torrent_utp::UtpListener>,
        info_hash_registry: Arc<DashMap<torrent_core::Id20, ()>>,
        validated_tx: mpsc::Sender<IdentifiedConnection>,
    ) -> Self {
        Self {
            tcp_listener,
            utp_listener,
            utp_listener_v6,
            info_hash_registry,
            validated_tx,
        }
    }

    /// Run the listener loop until the session drops the receiving end
    /// of `validated_tx`.
    pub async fn run(mut self) {
        let mut futs = FuturesUnordered::new();

        loop {
            tokio::select! {
                result = accept_transport(&mut self.tcp_listener) => {
                    match result {
                        Ok((stream, addr)) => {
                            let registry = self.info_hash_registry.clone();
                            futs.push(Self::identify_and_validate(stream, addr, registry));
                        }
                        Err(e) => {
                            warn!(error = %e, "TCP accept failed, backing off");
                            tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                        }
                    }
                }
                result = accept_utp(&mut self.utp_listener) => {
                    match result {
                        Ok((stream, addr)) => {
                            let registry = self.info_hash_registry.clone();
                            futs.push(Self::identify_and_validate(
                                BoxedStream::new(stream), addr, registry,
                            ));
                        }
                        Err(e) => {
                            warn!(error = %e, "uTP v4 accept failed, backing off");
                            tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                        }
                    }
                }
                result = accept_utp(&mut self.utp_listener_v6) => {
                    match result {
                        Ok((stream, addr)) => {
                            let registry = self.info_hash_registry.clone();
                            futs.push(Self::identify_and_validate(
                                BoxedStream::new(stream), addr, registry,
                            ));
                        }
                        Err(e) => {
                            warn!(error = %e, "uTP v6 accept failed, backing off");
                            tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                        }
                    }
                }
                Some(Ok(conn)) = futs.next(), if !futs.is_empty() => {
                    if self.validated_tx.send(conn).await.is_err() {
                        // SessionActor dropped the receiver — shut down.
                        break;
                    }
                }
            }
        }
    }

    /// Read the BEP 3 preamble, validate the info hash against the registry,
    /// and wrap the stream for replay.
    async fn identify_and_validate<S>(
        stream: S,
        addr: SocketAddr,
        registry: Arc<DashMap<torrent_core::Id20, ()>>,
    ) -> Result<IdentifiedConnection, ()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        match tokio::time::timeout(IDENTIFY_TIMEOUT, identify_plaintext_connection(stream)).await {
            Ok(Ok(Some((info_hash, prefixed_stream)))) => {
                if registry.contains_key(&info_hash) {
                    Ok(IdentifiedConnection {
                        stream: BoxedStream::new(prefixed_stream),
                        addr,
                        info_hash,
                    })
                } else {
                    debug!(
                        %addr,
                        info_hash = %info_hash,
                        "rejecting inbound connection: unknown info hash"
                    );
                    Err(())
                }
            }
            Ok(Ok(None)) => {
                // Encrypted (MSE) connection — not a plaintext BT handshake.
                debug!(%addr, "rejecting inbound connection: encrypted preamble");
                Err(())
            }
            Ok(Err(e)) => {
                debug!(%addr, error = %e, "error reading preamble from inbound connection");
                Err(())
            }
            Err(_elapsed) => {
                debug!(%addr, "timeout reading preamble from inbound connection");
                Err(())
            }
        }
    }
}

/// Accept a connection from an optional [`TransportListener`].
///
/// Returns `pending` if no listener is available, causing the `select!` arm
/// to be effectively disabled.
async fn accept_transport(
    listener: &mut Option<Box<dyn TransportListener>>,
) -> std::io::Result<(BoxedStream, SocketAddr)> {
    match listener {
        Some(l) => l.accept().await,
        None => std::future::pending().await,
    }
}

/// Accept a connection from an optional uTP listener.
///
/// Returns `pending` if no listener is available, causing the `select!` arm
/// to be effectively disabled.
async fn accept_utp(
    listener: &mut Option<torrent_utp::UtpListener>,
) -> std::io::Result<(torrent_utp::UtpStream, SocketAddr)> {
    match listener {
        Some(l) => l.accept().await.map_err(std::io::Error::other),
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::TokioListener;
    use tokio::io::AsyncWriteExt;
    use tokio::net::{TcpListener, TcpStream};

    /// Build a valid 48-byte BEP 3 preamble from an info hash.
    fn build_preamble(info_hash: &torrent_core::Id20) -> [u8; 48] {
        let mut buf = [0u8; 48];
        buf[0] = 0x13; // pstrlen
        buf[1..20].copy_from_slice(b"BitTorrent protocol"); // 19 bytes
        // buf[20..28] = reserved (zeros, already zeroed)
        buf[28..48].copy_from_slice(info_hash.as_bytes());
        buf
    }

    /// Create a `ListenerTask` wired to a real TCP listener on localhost,
    /// returning `(task, local_addr, receiver)`.
    async fn setup_listener(
        channel_capacity: usize,
        registry: Arc<DashMap<torrent_core::Id20, ()>>,
    ) -> (
        ListenerTask,
        SocketAddr,
        mpsc::Receiver<IdentifiedConnection>,
    ) {
        let tcp = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind to ephemeral port");
        let local_addr = tcp.local_addr().expect("local_addr");
        let (tx, rx) = mpsc::channel(channel_capacity);

        let task = ListenerTask::new(
            Some(Box::new(TokioListener(tcp))),
            None,
            None,
            registry,
            tx,
        );
        (task, local_addr, rx)
    }

    // -----------------------------------------------------------------------
    // Test 1: listener_accepts_valid_handshake
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn listener_accepts_valid_handshake() {
        let info_hash = torrent_core::Id20::from([0xAA; 20]);
        let registry = Arc::new(DashMap::new());
        registry.insert(info_hash, ());

        let (task, addr, mut rx) = setup_listener(4, registry).await;
        let handle = tokio::spawn(task.run());

        let mut client = TcpStream::connect(addr)
            .await
            .expect("connect to listener");
        client
            .write_all(&build_preamble(&info_hash))
            .await
            .expect("write preamble");

        let conn = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("receive within timeout")
            .expect("channel not closed");

        assert_eq!(conn.info_hash, info_hash);
        assert_eq!(conn.addr.ip(), client.local_addr().expect("client addr").ip());

        // Shut down the listener by dropping the receiver.
        drop(rx);
        // Send another connection to trigger the send-error exit path.
        let _ = TcpStream::connect(addr).await;
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    // -----------------------------------------------------------------------
    // Test 2: listener_rejects_invalid_protocol
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn listener_rejects_invalid_protocol() {
        let info_hash = torrent_core::Id20::from([0xBB; 20]);
        let registry = Arc::new(DashMap::new());
        registry.insert(info_hash, ());

        let (task, addr, mut rx) = setup_listener(4, registry).await;
        let _handle = tokio::spawn(task.run());

        // Send first byte 0xFF — not a BT preamble.
        let mut client = TcpStream::connect(addr)
            .await
            .expect("connect to listener");
        let mut bad_preamble = [0u8; 48];
        bad_preamble[0] = 0xFF;
        client
            .write_all(&bad_preamble)
            .await
            .expect("write bad preamble");

        // Nothing should arrive on the channel.
        let result = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(result.is_err(), "expected timeout, got a connection");
    }

    // -----------------------------------------------------------------------
    // Test 3: listener_rejects_unknown_info_hash
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn listener_rejects_unknown_info_hash() {
        let known_hash = torrent_core::Id20::from([0xCC; 20]);
        let unknown_hash = torrent_core::Id20::from([0xDD; 20]);
        let registry = Arc::new(DashMap::new());
        registry.insert(known_hash, ());

        let (task, addr, mut rx) = setup_listener(4, registry).await;
        let _handle = tokio::spawn(task.run());

        // Send a valid preamble but with an unknown info hash.
        let mut client = TcpStream::connect(addr)
            .await
            .expect("connect to listener");
        client
            .write_all(&build_preamble(&unknown_hash))
            .await
            .expect("write unknown hash preamble");

        // Nothing should arrive on the channel.
        let result = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(result.is_err(), "expected timeout, got a connection");
    }

    // -----------------------------------------------------------------------
    // Test 4: listener_timeout_on_slow_handshake
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn listener_timeout_on_slow_handshake() {
        tokio::time::pause();

        let info_hash = torrent_core::Id20::from([0xEE; 20]);
        let registry = Arc::new(DashMap::new());
        registry.insert(info_hash, ());

        // Test identify_and_validate directly with a DuplexStream that never
        // sends data, so the 5s IDENTIFY_TIMEOUT fires.
        let (client, _server) = tokio::io::duplex(64);
        let addr: SocketAddr = "127.0.0.1:9999".parse().expect("parse addr");

        let identify_fut =
            ListenerTask::identify_and_validate(client, addr, registry);

        // Spawn the identify future so we can advance time.
        let handle = tokio::spawn(identify_fut);

        // Advance past the 5s timeout.
        tokio::time::advance(Duration::from_secs(6)).await;

        // The spawned task should now be complete (timeout fired).
        let result = handle.await.expect("task did not panic");
        assert!(result.is_err(), "expected Err(()) from timeout");
    }

    // -----------------------------------------------------------------------
    // Test 5: listener_concurrent_handshakes
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn listener_concurrent_handshakes() {
        let info_hash = torrent_core::Id20::from([0x11; 20]);
        let registry = Arc::new(DashMap::new());
        registry.insert(info_hash, ());

        let (task, addr, mut rx) = setup_listener(16, registry).await;
        let _handle = tokio::spawn(task.run());

        // 10 simultaneous connections: 5 valid, 5 invalid.
        let mut clients = Vec::with_capacity(10);
        for i in 0u8..10 {
            let mut stream = TcpStream::connect(addr)
                .await
                .expect("connect to listener");
            if i < 5 {
                // Valid preamble.
                stream
                    .write_all(&build_preamble(&info_hash))
                    .await
                    .expect("write valid preamble");
            } else {
                // Invalid first byte.
                let mut bad = [0u8; 48];
                bad[0] = 0xFF;
                stream
                    .write_all(&bad)
                    .await
                    .expect("write invalid preamble");
            }
            clients.push(stream);
        }

        // Exactly 5 valid connections should arrive.
        let mut received = 0;
        for _ in 0..5 {
            let conn = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("receive within timeout")
                .expect("channel not closed");
            assert_eq!(conn.info_hash, info_hash);
            received += 1;
        }
        assert_eq!(received, 5);

        // No more should arrive.
        let extra = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(extra.is_err(), "expected no more connections");
    }

    // -----------------------------------------------------------------------
    // Test 6: listener_futures_unordered_ordering
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn listener_futures_unordered_ordering() {
        use futures::stream::{FuturesUnordered, StreamExt};
        use tokio::io::AsyncWriteExt;

        tokio::time::pause();

        let info_hash = torrent_core::Id20::from([0x22; 20]);
        let registry = Arc::new(DashMap::new());
        registry.insert(info_hash, ());

        // Two duplex pairs. Stream 1 delays, stream 2 sends immediately.
        let (client1, mut server1) = tokio::io::duplex(256);
        let (client2, mut server2) = tokio::io::duplex(256);

        let addr1: SocketAddr = "127.0.0.1:1001".parse().expect("parse addr");
        let addr2: SocketAddr = "127.0.0.1:1002".parse().expect("parse addr");

        let reg1 = registry.clone();
        let reg2 = registry.clone();

        let mut futs = FuturesUnordered::new();
        futs.push(ListenerTask::identify_and_validate(client1, addr1, reg1));
        futs.push(ListenerTask::identify_and_validate(client2, addr2, reg2));

        // Send preamble to stream 2 immediately.
        server2
            .write_all(&build_preamble(&info_hash))
            .await
            .expect("write preamble 2");

        // Yield to let the identify future for stream 2 complete.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(1)).await;

        // The first result from FuturesUnordered should be stream 2 (addr2).
        let first = futs.next().await.expect("at least one result");
        let conn = first.expect("stream 2 should succeed");
        assert_eq!(conn.addr, addr2, "second client should arrive first");

        // Now send preamble to stream 1.
        server1
            .write_all(&build_preamble(&info_hash))
            .await
            .expect("write preamble 1");

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(1)).await;

        let second = futs.next().await.expect("second result");
        let conn2 = second.expect("stream 1 should succeed");
        assert_eq!(conn2.addr, addr1, "first client should arrive second");
    }

    // -----------------------------------------------------------------------
    // Test 7: listener_channel_backpressure
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn listener_channel_backpressure() {
        let info_hash = torrent_core::Id20::from([0x33; 20]);
        let registry = Arc::new(DashMap::new());
        registry.insert(info_hash, ());

        // Channel capacity 1: only one connection can be buffered.
        let (task, addr, mut rx) = setup_listener(1, registry).await;
        let _handle = tokio::spawn(task.run());

        // Send two valid connections quickly.
        let mut client1 = TcpStream::connect(addr)
            .await
            .expect("connect client 1");
        client1
            .write_all(&build_preamble(&info_hash))
            .await
            .expect("write preamble 1");

        let mut client2 = TcpStream::connect(addr)
            .await
            .expect("connect client 2");
        client2
            .write_all(&build_preamble(&info_hash))
            .await
            .expect("write preamble 2");

        // First connection should arrive.
        let conn1 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("first recv within timeout")
            .expect("channel not closed");
        assert_eq!(conn1.info_hash, info_hash);

        // Second connection should arrive after we drained the first.
        let conn2 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("second recv within timeout")
            .expect("channel not closed");
        assert_eq!(conn2.info_hash, info_hash);

        // Verify both arrived without data loss by checking distinct peer addrs.
        assert_ne!(conn1.addr, conn2.addr, "connections should have distinct peer addresses");
    }

    // -----------------------------------------------------------------------
    // Test 8: listener_shutdown_via_channel_drop
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn listener_shutdown_via_channel_drop() {
        let info_hash = torrent_core::Id20::from([0x44; 20]);
        let registry = Arc::new(DashMap::new());
        registry.insert(info_hash, ());

        let (task, addr, rx) = setup_listener(4, registry).await;
        let handle = tokio::spawn(task.run());

        // Drop the receiver — the task should exit the next time it tries to send.
        drop(rx);

        // Send a valid connection so the task actually attempts to send and
        // discovers the receiver is gone.
        let mut client = TcpStream::connect(addr)
            .await
            .expect("connect to listener");
        client
            .write_all(&build_preamble(&info_hash))
            .await
            .expect("write preamble");

        // The task should exit within a reasonable time.
        let result = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(
            result.is_ok(),
            "listener task should exit after receiver is dropped"
        );
        // The JoinHandle itself should complete without panic.
        result
            .expect("timeout should not fire")
            .expect("task should not panic");
    }

    // -----------------------------------------------------------------------
    // Test 9: listener_torrent_removed_race
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn listener_torrent_removed_race() {
        // Tests time-of-check behavior: identify_and_validate validates the
        // info hash at the point of identification. If the hash is removed
        // from the registry afterward, the already-validated connection is
        // still delivered. This is the correct behavior — the session layer
        // handles the post-delivery race.

        let info_hash = torrent_core::Id20::from([0x55; 20]);
        let registry = Arc::new(DashMap::new());
        registry.insert(info_hash, ());

        // Call identify_and_validate directly with a DuplexStream.
        let (client, mut server) = tokio::io::duplex(256);
        let addr: SocketAddr = "127.0.0.1:5555".parse().expect("parse addr");

        let reg = registry.clone();

        // Write the preamble so identify_and_validate can complete.
        server
            .write_all(&build_preamble(&info_hash))
            .await
            .expect("write preamble");

        // Identify while hash is in registry — should succeed.
        let result = ListenerTask::identify_and_validate(client, addr, reg).await;
        let conn = result.expect("connection should be validated");
        assert_eq!(conn.info_hash, info_hash);
        assert_eq!(conn.addr, addr);

        // Now remove the hash from the registry — the connection was already
        // validated, proving the time-of-check is at identification time.
        registry.remove(&info_hash);
        assert!(
            !registry.contains_key(&info_hash),
            "hash should be removed from registry"
        );

        // A new connection with the same hash should now be rejected.
        let (client2, mut server2) = tokio::io::duplex(256);
        let reg2 = Arc::new(DashMap::new()); // empty registry
        server2
            .write_all(&build_preamble(&info_hash))
            .await
            .expect("write preamble 2");
        let result2 =
            ListenerTask::identify_and_validate(client2, addr, reg2).await;
        assert!(
            result2.is_err(),
            "connection with removed hash should be rejected"
        );
    }
}
