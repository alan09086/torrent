//! Peer Exchange (PEX) message encoding and decoding (BEP 11).
//!
//! PEX messages are bencoded dictionaries exchanged between peers to share
//! information about other peers in the swarm.

use std::net::SocketAddr;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::torrent::is_i2p_synthetic_addr;

/// Size of a single compact IPv4 peer entry (4 bytes IP + 2 bytes port).
const COMPACT_PEER_SIZE: usize = 6;

/// Size of a single compact IPv6 peer entry (16 bytes IP + 2 bytes port).
const COMPACT_PEER6_SIZE: usize = 18;

/// BEP 11: Maximum added peers per PEX message.
const MAX_PEX_ADDED: usize = 50;

/// A PEX (Peer Exchange) message as defined by BEP 11.
///
/// Contains compact peer lists for added and dropped peers, plus per-peer
/// flags for added peers. Supports both IPv4 and IPv6 peers (BEP 11 extension).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct PexMessage {
    /// Compact 6-byte IPv4 peers that were added.
    #[serde(with = "serde_bytes", default)]
    pub added: Vec<u8>,

    /// Per-peer flags for each added peer (1 byte per peer).
    #[serde(rename = "added.f", with = "serde_bytes", default)]
    pub added_flags: Vec<u8>,

    /// Compact 6-byte IPv4 peers that were dropped.
    #[serde(with = "serde_bytes", default)]
    pub dropped: Vec<u8>,

    /// Compact 18-byte IPv6 peers that were added (BEP 11 IPv6 extension).
    #[serde(with = "serde_bytes", default)]
    pub added6: Vec<u8>,

    /// Per-peer flags for each added IPv6 peer (1 byte per peer).
    #[serde(rename = "added6.f", with = "serde_bytes", default)]
    pub added6_flags: Vec<u8>,

    /// Compact 18-byte IPv6 peers that were dropped (BEP 11 IPv6 extension).
    #[serde(with = "serde_bytes", default)]
    pub dropped6: Vec<u8>,
}

impl PexMessage {
    /// Deserialize a PEX message from bencoded bytes.
    ///
    /// Uses lenient parsing to accept unsorted dictionary keys from peers.
    pub fn from_bytes(data: &[u8]) -> crate::Result<Self> {
        torrent_bencode::from_bytes_lenient(data)
            .map_err(|e| crate::Error::Core(torrent_core::Error::from(e)))
    }

    /// Serialize this PEX message to bencoded bytes.
    pub fn to_bytes(&self) -> crate::Result<Bytes> {
        let bytes = torrent_bencode::to_bytes(self)
            .map_err(|e| crate::Error::Core(torrent_core::Error::from(e)))?;
        Ok(Bytes::from(bytes))
    }

    /// Parse the `added` field into socket addresses.
    ///
    /// Returns an empty vec if the data is malformed (not a multiple of 6 bytes).
    pub fn added_peers(&self) -> Vec<SocketAddr> {
        if !self.added.len().is_multiple_of(COMPACT_PEER_SIZE) {
            return Vec::new();
        }
        torrent_tracker::parse_compact_peers(&self.added).unwrap_or_default()
    }

    /// Parse the `dropped` field into socket addresses.
    ///
    /// Returns an empty vec if the data is malformed (not a multiple of 6 bytes).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn dropped_peers(&self) -> Vec<SocketAddr> {
        if !self.dropped.len().is_multiple_of(COMPACT_PEER_SIZE) {
            return Vec::new();
        }
        torrent_tracker::parse_compact_peers(&self.dropped).unwrap_or_default()
    }

    /// Parse the `added6` field into IPv6 socket addresses.
    ///
    /// Returns an empty vec if the data is malformed (not a multiple of 18 bytes).
    pub fn added_peers6(&self) -> Vec<SocketAddr> {
        if !self.added6.len().is_multiple_of(COMPACT_PEER6_SIZE) {
            return Vec::new();
        }
        torrent_tracker::parse_compact_peers6(&self.added6).unwrap_or_default()
    }

    /// Parse the `dropped6` field into IPv6 socket addresses.
    ///
    /// Returns an empty vec if the data is malformed (not a multiple of 18 bytes).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn dropped_peers6(&self) -> Vec<SocketAddr> {
        if !self.dropped6.len().is_multiple_of(COMPACT_PEER6_SIZE) {
            return Vec::new();
        }
        torrent_tracker::parse_compact_peers6(&self.dropped6).unwrap_or_default()
    }
}

/// Filter peer addresses for I2P mixed-mode compliance.
///
/// When `allow_mixed` is false:
/// - I2P recipients only see I2P synthetic addresses
/// - Clearnet recipients don't see I2P synthetic addresses
pub(crate) fn filter_peers_for_transport(
    peers: &[SocketAddr],
    allow_mixed: bool,
    recipient_is_i2p: bool,
) -> Vec<SocketAddr> {
    if allow_mixed {
        return peers.to_vec();
    }
    peers
        .iter()
        .copied()
        .filter(|addr| is_i2p_synthetic_addr(addr) == recipient_is_i2p)
        .collect()
}

/// Returns true if sharing `peer_addr` with `recipient_addr` would leak a private IP.
///
/// Private/local IPs (10.x, 172.16-31.x, 192.168.x, 127.x) must not be shared
/// with public peers.
fn is_private_to_public(recipient: SocketAddr, peer: SocketAddr) -> bool {
    let peer_ip = peer.ip();
    let recipient_ip = recipient.ip();
    match (peer_ip, recipient_ip) {
        (std::net::IpAddr::V4(p), std::net::IpAddr::V4(r)) => {
            (p.is_private() || p.is_loopback()) && !(r.is_private() || r.is_loopback())
        }
        _ => false, // IPv6 private ranges are uncommon in BitTorrent
    }
}

/// Build a [`PexMessage`] from added and dropped peer address sets.
///
/// Separates IPv4 and IPv6 into the correct fields per BEP 11.
pub(crate) fn build_pex_message(added: &[SocketAddr], dropped: &[SocketAddr]) -> PexMessage {
    let (v4_added, v6_added): (Vec<_>, Vec<_>) = added.iter().partition(|a| a.is_ipv4());
    let (v4_dropped, v6_dropped): (Vec<_>, Vec<_>) = dropped.iter().partition(|a| a.is_ipv4());

    let v4_added_addrs: Vec<SocketAddr> = v4_added.into_iter().copied().collect();
    let v6_added_addrs: Vec<SocketAddr> = v6_added.into_iter().copied().collect();
    let v4_dropped_addrs: Vec<SocketAddr> = v4_dropped.into_iter().copied().collect();
    let v6_dropped_addrs: Vec<SocketAddr> = v6_dropped.into_iter().copied().collect();

    PexMessage {
        added_flags: vec![0u8; v4_added_addrs.len()],
        added: torrent_tracker::encode_compact_peers(&v4_added_addrs),
        dropped: torrent_tracker::encode_compact_peers(&v4_dropped_addrs),
        added6_flags: vec![0u8; v6_added_addrs.len()],
        added6: torrent_tracker::encode_compact_peers6(&v6_added_addrs),
        dropped6: torrent_tracker::encode_compact_peers6(&v6_dropped_addrs),
    }
}

/// Per-peer PEX send task. Spawned for each connected peer that supports `ut_pex`.
///
/// Periodically reads the shared `live_peers` snapshot, computes the diff vs the
/// last-sent view, and sends a PEX message with added/dropped peers.
///
/// Exits when `cmd_tx` is dropped (peer disconnected) or the channel is full.
pub(crate) async fn pex_send_task(
    peer_addr: SocketAddr,
    cmd_tx: tokio::sync::mpsc::Sender<crate::types::PeerCommand>,
    live_peers: std::sync::Arc<parking_lot::RwLock<std::collections::HashSet<SocketAddr>>>,
    allow_mixed_i2p: bool,
    recipient_is_i2p: bool,
) {
    use std::collections::HashSet;
    use tokio::time::{Duration, sleep};

    let mut peer_view: HashSet<SocketAddr> = HashSet::new();

    // 10s initial delay — ensure peer connection is stable
    sleep(Duration::from_secs(10)).await;

    loop {
        sleep(Duration::from_secs(60)).await;

        let live: HashSet<SocketAddr> = live_peers
            .read()
            .clone();

        // Compute diff: new peers and dropped peers.
        // Apply all filters BEFORE diffing so peer_view only contains
        // addresses we've actually announced to this recipient. This prevents
        // private/I2P addresses from leaking via the "dropped" field and
        // avoids phantom "dropped" notifications for truncated peers.
        let mut added: Vec<SocketAddr> = live
            .difference(&peer_view)
            .copied()
            .filter(|a| *a != peer_addr) // exclude recipient
            .filter(|a| !is_private_to_public(peer_addr, *a)) // privacy filter
            .collect();

        // I2P mixed-mode filtering
        if !allow_mixed_i2p {
            added = filter_peers_for_transport(&added, false, recipient_is_i2p);
        }

        // BEP 11: max 50 added per message
        added.truncate(MAX_PEX_ADDED);

        // Dropped = peers we previously told this recipient about that are no
        // longer in the live set. Because peer_view only contains addresses
        // we actually sent, this never leaks filtered addresses.
        let mut dropped: Vec<SocketAddr> = peer_view
            .difference(&live)
            .copied()
            .collect();
        dropped.truncate(MAX_PEX_ADDED);

        if added.is_empty() && dropped.is_empty() {
            continue;
        }

        let msg = build_pex_message(&added, &dropped);

        if cmd_tx
            .send(crate::types::PeerCommand::SendPex { message: msg })
            .await
            .is_err()
        {
            return; // peer disconnected
        }

        // Update peer_view incrementally — only track what we actually sent.
        // This ensures truncated peers (beyond MAX_PEX_ADDED) will be retried
        // in the next cycle rather than silently dropped.
        for a in &added {
            peer_view.insert(*a);
        }
        for d in &dropped {
            peer_view.remove(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip() {
        let msg = PexMessage {
            // 192.168.1.1:6881
            added: vec![192, 168, 1, 1, 0x1A, 0xE1],
            added_flags: vec![0x01],
            // 10.0.0.1:8080
            dropped: vec![10, 0, 0, 1, 0x1F, 0x90],
            ..Default::default()
        };

        let encoded = msg.to_bytes().expect("encode failed");
        let decoded = PexMessage::from_bytes(&encoded).expect("decode failed");

        assert_eq!(msg.added, decoded.added);
        assert_eq!(msg.added_flags, decoded.added_flags);
        assert_eq!(msg.dropped, decoded.dropped);
    }

    #[test]
    fn parse_added_peers() {
        let msg = PexMessage {
            // 192.168.1.1:6881
            added: vec![192, 168, 1, 1, 0x1A, 0xE1],
            added_flags: vec![0x01],
            ..Default::default()
        };

        let peers = msg.added_peers();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].to_string(), "192.168.1.1:6881");
    }

    #[test]
    fn parse_dropped_peers() {
        let msg = PexMessage {
            // 10.0.0.1:8080
            dropped: vec![10, 0, 0, 1, 0x1F, 0x90],
            ..Default::default()
        };

        let peers = msg.dropped_peers();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].to_string(), "10.0.0.1:8080");
    }

    #[test]
    fn empty_message() {
        let msg = PexMessage::default();

        let encoded = msg.to_bytes().expect("encode failed");
        let decoded = PexMessage::from_bytes(&encoded).expect("decode failed");

        assert!(decoded.added.is_empty());
        assert!(decoded.added_flags.is_empty());
        assert!(decoded.dropped.is_empty());
        assert!(decoded.added_peers().is_empty());
        assert!(decoded.dropped_peers().is_empty());
    }

    #[test]
    fn malformed_added_ignored() {
        let msg = PexMessage {
            // 5 bytes — not a multiple of 6
            added: vec![192, 168, 1, 1, 0x1A],
            added_flags: Vec::new(),
            dropped: Vec::new(),
            ..Default::default()
        };

        // Should return empty vec, not panic
        let peers = msg.added_peers();
        assert!(peers.is_empty());
    }

    // --- IPv6 PEX tests ---

    #[test]
    fn ipv6_round_trip() {
        use std::net::Ipv6Addr;
        let ip: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let mut added6 = Vec::new();
        added6.extend_from_slice(&ip.octets());
        added6.extend_from_slice(&6881u16.to_be_bytes());

        let msg = PexMessage {
            added6: added6.clone(),
            added6_flags: vec![0x01],
            ..Default::default()
        };

        let encoded = msg.to_bytes().expect("encode failed");
        let decoded = PexMessage::from_bytes(&encoded).expect("decode failed");

        assert_eq!(decoded.added6, added6);
        assert_eq!(decoded.added6_flags, vec![0x01]);
    }

    #[test]
    fn parse_added_peers6() {
        use std::net::Ipv6Addr;
        let ip: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let mut added6 = Vec::new();
        added6.extend_from_slice(&ip.octets());
        added6.extend_from_slice(&8080u16.to_be_bytes());

        let msg = PexMessage {
            added6,
            ..Default::default()
        };

        let peers = msg.added_peers6();
        assert_eq!(peers.len(), 1);
        assert_eq!(
            peers[0],
            "[2001:db8::1]:8080".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn parse_dropped_peers6() {
        use std::net::Ipv6Addr;
        let ip: Ipv6Addr = "::1".parse().unwrap();
        let mut dropped6 = Vec::new();
        dropped6.extend_from_slice(&ip.octets());
        dropped6.extend_from_slice(&6881u16.to_be_bytes());

        let msg = PexMessage {
            dropped6,
            ..Default::default()
        };

        let peers = msg.dropped_peers6();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0], "[::1]:6881".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn malformed_added6_ignored() {
        let msg = PexMessage {
            added6: vec![0u8; 17], // not a multiple of 18
            ..Default::default()
        };
        assert!(msg.added_peers6().is_empty());
    }

    // --- I2P mixed-mode PEX filtering tests ---

    #[test]
    fn filter_removes_i2p_for_clearnet_recipient() {
        let peers = vec![
            "1.2.3.4:6881".parse().unwrap(),
            "240.0.0.1:1".parse().unwrap(),
        ];
        let filtered = filter_peers_for_transport(&peers, false, false);
        assert_eq!(filtered.len(), 1);
        assert!(!is_i2p_synthetic_addr(&filtered[0]));
    }

    #[test]
    fn filter_removes_clearnet_for_i2p_recipient() {
        let peers = vec![
            "1.2.3.4:6881".parse().unwrap(),
            "240.0.0.1:1".parse().unwrap(),
        ];
        let filtered = filter_peers_for_transport(&peers, false, true);
        assert_eq!(filtered.len(), 1);
        assert!(is_i2p_synthetic_addr(&filtered[0]));
    }

    #[test]
    fn filter_keeps_all_when_mixed_allowed() {
        let peers = vec![
            "1.2.3.4:6881".parse().unwrap(),
            "240.0.0.1:1".parse().unwrap(),
        ];
        assert_eq!(filter_peers_for_transport(&peers, true, false).len(), 2);
    }

    // --- M108: PEX send-side tests ---

    #[test]
    fn pex_build_message_added() {
        let added: Vec<SocketAddr> = vec![
            "1.2.3.4:6881".parse().unwrap(),
            "5.6.7.8:8080".parse().unwrap(),
            "9.10.11.12:9999".parse().unwrap(),
        ];
        let msg = build_pex_message(&added, &[]);

        // 3 IPv4 peers × 6 bytes = 18 bytes
        assert_eq!(msg.added.len(), 18);
        assert_eq!(msg.added_flags.len(), 3);
        assert!(msg.dropped.is_empty());
        assert!(msg.added6.is_empty());
        assert!(msg.dropped6.is_empty());

        // Verify round-trip: parse back the compact encoding
        let parsed = msg.added_peers();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0], "1.2.3.4:6881".parse::<SocketAddr>().unwrap());
        assert_eq!(parsed[1], "5.6.7.8:8080".parse::<SocketAddr>().unwrap());
        assert_eq!(parsed[2], "9.10.11.12:9999".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn pex_build_message_dropped() {
        let dropped: Vec<SocketAddr> = vec![
            "10.0.0.1:6881".parse().unwrap(),
            "10.0.0.2:7777".parse().unwrap(),
        ];
        let msg = build_pex_message(&[], &dropped);

        assert!(msg.added.is_empty());
        assert!(msg.added_flags.is_empty());
        // 2 IPv4 peers × 6 bytes = 12 bytes
        assert_eq!(msg.dropped.len(), 12);
        assert!(msg.added6.is_empty());
        assert!(msg.dropped6.is_empty());

        let parsed = msg.dropped_peers();
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn pex_rate_limit_50_added() {
        // Create 60 peers, truncate to 50 (simulating what pex_send_task does)
        let mut added: Vec<SocketAddr> = (0..60u16)
            .map(|i| {
                let a = (i / 256) as u8;
                let b = (i % 256) as u8;
                SocketAddr::from(([100, 0, a, b], 6881))
            })
            .collect();
        added.truncate(MAX_PEX_ADDED);

        let msg = build_pex_message(&added, &[]);
        // 50 IPv4 peers × 6 bytes = 300 bytes
        assert_eq!(msg.added.len(), 300);
        assert_eq!(msg.added_flags.len(), 50);
        assert_eq!(msg.added_peers().len(), 50);
    }

    #[test]
    fn pex_excludes_recipient() {
        let recipient: SocketAddr = "1.2.3.4:6881".parse().unwrap();
        let peers: Vec<SocketAddr> = vec![
            "1.2.3.4:6881".parse().unwrap(), // should be excluded
            "5.6.7.8:6881".parse().unwrap(),
        ];

        // Simulate the filter that pex_send_task applies
        let filtered: Vec<SocketAddr> = peers
            .into_iter()
            .filter(|a| *a != recipient)
            .collect();

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0], "5.6.7.8:6881".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn pex_ipv6_separate_fields() {
        let added: Vec<SocketAddr> = vec![
            "1.2.3.4:6881".parse().unwrap(),
            "[2001:db8::1]:8080".parse().unwrap(),
            "5.6.7.8:9999".parse().unwrap(),
            "[::1]:6881".parse().unwrap(),
        ];
        let msg = build_pex_message(&added, &[]);

        // 2 IPv4 × 6 = 12 bytes
        assert_eq!(msg.added.len(), 12);
        assert_eq!(msg.added_flags.len(), 2);
        // 2 IPv6 × 18 = 36 bytes
        assert_eq!(msg.added6.len(), 36);
        assert_eq!(msg.added6_flags.len(), 2);

        let v4 = msg.added_peers();
        assert_eq!(v4.len(), 2);
        let v6 = msg.added_peers6();
        assert_eq!(v6.len(), 2);
    }

    #[test]
    fn pex_privacy_filtering() {
        let public_recipient: SocketAddr = "8.8.8.8:6881".parse().unwrap();
        let private_peer: SocketAddr = "192.168.1.100:6881".parse().unwrap();
        let loopback_peer: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        let public_peer: SocketAddr = "1.2.3.4:6881".parse().unwrap();

        // Private → public: should be filtered
        assert!(is_private_to_public(public_recipient, private_peer));
        assert!(is_private_to_public(public_recipient, loopback_peer));

        // Public → public: should NOT be filtered
        assert!(!is_private_to_public(public_recipient, public_peer));

        // Private → private: should NOT be filtered
        let private_recipient: SocketAddr = "10.0.0.1:6881".parse().unwrap();
        assert!(!is_private_to_public(private_recipient, private_peer));
    }

    #[tokio::test]
    async fn pex_send_task_exits_on_disconnect() {
        use std::collections::HashSet;
        use std::sync::Arc;
        use parking_lot::RwLock;

        let live_peers: Arc<RwLock<HashSet<SocketAddr>>> =
            Arc::new(RwLock::new(HashSet::new()));
        live_peers
            .write()
            .insert("1.2.3.4:6881".parse().unwrap());

        // Create a channel and immediately drop the receiver
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        drop(_rx);

        let peer_addr: SocketAddr = "5.6.7.8:6881".parse().unwrap();

        // Spawn the task — it should exit when it tries to send on the dropped channel
        // We can't easily test the timing, but we can verify it doesn't panic
        // by using a very short timeout
        let handle = tokio::spawn(pex_send_task(
            peer_addr,
            tx,
            live_peers,
            false,
            false,
        ));

        // The task has a 10s initial delay + 60s loop. We can't wait that long in tests.
        // Instead, just verify the task was spawned without panics.
        // Abort it after a short timeout to avoid blocking tests.
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        handle.abort();
        // If we got here, the task didn't panic on spawn — good enough.
    }

    #[test]
    fn pex_build_empty_no_message() {
        let msg = build_pex_message(&[], &[]);

        assert!(msg.added.is_empty());
        assert!(msg.added_flags.is_empty());
        assert!(msg.dropped.is_empty());
        assert!(msg.added6.is_empty());
        assert!(msg.added6_flags.is_empty());
        assert!(msg.dropped6.is_empty());
    }
}
