use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use torrent_core::Id20;

use crate::error::{Error, Result};

/// A DHT node: 20-byte ID + IPv4 socket address.
///
/// Encoded as 26 bytes: 20-byte node ID, 4-byte IPv4, 2-byte port (big-endian).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactNodeInfo {
    /// 20-byte node ID.
    pub id: Id20,
    /// IPv4 socket address (IP + port).
    pub addr: SocketAddr,
}

/// Size of one compact node info entry.
pub const COMPACT_NODE_SIZE: usize = 26;

impl CompactNodeInfo {
    /// Encode to 26 bytes.
    pub fn to_bytes(&self) -> [u8; COMPACT_NODE_SIZE] {
        let mut buf = [0u8; COMPACT_NODE_SIZE];
        buf[..20].copy_from_slice(self.id.as_bytes());
        match self.addr {
            SocketAddr::V4(v4) => {
                buf[20..24].copy_from_slice(&v4.ip().octets());
                buf[24..26].copy_from_slice(&v4.port().to_be_bytes());
            }
            SocketAddr::V6(_) => {
                // BEP 5 specifies IPv4 only; IPv6 nodes silently encode as 0.0.0.0:0
            }
        }
        buf
    }

    /// Decode from exactly 26 bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() != COMPACT_NODE_SIZE {
            return Err(Error::InvalidCompactNode(format!(
                "expected {COMPACT_NODE_SIZE} bytes, got {}",
                data.len()
            )));
        }
        let id = Id20::from_bytes(&data[..20])
            .map_err(|e| Error::InvalidCompactNode(e.to_string()))?;
        let ip = Ipv4Addr::new(data[20], data[21], data[22], data[23]);
        let port = u16::from_be_bytes([data[24], data[25]]);
        Ok(CompactNodeInfo {
            id,
            addr: SocketAddr::V4(SocketAddrV4::new(ip, port)),
        })
    }
}

/// Decode a byte slice of concatenated 26-byte compact node infos.
pub fn parse_compact_nodes(data: &[u8]) -> Result<Vec<CompactNodeInfo>> {
    if !data.len().is_multiple_of(COMPACT_NODE_SIZE) {
        return Err(Error::InvalidCompactNode(format!(
            "compact nodes length {} is not a multiple of {COMPACT_NODE_SIZE}",
            data.len()
        )));
    }
    let mut nodes = Vec::with_capacity(data.len() / COMPACT_NODE_SIZE);
    for chunk in data.chunks_exact(COMPACT_NODE_SIZE) {
        nodes.push(CompactNodeInfo::from_bytes(chunk)?);
    }
    Ok(nodes)
}

/// Encode a slice of compact node infos into bytes.
pub fn encode_compact_nodes(nodes: &[CompactNodeInfo]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(nodes.len() * COMPACT_NODE_SIZE);
    for node in nodes {
        buf.extend_from_slice(&node.to_bytes());
    }
    buf
}

/// A DHT node: 20-byte ID + IPv6 socket address.
///
/// Encoded as 38 bytes: 20-byte node ID, 16-byte IPv6, 2-byte port (big-endian).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactNodeInfo6 {
    /// 20-byte node ID.
    pub id: Id20,
    /// IPv6 socket address (IP + port).
    pub addr: SocketAddr,
}

/// Size of one compact IPv6 node info entry.
pub const COMPACT_NODE6_SIZE: usize = 38;

impl CompactNodeInfo6 {
    /// Encode to 38 bytes.
    pub fn to_bytes(&self) -> [u8; COMPACT_NODE6_SIZE] {
        let mut buf = [0u8; COMPACT_NODE6_SIZE];
        buf[..20].copy_from_slice(self.id.as_bytes());
        match self.addr {
            SocketAddr::V6(v6) => {
                buf[20..36].copy_from_slice(&v6.ip().octets());
                buf[36..38].copy_from_slice(&v6.port().to_be_bytes());
            }
            SocketAddr::V4(_) => {
                // BEP 24 specifies IPv6 only; IPv4 nodes silently encode as [::]:0
            }
        }
        buf
    }

    /// Decode from exactly 38 bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() != COMPACT_NODE6_SIZE {
            return Err(Error::InvalidCompactNode(format!(
                "expected {COMPACT_NODE6_SIZE} bytes, got {}",
                data.len()
            )));
        }
        let id = Id20::from_bytes(&data[..20])
            .map_err(|e| Error::InvalidCompactNode(e.to_string()))?;
        let ip = Ipv6Addr::from(<[u8; 16]>::try_from(&data[20..36]).unwrap());
        let port = u16::from_be_bytes([data[36], data[37]]);
        Ok(CompactNodeInfo6 {
            id,
            addr: SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)),
        })
    }
}

/// Decode a byte slice of concatenated 38-byte compact IPv6 node infos.
pub fn parse_compact_nodes6(data: &[u8]) -> Result<Vec<CompactNodeInfo6>> {
    if !data.len().is_multiple_of(COMPACT_NODE6_SIZE) {
        return Err(Error::InvalidCompactNode(format!(
            "compact nodes6 length {} is not a multiple of {COMPACT_NODE6_SIZE}",
            data.len()
        )));
    }
    let mut nodes = Vec::with_capacity(data.len() / COMPACT_NODE6_SIZE);
    for chunk in data.chunks_exact(COMPACT_NODE6_SIZE) {
        nodes.push(CompactNodeInfo6::from_bytes(chunk)?);
    }
    Ok(nodes)
}

/// Encode a slice of compact IPv6 node infos into bytes.
pub fn encode_compact_nodes6(nodes: &[CompactNodeInfo6]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(nodes.len() * COMPACT_NODE6_SIZE);
    for node in nodes {
        buf.extend_from_slice(&node.to_bytes());
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_node() -> CompactNodeInfo {
        CompactNodeInfo {
            id: Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap(),
            addr: "192.168.1.1:6881".parse().unwrap(),
        }
    }

    #[test]
    fn round_trip_single() {
        let node = sample_node();
        let bytes = node.to_bytes();
        assert_eq!(bytes.len(), 26);
        let decoded = CompactNodeInfo::from_bytes(&bytes).unwrap();
        assert_eq!(node, decoded);
    }

    #[test]
    fn round_trip_multiple() {
        let nodes = vec![
            sample_node(),
            CompactNodeInfo {
                id: Id20::ZERO,
                addr: "10.0.0.1:8080".parse().unwrap(),
            },
        ];
        let encoded = encode_compact_nodes(&nodes);
        assert_eq!(encoded.len(), 52);
        let decoded = parse_compact_nodes(&encoded).unwrap();
        assert_eq!(nodes, decoded);
    }

    #[test]
    fn boundary_ip_and_port() {
        let node = CompactNodeInfo {
            id: Id20::from_hex("ffffffffffffffffffffffffffffffffffffffff").unwrap(),
            addr: "255.255.255.255:65535".parse().unwrap(),
        };
        let bytes = node.to_bytes();
        let decoded = CompactNodeInfo::from_bytes(&bytes).unwrap();
        assert_eq!(node, decoded);
    }

    #[test]
    fn reject_wrong_length() {
        assert!(CompactNodeInfo::from_bytes(&[0u8; 25]).is_err());
        assert!(parse_compact_nodes(&[0u8; 27]).is_err());
    }

    #[test]
    fn empty_compact_nodes() {
        let nodes = parse_compact_nodes(&[]).unwrap();
        assert!(nodes.is_empty());
    }

    // --- CompactNodeInfo6 (IPv6) ---

    fn sample_node6() -> CompactNodeInfo6 {
        CompactNodeInfo6 {
            id: Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap(),
            addr: "[2001:db8::1]:6881".parse().unwrap(),
        }
    }

    #[test]
    fn round_trip_single_v6() {
        let node = sample_node6();
        let bytes = node.to_bytes();
        assert_eq!(bytes.len(), 38);
        let decoded = CompactNodeInfo6::from_bytes(&bytes).unwrap();
        assert_eq!(node, decoded);
    }

    #[test]
    fn round_trip_multiple_v6() {
        let nodes = vec![
            sample_node6(),
            CompactNodeInfo6 {
                id: Id20::ZERO,
                addr: "[::1]:8080".parse().unwrap(),
            },
        ];
        let encoded = encode_compact_nodes6(&nodes);
        assert_eq!(encoded.len(), 76);
        let decoded = parse_compact_nodes6(&encoded).unwrap();
        assert_eq!(nodes, decoded);
    }

    #[test]
    fn boundary_v6() {
        let node = CompactNodeInfo6 {
            id: Id20::from_hex("ffffffffffffffffffffffffffffffffffffffff").unwrap(),
            addr: "[ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff]:65535"
                .parse()
                .unwrap(),
        };
        let bytes = node.to_bytes();
        let decoded = CompactNodeInfo6::from_bytes(&bytes).unwrap();
        assert_eq!(node, decoded);
    }

    #[test]
    fn reject_wrong_length_v6() {
        assert!(CompactNodeInfo6::from_bytes(&[0u8; 37]).is_err());
        assert!(parse_compact_nodes6(&[0u8; 39]).is_err());
    }

    #[test]
    fn empty_compact_nodes6() {
        let nodes = parse_compact_nodes6(&[]).unwrap();
        assert!(nodes.is_empty());
    }
}
