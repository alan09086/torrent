//! Peer Exchange (PEX) message encoding and decoding (BEP 11).
//!
//! PEX messages are bencoded dictionaries exchanged between peers to share
//! information about other peers in the swarm.

use std::net::SocketAddr;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// Size of a single compact IPv4 peer entry (4 bytes IP + 2 bytes port).
const COMPACT_PEER_SIZE: usize = 6;

/// Size of a single compact IPv6 peer entry (16 bytes IP + 2 bytes port).
const COMPACT_PEER6_SIZE: usize = 18;

/// A PEX (Peer Exchange) message as defined by BEP 11.
///
/// Contains compact peer lists for added and dropped peers, plus per-peer
/// flags for added peers. Supports both IPv4 and IPv6 peers (BEP 11 extension).
#[allow(dead_code)] // consumed by peer module (not yet implemented)
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

#[allow(dead_code)]
impl PexMessage {
    /// Deserialize a PEX message from bencoded bytes.
    pub fn from_bytes(data: &[u8]) -> crate::Result<Self> {
        ferrite_bencode::from_bytes(data)
            .map_err(|e| crate::Error::Core(ferrite_core::Error::from(e)))
    }

    /// Serialize this PEX message to bencoded bytes.
    pub fn to_bytes(&self) -> crate::Result<Bytes> {
        let bytes = ferrite_bencode::to_bytes(self)
            .map_err(|e| crate::Error::Core(ferrite_core::Error::from(e)))?;
        Ok(Bytes::from(bytes))
    }

    /// Parse the `added` field into socket addresses.
    ///
    /// Returns an empty vec if the data is malformed (not a multiple of 6 bytes).
    pub fn added_peers(&self) -> Vec<SocketAddr> {
        if !self.added.len().is_multiple_of(COMPACT_PEER_SIZE) {
            return Vec::new();
        }
        ferrite_tracker::parse_compact_peers(&self.added).unwrap_or_default()
    }

    /// Parse the `dropped` field into socket addresses.
    ///
    /// Returns an empty vec if the data is malformed (not a multiple of 6 bytes).
    pub fn dropped_peers(&self) -> Vec<SocketAddr> {
        if !self.dropped.len().is_multiple_of(COMPACT_PEER_SIZE) {
            return Vec::new();
        }
        ferrite_tracker::parse_compact_peers(&self.dropped).unwrap_or_default()
    }

    /// Parse the `added6` field into IPv6 socket addresses.
    ///
    /// Returns an empty vec if the data is malformed (not a multiple of 18 bytes).
    pub fn added_peers6(&self) -> Vec<SocketAddr> {
        if !self.added6.len().is_multiple_of(COMPACT_PEER6_SIZE) {
            return Vec::new();
        }
        ferrite_tracker::parse_compact_peers6(&self.added6).unwrap_or_default()
    }

    /// Parse the `dropped6` field into IPv6 socket addresses.
    ///
    /// Returns an empty vec if the data is malformed (not a multiple of 18 bytes).
    pub fn dropped_peers6(&self) -> Vec<SocketAddr> {
        if !self.dropped6.len().is_multiple_of(COMPACT_PEER6_SIZE) {
            return Vec::new();
        }
        ferrite_tracker::parse_compact_peers6(&self.dropped6).unwrap_or_default()
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
}
