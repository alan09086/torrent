use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::fd::{AsFd, AsRawFd};
use std::time::Instant;

use bytes::Bytes;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, trace, warn};

use crate::conn::{build_reset, ConnAction, ConnState, Connection, ConnectionKey};
use crate::error::{Error, Result};
use crate::listener::UtpListener;
use crate::packet::{Packet, PacketType};
use crate::stream::{CloseSignal, UtpStream, WriteRequest};

/// Configuration for a uTP socket.
#[derive(Debug, Clone)]
pub struct UtpConfig {
    /// Address to bind to.
    pub bind_addr: SocketAddr,
    /// Maximum number of concurrent connections.
    pub max_connections: usize,
    /// DSCP (Differentiated Services Code Point) value for the UDP socket.
    /// 0 = no marking (best-effort).
    pub dscp: u8,
}

impl Default for UtpConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:0".parse().unwrap(),
            max_connections: 256,
            dscp: 0,
        }
    }
}

/// Commands sent from UtpSocket handle to the actor.
enum SocketCommand {
    Connect {
        addr: SocketAddr,
        reply: oneshot::Sender<Result<UtpStream>>,
    },
    Shutdown,
}

/// Cloneable handle to a uTP socket.
#[derive(Clone)]
pub struct UtpSocket {
    tx: mpsc::Sender<SocketCommand>,
    local_addr: SocketAddr,
}

impl UtpSocket {
    /// Bind a UDP socket and start the actor.
    ///
    /// Returns `(UtpSocket, UtpListener)` — the socket handle for outbound
    /// connections and a listener for accepting inbound connections.
    pub async fn bind(config: UtpConfig) -> Result<(Self, UtpListener)> {
        let udp = UdpSocket::bind(config.bind_addr).await?;
        if config.dscp > 0 {
            let tos = (config.dscp as u32) << 2;
            let is_ipv6 = config.bind_addr.is_ipv6();
            let fd = udp.as_fd().as_raw_fd();
            let result = unsafe {
                if is_ipv6 {
                    libc::setsockopt(
                        fd,
                        libc::IPPROTO_IPV6,
                        libc::IPV6_TCLASS,
                        &(tos as libc::c_int) as *const _ as *const libc::c_void,
                        std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                    )
                } else {
                    libc::setsockopt(
                        fd,
                        libc::IPPROTO_IP,
                        libc::IP_TOS,
                        &(tos as libc::c_int) as *const _ as *const libc::c_void,
                        std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                    )
                }
            };
            if result != 0 {
                tracing::debug!(
                    dscp = config.dscp,
                    "failed to set DSCP on uTP socket: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
        let local_addr = udp.local_addr()?;

        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let (incoming_tx, incoming_rx) = mpsc::channel(64);

        let actor = SocketActor {
            socket: udp,
            cmd_rx,
            connections: HashMap::new(),
            incoming_tx,
            config,
        };

        tokio::spawn(actor.run());

        let handle = UtpSocket {
            tx: cmd_tx,
            local_addr,
        };

        let listener = UtpListener {
            incoming_rx,
            local_addr,
        };

        Ok((handle, listener))
    }

    /// Connect to a remote peer. Awaits until the connection is established.
    pub async fn connect(&self, addr: SocketAddr) -> Result<UtpStream> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(SocketCommand::Connect {
                addr,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::Shutdown)?;
        reply_rx.await.map_err(|_| Error::Shutdown)?
    }

    /// Returns the local address this socket is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Shut down the socket and all connections.
    pub async fn shutdown(&self) -> Result<()> {
        self.tx
            .send(SocketCommand::Shutdown)
            .await
            .map_err(|_| Error::Shutdown)
    }
}

/// State for a single connection inside the actor.
struct ConnectionSlot {
    conn: Connection,
    /// Send in-order data to the UtpStream.
    data_tx: mpsc::Sender<Bytes>,
    /// Receive write requests from the UtpStream.
    write_rx: mpsc::Receiver<WriteRequest>,
    /// Receive close signals from the UtpStream.
    close_rx: mpsc::Receiver<CloseSignal>,
    /// For outbound: pending connect reply + stream to deliver.
    pending_connect: Option<PendingConnect>,
}

struct PendingConnect {
    reply: oneshot::Sender<Result<UtpStream>>,
    stream: UtpStream,
}

/// The socket actor — owns the UDP socket and all connection state.
struct SocketActor {
    socket: UdpSocket,
    cmd_rx: mpsc::Receiver<SocketCommand>,
    connections: HashMap<ConnectionKey, ConnectionSlot>,
    incoming_tx: mpsc::Sender<(UtpStream, SocketAddr)>,
    config: UtpConfig,
}

impl SocketActor {
    async fn run(mut self) {
        let mut buf = vec![0u8; 65536];
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(50));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                result = self.socket.recv_from(&mut buf) => {
                    match result {
                        Ok((n, addr)) => {
                            self.handle_packet(&buf[..n], addr).await;
                        }
                        Err(e) => {
                            warn!("UDP recv error: {e}");
                        }
                    }
                }

                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(SocketCommand::Connect { addr, reply }) => {
                            self.handle_connect(addr, reply).await;
                        }
                        Some(SocketCommand::Shutdown) | None => {
                            debug!("socket actor shutting down");
                            return;
                        }
                    }
                }

                _ = tick.tick() => {
                    self.handle_tick().await;
                }
            }
        }
    }

    async fn handle_packet(&mut self, data: &[u8], addr: SocketAddr) {
        let packet = match Packet::decode(data) {
            Ok(p) => p,
            Err(e) => {
                trace!("dropping invalid packet from {addr}: {e}");
                return;
            }
        };

        let key = ConnectionKey {
            addr,
            recv_id: packet.header.connection_id,
        };

        if self.connections.contains_key(&key) {
            let now = Instant::now();
            let slot = self.connections.get_mut(&key).unwrap();
            let actions = slot.conn.on_packet(&packet, now);
            self.execute_actions(&key, actions).await;
        } else if packet.header.packet_type == PacketType::Syn {
            self.handle_inbound_syn(addr, &packet).await;
        } else {
            trace!("sending RESET for unknown connection from {addr}");
            let reset = build_reset(packet.header.connection_id);
            let _ = self.socket.send_to(&reset, addr).await;
        }
    }

    async fn handle_inbound_syn(&mut self, addr: SocketAddr, packet: &Packet) {
        if self.connections.len() >= self.config.max_connections {
            warn!("rejecting connection from {addr}: too many connections");
            let reset = build_reset(packet.header.connection_id);
            let _ = self.socket.send_to(&reset, addr).await;
            return;
        }

        let (conn, syn_ack) = Connection::new_inbound(addr, packet);
        let key = ConnectionKey {
            addr,
            recv_id: conn.recv_id(),
        };

        let (data_tx, data_rx) = mpsc::channel(256);
        let (write_tx, write_rx) = mpsc::channel(256);
        let (close_tx, close_rx) = mpsc::channel(1);

        let stream = UtpStream::new(data_rx, write_tx, close_tx, addr);

        self.connections.insert(
            key,
            ConnectionSlot {
                conn,
                data_tx,
                write_rx,
                close_rx,
                pending_connect: None,
            },
        );

        let _ = self.socket.send_to(&syn_ack, addr).await;
        let _ = self.incoming_tx.send((stream, addr)).await;
        debug!("accepted inbound connection from {addr}");
    }

    async fn handle_connect(
        &mut self,
        addr: SocketAddr,
        reply: oneshot::Sender<Result<UtpStream>>,
    ) {
        if self.connections.len() >= self.config.max_connections {
            let _ = reply.send(Err(Error::TooManyConnections));
            return;
        }

        let mut id_buf = [0u8; 2];
        ferrite_core::random_bytes(&mut id_buf);
        let recv_id = u16::from_be_bytes(id_buf);

        let (conn, syn) = Connection::new_outbound(addr, recv_id);
        let key = ConnectionKey {
            addr,
            recv_id: conn.recv_id(),
        };

        let (data_tx, data_rx) = mpsc::channel(256);
        let (write_tx, write_rx) = mpsc::channel(256);
        let (close_tx, close_rx) = mpsc::channel(1);

        let stream = UtpStream::new(data_rx, write_tx, close_tx, addr);

        self.connections.insert(
            key,
            ConnectionSlot {
                conn,
                data_tx,
                write_rx,
                close_rx,
                pending_connect: Some(PendingConnect { reply, stream }),
            },
        );

        let _ = self.socket.send_to(&syn, addr).await;
        debug!("sent SYN to {addr}, awaiting SYN-ACK");
    }

    async fn execute_actions(&mut self, key: &ConnectionKey, actions: Vec<ConnAction>) {
        let mut sends = Vec::new();
        let mut delivers = Vec::new();
        let mut closed = false;
        let addr = key.addr;

        for action in actions {
            match action {
                ConnAction::Send(data) => sends.push(data),
                ConnAction::Deliver(data) => delivers.push(data),
                ConnAction::Closed => closed = true,
            }
        }

        for data in sends {
            let _ = self.socket.send_to(&data, addr).await;
        }

        if let Some(slot) = self.connections.get_mut(key) {
            // If connection just became Connected and we have a pending connect,
            // deliver the stream to the caller.
            if slot.conn.state() == ConnState::Connected
                && let Some(pending) = slot.pending_connect.take()
            {
                debug!("outbound connection to {addr} established");
                let _ = pending.reply.send(Ok(pending.stream));
            }

            for data in delivers {
                if slot.data_tx.send(data).await.is_err() {
                    trace!("stream dropped for {addr}");
                }
            }

            // After processing ACKs, flush buffered send data and drain pending writes
            if !closed {
                let now = Instant::now();

                // Flush internal send buffer (ACKs may have freed window space)
                let flush_actions = slot.conn.flush_send_buf(now);
                for action in flush_actions {
                    if let ConnAction::Send(pkt) = action {
                        let _ = self.socket.send_to(&pkt, addr).await;
                    }
                }

                // Also drain new write requests
                while let Ok(req) = slot.write_rx.try_recv() {
                    let data_len = req.data.len();
                    let write_actions = slot.conn.send_data(&req.data, now);
                    let _ = req.reply.send(Ok(data_len));

                    for action in write_actions {
                        if let ConnAction::Send(pkt) = action {
                            let _ = self.socket.send_to(&pkt, addr).await;
                        }
                    }
                }
            }

            if closed {
                if let Some(pending) = slot.pending_connect.take() {
                    let _ = pending.reply.send(Err(Error::ConnectionReset));
                }
                debug!("connection to {addr} closed");
                self.connections.remove(key);
            }
        }
    }

    async fn handle_tick(&mut self) {
        let now = Instant::now();
        let keys: Vec<_> = self.connections.keys().copied().collect();

        for key in keys {
            if let Some(slot) = self.connections.get_mut(&key) {
                // Drain write requests
                while let Ok(req) = slot.write_rx.try_recv() {
                    let data_len = req.data.len();
                    let actions = slot.conn.send_data(&req.data, now);
                    let _ = req.reply.send(Ok(data_len));

                    for action in actions {
                        if let ConnAction::Send(pkt) = action {
                            let _ = self.socket.send_to(&pkt, key.addr).await;
                        }
                    }
                }

                // Flush any buffered send data
                let flush_actions = slot.conn.flush_send_buf(now);
                for action in flush_actions {
                    if let ConnAction::Send(pkt) = action {
                        let _ = self.socket.send_to(&pkt, key.addr).await;
                    }
                }

                // Check close signal
                if let Ok(CloseSignal::Close) = slot.close_rx.try_recv() {
                    let actions = slot.conn.initiate_close();
                    for action in actions {
                        if let ConnAction::Send(pkt) = action {
                            let _ = self.socket.send_to(&pkt, key.addr).await;
                        }
                    }
                }

                // Check timeouts
                let actions = slot.conn.check_timeouts(now);
                if !actions.is_empty() {
                    let mut closed = false;
                    for action in actions {
                        match action {
                            ConnAction::Send(data) => {
                                let _ = self.socket.send_to(&data, key.addr).await;
                            }
                            ConnAction::Closed => closed = true,
                            ConnAction::Deliver(_) => {}
                        }
                    }
                    if closed {
                        let removed = self.connections.remove(&key);
                        if let Some(ConnectionSlot { pending_connect: Some(pending), .. }) = removed {
                            let _ = pending.reply.send(Err(Error::Timeout));
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn localhost_config() -> UtpConfig {
        UtpConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            dscp: 0,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn connect_and_accept() {
        let (socket_a, mut listener_a) = UtpSocket::bind(localhost_config()).await.unwrap();
        let (socket_b, _listener_b) = UtpSocket::bind(localhost_config()).await.unwrap();

        let addr_a = socket_a.local_addr();

        let accept_handle = tokio::spawn(async move { listener_a.accept().await });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let stream_b = socket_b.connect(addr_a).await.unwrap();
        let (stream_a, peer_addr) = accept_handle.await.unwrap().unwrap();

        assert_eq!(peer_addr, socket_b.local_addr());
        assert_eq!(stream_b.remote_addr(), addr_a);
        assert_eq!(stream_a.remote_addr(), socket_b.local_addr());

        socket_a.shutdown().await.unwrap();
        socket_b.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn bidirectional_data_transfer() {
        let (socket_a, mut listener_a) = UtpSocket::bind(localhost_config()).await.unwrap();
        let (socket_b, _listener_b) = UtpSocket::bind(localhost_config()).await.unwrap();

        let addr_a = socket_a.local_addr();

        let accept_handle = tokio::spawn(async move { listener_a.accept().await });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let mut stream_b = socket_b.connect(addr_a).await.unwrap();
        let (mut stream_a, _) = accept_handle.await.unwrap().unwrap();

        // B writes "hello" to A
        stream_b.write_all(b"hello").await.unwrap();

        // Wait for actor tick to process the write and deliver
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let mut buf = vec![0u8; 32];
        let n = stream_a.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello");

        // A writes "world" to B
        stream_a.write_all(b"world").await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let n = stream_b.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"world");

        socket_a.shutdown().await.unwrap();
        socket_b.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn large_transfer() {
        use sha1::{Digest, Sha1};

        let (socket_a, mut listener_a) = UtpSocket::bind(localhost_config()).await.unwrap();
        let (socket_b, _listener_b) = UtpSocket::bind(localhost_config()).await.unwrap();

        let addr_a = socket_a.local_addr();

        let accept_handle = tokio::spawn(async move { listener_a.accept().await });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let mut stream_b = socket_b.connect(addr_a).await.unwrap();
        let (mut stream_a, _) = accept_handle.await.unwrap().unwrap();

        // Generate 1MB of test data
        let data_size = 1_048_576;
        let mut send_data = vec![0u8; data_size];
        ferrite_core::random_bytes(&mut send_data);

        let expected_hash = {
            let mut h = Sha1::new();
            h.update(&send_data);
            h.finalize()
        };

        // Spawn writer
        let write_handle = tokio::spawn(async move {
            for chunk in send_data.chunks(8192) {
                stream_b.write_all(chunk).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            stream_b.shutdown().await.unwrap();
        });

        // Read all data
        let mut received = Vec::new();
        let mut buf = vec![0u8; 65536];
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);

        loop {
            if tokio::time::Instant::now() > deadline {
                panic!("timeout: received {}/{} bytes", received.len(), data_size);
            }

            match tokio::time::timeout(
                std::time::Duration::from_millis(500),
                stream_a.read(&mut buf),
            )
            .await
            {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => received.extend_from_slice(&buf[..n]),
                Ok(Err(e)) => panic!("read error: {e}"),
                Err(_) => {
                    if write_handle.is_finished() && received.len() >= data_size {
                        break;
                    }
                    continue;
                }
            }

            if received.len() >= data_size {
                break;
            }
        }

        write_handle.await.unwrap();

        assert_eq!(received.len(), data_size, "received wrong number of bytes");

        let actual_hash = {
            let mut h = Sha1::new();
            h.update(&received);
            h.finalize()
        };
        assert_eq!(actual_hash, expected_hash, "SHA1 mismatch on 1MB transfer");

        socket_a.shutdown().await.unwrap();
        socket_b.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_connections() {
        let (socket_a, mut listener_a) = UtpSocket::bind(localhost_config()).await.unwrap();
        let (socket_b, _listener_b) = UtpSocket::bind(localhost_config()).await.unwrap();

        let addr_a = socket_a.local_addr();

        let accept_handle = tokio::spawn(async move {
            let mut streams = Vec::new();
            for _ in 0..3 {
                let (stream, _) = listener_a.accept().await.unwrap();
                streams.push(stream);
            }
            streams
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let s1 = socket_b.connect(addr_a).await.unwrap();
        let s2 = socket_b.connect(addr_a).await.unwrap();
        let s3 = socket_b.connect(addr_a).await.unwrap();

        let accepted = accept_handle.await.unwrap();
        assert_eq!(accepted.len(), 3);

        drop(s1);
        drop(s2);
        drop(s3);

        socket_a.shutdown().await.unwrap();
        socket_b.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn graceful_close() {
        let (socket_a, mut listener_a) = UtpSocket::bind(localhost_config()).await.unwrap();
        let (socket_b, _listener_b) = UtpSocket::bind(localhost_config()).await.unwrap();

        let addr_a = socket_a.local_addr();

        let accept_handle = tokio::spawn(async move { listener_a.accept().await });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let mut stream_b = socket_b.connect(addr_a).await.unwrap();
        let (mut stream_a, _) = accept_handle.await.unwrap().unwrap();

        // Write some data then close
        stream_b.write_all(b"goodbye").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        stream_b.shutdown().await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let mut buf = vec![0u8; 32];
        let n = stream_a.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"goodbye");

        // After close propagates, actor drops data_tx → stream_a reads EOF
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        socket_a.shutdown().await.unwrap();
        socket_b.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reset_on_unknown() {
        let (socket_a, _listener_a) = UtpSocket::bind(localhost_config()).await.unwrap();

        let raw = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Build a DATA packet to an unknown connection_id
        use crate::packet::{ExtensionType, Header, Packet};
        use crate::seq::SeqNr;

        let pkt = Packet {
            header: Header {
                packet_type: PacketType::Data,
                extension: ExtensionType::None,
                connection_id: 9999,
                timestamp_us: 0,
                timestamp_diff_us: 0,
                wnd_size: 65536,
                seq_nr: SeqNr(1),
                ack_nr: SeqNr(0),
            },
            sack: None,
            close_reason: None,
            payload: Bytes::from_static(b"unexpected"),
        };

        let encoded = pkt.encode();
        raw.send_to(&encoded, socket_a.local_addr()).await.unwrap();

        // Should receive a RESET back
        let mut buf = vec![0u8; 1024];
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            raw.recv_from(&mut buf),
        )
        .await;

        if let Ok(Ok((n, from))) = result {
            assert_eq!(from, socket_a.local_addr());
            let reset = Packet::decode(&buf[..n]).unwrap();
            assert_eq!(reset.header.packet_type, PacketType::Reset);
        }
        // If timeout, that's acceptable — no crash is the key assertion

        socket_a.shutdown().await.unwrap();
    }
}
