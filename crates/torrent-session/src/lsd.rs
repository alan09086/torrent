//! BEP 14 Local Peer Discovery (LSD).
//!
//! UDP multicast announcer for discovering peers on the local network.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Instant;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use torrent_core::Id20;

/// Multicast group for LSD (BEP 14).
const LSD_MULTICAST: Ipv4Addr = Ipv4Addr::new(239, 192, 152, 143);
const LSD_PORT: u16 = 6771;
/// Maximum LSD packet size.
const MAX_PACKET_SIZE: usize = 1400;
/// Minimum announce interval per torrent.
const ANNOUNCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// Format an LSD announce message for the given info hashes and listen port.
///
/// Returns empty Vec if no info hashes are provided.
/// Batches multiple info hashes per message, respecting the MTU limit.
pub(crate) fn format_announce(info_hashes: &[Id20], listen_port: u16) -> Vec<Vec<u8>> {
    if info_hashes.is_empty() {
        return Vec::new();
    }

    let mut messages = Vec::new();
    let mut current_hashes: Vec<&Id20> = Vec::new();

    for ih in info_hashes {
        let header_size = format!(
            "BT-SEARCH * HTTP/1.1\r\nHost: {}:{}\r\nPort: {}\r\n",
            LSD_MULTICAST, LSD_PORT, listen_port
        )
        .len();
        let per_hash = 52; // "Infohash: " + 40 hex + "\r\n"
        let footer = 2; // final "\r\n"
        let estimated = header_size + (current_hashes.len() + 1) * per_hash + footer;

        if estimated > MAX_PACKET_SIZE && !current_hashes.is_empty() {
            messages.push(build_message(&current_hashes, listen_port));
            current_hashes.clear();
        }
        current_hashes.push(ih);
    }

    if !current_hashes.is_empty() {
        messages.push(build_message(&current_hashes, listen_port));
    }

    messages
}

fn build_message(info_hashes: &[&Id20], listen_port: u16) -> Vec<u8> {
    let mut msg = format!(
        "BT-SEARCH * HTTP/1.1\r\nHost: {}:{}\r\nPort: {}\r\n",
        LSD_MULTICAST, LSD_PORT, listen_port
    );
    for ih in info_hashes {
        msg.push_str(&format!("Infohash: {}\r\n", ih.to_hex()));
    }
    msg.push_str("\r\n");
    msg.into_bytes()
}

/// Parsed LSD announce from a remote peer.
#[derive(Debug, Clone)]
pub(crate) struct LsdAnnounce {
    pub port: u16,
    pub info_hashes: Vec<Id20>,
}

/// Parse an incoming LSD message.
///
/// Returns `None` if the message is malformed.
pub(crate) fn parse_announce(data: &[u8]) -> Option<LsdAnnounce> {
    let text = std::str::from_utf8(data).ok()?;

    if !text.starts_with("BT-SEARCH * HTTP/1.1\r\n") {
        return None;
    }

    let mut port: Option<u16> = None;
    let mut info_hashes = Vec::new();

    for line in text.split("\r\n") {
        if let Some(value) = line.strip_prefix("Port: ") {
            port = value.trim().parse().ok();
        } else if let Some(value) = line.strip_prefix("Infohash: ")
            && let Ok(ih) = Id20::from_hex(value.trim())
        {
            info_hashes.push(ih);
        }
    }

    let port = port?;
    if info_hashes.is_empty() {
        return None;
    }

    Some(LsdAnnounce { port, info_hashes })
}

/// Tracks LSD announce rate limiting per info_hash.
pub(crate) struct LsdRateLimiter {
    last_announce: HashMap<Id20, Instant>,
}

impl LsdRateLimiter {
    pub fn new() -> Self {
        Self {
            last_announce: HashMap::new(),
        }
    }

    /// Filter info hashes to only those eligible for announce (respecting rate limit).
    pub fn filter_eligible(&mut self, info_hashes: &[Id20]) -> Vec<Id20> {
        let now = Instant::now();
        let mut eligible = Vec::new();
        for ih in info_hashes {
            let can_announce = self
                .last_announce
                .get(ih)
                .map(|t| now.duration_since(*t) >= ANNOUNCE_INTERVAL)
                .unwrap_or(true);
            if can_announce {
                eligible.push(*ih);
                self.last_announce.insert(*ih, now);
            }
        }
        eligible
    }

    /// Get the multicast address for sending.
    pub fn multicast_addr() -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(LSD_MULTICAST, LSD_PORT))
    }
}

// ---------------------------------------------------------------------------
// LSD Actor (BEP 14 UDP multicast)
// ---------------------------------------------------------------------------

pub(crate) enum LsdCommand {
    Announce { info_hashes: Vec<Id20> },
    Shutdown,
}

#[derive(Clone)]
pub(crate) struct LsdHandle {
    cmd_tx: mpsc::Sender<LsdCommand>,
}

impl LsdHandle {
    /// Start the LSD actor, binding to the multicast port.
    pub async fn start(
        listen_port: u16,
    ) -> std::io::Result<(Self, mpsc::Receiver<(Id20, SocketAddr)>)> {
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, LSD_PORT)).await?;
        socket.set_broadcast(true)?;
        socket.join_multicast_v4(LSD_MULTICAST, Ipv4Addr::UNSPECIFIED)?;

        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let (peer_tx, peer_rx) = mpsc::channel(256);

        let actor = LsdActor {
            socket,
            listen_port,
            rate_limiter: LsdRateLimiter::new(),
            cmd_rx,
            peer_tx,
        };
        tokio::spawn(actor.run());

        Ok((Self { cmd_tx }, peer_rx))
    }

    pub async fn announce(&self, info_hashes: Vec<Id20>) {
        let _ = self.cmd_tx.send(LsdCommand::Announce { info_hashes }).await;
    }

    pub async fn shutdown(&self) {
        let _ = self.cmd_tx.send(LsdCommand::Shutdown).await;
    }
}

struct LsdActor {
    socket: UdpSocket,
    listen_port: u16,
    rate_limiter: LsdRateLimiter,
    cmd_rx: mpsc::Receiver<LsdCommand>,
    peer_tx: mpsc::Sender<(Id20, SocketAddr)>,
}

impl LsdActor {
    async fn run(mut self) {
        let mut buf = [0u8; MAX_PACKET_SIZE];
        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(LsdCommand::Announce { info_hashes }) => {
                            self.do_announce(&info_hashes).await;
                        }
                        Some(LsdCommand::Shutdown) | None => return,
                    }
                }
                result = self.socket.recv_from(&mut buf) => {
                    if let Ok((len, src)) = result {
                        self.handle_incoming(&buf[..len], src).await;
                    }
                }
            }
        }
    }

    async fn do_announce(&mut self, info_hashes: &[Id20]) {
        let eligible = self.rate_limiter.filter_eligible(info_hashes);
        if eligible.is_empty() {
            return;
        }
        let messages = format_announce(&eligible, self.listen_port);
        let dest = LsdRateLimiter::multicast_addr();
        for msg in &messages {
            if let Err(e) = self.socket.send_to(msg, dest).await {
                warn!("LSD send failed: {e}");
                return;
            }
        }
        debug!(count = eligible.len(), "LSD announce sent");
    }

    async fn handle_incoming(&self, data: &[u8], src: SocketAddr) {
        let announce = match parse_announce(data) {
            Some(a) => a,
            None => return,
        };
        let peer_addr = SocketAddr::new(src.ip(), announce.port);
        for ih in announce.info_hashes {
            if self.peer_tx.send((ih, peer_addr)).await.is_err() {
                return; // receiver dropped
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_hash() -> Id20 {
        Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap()
    }

    fn test_hash2() -> Id20 {
        Id20::from_hex("0102030405060708091011121314151617181920").unwrap()
    }

    #[test]
    fn format_single_announce() {
        let msgs = format_announce(&[test_hash()], 6881);
        assert_eq!(msgs.len(), 1);
        let text = String::from_utf8(msgs[0].clone()).unwrap();
        assert!(text.starts_with("BT-SEARCH * HTTP/1.1\r\n"));
        assert!(text.contains("Host: 239.192.152.143:6771\r\n"));
        assert!(text.contains("Port: 6881\r\n"));
        assert!(text.contains(&format!("Infohash: {}\r\n", test_hash().to_hex())));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn format_batch_announce() {
        let hashes = vec![test_hash(), test_hash2()];
        let msgs = format_announce(&hashes, 6881);
        assert_eq!(msgs.len(), 1);
        let text = String::from_utf8(msgs[0].clone()).unwrap();
        assert!(text.contains(&format!("Infohash: {}\r\n", test_hash().to_hex())));
        assert!(text.contains(&format!("Infohash: {}\r\n", test_hash2().to_hex())));
    }

    #[test]
    fn format_empty() {
        let msgs = format_announce(&[], 6881);
        assert!(msgs.is_empty());
    }

    #[test]
    fn parse_valid_announce() {
        let msg = format!(
            "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 6881\r\nInfohash: {}\r\n\r\n",
            test_hash().to_hex()
        );
        let parsed = parse_announce(msg.as_bytes()).unwrap();
        assert_eq!(parsed.port, 6881);
        assert_eq!(parsed.info_hashes.len(), 1);
        assert_eq!(parsed.info_hashes[0], test_hash());
    }

    #[test]
    fn parse_multiple_infohashes() {
        let msg = format!(
            "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 9999\r\nInfohash: {}\r\nInfohash: {}\r\n\r\n",
            test_hash().to_hex(),
            test_hash2().to_hex()
        );
        let parsed = parse_announce(msg.as_bytes()).unwrap();
        assert_eq!(parsed.port, 9999);
        assert_eq!(parsed.info_hashes.len(), 2);
    }

    #[test]
    fn parse_invalid_no_port() {
        let msg = format!(
            "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nInfohash: {}\r\n\r\n",
            test_hash().to_hex()
        );
        assert!(parse_announce(msg.as_bytes()).is_none());
    }

    #[test]
    fn parse_invalid_not_bt_search() {
        let msg = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        assert!(parse_announce(msg).is_none());
    }

    #[test]
    fn rate_limiter_first_announce_allowed() {
        let mut limiter = LsdRateLimiter::new();
        let eligible = limiter.filter_eligible(&[test_hash()]);
        assert_eq!(eligible.len(), 1);
    }

    #[test]
    fn rate_limiter_immediate_reannounce_blocked() {
        let mut limiter = LsdRateLimiter::new();
        let _ = limiter.filter_eligible(&[test_hash()]);
        let eligible = limiter.filter_eligible(&[test_hash()]);
        assert!(eligible.is_empty());
    }

    #[tokio::test]
    async fn lsd_actor_start_and_shutdown() {
        // Port 6771 may be unavailable in CI — gracefully skip
        let result = LsdHandle::start(6881).await;
        match result {
            Ok((handle, _peer_rx)) => {
                handle.announce(vec![test_hash()]).await;
                handle.shutdown().await;
            }
            Err(e) => {
                eprintln!("LSD actor test skipped (port unavailable): {e}");
            }
        }
    }
}
